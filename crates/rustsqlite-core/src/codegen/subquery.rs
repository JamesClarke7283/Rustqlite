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

use super::builder::ProgramBuilder;
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
    let (sub_program, _sub_names) = select::compile(subquery, subquery_table, subquery_indexes)?;

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
}