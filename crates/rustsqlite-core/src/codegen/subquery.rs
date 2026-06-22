//! `FROM (subquery)` materialization (mirrors the `SRT_EphemTab` path in `select.c`).
//!
//! The subquery's result rows are written into an in-memory ephemeral b-tree
//! ([`crate::vdbe::ephemeral::Ephemeral`] opened via `OP_OpenEphemeral`), and the outer
//! `SELECT` scans that ephemeral as if it were a regular table. This is the simplest shape
//! upstream supports for `FROM (SELECT ...)` — the `sqlite3Select` "materialize" path
//! (`tag-select-0488`) compiled with `SRT_EphemTab`.
//!
//! The subquery body is compiled in-line: its `ResultRow` instructions are rewritten into
//! `MakeRecord + Insert` (with a `NewRowid` to allocate the rowid) so each yielded row is
//! appended to the ephemeral cursor. After the subquery completes, the outer scan runs
//! against the same cursor (the ephemeral supports `Rewind`/`Next`/`Column`).
//!
//! Only the simplest outer shape is supported: a single-table scan / aggregate / constant
//! projection over the materialized subquery. Index access, joins, and other multi-table
//! shapes land with later milestones.

use rustqlite_parser::{Expr, SelectStmt, TableOrJoin};

use crate::error::{Error, Result};
use crate::schema::{Column, IndexObject, Table};
use crate::types::{Affinity, Collation};
use crate::vdbe::program::{Instruction, Program, P4};
use crate::vdbe::Opcode;

use super::builder::{Label, ProgramBuilder};
use super::expr::{compile_expr, compile_jump, Ctx};
use super::select::{
    self, eval_limit_offset, expand_columns, resolve_order_term, emit_int,
};

/// Compile `SELECT ... FROM (subquery) AS alias [...]` by materializing `subquery` into an
/// ephemeral table and then scanning that ephemeral as the outer SELECT's source.
///
/// `subquery` is the inner `SelectStmt`; `subquery_table`/`subquery_indexes` describe the
/// inner FROM table (if any) so the inner body can be compiled. The outer SELECT is compiled
/// against a synthesized [`Table`] whose columns match the subquery's output column names.
#[allow(clippy::too_many_arguments)]
pub fn compile_from_subquery(
    outer: &SelectStmt,
    subquery: &SelectStmt,
    _alias: &str,
    subquery_table: Option<&Table>,
    subquery_indexes: &[IndexObject],
) -> Result<(Program, Vec<String>)> {
    // Reject outer shapes the first slice does not support. The outer SELECT must not have
    // its own compound arms, and its FROM must be exactly the single subquery entry.
    if !outer.compound.is_empty() {
        return Err(Error::msg(
            "compound SELECT (UNION/INTERSECT/EXCEPT) is not supported by the executor yet",
        ));
    }
    if outer.from.len() != 1 || !matches!(outer.from[0], TableOrJoin::Subquery { .. }) {
        return Err(Error::msg("subquery materialization expects a single FROM subquery"));
    }
    // The outer FROM clause must not be a join — only one subquery entry is allowed.
    // (A subquery mixed with other FROM entries is a join and lands with M7+.)

    // 1. Derive the subquery's output column names. These become the synthesized table's
    //    columns. The subquery is expanded as a standalone SELECT against its own FROM table
    //    (or as a VALUES/constant select when it has no FROM).
    let inner_outputs = expand_columns(subquery, subquery_table)?;
    let inner_names: Vec<String> = inner_outputs.iter().map(|(_, n)| n.clone()).collect();
    let inner_ncol = inner_outputs.len() as i32;

    // 2. Synthesize the outer Table. The columns inherit BLOB affinity (no coercion), like a
    //    subquery result in SQLite. There is no rowid alias and no WITHOUT ROWID storage —
    //    the ephemeral is a rowid-keyed table.
    let outer_table = Table {
        name: String::new(),
        rootpage: 0,
        columns: inner_names
            .iter()
            .map(|n| Column {
                name: n.clone(),
                affinity: Affinity::Blob,
                collation: Collation::Binary,
                notnull: false,
                pk: false,
                default: None,
            })
            .collect(),
        rowid_alias: None,
        without_rowid: false,
        pk_columns: Vec::new(),
    };

    // 3. Expand the outer SELECT's projection against the synthesized table.
    let outputs = expand_columns_for_outer(outer, &outer_table)?;
    let names: Vec<String> = outputs.iter().map(|(_, n)| n.clone()).collect();
    let (limit, offset) = eval_limit_offset(outer)?;
    let ncol = outputs.len() as i32;

    // 4. Build the program: prologue, ephemeral open, subquery materialization, outer scan.
    // The ephemeral cursor lives at a high cursor number so it cannot collide with any cursor
    // the subquery body opens (table=0, sorter=1, distinct=2 in the current codegen) or that
    // the outer scan opens (sorter=1, distinct=2). Cursor 10 is well clear of both.
    let ephemeral_cursor = 10i32;
    let ctx = Ctx {
        table: &outer_table,
        cursor: ephemeral_cursor,
        register_base: None,
        index_read: None,
        join_tables: None,
        subquery_resolver: None,
    };
    let mut b = ProgramBuilder::new();

    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    let after_init = b.cur_addr();

    // LIMIT 0 → no rows at all (mirrors compile_scan).
    if limit == Some(0) {
        b.emit(Opcode::Halt, 0, 0, 0);
        b.resolve(setup);
        b.emit(Opcode::Transaction, 0, 0, 0);
        b.emit(Opcode::Goto, 0, after_init, 0);
        return Ok((b.finish(), names));
    }

    // Open the ephemeral table cursor (rowid-keyed, no KeyInfo P4). Each row holds
    // `inner_ncol` columns matching the subquery's projection.
    let oe = b.emit(Opcode::OpenEphemeral, ephemeral_cursor, inner_ncol, 0);
    // No KeyInfo → table variant (rowid-keyed), matching the default in `OP_OpenEphemeral`.
    b.note_cursor(ephemeral_cursor);
    let _ = oe;

    // --- Materialize the subquery into the ephemeral. ---
    // Compile the subquery body as a sub-program, then inline its instructions. The subquery
    // program has the shape `Init; <scan code>; Halt; <setup: Transaction? + Goto>`. We inline
    // ONLY the scan code (skipping the leading `Init` and everything from `Halt` onward) so the
    // outer program's own `Init`/`Transaction`/setup remain canonical and no stray `Goto` loops
    // back into the inlined scan. Each `ResultRow` is rewritten into
    // `MakeRecord + NewRowid + Insert` to append the row to the ephemeral cursor.
    //
    // Because `ResultRow` expands to multiple instructions, the inlined addresses do NOT map
    // 1:1 to the subquery's addresses with a constant offset. We build an address map
    // (`sub_addr -> inlined_addr`) as we inline, then patch every jump's `p2` using it. Jumps
    // targeting the subquery's `Halt` (the scan-end label) are redirected to `after_sub` so an
    // empty subquery or scan exhaustion falls through to the outer scan.
    let (sub_program, _sub_names) = select::compile(subquery, subquery_table, subquery_indexes, None)?;

    // The address at which the inlined subquery scan code begins (after `Init` + `OpenEphemeral`
    // in the outer program). Used to bound the jump-patch loop below.
    let sub_start = b.cur_addr();

    // Find the `Halt` that terminates the scan code (the first Halt after the Init). Everything
    // from the Halt onward is the subquery's setup block (Halt, Transaction?, Goto) — we skip it.
    let halt_idx = sub_program
        .instructions
        .iter()
        .position(|i| i.opcode == Opcode::Halt)
        .ok_or_else(|| Error::msg("subquery program has no Halt"))?;

    // `after_sub` is the continuation into the outer scan, resolved at the end of the inlined
    // scan block. Jumps that targeted the subquery's `Halt` are redirected here.
    let after_sub = b.new_label();

    // Address map: subquery_addr -> inlined_addr. Built as we inline each instruction.
    // `ResultRow` expands to a 5-instruction sequence (SCopy*ncol, MakeRecord, NewRowid, Insert)
    // plus the per-row padding Nulls; the map entry for the subquery's ResultRow address points
    // to the first emitted instruction of the expansion (so any jump landing on a ResultRow
    // would resume the rewrite — though no jump should target a ResultRow in practice).
    let mut addr_map: std::collections::HashMap<i32, i32> = std::collections::HashMap::new();

    // Inline scan code: indices 1..halt_idx (skipping the leading Init at index 0).
    // The subquery's idx 0 is its `Init`; jumps inside the subquery never target it (it's the
    // entry point), so we leave it unmapped.
    for idx in 1..halt_idx {
        let inst = &sub_program.instructions[idx];
        let sub_addr = idx as i32;
        let inlined_addr = b.cur_addr();
        addr_map.insert(sub_addr, inlined_addr);
        match inst.opcode {
            Opcode::ResultRow => {
                let result_start = inst.p1;
                let nres = inst.p2;
                // Build a record of the subquery's output columns, padding short rows with NULL.
                let block = b.alloc_regs(inner_ncol);
                for j in 0..nres.min(inner_ncol) {
                    b.emit(Opcode::SCopy, result_start + j, block + j, 0);
                }
                for j in nres..inner_ncol {
                    b.emit(Opcode::Null, 0, block + j, 0);
                }
                let rec = b.alloc_reg();
                b.emit(Opcode::MakeRecord, block, inner_ncol, rec);
                let rowid_reg = b.alloc_reg();
                b.emit(Opcode::NewRowid, ephemeral_cursor, rowid_reg, 0);
                b.emit(Opcode::Insert, ephemeral_cursor, rec, rowid_reg);
            }
            _ => {
                // Defer jump fixup: copy the instruction with p2 unchanged; we patch it after
                // the map is complete (so forward jumps inside the scan block resolve).
                b.append(inst.clone());
            }
        }
    }

    // Resolve `after_sub` to the next emitted instruction (the outer scan's first opcode).
    b.resolve(after_sub);

    // LIMIT / OFFSET counter registers for the outer scan. Allocated AFTER the subquery
    // inlining so the subquery's own registers (1..N) cannot collide with them.
    let limit_reg = match limit {
        Some(n) if n > 0 => Some(emit_int(&mut b, n)),
        _ => None,
    };
    let offset_reg = (offset > 0).then(|| emit_int(&mut b, offset));

    // Now patch every inlined jump's `p2` using the address map. Jumps targeting the subquery's
    // `Halt` (idx == halt_idx) are redirected to `after_sub` via the label fixup machinery.
    // Only the inlined range [`sub_start`, `after_sub_addr`) is patched — the outer program's
    // own jumps (the `Init` and any outer scan jumps emitted below) are left alone.
    let after_sub_addr = b.label_addr_of(after_sub);
    let sub_start_addr = sub_start;
    for (i, inst) in b.iter_insts_mut().enumerate() {
        let addr = i as i32;
        if addr < sub_start_addr || addr >= after_sub_addr {
            continue;
        }
        if !is_absolute_jump(inst) {
            continue;
        }
        let sub_target = inst.p2;
        if sub_target == halt_idx as i32 {
            // Redirect to `after_sub`.
            inst.p2 = after_sub_addr;
        } else if let Some(&inlined) = addr_map.get(&sub_target) {
            inst.p2 = inlined;
        } else if sub_target == 0 {
            // Jumps targeting the subquery's `Init` (idx 0) are not expected inside the scan
            // code; leave them as-is defensively (they would jump to address 0 of the outer
            // program, the outer Init, which re-runs setup — a benign no-op for a read).
            inst.p2 = 0;
        } else {
            // Unknown target — should not happen for well-formed subquery programs. Leave as-is
            // rather than crash, so a debug run can surface the issue.
        }
    }

    // --- Outer scan over the ephemeral. ---
    // No `OpenRead` here — the ephemeral cursor was opened above. The scan reads via
    // `Rewind`/`Next`/`Column` which all dispatch to the `Ephemeral` variant.
    if outer.order_by.is_empty() {
        compile_outer_scan_unordered(
            &mut b,
            outer,
            ctx,
            &outputs,
            ncol,
            limit_reg,
            offset_reg,
        )?;
    } else {
        compile_outer_scan_ordered(
            &mut b,
            outer,
            ctx,
            &outputs,
            ncol,
            limit_reg,
            offset_reg,
        )?;
    }

    b.resolve(setup);
    b.emit(Opcode::Transaction, 0, 0, 0);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok((b.finish(), names))
}

/// Expand the outer SELECT's projection against the synthesized subquery-result table.
/// Reuses [`select::expand_columns`] but lives here so the call site doesn't need to import
/// the inner helper.
fn expand_columns_for_outer(
    outer: &SelectStmt,
    table: &Table,
) -> Result<Vec<(Expr, String)>> {
    expand_columns(outer, Some(table))
}

/// The unordered outer scan loop. Mirrors `compile_scan_unordered` but the cursor is the
/// already-opened ephemeral (no `OpenRead` here). The DISTINCT dedup cursor is allocated
/// past the ephemeral.
#[allow(clippy::too_many_arguments)]
fn compile_outer_scan_unordered(
    b: &mut ProgramBuilder,
    outer: &SelectStmt,
    ctx: Ctx,
    outputs: &[(Expr, String)],
    ncol: i32,
    limit_reg: Option<i32>,
    offset_reg: Option<i32>,
) -> Result<()> {
    let cursor = ctx.cursor;
    let distinct_cursor = outer.distinct.then(|| {
        let c = 2i32;
        let oe = b.emit(Opcode::OpenEphemeral, c, ncol, 0);
        b.set_p4(oe, P4::KeyInfo(Vec::new()));
        b.note_cursor(c);
        c
    });
    let end = b.new_label();
    b.emit_jump(Opcode::Rewind, cursor, end, 0);
    let loop_top = b.cur_addr();
    let next_label = b.new_label();

    if let Some(w) = &outer.where_clause {
        compile_jump(b, w, next_label, false, true, ctx)?;
    }
    let result_reg = b.alloc_regs(ncol);
    for (j, (expr, _)) in outputs.iter().enumerate() {
        compile_expr(b, expr, result_reg + j as i32, ctx)?;
    }
    if let Some(dc) = distinct_cursor {
        let found = b.emit_jump(Opcode::Found, dc, next_label, result_reg);
        b.set_p4(found, P4::Int(ncol as i64));
        let rec = b.alloc_reg();
        b.emit(Opcode::MakeRecord, result_reg, ncol, rec);
        b.emit(Opcode::IdxInsert, dc, rec, 0);
    }
    if let Some(oreg) = offset_reg {
        b.emit_jump(Opcode::IfPos, oreg, next_label, 1);
    }
    b.emit(Opcode::ResultRow, result_reg, ncol, 0);
    if let Some(lreg) = limit_reg {
        b.emit_jump(Opcode::DecrJumpZero, lreg, end, 0);
    }

    b.resolve(next_label);
    b.emit(Opcode::Next, cursor, loop_top, 0);
    b.resolve(end);
    b.emit(Opcode::Halt, 0, 0, 0);
    Ok(())
}

/// The ordered outer scan loop (sorter-backed). Mirrors `compile_scan_ordered` but the
/// scan cursor is the already-opened ephemeral.
#[allow(clippy::too_many_arguments)]
fn compile_outer_scan_ordered(
    b: &mut ProgramBuilder,
    outer: &SelectStmt,
    ctx: Ctx,
    outputs: &[(Expr, String)],
    ncol: i32,
    limit_reg: Option<i32>,
    offset_reg: Option<i32>,
) -> Result<()> {
    let cursor = ctx.cursor;
    let sorter = 1i32;
    let order = &outer.order_by;
    let nkey = order.len() as i32;

    let keyinfo: Vec<crate::vdbe::KeyField> = order
        .iter()
        .map(|t| crate::vdbe::KeyField {
            desc: t.desc,
            collation: crate::types::Collation::Binary,
        })
        .collect();
    let so = b.emit(Opcode::SorterOpen, sorter, nkey + ncol, 0);
    b.set_p4(so, P4::KeyInfo(keyinfo));

    let end_scan = b.new_label();
    b.emit_jump(Opcode::Rewind, cursor, end_scan, 0);
    let scan_top = b.cur_addr();
    let scan_next = b.new_label();

    if let Some(w) = &outer.where_clause {
        compile_jump(b, w, scan_next, false, true, ctx)?;
    }
    let block = b.alloc_regs(nkey + ncol);
    for (k, term) in order.iter().enumerate() {
        let key_expr = resolve_order_term(term, outputs)?;
        compile_expr(b, &key_expr, block + k as i32, ctx)?;
    }
    for (j, (expr, _)) in outputs.iter().enumerate() {
        compile_expr(b, expr, block + nkey + j as i32, ctx)?;
    }
    let rec = b.alloc_reg();
    b.emit(Opcode::MakeRecord, block, nkey + ncol, rec);
    b.emit(Opcode::SorterInsert, sorter, rec, 0);
    b.resolve(scan_next);
    b.emit(Opcode::Next, cursor, scan_top, 0);
    b.resolve(end_scan);

    let end_out = b.new_label();
    b.emit_jump(Opcode::SorterSort, sorter, end_out, 0);
    let out_top = b.cur_addr();
    let sort_next = b.new_label();
    b.emit(Opcode::SorterData, sorter, 0, 0);
    if let Some(oreg) = offset_reg {
        b.emit_jump(Opcode::IfPos, oreg, sort_next, 1);
    }
    let result_reg = b.alloc_regs(ncol);
    for j in 0..ncol {
        b.emit(Opcode::Column, sorter, nkey + j, result_reg + j);
    }
    b.emit(Opcode::ResultRow, result_reg, ncol, 0);
    if let Some(lreg) = limit_reg {
        b.emit_jump(Opcode::DecrJumpZero, lreg, end_out, 0);
    }
    b.resolve(sort_next);
    b.emit(Opcode::SorterNext, sorter, out_top, 0);
    b.resolve(end_out);
    b.emit(Opcode::Halt, 0, 0, 0);
    Ok(())
}

/// Whether an instruction uses `p2` as an absolute jump target that must be rebased when the
/// program is inlined into a larger program. Includes every opcode whose `p2` is a jump
/// destination in the VDBE: `Init`, `Goto`, `Gosub`, `If`, `IfNot`, `IsNull`, `NotNull`,
/// `IfPos`, `DecrJumpZero`, the comparison opcodes (`Eq`/`Ne`/`Lt`/`Le`/`Gt`/`Ge`),
/// `Rewind`, `Next`, `NotExists`, `Seek*`, `Idx*` boundary checks, `Found`/`NotFound`,
/// `SorterSort`, `SorterNext`, and the aggregate `Jump`. (Not `Halt`/`HaltIfNull` — those
/// terminate the program — nor `ResultRow`, which yields.)
fn is_absolute_jump(inst: &Instruction) -> bool {
    use Opcode::*;
    matches!(
        inst.opcode,
        Goto | Init | Gosub | If | IfNot | IsNull | NotNull | IfPos | DecrJumpZero | Eq | Ne | Lt
            | Le | Gt | Ge | Rewind | Next | NotExists | SeekGE | SeekGT | SeekLE | SeekLT
            | IdxGE | IdxGT | IdxLE | IdxLT | Found | NotFound | SorterSort | SorterNext
    )
}

/// Compile a scalar subquery `(SELECT …)` used as an expression, returning the register that
/// holds the scalar result (the first column of the first row, or NULL if no rows). Mirrors
/// `sqlite3CodeSubselect` in `expr.c` for the `TK_SELECT` case: the subquery body is compiled
/// as a subroutine (`OP_Gosub`/`OP_Return`), wrapped in `OP_Once` so a non-correlated
/// subquery runs only once per statement even if the expression is evaluated many times.
///
/// Each `ResultRow` in the inlined subquery body is rewritten into `SCopy <col0>, result_reg`
/// followed by `Goto <end_of_subroutine>` — i.e. the first yielded row's first column is
/// captured into `result_reg` and the subroutine returns immediately (the equivalent of
/// upstream's `LIMIT 1` injection). The body's `Halt` (the scan-end label) is rewritten to
/// the subroutine's `Return`.
///
/// The M8.7 first slice assumes the subquery is **non-correlated** — it must not reference
/// outer-query columns. The `OP_Once` wrapping caches the first row's result across all
/// encounters; a correlated subquery would need to re-run on each outer row (M8.11 `Param` +
/// M8.13 re-materialization). If a correlated reference is present, column resolution inside
/// the inlined body fails with "no such column" — the right error for unsupported correlation.
///
/// `subquery_table`/`subquery_indexes` describe the subquery's own FROM table (or `None` for
/// a constant / `VALUES` subquery), resolved by the caller via a [`super::expr::SubqueryResolver`].
pub fn compile_scalar_subquery(
    b: &mut ProgramBuilder,
    subquery: &SelectStmt,
    subquery_table: Option<&Table>,
    subquery_indexes: &[IndexObject],
) -> Result<i32> {
    // A scalar subquery must produce exactly one column. Upstream raises
    // "sub-select returns more than one column" for multi-column scalar subqueries; we surface
    // the same error here.
    let inner_outputs = expand_columns(subquery, subquery_table)?;
    let ncol = inner_outputs.len();
    if ncol != 1 {
        return Err(Error::msg(format!(
            "sub-select returns more than one column ({ncol})"
        )));
    }

    // Compile the subquery body first so we know its register count. The inlined body uses
    // registers 1..N (its own `ProgramBuilder` allocation); our `result_reg`/`return_reg` must
    // live ABOVE that range to avoid being clobbered by the subquery's `Column`/`Integer`/
    // comparison-register writes. We compile into a throwaway builder, extract the program,
    // then allocate our registers in the OUTER builder past the subquery's high-water mark.
    let (sub_program, _sub_names) = select::compile(subquery, subquery_table, subquery_indexes, None)?;
    let sub_num_regs = sub_program.num_registers as i32;

    // Cursor offset: the subquery's `select::compile` hardcodes cursor 0 for its table scan
    // (and 1 for a sorter, 2 for DISTINCT dedup). The outer program may already be using
    // those cursor numbers for its own scan. Offset the subquery's cursor numbers by the
    // outer builder's `next_cursor()` so they land in a free range.
    let cursor_offset = b.next_cursor();

    // Allocate the scalar-result register and the subroutine return-address register ABOVE the
    // subquery's register range. The result register is pre-filled with NULL so a subquery
    // that yields no rows leaves it NULL (matching SQLite's "NULL if the subquery returns no
    // rows" semantics).
    //
    // We reserve a contiguous block of size `sub_num_regs + 2` in the outer builder so the
    // inlined subquery body (which writes to registers 1..sub_num_regs) lands in that block
    // and our two extra registers (result, return) sit just past it. The block starts at the
    // outer builder's current `next_reg`, which becomes the inlined body's register-1 offset.
    let reg_offset = b.next_reg() - 1; // sub reg R -> outer reg reg_offset + R
    let _ = b.alloc_regs(sub_num_regs); // reserve 1..sub_num_regs (offset to outer)
    let result_reg = b.alloc_reg(); // = reg_offset + sub_num_regs + 1
    let return_reg = b.alloc_reg(); // = reg_offset + sub_num_regs + 2

    // `OP_Once` wraps the subroutine call so a non-correlated subquery runs only once. On a
    // repeat encounter, `Once` jumps past the subroutine body to the caller's continuation
    // (bound to `after_sub` below), so the cached `result_reg` is reused.
    let after_sub = b.new_label();
    b.emit_jump(Opcode::Once, 0, after_sub, 0);

    // Pre-fill the result with NULL (the no-rows case). The subroutine body will overwrite
    // this on the first yielded row.
    b.emit(Opcode::Null, 0, result_reg, 0);

    // Call the subroutine. The subroutine address is bound when `subroutine_start` is resolved
    // below (after the `Gosub`'s `Goto` is emitted, so the subroutine body physically follows
    // the `Goto`). The `Gosub` stores `pc + 1` in `r[return_reg]` — i.e. the address of the
    // `Goto after_sub` emitted next — so when the subroutine's `Return` reads `r[return_reg]`
    // it lands on that `Goto`, which skips over the subroutine body to the caller's
    // continuation.
    let subroutine_start = b.new_label();
    b.emit_jump(Opcode::Gosub, return_reg, subroutine_start, 0);
    b.emit_jump(Opcode::Goto, 0, after_sub, 0);

    // The caller copies `result_reg` into its own target register after this function returns;
    // those instructions land at `after_sub` (resolved at the end of this function).

    // --- Subroutine body: the inlined subquery scan. ---
    b.resolve(subroutine_start);

    // Find the Halt that terminates the scan code (the first Halt after the Init). Everything
    // from the Halt onward is the subquery's setup block (Halt, Transaction?, Goto) — skip it,
    // replacing the Halt with a `Return`.
    let halt_idx = sub_program
        .instructions
        .iter()
        .position(|i| i.opcode == Opcode::Halt)
        .ok_or_else(|| Error::msg("scalar subquery program has no Halt"))?;

    // The subroutine's return. `Return` with `p3 == 1` is the conditional form (jumps if
    // `r[return_reg]` is an integer), which is what upstream uses after `Gosub`. We bind a
    // label here so the inlined body's rewritten `ResultRow`s can `Goto` this address.
    let subroutine_end = b.new_label();

    // Address map: subquery_addr -> inlined_addr. Built as we inline each instruction so
    // jumps inside the body can be rebased (same approach as `compile_from_subquery`).
    let mut addr_map: std::collections::HashMap<i32, i32> = std::collections::HashMap::new();
    let sub_start = b.cur_addr();

    // Inline scan code: indices 1..halt_idx (skipping the leading Init at index 0). Every
    // register operand in the inlined instructions is rebased by `reg_offset` so the
    // subquery's register N becomes the outer program's register `reg_offset + N`, landing in
    // the block we reserved above (clear of `result_reg`/`return_reg`).
    for idx in 1..halt_idx {
        let inst = &sub_program.instructions[idx];
        let sub_addr = idx as i32;
        let inlined_addr = b.cur_addr();
        addr_map.insert(sub_addr, inlined_addr);
        match inst.opcode {
            Opcode::ResultRow => {
                // SRT_Mem rewrite: copy the first column of the yielded row into `result_reg`
                // and jump to the subroutine's `Return`. The subquery's `ResultRow p1 p2 p3`
                // has `p1` = result start register (in subquery register space) and `p2` =
                // ncol (always 1 here, validated above); the first column is at
                // `r[reg_offset + p1]`.
                b.emit(Opcode::SCopy, reg_offset + inst.p1, result_reg, 0);
                b.emit_jump(Opcode::Goto, 0, subroutine_end, 0);
            }
            _ => {
                // Rebase every register operand by `reg_offset` and every cursor operand by
                // `cursor_offset`. The opcodes that use p1/p2/p3 as register numbers get
                // `reg_offset`; cursor-number operands get `cursor_offset`; jump targets
                // (p2 of control-flow opcodes) are NOT rebased here (the patch loop below
                // handles them via the address map). Per-opcode rules mirror the VDBE's
                // operand-type conventions.
                let mut new_inst = inst.clone();
                rebase_operands(&mut new_inst, reg_offset, cursor_offset);
                b.append(new_inst);
            }
        }
    }

    // Bind the subroutine's end (the `Return`). Jumps targeting the subquery's `Halt`
    // (idx == halt_idx) are redirected here.
    b.resolve(subroutine_end);
    b.emit(Opcode::Return, return_reg, 0, 1);

    // Bind `after_sub` to the next emitted instruction — the caller's continuation (the
    // `SCopy result_reg, target` emitted by `compile_expr` after this function returns, plus
    // any subsequent code). Both the `Once` repeat-encounter jump and the `Gosub`'s return
    // land here.
    b.resolve(after_sub);

    // Patch every inlined jump's `p2` using the address map. Jumps targeting the subquery's
    // `Halt` (idx == halt_idx) are redirected to `subroutine_end` (the `Return`).
    let subroutine_end_addr = b.label_addr_of(subroutine_end);
    for (i, inst) in b.iter_insts_mut().enumerate() {
        let addr = i as i32;
        if addr < sub_start || addr >= subroutine_end_addr {
            continue;
        }
        if !is_absolute_jump(inst) {
            continue;
        }
        let sub_target = inst.p2; // jump targets are NOT rebased by `rebase_regs`
        if sub_target == halt_idx as i32 {
            inst.p2 = subroutine_end_addr;
        } else if let Some(&inlined) = addr_map.get(&sub_target) {
            inst.p2 = inlined;
        } else if sub_target == 0 {
            // Jumps targeting the subquery's Init (idx 0): redirect to the subroutine start so
            // a re-init doesn't escape into the outer program. (No well-formed subquery scan
            // should target its own Init; this is defensive.)
            inst.p2 = sub_start;
        }
        // else: unknown target — leave as-is defensively.
    }

    Ok(result_reg)
}

/// Compile an `EXISTS (SELECT …)` expression, returning the register that holds the boolean
/// result (1 if the subquery returns at least one row, 0 otherwise). Mirrors
/// `sqlite3CodeSubselect` in `expr.c` for the `TK_EXISTS` case: the subquery body is compiled
/// as a subroutine (`OP_Gosub`/`OP_Return`), wrapped in `OP_Once` so a non-correlated
/// subquery runs only once per statement even if the expression is evaluated many times.
///
/// Each `ResultRow` in the inlined subquery body is rewritten into `Integer 1, result_reg`
/// followed by `Goto <end_of_subroutine>` — i.e. the first yielded row sets `result_reg` to 1
/// and the subroutine returns immediately (the equivalent of upstream's `LIMIT 1` injection).
/// The body's `Halt` (the scan-end label) is rewritten to the subroutine's `Return`.
///
/// Like [`compile_scalar_subquery`], the M8.8 first slice assumes the subquery is
/// **non-correlated** — it must not reference outer-query columns. The `OP_Once` wrapping
/// caches the result across all encounters; a correlated subquery would need to re-run on
/// each outer row (M8.11 `Param` + M8.13 re-materialization).
pub fn compile_exists_subquery(
    b: &mut ProgramBuilder,
    subquery: &SelectStmt,
    subquery_table: Option<&Table>,
    subquery_indexes: &[IndexObject],
) -> Result<i32> {
    // Compile the subquery body first so we know its register count. The inlined body uses
    // registers 1..N (its own `ProgramBuilder` allocation); our `result_reg`/`return_reg`
    // must live ABOVE that range to avoid being clobbered by the subquery's `Column`/`Integer`/
    // comparison-register writes.
    let (sub_program, _sub_names) = select::compile(subquery, subquery_table, subquery_indexes, None)?;
    let sub_num_regs = sub_program.num_registers as i32;

    // Cursor offset: the subquery's `select::compile` hardcodes cursor 0 for its table scan
    // (and 1 for a sorter, 2 for DISTINCT dedup). The outer program may already be using those
    // cursor numbers for its own scan. Offset the subquery's cursor numbers by the outer
    // builder's `next_cursor()` so they land in a free range.
    let cursor_offset = b.next_cursor();

    // Allocate the result register and the subroutine return-address register ABOVE the
    // subquery's register range. The result register is pre-filled with 0 (the no-rows case);
    // the first yielded row overwrites it with 1 (matching SQLite's `SRT_Exists` destination).
    let reg_offset = b.next_reg() - 1;
    let _ = b.alloc_regs(sub_num_regs);
    let result_reg = b.alloc_reg();
    let return_reg = b.alloc_reg();

    // `OP_Once` wraps the subroutine call so a non-correlated subquery runs only once.
    let after_sub = b.new_label();
    b.emit_jump(Opcode::Once, 0, after_sub, 0);

    // Pre-fill the result with 0 (the no-rows case).
    b.emit(Opcode::Integer, 0, result_reg, 0);

    // Call the subroutine.
    let subroutine_start = b.new_label();
    b.emit_jump(Opcode::Gosub, return_reg, subroutine_start, 0);
    b.emit_jump(Opcode::Goto, 0, after_sub, 0);

    // --- Subroutine body: the inlined subquery scan. ---
    b.resolve(subroutine_start);

    let halt_idx = sub_program
        .instructions
        .iter()
        .position(|i| i.opcode == Opcode::Halt)
        .ok_or_else(|| Error::msg("EXISTS subquery program has no Halt"))?;

    let subroutine_end = b.new_label();

    let mut addr_map: std::collections::HashMap<i32, i32> = std::collections::HashMap::new();
    let sub_start = b.cur_addr();

    for idx in 1..halt_idx {
        let inst = &sub_program.instructions[idx];
        let sub_addr = idx as i32;
        let inlined_addr = b.cur_addr();
        addr_map.insert(sub_addr, inlined_addr);
        match inst.opcode {
            Opcode::ResultRow => {
                // SRT_Exists rewrite: set `result_reg` to 1 and jump to the subroutine's
                // `Return`. The first yielded row flips the result from 0 to 1 and the
                // subroutine returns immediately (the LIMIT 1 injection).
                b.emit(Opcode::Integer, 1, result_reg, 0);
                b.emit_jump(Opcode::Goto, 0, subroutine_end, 0);
            }
            _ => {
                let mut new_inst = inst.clone();
                rebase_operands(&mut new_inst, reg_offset, cursor_offset);
                b.append(new_inst);
            }
        }
    }

    b.resolve(subroutine_end);
    b.emit(Opcode::Return, return_reg, 0, 1);

    b.resolve(after_sub);

    // Patch every inlined jump's `p2` using the address map.
    let subroutine_end_addr = b.label_addr_of(subroutine_end);
    for (i, inst) in b.iter_insts_mut().enumerate() {
        let addr = i as i32;
        if addr < sub_start || addr >= subroutine_end_addr {
            continue;
        }
        if !is_absolute_jump(inst) {
            continue;
        }
        let sub_target = inst.p2;
        if sub_target == halt_idx as i32 {
            inst.p2 = subroutine_end_addr;
        } else if let Some(&inlined) = addr_map.get(&sub_target) {
            inst.p2 = inlined;
        } else if sub_target == 0 {
            inst.p2 = sub_start;
        }
    }

    Ok(result_reg)
}

/// Rebase every register operand of `inst` by `reg_offset` and every cursor operand by
/// `cursor_offset`, so an inlined sub-program's register N becomes the outer program's
/// register `reg_offset + N` and its cursor C becomes `cursor_offset + C`. Per-opcode rules
/// mirror the VDBE's operand-type conventions: which of `p1`/`p2`/`p3` are register numbers
/// vs. cursor numbers vs. integer immediates vs. jump targets. Jump targets (`p2` of
/// `Goto`/`If`/`Rewind`/`Next`/...) are NOT rebased here; the caller's jump-patch loop
/// handles them via the address map.
fn rebase_operands(inst: &mut Instruction, reg_offset: i32, cursor_offset: i32) {
    use Opcode::*;
    // Helper: rebase a register operand.
    let r = |x: &mut i32| *x += reg_offset;
    // Helper: rebase a cursor operand.
    let c = |x: &mut i32| *x += cursor_offset;
    match inst.opcode {
        // Control flow — p2 is a jump target (NOT rebased here). p1/p3 are registers where
        // applicable.
        Goto | Init | Once => {} // no register/cursor operands
        Gosub => {
            r(&mut inst.p1); // return-address register
        }
        Return => {
            r(&mut inst.p1);
        }
        If | IfNot | IsNull | NotNull => {
            r(&mut inst.p1);
        }
        IfPos | DecrJumpZero => {
            r(&mut inst.p1);
        }
        Eq | Ne | Lt | Le | Gt | Ge => {
            r(&mut inst.p1);
            r(&mut inst.p3);
        }
        Rewind | Next => {
            c(&mut inst.p1); // cursor
        }
        NotExists => {
            c(&mut inst.p1); // cursor
            r(&mut inst.p3); // rowid register
        }
        SeekGE | SeekGT | SeekLE | SeekLT => {
            c(&mut inst.p1); // cursor
            r(&mut inst.p3); // key register
        }
        IdxGE | IdxGT | IdxLE | IdxLT => {
            c(&mut inst.p1); // cursor
            r(&mut inst.p3); // key register
        }
        Found | NotFound => {
            c(&mut inst.p1); // cursor
            r(&mut inst.p3); // record start register
        }
        SorterSort | SorterNext => {
            c(&mut inst.p1); // cursor
        }
        // Cursors / scans.
        OpenRead | OpenWrite | OpenWriteReg | OpenEphemeral | OpenPseudo | Close => {
            c(&mut inst.p1); // cursor number
            // OpenEphemeral p2 = column count; OpenRead p2 = rootpage; not rebased.
            // OpenPseudo p2 = source register; rebased below.
            if inst.opcode == OpenPseudo {
                r(&mut inst.p2);
            }
        }
        RowData => {
            c(&mut inst.p1); // source cursor
            r(&mut inst.p2); // destination register
        }
        Column => {
            c(&mut inst.p1); // cursor
            r(&mut inst.p3); // target register
        }
        Rowid => {
            c(&mut inst.p1);
            r(&mut inst.p2); // target register
        }
        NullRow => {
            c(&mut inst.p1);
        }
        Clear | Destroy => {
            c(&mut inst.p1);
        }
        IdxInsert => {
            c(&mut inst.p1);
            r(&mut inst.p2); // record register
        }
        IdxDelete => {
            c(&mut inst.p1);
            r(&mut inst.p2); // key register
        }
        IdxRowid => {
            c(&mut inst.p1);
            r(&mut inst.p2); // target register
        }
        SorterOpen => {
            c(&mut inst.p1); // cursor
            // p2 = field count, not rebased.
        }
        SorterInsert => {
            c(&mut inst.p1);
            r(&mut inst.p2); // record register
        }
        SorterData => {
            c(&mut inst.p1);
            r(&mut inst.p2); // register
        }
        OpenDup => {
            c(&mut inst.p1);
            c(&mut inst.p2);
        }
        ResetSorter | Last | Prev => {
            c(&mut inst.p1);
        }
        // Record building.
        MakeRecord => {
            r(&mut inst.p1); // source start
            r(&mut inst.p3); // destination
        }
        NewRowid => {
            c(&mut inst.p1);
            r(&mut inst.p2); // output rowid register
        }
        Insert => {
            c(&mut inst.p1);
            r(&mut inst.p2); // record register
            r(&mut inst.p3); // rowid register
        }
        Delete => {
            c(&mut inst.p1);
            r(&mut inst.p2); // rowid register (for index maintenance)
        }
        // Constants — p2 = destination register. p1 holds the immediate (Integer) or is 0.
        Integer | Int64 | Real | String8 | Null | Blob => {
            r(&mut inst.p2);
            if inst.opcode == Null && inst.p3 > 0 {
                r(&mut inst.p3); // register range end
            }
        }
        // Arithmetic / bitwise / boolean — r[p3] = r[p2] OP r[p1].
        Add | Subtract | Multiply | Divide | Remainder | Concat | BitAnd | BitOr
        | ShiftLeft | ShiftRight => {
            r(&mut inst.p1);
            r(&mut inst.p2);
            r(&mut inst.p3);
        }
        BitNot => {
            r(&mut inst.p1);
            r(&mut inst.p2);
        }
        Not => {
            r(&mut inst.p1);
            r(&mut inst.p2);
        }
        And | Or => {
            r(&mut inst.p1);
            r(&mut inst.p2);
        }
        // Register copies.
        SCopy => {
            r(&mut inst.p1);
            r(&mut inst.p2);
        }
        Move | Copy => {
            r(&mut inst.p1);
            r(&mut inst.p2);
        }
        // Affinity / RealAffinity.
        Affinity => {
            r(&mut inst.p1);
        }
        RealAffinity => {
            r(&mut inst.p1);
        }
        // Function — p2 = first arg register, p3 = result register.
        Function => {
            r(&mut inst.p2);
            r(&mut inst.p3);
        }
        // Aggregate opcodes.
        AggStep => {
            r(&mut inst.p2); // first arg reg
            r(&mut inst.p3); // accumulator reg
        }
        AggInverse => {
            r(&mut inst.p2); // first arg reg
            r(&mut inst.p3); // accumulator reg
        }
        AggFinal => {
            r(&mut inst.p1); // accumulator reg
        }
        AggValue => {
            r(&mut inst.p3); // result reg
        }
        HaltIfNull => {
            r(&mut inst.p3);
        }
        AddImm => {
            r(&mut inst.p1);
        }
        SeekRowid => {
            c(&mut inst.p1);
            r(&mut inst.p3);
        }
        // Opcodes that don't appear in a scalar subquery scan body — leave as-is.
        Compare | Jump | Transaction | SetCookie | ParseSchema | CreateBtree | Halt
        | ResultRow => {}
        // Coroutine opcodes — p1 is a coroutine register.
        InitCoroutine => {
            r(&mut inst.p1);
        }
        EndCoroutine => {
            r(&mut inst.p1);
        }
        Yield => {
            r(&mut inst.p1);
        }
        // Sub-program invocation opcodes. These do not appear inside the scalar/EXISTS/IN
        // subquery bodies that `rebase_operands` is used for (those are inlined scan-by-scan,
        // not compiled as separate `Program`-invoked sub-programs), so the arms are defensive.
        Program => {
            // p3 is the runtime register (unused by us); p1 is the parent register base. Both
            // belong to the calling (outer) frame, not the inlined scan, so leave them alone.
        }
        Param => {
            // p2 is a register in the sub-program's own frame; rebase it. p1 is an offset from
            // the parent's base, not a register, so leave it alone.
            r(&mut inst.p2);
        }
    }
}

/// Compile an `X [NOT] IN (SELECT …)` expression as a jump, mirroring `sqlite3ExprCodeIN` in
/// `expr.c` for the `ExprUseXSelect` case (the `IN_INDEX_EPH` path).
///
/// The subquery's result rows are materialized into an ephemeral index (one column for a scalar
/// LHS), then the LHS is evaluated and probed with `OP_NotFound`/`OP_Found`. The
/// `dest_if_false`/`dest_if_null` labels match upstream's semantics:
/// * fall through (next instruction) when the LHS is a member of the RHS set (TRUE);
/// * jump to `dest_if_false` when the LHS is definitely not a member (FALSE);
/// * jump to `dest_if_null` when NULL values make the answer indeterminate (NULL).
///
/// For `NOT IN`, the caller swaps the FALSE/NULL destinations and the fall-through target so
/// the same code shape produces the negated result.
///
/// Like [`compile_scalar_subquery`] / [`compile_exists_subquery`], the M8.9 first slice assumes
/// the subquery is **non-correlated** — the materialization is wrapped in `OP_Once` so the
/// ephemeral is populated only once per statement. A correlated subquery would need to
/// re-materialize on each outer row (M8.11 `Param` + M8.13 re-materialization); for now an
/// outer-column reference inside the subquery fails column resolution with "no such column",
/// which is the right error for unsupported correlation.
///
/// `subquery_table`/`subquery_indexes` describe the subquery's own FROM table (or `None` for
/// a constant / `VALUES` subquery), resolved by the caller via a [`super::expr::SubqueryResolver`].
pub fn compile_in_subquery(
    b: &mut ProgramBuilder,
    expr: &Expr,
    subquery: &SelectStmt,
    negated: bool,
    subquery_table: Option<&Table>,
    subquery_indexes: &[IndexObject],
    dest_if_false: Label,
    dest_if_null: Label,
    ctx: Ctx,
) -> Result<()> {
    // The LHS must be a scalar (row-value IN lands with M2.60 row-value expressions; the parser
    // builds `Expr::InSubquery` with a single-column LHS for the common case). A vector LHS
    // would require matching the subquery's column count — we surface the same error SQLite
    // raises when the counts don't match.
    let n_vector = 1i32;

    // Validate the subquery produces exactly n_vector columns. SQLite raises
    // "sub-select returns N columns - expected M" on a mismatch.
    let inner_outputs = expand_columns(subquery, subquery_table)?;
    let inner_ncol = inner_outputs.len();
    if inner_ncol as i32 != n_vector {
        return Err(Error::msg(format!(
            "{n_vector} columns on the LHS of IN but {inner_ncol} on the RHS"
        )));
    }

    // 1. Compile the subquery body first so we know its register/cursor high-water mark. The
    //    inlined body uses registers 1..N (its own `ProgramBuilder` allocation) and cursors
    //    0/1/2 (table/sorter/distinct). Our ephemeral index cursor must live ABOVE that range,
    //    and our result registers must live ABOVE the subquery's register range.
    let (sub_program, _sub_names) = select::compile(subquery, subquery_table, subquery_indexes, None)?;
    let sub_num_regs = sub_program.num_registers as i32;

    // Cursor offset for the inlined subquery body. The subquery's `select::compile` hardcodes
    // cursor 0 for its table scan, 1 for a sorter, 2 for DISTINCT dedup. Offset past the outer
    // builder's `next_cursor()` so they land in a free range.
    let cursor_offset = b.next_cursor();
    // The subquery uses at most cursors 0, 1, 2 (table/sorter/distinct). The ephemeral index
    // cursor for the IN set must live past all of those, so place it at `cursor_offset + 3`.
    let eph_cursor = cursor_offset + 3;
    b.note_cursor(eph_cursor);

    // Reserve the subquery's register block in the outer builder so the inlined body's
    // register writes (1..sub_num_regs) land in a free range, then allocate the registers we
    // need above it: the LHS register, the rRhsHasNull register, and the subroutine return reg.
    let reg_offset = b.next_reg() - 1; // sub reg R -> outer reg reg_offset + R
    let _ = b.alloc_regs(sub_num_regs);
    let lhs_reg = b.alloc_reg();
    let rhs_has_null_reg = b.alloc_reg();
    let return_reg = b.alloc_reg();

    // `OP_Once` wraps the materialization so a non-correlated subquery runs only once. On a
    // repeat encounter, `Once` jumps past the subroutine to the membership test.
    let after_sub = b.new_label();
    b.emit_jump(Opcode::Once, 0, after_sub, 0);

    // Open the ephemeral index: single-column key, BINARY collation (the LHS/RHS comparison in
    // `OP_Found`/`OP_NotFound` on an ephemeral uses BINARY collation with NULL-equality, matching
    // `ephemeral::find_values`).
    let oe = b.emit(Opcode::OpenEphemeral, eph_cursor, 1, 0);
    b.set_p4(oe, P4::KeyInfo(Vec::new()));

    // Pre-fill `rhs_has_null_reg` with a non-NULL sentinel (Integer 0). During the subquery
    // materialization, each yielded row whose first column is NULL sets this register to NULL
    // (it sticks — once NULL it stays NULL). This is the "RHS contains a NULL" flag used by the
    // post-probe FALSE/NULL distinction (Step 4 of in-operator.md). Our ephemeral is unsorted
    // (Vec-backed, not a b-tree), so we cannot use upstream's "first row only" optimization;
    // the per-row NULL check is O(n) per materialization, same asymptotic as the materialization.
    b.emit(Opcode::Integer, 0, rhs_has_null_reg, 0);

    // Call the materialization subroutine.
    let subroutine_start = b.new_label();
    b.emit_jump(Opcode::Gosub, return_reg, subroutine_start, 0);
    b.emit_jump(Opcode::Goto, 0, after_sub, 0);

    // --- Subroutine body: the inlined subquery scan, materializing into the ephemeral. ---
    b.resolve(subroutine_start);

    let halt_idx = sub_program
        .instructions
        .iter()
        .position(|i| i.opcode == Opcode::Halt)
        .ok_or_else(|| Error::msg("IN subquery program has no Halt"))?;

    let subroutine_end = b.new_label();

    let mut addr_map: std::collections::HashMap<i32, i32> = std::collections::HashMap::new();
    let sub_start = b.cur_addr();

    for idx in 1..halt_idx {
        let inst = &sub_program.instructions[idx];
        let sub_addr = idx as i32;
        let inlined_addr = b.cur_addr();
        addr_map.insert(sub_addr, inlined_addr);
        match inst.opcode {
            Opcode::ResultRow => {
                // SRT_Set rewrite: build a 1-column record from the yielded row and `IdxInsert`
                // it into the ephemeral. The subquery's `ResultRow p1 p2 p3` has `p1` = result
                // start (in subquery register space) and `p2` = ncol. For a scalar IN we take
                // the first column; if the subquery has more than one column the codegen would
                // have errored at expand_columns time (we validate ncol == 1 below).
                let result_start = inst.p1;
                let nres = inst.p2;
                let _ = n_vector; // scalar — validated by expand_columns
                let col_reg = b.alloc_reg();
                if nres >= 1 {
                    b.emit(Opcode::SCopy, reg_offset + result_start, col_reg, 0);
                } else {
                    b.emit(Opcode::Null, 0, col_reg, 0);
                }
                let rec = b.alloc_reg();
                b.emit(Opcode::MakeRecord, col_reg, 1, rec);
                b.emit(Opcode::IdxInsert, eph_cursor, rec, 0);
                // Track whether the RHS contains a NULL in its first column. Once the flag is
                // NULL it stays NULL (subsequent rows can't un-NULL it). Skip-over pattern:
                //   IsNull col_reg → set_null
                //   Goto after
                // set_null: Null 0, rhs_has_null_reg
                // after:
                let set_null = b.new_label();
                let after = b.new_label();
                b.emit_jump(Opcode::IsNull, col_reg, set_null, 0);
                b.emit_jump(Opcode::Goto, 0, after, 0);
                b.resolve(set_null);
                b.emit(Opcode::Null, 0, rhs_has_null_reg, 0);
                b.resolve(after);
            }
            _ => {
                let mut new_inst = inst.clone();
                rebase_operands(&mut new_inst, reg_offset, cursor_offset);
                b.append(new_inst);
            }
        }
    }
    let _ = ctx; // the LHS is evaluated outside the subroutine, against the outer ctx.

    b.resolve(subroutine_end);
    b.emit(Opcode::Return, return_reg, 0, 1);

    // Bind `after_sub` to the membership test (the next emitted instruction).
    b.resolve(after_sub);

    // Patch every inlined jump's `p2` using the address map.
    let subroutine_end_addr = b.label_addr_of(subroutine_end);
    for (i, inst) in b.iter_insts_mut().enumerate() {
        let addr = i as i32;
        if addr < sub_start || addr >= subroutine_end_addr {
            continue;
        }
        if !is_absolute_jump(inst) {
            continue;
        }
        let sub_target = inst.p2;
        if sub_target == halt_idx as i32 {
            inst.p2 = subroutine_end_addr;
        } else if let Some(&inlined) = addr_map.get(&sub_target) {
            inst.p2 = inlined;
        } else if sub_target == 0 {
            inst.p2 = sub_start;
        }
    }

    // 2. Evaluate the LHS into `lhs_reg`.
    compile_expr(b, expr, lhs_reg, ctx)?;

    // 3. Step 2 of in-operator.md: if the LHS is NULL (total-NULL for a scalar), the result is
    //    NULL when the RHS is non-empty (Step 6's first comparison is NULL) and FALSE when the
    //    RHS is empty (Step 7). So a NULL LHS jumps to the Step 6 loop, NOT directly to
    //    dest_if_null. When dest_if_false == dest_if_null (the combined case), the LHS NULL
    //    check jumps straight to dest_if_false (we don't distinguish FALSE from NULL).
    let step6_label = b.new_label();
    if dest_if_false == dest_if_null {
        b.emit_jump(Opcode::IsNull, lhs_reg, dest_if_false, 0);
    } else {
        b.emit_jump(Opcode::IsNull, lhs_reg, step6_label, 0);
    }

    // 4. Step 3: probe the ephemeral index with the LHS. If the LHS is found, the IN is TRUE
    //    (fall through). If not found, the result is FALSE or NULL (depending on RHS NULLs).
    //    When dest_if_false == dest_if_null, combine Step 3 + Step 5: a single `NotFound`
    //    jumps to dest_if_false (we don't need to distinguish FALSE from NULL).
    if dest_if_false == dest_if_null {
        let nf = b.emit_jump(Opcode::NotFound, eph_cursor, dest_if_false, lhs_reg);
        b.set_p4(nf, P4::Int(1));
        // Fall through = member = TRUE.
    } else {
        // Step 3 (distinct FALSE/NULL): `Found` to a "truth" label; the not-found case falls
        // through to Step 4/5/6.
        let truth_label = b.new_label();
        let found = b.emit_jump(Opcode::Found, eph_cursor, truth_label, lhs_reg);
        b.set_p4(found, P4::Int(1));
        // Step 4: if the RHS is known to have no NULLs (the `rhs_has_null_reg` flag is
        // non-NULL), the not-found case is FALSE — jump to dest_if_false.
        b.emit_jump(Opcode::NotNull, rhs_has_null_reg, dest_if_false, 0);
        // Step 5: distinct-dest path — we care about FALSE vs NULL, so fall through to Step 6.
        // Step 6: scan the RHS. For each row, compare LHS against the RHS row: if the
        // comparison is NULL (one side is NULL), the IN result is NULL → dest_if_null. If the
        // comparison is TRUE (not equal) for all rows, the result is FALSE → dest_if_false.
        // (Upstream's ephemeral is a sorted b-tree so NULLs come first and it only checks the
        // first row; our Vec-backed ephemeral is unsorted, so we scan all rows. A NULL
        // comparison short-circuits to dest_if_null on the first NULL row.)
        b.resolve(step6_label);
        b.emit_jump(Opcode::Rewind, eph_cursor, dest_if_false, 0);
        let loop_top = b.cur_addr();
        let col_reg = b.alloc_reg();
        b.emit(Opcode::Column, eph_cursor, 0, col_reg);
        // `Ne lhs, dest_not_null, col_reg` — jump to dest_not_null when `lhs != col_reg` is
        // TRUE (they differ). Fall through when the comparison is FALSE (equal — should not
        // happen post-probe, but be defensive) or NULL (one side NULL → result is NULL).
        let dest_not_null = b.new_label();
        let ne_idx = b.emit_jump(Opcode::Ne, lhs_reg, dest_not_null, col_reg);
        // NULL comparison (fall-through from `Ne`): result is NULL.
        b.emit_jump(Opcode::Goto, 0, dest_if_null, 0);
        b.resolve(dest_not_null);
        b.emit(Opcode::Next, eph_cursor, loop_top, 0);
        // Step 7: no NULL comparison found — the result is FALSE.
        b.emit_jump(Opcode::Goto, 0, dest_if_false, 0);
        // The "truth" label: the LHS is a member — fall through (TRUE).
        b.resolve(truth_label);
        let _ = ne_idx;
    }

    // `negated` is unused here — the IN form is compiled; the caller (`compile_expr`'s value
    // form) handles the `NOT IN` negation by swapping the TRUE/FALSE storage. The jump form
    // (`compile_jump`'s `other` arm) also handles it via the value form + `IfNot`. Kept in the
    // signature so a future direct jump-form negation can use it without reshaping the API.
    let _ = negated;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{IndexObject, SchemaObject, Table};
    use rustqlite_parser::{parse, Stmt};

    fn compile_constant_subquery(sql: &str) -> (Program, Vec<String>) {
        let Stmt::Select(outer) = parse(sql).unwrap().into_iter().next().unwrap() else {
            panic!("expected SELECT");
        };
        let TableOrJoin::Subquery { query, alias } = &outer.from[0] else {
            panic!("expected subquery in FROM");
        };
        compile_from_subquery(&outer, query, alias, None, &[]).unwrap()
    }

    fn compile_subquery_over_table(sql: &str, create: &str) -> (Program, Vec<String>) {
        let obj = SchemaObject {
            rowid: 1,
            obj_type: "table".into(),
            name: "t".into(),
            tbl_name: "t".into(),
            rootpage: 2,
            sql: Some(create.into()),
        };
        let table = Table::from_schema_object(&obj).unwrap();
        let indexes: Vec<IndexObject> = Vec::new();
        let Stmt::Select(outer) = parse(sql).unwrap().into_iter().next().unwrap() else {
            panic!("expected SELECT");
        };
        let TableOrJoin::Subquery { query, alias } = &outer.from[0] else {
            panic!("expected subquery in FROM");
        };
        compile_from_subquery(&outer, query, alias, Some(&table), &indexes).unwrap()
    }

    /// Golden test for the canonical constant-subquery program shape. The outer SELECT scans
    /// the ephemeral that the inlined subquery populated. Addresses and operand values are
    /// hand-verified against the codegen.
    #[test]
    fn golden_constant_subquery_program() {
        let (prog, names) =
            compile_constant_subquery("SELECT * FROM (SELECT 1 AS x, 2 AS y) AS sq;");
        assert_eq!(names, vec!["x".to_string(), "y".to_string()]);
        let expected = vec![
            "0 Init 0 15 0 None 0",
            "1 OpenEphemeral 10 2 0 None 0",
            // Inlined subquery: Integer 1 -> r1, Integer 2 -> r2, then ResultRow rewrite.
            "2 Integer 1 1 0 None 0",
            "3 Integer 2 2 0 None 0",
            "4 SCopy 1 1 0 None 0",
            "5 SCopy 2 2 0 None 0",
            "6 MakeRecord 1 2 3 None 0",
            "7 NewRowid 10 4 0 None 0",
            "8 Insert 10 3 4 None 0",
            // Outer scan over the ephemeral.
            "9 Rewind 10 14 0 None 0",
            "10 Column 10 0 5 None 0",
            "11 Column 10 1 6 None 0",
            "12 ResultRow 5 2 0 None 0",
            "13 Next 10 10 0 None 0",
            "14 Halt 0 0 0 None 0",
            "15 Transaction 0 0 0 None 0",
            "16 Goto 0 1 0 None 0",
        ];
        let got: Vec<String> = prog
            .instructions
            .iter()
            .enumerate()
            .map(|(addr, i)| {
                format!(
                    "{addr} {} {} {} {} {:?} {}",
                    i.opcode.name(),
                    i.p1,
                    i.p2,
                    i.p3,
                    i.p4,
                    i.p5
                )
            })
            .collect();
        assert_eq!(got, expected);
    }

    /// Golden test for a subquery over a real table with a WHERE clause. Verifies that the
    /// inlined scan code's jumps are rebased correctly (loop_top, scan-end, next_label).
    #[test]
    fn golden_subquery_over_table_program() {
        let (prog, names) = compile_subquery_over_table(
            "SELECT a FROM (SELECT a, b FROM t WHERE a > 1) AS sq;",
            "CREATE TABLE t(a, b)",
        );
        assert_eq!(names, vec!["a".to_string()]);
        let expected = vec![
            "0 Init 0 20 0 None 0",
            "1 OpenEphemeral 10 2 0 None 0",
            // Inlined subquery scan.
            "2 OpenRead 0 2 0 Int(2) 0",
            "3 Rewind 0 15 0 None 0",
            "4 Column 0 0 1 None 0",
            "5 Integer 1 2 0 None 0",
            "6 Le 2 14 1 None 17",
            "7 Column 0 0 3 None 0",
            "8 Column 0 1 4 None 0",
            "9 SCopy 3 1 0 None 0",
            "10 SCopy 4 2 0 None 0",
            "11 MakeRecord 1 2 3 None 0",
            "12 NewRowid 10 4 0 None 0",
            "13 Insert 10 3 4 None 0",
            "14 Next 0 4 0 None 0",
            // Outer scan over the ephemeral.
            "15 Rewind 10 19 0 None 0",
            "16 Column 10 0 5 None 0",
            "17 ResultRow 5 1 0 None 0",
            "18 Next 10 16 0 None 0",
            "19 Halt 0 0 0 None 0",
            "20 Transaction 0 0 0 None 0",
            "21 Goto 0 1 0 None 0",
        ];
        let got: Vec<String> = prog
            .instructions
            .iter()
            .enumerate()
            .map(|(addr, i)| {
                format!(
                    "{addr} {} {} {} {} {:?} {}",
                    i.opcode.name(),
                    i.p1,
                    i.p2,
                    i.p3,
                    i.p4,
                    i.p5
                )
            })
            .collect();
        assert_eq!(got, expected);
    }

    /// A subquery with no rows (empty result) materializes an empty ephemeral; the outer scan's
    /// `Rewind` jumps straight to the end label, emitting no rows. Verifies the scan-end
    /// redirection (Rewind jumps to `after_sub`, not into the rewritten ResultRow block).
    #[test]
    fn subquery_with_no_rows() {
        let (prog, _names) = compile_subquery_over_table(
            "SELECT a FROM (SELECT a FROM t WHERE a > 9999) AS sq;",
            "CREATE TABLE t(a, b)",
        );
        // The subquery's `Rewind 0 <end>` must be redirected to the outer scan's start, not
        // into the rewritten ResultRow block. Find the Rewind and verify its p2 is the
        // outer-scan-start address (the address of the outer Rewind).
        let rewind_idx = prog
            .instructions
            .iter()
            .position(|i| i.opcode == Opcode::Rewind && i.p1 == 0)
            .expect("subquery Rewind on cursor 0");
        let outer_rewind_idx = prog
            .instructions
            .iter()
            .position(|i| i.opcode == Opcode::Rewind && i.p1 == 10)
            .expect("outer Rewind on cursor 10");
        assert_eq!(
            prog.instructions[rewind_idx].p2 as usize, outer_rewind_idx,
            "subquery Rewind must jump to the outer scan start when the subquery is empty"
        );
    }

    /// Compiling a `FROM (subquery)` whose outer SELECT has a `LIMIT 0` should produce a program
    /// that emits zero rows (the LIMIT-0 short-circuit at the top of compile_from_subquery).
    #[test]
    fn subquery_with_limit_zero_emits_no_rows() {
        let (prog, _names) =
            compile_constant_subquery("SELECT * FROM (SELECT 1 AS x) AS sq LIMIT 0;");
        // The program should be very short: Init, Halt, setup block.
        assert!(prog.instructions.iter().any(|i| i.opcode == Opcode::Halt));
        assert!(
            prog.instructions
                .iter()
                .filter(|i| i.opcode == Opcode::ResultRow)
                .count()
                == 0,
            "LIMIT 0 program must not emit any ResultRow"
        );
    }

    /// A scalar subquery in a projection column: the subquery's first row's first column is
    /// captured into a result register and emitted as the outer SELECT's result. Golden test
    /// for `SELECT (SELECT 1)` — the canonical constant scalar subquery.
    #[test]
    fn golden_scalar_subquery_constant() {
        let stmt = "SELECT 1";
        let parsed = rustqlite_parser::parse(stmt).unwrap().into_iter().next().unwrap();
        let rustqlite_parser::Stmt::Select(s) = parsed else { panic!("expected SELECT") };
        let mut b = ProgramBuilder::new();
        let setup = b.new_label();
        b.emit_jump(Opcode::Init, 0, setup, 0);
        let after_init = b.cur_addr();
        let result_reg = compile_scalar_subquery(&mut b, &s, None, &[]).unwrap();
        let target = b.alloc_reg();
        b.emit(Opcode::SCopy, result_reg, target, 0);
        b.emit(Opcode::ResultRow, target, 1, 0);
        b.emit(Opcode::Halt, 0, 0, 0);
        b.resolve(setup);
        b.emit(Opcode::Transaction, 0, 0, 0);
        b.emit(Opcode::Goto, 0, after_init, 0);
        let prog = b.finish();
        let got: Vec<String> = prog
            .instructions
            .iter()
            .enumerate()
            .map(|(addr, i)| {
                format!(
                    "{addr} {} {} {} {} {:?} {}",
                    i.opcode.name(),
                    i.p1,
                    i.p2,
                    i.p3,
                    i.p4,
                    i.p5
                )
            })
            .collect();
        let expected = vec![
            "0 Init 0 12 0 None 0",
            "1 Once 0 9 0 None 0",
            "2 Null 0 3 0 None 0",
            "3 Gosub 4 5 0 None 0",
            "4 Goto 0 9 0 None 0",
            "5 Integer 1 1 0 None 0",
            "6 SCopy 1 3 0 None 0",
            "7 Goto 0 8 0 None 0",
            "8 Return 4 0 1 None 0",
            "9 SCopy 3 5 0 None 0",
            "10 ResultRow 5 1 0 None 0",
            "11 Halt 0 0 0 None 0",
            "12 Transaction 0 0 0 None 0",
            "13 Goto 0 1 0 None 0",
        ];
        assert_eq!(got, expected);
    }

    /// An `EXISTS (SELECT 1)` golden test: the subquery's first yielded row sets `result_reg`
    /// to 1 and the subroutine returns immediately. Mirrors the scalar-subquery golden test
    /// but with `Integer 0` initialization (the no-rows case) and `Integer 1, result_reg` on
    /// the `ResultRow` rewrite (the `SRT_Exists` destination).
    #[test]
    fn golden_exists_subquery_constant() {
        let stmt = "SELECT 1";
        let parsed = rustqlite_parser::parse(stmt).unwrap().into_iter().next().unwrap();
        let rustqlite_parser::Stmt::Select(s) = parsed else { panic!("expected SELECT") };
        let mut b = ProgramBuilder::new();
        let setup = b.new_label();
        b.emit_jump(Opcode::Init, 0, setup, 0);
        let after_init = b.cur_addr();
        let result_reg = compile_exists_subquery(&mut b, &s, None, &[]).unwrap();
        let target = b.alloc_reg();
        b.emit(Opcode::SCopy, result_reg, target, 0);
        b.emit(Opcode::ResultRow, target, 1, 0);
        b.emit(Opcode::Halt, 0, 0, 0);
        b.resolve(setup);
        b.emit(Opcode::Transaction, 0, 0, 0);
        b.emit(Opcode::Goto, 0, after_init, 0);
        let prog = b.finish();
        let got: Vec<String> = prog
            .instructions
            .iter()
            .enumerate()
            .map(|(addr, i)| {
                format!(
                    "{addr} {} {} {} {} {:?} {}",
                    i.opcode.name(),
                    i.p1,
                    i.p2,
                    i.p3,
                    i.p4,
                    i.p5
                )
            })
            .collect();
        let expected = vec![
            "0 Init 0 12 0 None 0",
            "1 Once 0 9 0 None 0",
            "2 Integer 0 3 0 None 0",
            "3 Gosub 4 5 0 None 0",
            "4 Goto 0 9 0 None 0",
            "5 Integer 1 1 0 None 0",
            "6 Integer 1 3 0 None 0",
            "7 Goto 0 8 0 None 0",
            "8 Return 4 0 1 None 0",
            "9 SCopy 3 5 0 None 0",
            "10 ResultRow 5 1 0 None 0",
            "11 Halt 0 0 0 None 0",
            "12 Transaction 0 0 0 None 0",
            "13 Goto 0 1 0 None 0",
        ];
        assert_eq!(got, expected);
    }
}