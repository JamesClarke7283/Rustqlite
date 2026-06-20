//! Lowering a single-table `SELECT` to a VDBE program (mirrors `sqlite3Select` in `select.c`).
//!
//! Two shapes are produced:
//! * a **table scan** — `Init → Transaction → OpenRead → Rewind → [WHERE; project; ResultRow] →
//!   Next → Halt`, with `ORDER BY` lowering to a sorter and `LIMIT`/`OFFSET` wrapping the output;
//! * a **constant** `SELECT` (no `FROM`) — evaluate the projection once and emit a single row.
//! * (M5.1) an **indexed equality** — for `WHERE <indexed_col> = <const>` with a usable
//!   single-column index, the scan walks the index, looks up each rowid in the table, and
//!   projects; the sorter is dropped when `ORDER BY <indexed_col> ASC` is also present (the
//!   index is already ordered).

use rustqlite_parser::{Expr, FunctionArgs, Literal, OrderingTerm, ResultColumn, SelectStmt, UnaryOp, Window};

use crate::error::{Error, Result};
use crate::func::aggregate::{is_aggregate_call, AggregateKind};
use crate::schema::{IndexObject, Table};
use crate::types::Value;
use crate::util::fp::fp_to_text;
use crate::vdbe::program::{Program, P4};
use crate::vdbe::{KeyField, Opcode};

use super::builder::{Label, ProgramBuilder};
use super::expr::{compile_expr, compile_jump, Ctx, SubqueryResolver};
use super::index_planner::{pick_index, IndexPlan};

/// Compile a single-table (or constant) `SELECT`, returning the program and the result column
/// names. `table` is the resolved table when there is exactly one `FROM` entry, else `None`.
/// `indexes` is the list of indexes attached to `table`; when an indexed equality prefix
/// is present in the `WHERE`, the M5.2 planner routes through the index.
///
/// `subquery_resolver`, when set, lets expression codegen compile scalar subqueries /
/// `EXISTS` / `IN (SELECT ...)` against the catalog. `None` leaves those expression kinds
/// raising "unsupported" (the pre-M8.7 behavior).
pub fn compile(
    select: &SelectStmt,
    table: Option<&Table>,
    indexes: &[IndexObject],
    subquery_resolver: Option<&dyn SubqueryResolver>,
) -> Result<(Program, Vec<String>)> {
    // A compound SELECT (UNION/UNION ALL/INTERSECT/EXCEPT) is lowered by the dedicated
    // `codegen::compound` module, which mirrors `multiSelect`/`multiSelectByMerge` in
    // `select.c`. The non-compound path continues below.
    if !select.compound.is_empty() {
        return super::compound::compile_compound(select, table, indexes, subquery_resolver);
    }
    reject_unsupported(select)?;

    let outputs = expand_columns(select, table)?;
    // HAVING is only valid on an aggregate query (one with a GROUP BY or an aggregate function
    // call in the projection list). Mirrors the `resolve.c` check that raises
    // "HAVING clause on a non-aggregate query". A HAVING on a non-aggregate query is a semantic
    // error, not a "not supported" limitation — the message matches the oracle.
    if select.having.is_some() && !is_aggregate_query(select, &outputs) {
        return Err(Error::msg("HAVING clause on a non-aggregate query"));
    }
    // Window-function queries (any function call with an `OVER` clause) are not yet supported
    // by the codegen — the partition-sort + frame-step driver lands in M11.7. Raise the
    // upstream-faithful "misuse of window function <name>()" error for a window-only function
    // used without an `OVER` clause, and "window functions are not yet supported" for a
    // properly windowed call (the latter is a Rustqlite-specific limitation, not an upstream
    // error — upstream supports them; we don't yet).
    if has_window_function_query(select, &outputs) {
        return Err(Error::msg(
            "window functions are not yet supported (M11.7: codegen driver pending)",
        ));
    }
    // A window-only function (row_number/rank/…/lead/lag) used *without* an `OVER` clause is a
    // semantic error in upstream ("misuse of window function <name>()"). We detect it here so
    // the user sees the right message rather than a downstream "no such function" or a wrong
    // scalar codegen path.
    for (e, _) in &outputs {
        check_no_window_only_without_over(e)?;
    }
    let names: Vec<String> = outputs.iter().map(|(_, n)| n.clone()).collect();
    let (limit, offset) = eval_limit_offset(select)?;

    let program = if !select.values.is_empty() {
        compile_values(select, &outputs, limit, offset, subquery_resolver)?
    } else {
        match table {
            Some(t) => {
                if is_aggregate_query(select, &outputs) {
                    compile_aggregate(select, t, &outputs, limit, offset, subquery_resolver)?
                } else if let Some(plan) = pick_index(select, t, indexes) {
                    compile_indexed_select(select, t, &plan, &outputs, limit, offset, subquery_resolver)?
                } else {
                    compile_scan(select, t, &outputs, limit, offset, subquery_resolver)?
                }
            }
            None => compile_constant(select, &outputs, limit, offset, subquery_resolver)?,
        }
    };
    Ok((program, names))
}

/// Compile an aggregate `SELECT` — either a bare `SELECT agg(...) FROM t` (no GROUP BY) or a
/// `SELECT ..., agg(...) FROM t GROUP BY ...` (with GROUP BY). Mirrors the
/// `tag-select-0810`/`tag-select-0820` branches of `sqlite3Select` in `select.c`.
///
/// For the GROUP BY case the strategy is the simplest "always sort" path upstream documents:
/// 1. Scan the table, applying the WHERE filter.
/// 2. For each passing row, evaluate the GROUP BY key expressions and the aggregate argument
///    expressions, build a `[group_keys..., agg_args...]` record, and `SorterInsert` it.
/// 3. Sort by the group key (ASC, BINARY — matching SQLite's default group ordering).
/// 4. Walk the sorted rows: load the key registers, `Compare` against the previous group's key,
///    and on a mismatch call the output subroutine (which finalizes the accumulators and emits
///    a `ResultRow`). On every row call `AggStep` with the row's args.
/// 5. After the loop, emit the final group's row.
///
/// For the no-GROUP-BY case the whole scan+step loop accumulates one row, which is emitted once
/// at the end. This is the `tag-select-0822` branch (without the min/max optimization).
#[allow(clippy::too_many_arguments)]
fn compile_aggregate(
    select: &SelectStmt,
    table: &Table,
    outputs: &[(Expr, String)],
    limit: Option<i64>,
    offset: i64,
    subquery_resolver: Option<&dyn SubqueryResolver>,
) -> Result<Program> {
    // (A) Collect every aggregate call from the projection list and the HAVING clause. Both
    // sites share one set of accumulator registers (matching upstream's `AggInfo` which walks
    // pEList and pHaving together via `sqlite3ExprAnalyzeAggList` + `sqlite3ExprAnalyzeAggregates`).
    let mut agg_calls: Vec<AggCall> = Vec::new();
    for (e, _) in outputs {
        collect_aggregates(e, &mut agg_calls);
    }
    if let Some(having) = &select.having {
        collect_aggregates(having, &mut agg_calls);
    }
    // Deduplicate by syntactic identity (name + args) so the same call appearing twice shares
    // one accumulator. Matches upstream's `AggInfo` deduplication.
    let mut dedup: Vec<AggCall> = Vec::new();
    for c in &agg_calls {
        if !dedup.iter().any(|d| agg_call_eq(d, c)) {
            dedup.push(c.clone());
        }
    }
    let agg_calls = dedup;

    // (B) Resolve each call to an `AggregateKind`, validating the argument count.
    let kinds: Vec<AggregateKind> = agg_calls
        .iter()
        .map(|c| {
            let n_arg = match &c.args {
                FunctionArgs::Star => 0,
                FunctionArgs::List(v) => v.len(),
            };
            AggregateKind::from_name(&c.name, n_arg)
                .ok_or_else(|| Error::msg(format!("no such aggregate: {}({})", c.name, n_arg)))
        })
        .collect::<Result<Vec<_>>>()?;

    let cursor = 0i32;
    let ncol = outputs.len() as i32;
    let nkey = select.group_by.len() as i32;
    let n_agg = agg_calls.len() as i32;
    let ctx = Ctx {
        table,
        cursor,
        register_base: None, join_tables: None,
        index_read: None,
        subquery_resolver,
    };
    let mut b = ProgramBuilder::new();

    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    let after_init = b.cur_addr();

    // LIMIT 0 → no rows.
    if limit == Some(0) {
        b.emit(Opcode::Halt, 0, 0, 0);
        b.resolve(setup);
        b.emit(Opcode::Transaction, 0, 0, 0);
        b.emit(Opcode::Goto, 0, after_init, 0);
        return Ok(b.finish());
    }
    let limit_reg = match limit {
        Some(n) if n > 0 => Some(emit_int(&mut b, n)),
        _ => None,
    };
    let offset_reg = (offset > 0).then(|| emit_int(&mut b, offset));

    // (C') When the query has an ORDER BY, the aggregate pass writes its per-group result rows
    // into an output sorter (keyed by the ORDER BY expressions) rather than emitting them
    // directly. After the aggregate pass, a tail block sorts the output sorter and walks it
    // with OFFSET/LIMIT to emit the final ResultRows. This is the two-pass shape upstream
    // uses for `GROUP BY ... ORDER BY` (aggregate, then sort the result).
    let has_order_by = !select.order_by.is_empty();
    let norder = select.order_by.len() as i32;
    // The output sorter cursor number. The no-GROUP-BY path uses cursor 0 (table) and sorter 1
    // (unused there but reserved for symmetry); the GROUP BY path uses cursor 0 (table), sorter
    // 1 (group sorter). The output sorter is the next free cursor: 2 in the GROUP BY path, 1
    // in the no-GROUP-BY path (which doesn't open a group sorter). We compute it per-path
    // below; the tail block reads it back.
    let output_sorter: i32 = if nkey > 0 { 2 } else { 1 };

    // (C) Allocate the per-aggregate accumulator registers and build the lookup table that
    // `rewrite_aggregates` uses to replace each call with an `AggRef(reg)`.
    let agg_reg_base = if n_agg > 0 { b.alloc_regs(n_agg) } else { 0 };
    // For the GROUP BY path, allocate the "previous group key" registers (iAMem) now so the
    // projection rewrite can substitute GROUP BY expression matches with `AggRef` references
    // to them. The output subroutine reads the *previous* group's key from iAMem — when a group
    // change is detected, the previous group is emitted before iAMem is overwritten with the
    // new group's key. (For the no-GROUP-BY path these are unused — `nkey == 0`.)
    let i_amem = if nkey > 0 { b.alloc_regs(nkey) } else { 0 };
    let reg_of = |call: &AggCall| -> Option<i32> {
        agg_calls
            .iter()
            .position(|d| agg_call_eq(d, call))
            .map(|i| agg_reg_base + i as i32)
    };
    let group_key_of = |expr: &Expr| -> Option<i32> {
        select
            .group_by
            .iter()
            .position(|g| g == expr)
            .map(|i| i_amem + i as i32)
    };
    // Rewrite each output expression: aggregate calls → `AggRef(agg_reg)`, and any subexpression
    // that exactly matches a GROUP BY expression → `AggRef(i_amem)` (so the projection in
    // `SELECT g, count(*) FROM t GROUP BY g` reads `g` from the previous-group-key register
    // during the output subroutine, instead of the now-exhausted table cursor). The GROUP BY
    // substitution happens at the top level only: if the whole expression equals a GROUP BY
    // expr, replace it; otherwise only rewrite aggregate calls inside it.
    let rewritten_outputs: Vec<(Expr, String)> = outputs
        .iter()
        .map(|(e, n)| (rewrite_projection_expr(e, &reg_of, &group_key_of), n.clone()))
        .collect();
    // Rewrite the HAVING expression the same way: aggregate calls → `AggRef(agg_reg)`, GROUP BY
    // key subexpressions → `AggRef(i_amem)`. Emitted in the output subroutine after `AggFinal`.
    let rewritten_having = select
        .having
        .as_ref()
        .map(|h| rewrite_projection_expr(h, &reg_of, &group_key_of));

    // Rewrite the ORDER BY expressions the same way as the projection: aggregate calls →
    // `AggRef(agg_reg)`, GROUP BY expression matches → `AggRef(i_amem)`. An ordinal `ORDER BY
    // n` selects the n-th output column (which is already rewritten); resolve it here so the
    // output sorter key is the output column value.
    let rewritten_order_by: Vec<(Expr, bool)> = if has_order_by {
        select
            .order_by
            .iter()
            .map(|term| {
                let expr = resolve_order_term(term, outputs)?;
                Ok((
                    rewrite_projection_expr(&expr, &reg_of, &group_key_of),
                    term.desc,
                ))
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };

    // (D) Open the table cursor.
    let open = b.emit(Opcode::OpenRead, cursor, table.rootpage as i32, 0);
    b.note_cursor(cursor);
    if table.without_rowid {
        b.set_p4(open, P4::KeyInfo(table.without_rowid_key_info()));
    } else {
        b.set_p4(open, P4::Int(table.columns.len() as i64));
    }

    // (D') Open the output sorter when ORDER BY is present. The record layout is
    // `[order_by_keys..., projection_columns...]`; the KeyInfo carries the ORDER BY
    // directions and BINARY collation (a future task threads through explicit COLLATE).
    if has_order_by {
        let keyinfo: Vec<KeyField> = select
            .order_by
            .iter()
            .map(|t| KeyField {
                desc: t.desc,
                collation: crate::types::Collation::Binary,
            })
            .collect();
        let so = b.emit(Opcode::SorterOpen, output_sorter, norder + ncol, 0);
        b.set_p4(so, P4::KeyInfo(keyinfo));
        b.note_cursor(output_sorter);
    }

    if nkey == 0 {
        // ---- No GROUP BY: accumulate one row over the whole scan, then emit once. ----
        // This is the `tag-select-0822` path (minus the min/max optimization, which we don't
        // implement yet). The accumulators are reset implicitly by `Accumulator::new` on the
        // first `AggStep` call (they start fresh); there is only one group.
        let end_scan = b.new_label();
        b.emit_jump(Opcode::Rewind, cursor, end_scan, 0);
        let scan_top = b.new_label();
        b.resolve(scan_top);
        let scan_next = b.new_label();
        if let Some(w) = &select.where_clause {
            compile_jump(&mut b, w, scan_next, false, true, ctx)?;
        }
        // Evaluate each aggregate's arguments and `AggStep` it.
        for (i, call) in agg_calls.iter().enumerate() {
            let kind = kinds[i];
            let n_arg = match &call.args {
                FunctionArgs::Star => 0u8,
                FunctionArgs::List(v) => v.len() as u8,
            };
            let arg_base = match &call.args {
                FunctionArgs::Star => 0,
                FunctionArgs::List(v) => {
                    let r = b.alloc_regs(v.len() as i32);
                    for (k, a) in v.iter().enumerate() {
                        compile_expr(&mut b, a, r + k as i32, ctx)?;
                    }
                    r
                }
            };
            let idx = b.emit(Opcode::AggStep, 0, arg_base, agg_reg_base + i as i32);
            b.set_p4(idx, P4::FuncDef(kind));
            b.set_p5(idx, n_arg);
        }
        b.resolve(scan_next);
        b.emit_jump(Opcode::Next, cursor, scan_top, 0);
        b.resolve(end_scan);

        // Finalize the accumulators into their registers.
        for (i, kind) in kinds.iter().enumerate() {
            let idx = b.emit(Opcode::AggFinal, agg_reg_base + i as i32, 0, 0);
            b.set_p4(idx, P4::FuncDef(*kind));
        }

        // Emit the single result row. OFFSET/LIMIT apply to the result of an aggregate without
        // GROUP BY (the LIMIT of a `SELECT count(*) FROM t LIMIT 0` is zero rows).
        let emit_end = b.new_label();
        // HAVING: filter the single aggregated row. `sqlite3ExprIfFalse(pHaving, addrEnd,
        // SQLITE_JUMPIFNULL)` — false or NULL jumps past the emission to `emit_end` (which
        // resolves to the Halt). This matches the upstream no-GROUP-BY path.
        if let Some(having) = &rewritten_having {
            compile_jump(&mut b, having, emit_end, false, true, ctx)?;
        }
        let result_reg = b.alloc_regs(ncol);
        for (j, (expr, _)) in rewritten_outputs.iter().enumerate() {
            compile_expr(&mut b, expr, result_reg + j as i32, ctx)?;
        }
        if has_order_by {
            // Insert the single aggregated row into the output sorter, then run the sort tail.
            let block = b.alloc_regs(norder + ncol);
            for (k, (expr, _)) in rewritten_order_by.iter().enumerate() {
                compile_expr(&mut b, expr, block + k as i32, ctx)?;
            }
            for j in 0..ncol {
                b.emit(Opcode::SCopy, result_reg + j, block + norder + j, 0);
            }
            let rec = b.alloc_reg();
            b.emit(Opcode::MakeRecord, block, norder + ncol, rec);
            b.emit(Opcode::SorterInsert, output_sorter, rec, 0);
            // Fall through to the sort tail.
            emit_sort_tail(
                &mut b,
                output_sorter,
                norder,
                ncol,
                limit_reg,
                offset_reg,
                emit_end,
            );
        } else {
            // No ORDER BY: emit the single row directly with OFFSET/LIMIT.
            if let Some(oreg) = offset_reg {
                b.emit_jump(Opcode::IfPos, oreg, emit_end, 1);
            }
            b.emit(Opcode::ResultRow, result_reg, ncol, 0);
            if let Some(lreg) = limit_reg {
                b.emit_jump(Opcode::DecrJumpZero, lreg, emit_end, 0);
            }
        }
        b.resolve(emit_end);
        b.emit(Opcode::Halt, 0, 0, 0);
        b.resolve(setup);
        b.emit(Opcode::Transaction, 0, 0, 0);
        b.emit(Opcode::Goto, 0, after_init, 0);
        return Ok(b.finish());
    }

    // ---- GROUP BY case: sort by group key, walk, emit per group. ----

    // Sorter records: [group_key_0, ..., group_key_{nkey-1}, agg_args..., payload...].
    // The payload is the per-row aggregate argument values; the key is the group key.
    //
    // Actually the simpler layout: [group_keys..., agg_args...] where agg_args is the
    // concatenation of every aggregate's argument registers. The output pass reads group keys
    // from the sorter directly; the agg-args are read for the `AggStep` call.

    // Determine per-aggregate arg counts.
    let agg_arg_counts: Vec<i32> = agg_calls
        .iter()
        .map(|c| match &c.args {
            FunctionArgs::Star => 0,
            FunctionArgs::List(v) => v.len() as i32,
        })
        .collect();
    let total_agg_args: i32 = agg_arg_counts.iter().sum();
    let n_sorter_fields = nkey + total_agg_args;

    // KeyInfo for the sorter: the GROUP BY keys, ASC BINARY (matches SQLite's default group
    // ordering; a future task threads through explicit COLLATE / DESC).
    let keyinfo: Vec<KeyField> = select
        .group_by
        .iter()
        .map(|_| KeyField::asc_binary())
        .collect();
    let sorter = 1i32;
    let so = b.emit(Opcode::SorterOpen, sorter, n_sorter_fields, 0);
    b.set_p4(so, P4::KeyInfo(keyinfo));
    b.note_cursor(sorter);

    // ---- Scan loop: filter, evaluate group keys + agg args, build record, sorter insert. ----
    let end_scan = b.new_label();
    b.emit_jump(Opcode::Rewind, cursor, end_scan, 0);
    let scan_top = b.new_label();
    b.resolve(scan_top);
    let scan_next = b.new_label();
    if let Some(w) = &select.where_clause {
        compile_jump(&mut b, w, scan_next, false, true, ctx)?;
    }
    let block = b.alloc_regs(n_sorter_fields);
    // Group keys first.
    for (k, gexpr) in select.group_by.iter().enumerate() {
        compile_expr(&mut b, gexpr, block + k as i32, ctx)?;
    }
    // Then each aggregate's arguments, in declaration order.
    let mut arg_offset = nkey;
    for (i, call) in agg_calls.iter().enumerate() {
        match &call.args {
            FunctionArgs::Star => {}
            FunctionArgs::List(v) => {
                for (k, a) in v.iter().enumerate() {
                    compile_expr(&mut b, a, block + arg_offset + k as i32, ctx)?;
                }
            }
        }
        arg_offset += agg_arg_counts[i];
    }
    let rec = b.alloc_reg();
    b.emit(Opcode::MakeRecord, block, n_sorter_fields, rec);
    b.emit(Opcode::SorterInsert, sorter, rec, 0);
    b.resolve(scan_next);
    b.emit_jump(Opcode::Next, cursor, scan_top, 0);
    b.resolve(end_scan);

    // ---- Output pass: walk the sorted sorter, detect group changes, emit per group. ----
    // Registers:
    //   iUseFlag    — 1 once the accumulator has seen any row (suppresses emitting an empty
    //                 first row when the input is empty).
    //   iAMem       — `nkey` registers holding the *previous* group key (for `Compare`).
    //   iBMem       — `nkey` registers holding the *current* group key (just read from sorter).
    let i_use_flag = b.alloc_reg();
    b.emit(Opcode::Integer, 0, i_use_flag, 0);
    // iAMem was already allocated during the projection rewrite (so `AggRef(i_amem + i)` in the
    // rewritten outputs reads the previous group's key during the output subroutine). Initialize
    // it to NULL here — the first group change's Compare sees NULL < key and routes to the
    // group-changed handler, which (because iUseFlag is still 0) skips emitting.
    b.emit(Opcode::Null, i_amem, i_amem + nkey - 1, 0);
    let i_bmem = b.alloc_regs(nkey);

    // Output subroutine addresses (resolved later).
    let addr_end = b.new_label();
    let addr_output_row = b.new_label();
    let reg_output_row = b.alloc_reg(); // return address register for the output subroutine
    let addr_reset = b.new_label();
    let reg_reset = b.alloc_reg(); // return address register for the reset subroutine

    // Kick off by resetting the accumulator (this is a no-op on the first call but matches
    // upstream's `OP_Gosub regReset, addrReset`).
    b.emit_jump(Opcode::Gosub, reg_reset, addr_reset, 0);

    // DISTINCT dedup cursor (one per output pass). Opened here so it survives across the
    // whole output walk; each emitted group row is checked for novelty against it.
    let distinct_cursor = select.distinct.then(|| {
        let c = 2i32;
        let oe = b.emit(Opcode::OpenEphemeral, c, ncol, 0);
        b.set_p4(oe, P4::KeyInfo(Vec::new()));
        c
    });

    // Sort the sorter and position at the first record, or jump to `addr_end` if empty.
    b.emit_jump(Opcode::SorterSort, sorter, addr_end, 0);
    let loop_top = b.new_label();
    b.resolve(loop_top);

    // Load the current group key from the sorter into iBMem.
    b.emit(Opcode::SorterData, sorter, 0, 0);
    for j in 0..nkey {
        b.emit(Opcode::Column, sorter, j, i_bmem + j);
    }

    // Compare the current key (iBMem) against the previous key (iAMem) under the group's
    // KeyInfo. The `Jump` that follows routes to:
    //   p1 = addr_after_step   — current < previous (shouldn't happen with a stable ASC sort,
    //                            but treat like "different group" → emit then step).
    //   p2 = addr_step          — equal → step only.
    //   p3 = addr_emit_then_step — current > previous → emit the previous group, then step.
    //
    // To match upstream's `OP_Compare`/`OP_Jump` idiom (P1=Less, P2=Equal, P3=Greater), we set:
    //   p1 = "group changed"  (treat as different — emit, then step)
    //   p2 = "same group"     (step only)
    //   p3 = "group changed"  (emit, then step)
    // We resolve these labels after emitting the step block.
    let addr_step_only = b.new_label();
    let addr_group_changed = b.new_label();
    {
        // We need a KeyInfo on the Compare. Reuse the GROUP BY KeyInfo (ASC BINARY).
        let cmp_ki: Vec<KeyField> = select
            .group_by
            .iter()
            .map(|_| KeyField::asc_binary())
            .collect();
        let cmp = b.emit(Opcode::Compare, i_amem, i_bmem, nkey);
        b.set_p4(cmp, P4::KeyInfo(cmp_ki));
        // Jump: Less→group_changed, Equal→step_only, Greater→group_changed.
        b.emit_jump3(Opcode::Jump, addr_group_changed, addr_step_only, addr_group_changed);
    }

    // ---- group-changed handler: emit a result row for the *previous* group, then step. ----
    b.resolve(addr_group_changed);
    b.emit_jump(Opcode::Gosub, reg_output_row, addr_output_row, 0);
    // Copy iBMem → iAMem so the new group becomes the "previous" for the next comparison.
    b.emit(Opcode::Copy, i_bmem, i_amem, nkey - 1);
    // Reset the accumulator for the new group (clears the per-group state).
    b.emit_jump(Opcode::Gosub, reg_reset, addr_reset, 0);
    // Fall through to the step block.

    // ---- step-only handler: update the accumulator with the current row's args. ----
    b.resolve(addr_step_only);
    for (i, call) in agg_calls.iter().enumerate() {
        let kind = kinds[i];
        let n_arg = match &call.args {
            FunctionArgs::Star => 0u8,
            FunctionArgs::List(v) => v.len() as u8,
        };
        // The args for aggregate `i` live at sorter column `nkey + agg_arg_offset_i`.
        let agg_arg_base = b.alloc_regs(agg_arg_counts[i].max(1));
        match &call.args {
            FunctionArgs::Star => {}
            FunctionArgs::List(v) => {
                // The starting sorter column index for this aggregate's args is
                // `nkey + sum(agg_arg_counts[0..i])`.
                let start_col = nkey
                    + agg_arg_counts[..i].iter().sum::<i32>();
                for (k, _) in v.iter().enumerate() {
                    b.emit(
                        Opcode::Column,
                        sorter,
                        start_col + k as i32,
                        agg_arg_base + k as i32,
                    );
                }
            }
        }
        let idx = b.emit(Opcode::AggStep, 0, agg_arg_base, agg_reg_base + i as i32);
        b.set_p4(idx, P4::FuncDef(kind));
        b.set_p5(idx, n_arg);
    }
    // Mark that we've seen at least one row in this group.
    b.emit(Opcode::Integer, 1, i_use_flag, 0);
    // Advance the sorter; loop back to the top.
    b.emit_jump(Opcode::SorterNext, sorter, loop_top, 0);

    // After the loop: emit the final group's row.
    b.emit_jump(Opcode::Gosub, reg_output_row, addr_output_row, 0);

    // When ORDER BY is present, the output sorter has been populated by the output
    // subroutine; run the sort tail now to emit the final ResultRows with OFFSET/LIMIT.
    if has_order_by {
        emit_sort_tail(
            &mut b,
            output_sorter,
            norder,
            ncol,
            limit_reg,
            offset_reg,
            addr_end,
        );
    }

    // Jump past the subroutines to `addr_end`.
    b.emit_jump(Opcode::Goto, 0, addr_end, 0);

    // ---- output subroutine: finalize accumulators, evaluate projection, emit a ResultRow. ----
    b.resolve(addr_output_row);
    // If we never saw any row (iUseFlag == 0), skip emitting. This is the upstream behavior
    // (the `OP_IfPos iUseFlag` guard before the finalize/emit body).
    let output_skip = b.new_label();
    b.emit_jump(Opcode::IfNot, i_use_flag, output_skip, 1);
    // Finalize each accumulator into its register.
    for (i, kind) in kinds.iter().enumerate() {
        let idx = b.emit(Opcode::AggFinal, agg_reg_base + i as i32, 0, 0);
        b.set_p4(idx, P4::FuncDef(*kind));
    }
    // HAVING: filter this group's row. `sqlite3ExprIfFalse(pHaving, addrOutputRow+1,
    // SQLITE_JUMPIFNULL)` — false or NULL jumps past the projection emission to `output_skip`
    // (which resolves just before the `Return`, matching upstream's `addrOutputRow+1` = OP_Return).
    if let Some(having) = &rewritten_having {
        compile_jump(&mut b, having, output_skip, false, true, ctx)?;
    }
    // Evaluate the projection (now containing `AggRef`s to the finalized registers and plain
    // column refs that read from the sorter's group-key columns).
    let result_reg = b.alloc_regs(ncol);
    for (j, (expr, _)) in rewritten_outputs.iter().enumerate() {
        compile_expr(&mut b, expr, result_reg + j as i32, ctx)?;
    }
    // DISTINCT dedup: skip this group's row if its output has been seen before; otherwise
    // record it. Mirrors the unordered scan's `Found`+`IdxInsert` pair.
    if let Some(dc) = distinct_cursor {
        let found = b.emit_jump(Opcode::Found, dc, output_skip, result_reg);
        b.set_p4(found, P4::Int(ncol as i64));
        let rec = b.alloc_reg();
        b.emit(Opcode::MakeRecord, result_reg, ncol, rec);
        b.emit(Opcode::IdxInsert, dc, rec, 0);
    }
    if has_order_by {
        // ORDER BY present: insert the group's row into the output sorter (keyed by the
        // ORDER BY expressions). The OFFSET/LIMIT and ResultRow emission happen in the sort
        // tail after the aggregate pass completes.
        let block = b.alloc_regs(norder + ncol);
        for (k, (expr, _)) in rewritten_order_by.iter().enumerate() {
            compile_expr(&mut b, expr, block + k as i32, ctx)?;
        }
        for j in 0..ncol {
            b.emit(Opcode::SCopy, result_reg + j, block + norder + j, 0);
        }
        let rec = b.alloc_reg();
        b.emit(Opcode::MakeRecord, block, norder + ncol, rec);
        b.emit(Opcode::SorterInsert, output_sorter, rec, 0);
    } else {
        // No ORDER BY: emit the row directly with OFFSET/LIMIT.
        let row_end = b.new_label();
        if let Some(oreg) = offset_reg {
            b.emit_jump(Opcode::IfPos, oreg, row_end, 1);
        }
        b.emit(Opcode::ResultRow, result_reg, ncol, 0);
        if let Some(lreg) = limit_reg {
            b.emit_jump(Opcode::DecrJumpZero, lreg, addr_end, 0);
        }
        b.resolve(row_end);
    }
    b.resolve(output_skip);
    b.emit(Opcode::Return, reg_output_row, 0, 0);

    // ---- reset subroutine: clear the accumulator state. ----
    b.resolve(addr_reset);
    // Clear each accumulator register (the executor's `aggregates` map is keyed by register;
    // setting the register to NULL would not remove the entry, so we instead rely on
    // `Accumulator::step`'s `finalized` reset path. The cleanest faithful approach is to remove
    // the entry, but the executor owns the map; we emit a no-op here and let the first
    // `AggStep` of the new group lazily create a fresh accumulator — the previous group's was
    // already consumed by `AggFinal` in the output subroutine).
    // Set iUseFlag = 0 to indicate the accumulator is empty.
    b.emit(Opcode::Integer, 0, i_use_flag, 0);
    b.emit(Opcode::Return, reg_reset, 0, 0);

    b.resolve(addr_end);
    b.emit(Opcode::Halt, 0, 0, 0);
    b.resolve(setup);
    b.emit(Opcode::Transaction, 0, 0, 0);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

/// Emit the sort-and-walk tail for an output sorter populated by the aggregate pass. The
/// sorter record layout is `[order_by_keys..., projection_columns...]` (the first `norder`
/// columns are the ORDER BY keys, the remaining `ncol` are the projection). This sorts the
/// sorter, walks it in sorted order, applies OFFSET/LIMIT, and emits a `ResultRow` per row.
/// `end_label` is resolved to the instruction after the walk (the caller emits the Halt there
/// or jumps past it).
fn emit_sort_tail(
    b: &mut ProgramBuilder,
    sorter: i32,
    norder: i32,
    ncol: i32,
    limit_reg: Option<i32>,
    offset_reg: Option<i32>,
    end_label: Label,
) {
    // Sort the sorter and position at the first record, or jump to `end_label` if empty.
    b.emit_jump(Opcode::SorterSort, sorter, end_label, 0);
    let out_top = b.cur_addr();
    let sort_next = b.new_label();
    b.emit(Opcode::SorterData, sorter, 0, 0);
    // OFFSET gate.
    if let Some(oreg) = offset_reg {
        b.emit_jump(Opcode::IfPos, oreg, sort_next, 1);
    }
    // Read the projection columns from the sorter record (after the ORDER BY keys).
    let result_reg = b.alloc_regs(ncol);
    for j in 0..ncol {
        b.emit(Opcode::Column, sorter, norder + j, result_reg + j);
    }
    b.emit(Opcode::ResultRow, result_reg, ncol, 0);
    if let Some(lreg) = limit_reg {
        b.emit_jump(Opcode::DecrJumpZero, lreg, end_label, 0);
    }
    b.resolve(sort_next);
    b.emit(Opcode::SorterNext, sorter, out_top, 0);
    // Fall through to `end_label` (the caller resolves it).
    let _ = end_label;
}

/// `true` if two aggregate calls are syntactically identical (same name, same args). Used to
/// deduplicate the same aggregate call appearing twice in the projection list so both sites
/// share one accumulator register — matching upstream's `AggInfo` deduplication.
fn agg_call_eq(a: &AggCall, b: &AggCall) -> bool {
    a.name.eq_ignore_ascii_case(&b.name) && a.args == b.args
}

/// Rewrite a projection expression for the GROUP BY output pass. This combines two
/// substitutions applied recursively:
/// * Any subexpression that exactly matches a GROUP BY expression becomes `AggRef(i_amem_reg)`
///   — this is checked *before* recursing, so a GROUP BY expression is treated as an atomic
///   value during the output pass (we don't look for aggregates inside it). This is what lets
///   `SELECT g, count(*) FROM t GROUP BY g` and `SELECT g || '!', count(*) FROM t GROUP BY g`
///   read `g` from the sorter-loaded iAMem register instead of the now-exhausted table cursor.
/// * Aggregate calls become `AggRef(agg_reg)` (via [`rewrite_aggregates`]).
fn rewrite_projection_expr(
    e: &Expr,
    reg_of: &impl Fn(&AggCall) -> Option<i32>,
    group_key_of: &impl Fn(&Expr) -> Option<i32>,
) -> Expr {
    if let Some(reg) = group_key_of(e) {
        return Expr::AggRef(reg);
    }
    // Not a GROUP BY expression as a whole — recurse and rewrite aggregates / nested GROUP BY
    // expression matches inside.
    rewrite_aggregates_with_group_keys(e, reg_of, group_key_of)
}

/// Like [`rewrite_aggregates`] but also substitutes any subexpression that exactly matches a
/// GROUP BY expression with `AggRef(i_amem_reg)`. The GROUP BY match is checked at every node
/// before recursing, so a GROUP BY expression nested inside a larger expression (e.g. `g` inside
/// `g || '!'`) is replaced while the surrounding `'!'` / `||` structure is preserved.
fn rewrite_aggregates_with_group_keys(
    e: &Expr,
    reg_of: &impl Fn(&AggCall) -> Option<i32>,
    group_key_of: &impl Fn(&Expr) -> Option<i32>,
) -> Expr {
    if let Some(reg) = group_key_of(e) {
        return Expr::AggRef(reg);
    }
    match e {
        Expr::Function { name, args, over, .. }
            if over.is_none()
                && is_aggregate_call(
                    name,
                    match args {
                        FunctionArgs::Star => 0,
                        FunctionArgs::List(v) => v.len(),
                    },
                ) =>
        {
            let call = AggCall {
                name: name.clone(),
                args: args.clone(),
            };
            match reg_of(&call) {
                Some(reg) => Expr::AggRef(reg),
                None => e.clone(),
            }
        }
        Expr::Unary { op, expr } => Expr::Unary {
            op: *op,
            expr: Box::new(rewrite_aggregates_with_group_keys(expr, reg_of, group_key_of)),
        },
        Expr::Binary { op, left, right } => Expr::Binary {
            op: *op,
            left: Box::new(rewrite_aggregates_with_group_keys(left, reg_of, group_key_of)),
            right: Box::new(rewrite_aggregates_with_group_keys(right, reg_of, group_key_of)),
        },
        Expr::Collate { expr, collation } => Expr::Collate {
            expr: Box::new(rewrite_aggregates_with_group_keys(expr, reg_of, group_key_of)),
            collation: collation.clone(),
        },
        Expr::Cast { expr, type_name } => Expr::Cast {
            expr: Box::new(rewrite_aggregates_with_group_keys(expr, reg_of, group_key_of)),
            type_name: type_name.clone(),
        },
        // Leaves and non-aggregate function calls: clone unchanged. (A plain column reference
        // that is NOT a GROUP BY expression reads from the table cursor — but the table cursor
        // is exhausted during the output pass, so such a query is actually invalid SQL. We
        // leave it as-is and let the executor error, matching upstream's "no such column" /
        // "misuse" behavior.)
        other => other.clone(),
    }
}

/// An indexed codegen. The shape depends on the plan's benefits:
///
/// * **WHERE equality prefix** (the `equality` list is non-empty): `SeekGE` the index to the
///   first entry `>=` the search key, then `IdxGT` at the top of every iteration to verify the
///   prefix is still equal. The body pulls the rowid, seeks the table (unless covering), and
///   projects.
/// * **ORDER BY only** (no WHERE equality, but `order_by_satisfied`): `Rewind` the index and
///   walk forward — the index order is the requested ORDER BY order, so no sorter is needed.
/// * **Covering** (`covering`): no table cursor is opened and no `IdxRowid`/`NotExists` pair
///   is emitted; projection / WHERE / ORDER BY read directly from the index cursor at the
///   mapped record positions.
///
/// The three compose: a covering ORDER-BY-only plan is the simplest (`Rewind` + project from
/// the index); a non-covering WHERE-equality plan is the M5.1 shape (seek + table lookup).
#[allow(clippy::too_many_arguments)]
fn compile_indexed_select(
    select: &SelectStmt,
    table: &Table,
    plan: &IndexPlan,
    outputs: &[(Expr, String)],
    limit: Option<i64>,
    offset: i64,
    subquery_resolver: Option<&dyn SubqueryResolver>,
) -> Result<Program> {
    let idx_cursor = 0i32;
    let table_cursor = 1i32;
    let ncol = outputs.len() as i32;
    let nkey_fields = plan.index.nkey_fields();

    // Build the covering-index column-position map: table_column_index → position in the
    // index key record. The index record is `[indexed cols..., rowid]`; the rowid-alias
    // column (if any) maps to `nkey_fields` (the trailing rowid).
    let column_positions: Vec<usize> = build_index_column_positions(table, &plan.index);
    let covering = plan.covering;
    let index_read = covering.then(|| super::expr::IndexRead {
        cursor: idx_cursor,
        column_positions: &column_positions,
        rowid_position: nkey_fields,
    });

    // The Ctx used for projection / WHERE / ORDER BY evaluation. For a covering plan it
    // points at the index cursor; otherwise at the table cursor (the rowid-seek target).
    let ctx = Ctx {
        table,
        cursor: if covering { idx_cursor } else { table_cursor },
        register_base: None, join_tables: None,
        index_read,
        subquery_resolver,
    };
    let mut b = ProgramBuilder::new();

    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    let after_init = b.cur_addr();

    // LIMIT 0 → no rows.
    if limit == Some(0) {
        b.emit(Opcode::Halt, 0, 0, 0);
        b.resolve(setup);
        b.emit(Opcode::Transaction, 0, 0, 0);
        b.emit(Opcode::Goto, 0, after_init, 0);
        return Ok(b.finish());
    }

    let limit_reg = match limit {
        Some(n) if n > 0 => Some(emit_int(&mut b, n)),
        _ => None,
    };
    let offset_reg = (offset > 0).then(|| emit_int(&mut b, offset));

    // (1) Open cursors. A covering plan opens only the index; a non-covering plan also opens
    // the table for the rowid seek.
    if !covering {
        let open_table = b.emit(Opcode::OpenRead, table_cursor, table.rootpage as i32, 0);
        b.set_p4(open_table, P4::Int(table.columns.len() as i64));
        b.note_cursor(table_cursor);
    }
    let open_idx = b.emit(Opcode::OpenRead, idx_cursor, plan.index.rootpage as i32, 0);
    b.note_cursor(idx_cursor);
    let key_info: Vec<KeyField> = plan
        .index
        .columns
        .iter()
        .map(|ic| KeyField {
            desc: ic.desc,
            collation: ic.collation,
        })
        .collect();
    b.set_p4(open_idx, P4::KeyInfo(key_info));

    // (2) Position the index. A WHERE-equality plan seeks to the first entry `>=` the search
    // key prefix; an ORDER-BY-only (or covering-only) plan rewinds to the first entry.
    let nkey = plan.equality.len() as i32;
    let key_reg = if nkey > 0 {
        let key_reg = b.alloc_regs(nkey);
        for (i, ek) in plan.equality.iter().enumerate() {
            emit_value_load(&mut b, &ek.value, key_reg + i as i32);
        }
        Some(key_reg)
    } else {
        None
    };
    let end_seek = b.new_label();
    if nkey > 0 {
        let seek = b.emit_jump(Opcode::SeekGE, idx_cursor, end_seek, key_reg.unwrap());
        b.set_p4(seek, P4::Int(nkey as i64));
    } else {
        b.emit_jump(Opcode::Rewind, idx_cursor, end_seek, 0);
    }

    // DISTINCT dedup cursor (opened before the loop so it survives across iterations).
    let distinct_cursor_id = if covering { 1i32 } else { 2i32 };
    let distinct_cursor = select.distinct.then(|| {
        let c = 2i32;
        let oe = b.emit(Opcode::OpenEphemeral, c, ncol, 0);
        b.set_p4(oe, P4::KeyInfo(Vec::new()));
        b.note_cursor(c);
        c
    });

    // (3) Loop body. The IdxGT boundary check is re-emitted at the top of every iteration
    // (only for the WHERE-equality shape) so the loop terminates when the index key prefix
    // no longer matches. A covering plan skips the IdxRowid + NotExists table lookup.
    let loop_top = b.new_label();
    b.resolve(loop_top);
    if nkey > 0 {
        let idx_gt = b.emit_jump(Opcode::IdxGT, idx_cursor, end_seek, key_reg.unwrap());
        b.set_p4(idx_gt, P4::Int(nkey as i64));
    }
    let idx_next = b.new_label();
    if !covering {
        let rowid_reg = b.alloc_reg();
        b.emit(Opcode::IdxRowid, idx_cursor, rowid_reg, 0);
        b.emit_jump(Opcode::NotExists, table_cursor, idx_next, rowid_reg);
    }

    // Re-check the WHERE clause on the row. The SeekGE+IdxGT only verified the indexed-column
    // equality prefix; a WHERE with additional terms (or a non-equality predicate that the
    // planner couldn't turn into a prefix, like `IS NULL`) is re-evaluated here against the
    // row's column values. For a covering plan the columns are read from the index cursor
    // (via `ctx.index_read`); for a non-covering plan they're read from the table cursor.
    if let Some(w) = &select.where_clause {
        compile_jump(&mut b, w, idx_next, false, true, ctx)?;
    }

    // OFFSET gate.
    if let Some(oreg) = offset_reg {
        b.emit_jump(Opcode::IfPos, oreg, idx_next, 1);
    }

    // Project the result columns.
    let result_reg = b.alloc_regs(ncol);
    for (j, (expr, _)) in outputs.iter().enumerate() {
        compile_expr(&mut b, expr, result_reg + j as i32, ctx)?;
    }
    if let Some(dc) = distinct_cursor {
        let found = b.emit_jump(Opcode::Found, dc, idx_next, result_reg);
        b.set_p4(found, P4::Int(ncol as i64));
        let rec = b.alloc_reg();
        b.emit(Opcode::MakeRecord, result_reg, ncol, rec);
        b.emit(Opcode::IdxInsert, dc, rec, 0);
    }
    b.emit(Opcode::ResultRow, result_reg, ncol, 0);
    if let Some(lreg) = limit_reg {
        b.emit_jump(Opcode::DecrJumpZero, lreg, end_seek, 0);
    }

    // Advance: next index entry, jumping back to the top of the body. `idx_next` is the
    // "skip this row" target — it lands on the `Next` so the cursor still advances (a skip
    // must NOT terminate the scan). `end_seek` is the "stop the scan" target (Halt).
    b.resolve(idx_next);
    b.emit_jump(Opcode::Next, idx_cursor, loop_top, 0);
    b.resolve(end_seek);

    b.emit(Opcode::Halt, 0, 0, 0);
    b.resolve(setup);
    b.emit(Opcode::Transaction, 0, 0, 0);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

/// Build the covering-index column-position map: `column_positions[table_col_idx]` = position
/// of that table column's value in the index key record. The index record is
/// `[indexed cols..., rowid]`, so an indexed column at index position `j` maps to `j`, and the
/// rowid-alias column (if any) maps to `nkey_fields` (the trailing rowid). Columns not in the
/// index get `usize::MAX` (a covering plan will never read them, but the sentinel is
/// defensive).
fn build_index_column_positions(table: &Table, index: &IndexObject) -> Vec<usize> {
    let nkey = index.nkey_fields();
    let mut out = vec![usize::MAX; table.columns.len()];
    for (j, ic) in index.columns.iter().enumerate() {
        if let Some(ci) = table.column_index(&ic.name) {
            out[ci] = j;
        }
    }
    if let Some(alias) = table.rowid_alias {
        out[alias] = nkey;
    }
    out
}

/// Emit a register load of a literal [`Value`] (used by the indexed path's constant-RHS).
fn emit_value_load(b: &mut ProgramBuilder, v: &Value, target: i32) {
    match v {
        Value::Null => {
            b.emit(Opcode::Null, 0, target, 0);
        }
        Value::Int(n) => match i32::try_from(*n) {
            Ok(n32) => {
                b.emit(Opcode::Integer, n32, target, 0);
            }
            Err(_) => {
                let i = b.emit(Opcode::Int64, 0, target, 0);
                b.set_p4(i, P4::Int(*n));
            }
        },
        Value::Real(r) => {
            let i = b.emit(Opcode::Real, 0, target, 0);
            b.set_p4(i, P4::Real(*r));
        }
        Value::Text(s) => {
            let i = b.emit(Opcode::String8, 0, target, 0);
            b.set_p4(i, P4::Text(s.clone()));
        }
        Value::Blob(bytes) => {
            let i = b.emit(Opcode::Blob, 0, target, 0);
            b.set_p4(i, P4::Blob(bytes.clone()));
        }
    }
}

/// Reject out-of-scope features with a clear message.
fn reject_unsupported(select: &SelectStmt) -> Result<()> {
    if !select.compound.is_empty() {
        return Err(Error::msg(
            "compound SELECT (UNION/INTERSECT/EXCEPT) is not supported by the executor yet",
        ));
    }
    if select.distinct && !select.order_by.is_empty() {
        // The DISTINCT+ORDER BY combination needs the WHERE_DISTINCT_ORDERED path (a
        // single sorted pass with prev-row comparison). M6.6 ships the UNORDERED path
        // (ephemeral index); the combined case lands with M6.8 (GROUP BY + ORDER BY).
        return Err(Error::msg(
            "DISTINCT with ORDER BY is not supported yet (M6.8)",
        ));
    }
    Ok(())
}

/// Recursively check that no window-only function (row_number/rank/dense_rank/percent_rank/
/// cume_dist/ntile/first_value/last_value/nth_value/lead/lag) is used *without* an `OVER`
/// clause. Upstream raises "misuse of window function <name>()" — the user must write `OVER
/// (...)` (even `OVER ()`) for these. A window-only function with `OVER` is a window query
/// rejected earlier by [`has_window_function_query`]; this walker catches the bare-window-only
/// case so it doesn't fall through to the scalar codegen path (which would call `func::check`
/// and either wrongly accept it as a scalar or raise a less informative "no such function").
fn check_no_window_only_without_over(e: &Expr) -> Result<()> {
    use crate::func::aggregate::is_window_only_name;
    match e {
        Expr::Function { name, args, over, .. } => {
            let n_arg = match args {
                FunctionArgs::Star => 0,
                FunctionArgs::List(v) => v.len(),
            };
            if over.is_none() && is_window_only_name(name) {
                // Resolve the arity to confirm this is the window-only function at this arity
                // (e.g. `lead` at 4 args is "no such function", not "misuse of window function").
                if crate::func::aggregate::AggregateKind::from_name(name, n_arg).is_some() {
                    return Err(Error::msg(format!(
                        "misuse of window function {}()",
                        name
                    )));
                }
                // Wrong arity falls through to the scalar `check` path, which raises the
                // upstream "no such function" / "wrong number of arguments" error.
            }
            if let FunctionArgs::List(v) = args {
                for a in v {
                    check_no_window_only_without_over(a)?;
                }
            }
        }
        Expr::Unary { expr, .. } => check_no_window_only_without_over(expr)?,
        Expr::Binary { left, right, .. } => {
            check_no_window_only_without_over(left)?;
            check_no_window_only_without_over(right)?;
        }
        Expr::Between { expr, low, high, .. } => {
            check_no_window_only_without_over(expr)?;
            check_no_window_only_without_over(low)?;
            check_no_window_only_without_over(high)?;
        }
        Expr::In { expr, values, .. } => {
            check_no_window_only_without_over(expr)?;
            for v in values {
                check_no_window_only_without_over(v)?;
            }
        }
        Expr::InSubquery { expr, .. } => check_no_window_only_without_over(expr)?,
        Expr::Cast { expr, .. } => check_no_window_only_without_over(expr)?,
        Expr::Case {
            base,
            when_then,
            else_expr,
        } => {
            if let Some(b) = base {
                check_no_window_only_without_over(b)?;
            }
            for (w, t) in when_then {
                check_no_window_only_without_over(w)?;
                check_no_window_only_without_over(t)?;
            }
            if let Some(e) = else_expr {
                check_no_window_only_without_over(e)?;
            }
        }
        Expr::Collate { expr, .. } => check_no_window_only_without_over(expr)?,
        Expr::IsDistinctFrom { left, right, .. } => {
            check_no_window_only_without_over(left)?;
            check_no_window_only_without_over(right)?;
        }
        Expr::Row(es) => {
            for e in es {
                check_no_window_only_without_over(e)?;
            }
        }
        Expr::Coalesce2 { left, right } => {
            check_no_window_only_without_over(left)?;
            check_no_window_only_without_over(right)?;
        }
        Expr::Literal(_) | Expr::Column { .. } | Expr::BindParam(_)
        | Expr::Exists(_) | Expr::Subquery(_) | Expr::AggRef(_) => {}
    }
    Ok(())
}

/// `true` if `e` contains an aggregate function call (recursing through the operator tree).
/// Mirrors the analysis walk upstream does in `sqlite3ExprAnalyzeAggregates`. A function name
/// that is also an aggregate name (e.g. `max`) only counts as an aggregate when its argument
/// count matches the aggregate's arity — `max(a, 0)` is the scalar `max`, not the aggregate.
///
/// A call with an `OVER (...)` clause is a **window** function call, not a plain aggregate,
/// even if the name is one of the aggregate names (`count(*) OVER (...)`); those are detected
/// separately by [`contains_window_function`]. This function returns `true` only for plain
/// aggregate calls (`over.is_none()`).
fn contains_aggregate(e: &Expr) -> bool {
    match e {
        Expr::Function { name, args, over, .. } => {
            let n_arg = match args {
                FunctionArgs::Star => 0,
                FunctionArgs::List(v) => v.len(),
            };
            // A windowed call (OVER present) is not a plain aggregate even if the name matches.
            over.is_none()
                && is_aggregate_call(name, n_arg)
                || matches!(args, FunctionArgs::List(v) if v.iter().any(contains_aggregate))
        }
        Expr::Unary { expr, .. } => contains_aggregate(expr),
        Expr::Binary { left, right, .. } => contains_aggregate(left) || contains_aggregate(right),
        Expr::Between { expr, low, high, .. } => {
            contains_aggregate(expr) || contains_aggregate(low) || contains_aggregate(high)
        }
        Expr::In { expr, values, .. } => {
            contains_aggregate(expr) || values.iter().any(contains_aggregate)
        }
        Expr::InSubquery { expr, .. } => contains_aggregate(expr),
        Expr::Cast { expr, .. } => contains_aggregate(expr),
        Expr::Case {
            base,
            when_then,
            else_expr,
        } => {
            base.as_ref().is_some_and(|b| contains_aggregate(b))
                || when_then
                    .iter()
                    .any(|(w, t)| contains_aggregate(w) || contains_aggregate(t))
                || else_expr.as_ref().is_some_and(|e| contains_aggregate(e))
        }
        Expr::Collate { expr, .. } => contains_aggregate(expr),
        Expr::IsDistinctFrom { left, right, .. } => {
            contains_aggregate(left) || contains_aggregate(right)
        }
        Expr::Row(es) => es.iter().any(contains_aggregate),
        Expr::Coalesce2 { left, right } => contains_aggregate(left) || contains_aggregate(right),
        // Plain leaves — no aggregate hidden inside.
        Expr::Literal(_)
        | Expr::Column { .. }
        | Expr::BindParam(_)
        | Expr::Exists(_)
        | Expr::Subquery(_)
        | Expr::AggRef(_) => false,
    }
}

/// Whether this SELECT is an aggregate query: has a GROUP BY clause, or any aggregate function
/// call in its projection list.
fn is_aggregate_query(select: &SelectStmt, outputs: &[(Expr, String)]) -> bool {
    !select.group_by.is_empty() || outputs.iter().any(|(e, _)| contains_aggregate(e))
}

/// `true` if `e` contains a **window function call** — any function call with an `OVER (...)`
/// clause (aggregate-as-window or window-only). Mirrors the `EP_WinFunc` flag upstream sets
/// during expression analysis.
pub(crate) fn contains_window_function(e: &Expr) -> bool {
    match e {
        Expr::Function { over, args, .. } => {
            over.is_some()
                || matches!(args, FunctionArgs::List(v) if v.iter().any(contains_window_function))
        }
        Expr::Unary { expr, .. } => contains_window_function(expr),
        Expr::Binary { left, right, .. } => {
            contains_window_function(left) || contains_window_function(right)
        }
        Expr::Between { expr, low, high, .. } => {
            contains_window_function(expr)
                || contains_window_function(low)
                || contains_window_function(high)
        }
        Expr::In { expr, values, .. } => {
            contains_window_function(expr) || values.iter().any(contains_window_function)
        }
        Expr::InSubquery { expr, .. } => contains_window_function(expr),
        Expr::Cast { expr, .. } => contains_window_function(expr),
        Expr::Case {
            base,
            when_then,
            else_expr,
        } => {
            base.as_ref().is_some_and(|b| contains_window_function(b))
                || when_then
                    .iter()
                    .any(|(w, t)| contains_window_function(w) || contains_window_function(t))
                || else_expr.as_ref().is_some_and(|e| contains_window_function(e))
        }
        Expr::Collate { expr, .. } => contains_window_function(expr),
        Expr::IsDistinctFrom { left, right, .. } => {
            contains_window_function(left) || contains_window_function(right)
        }
        Expr::Row(es) => es.iter().any(contains_window_function),
        Expr::Coalesce2 { left, right } => {
            contains_window_function(left) || contains_window_function(right)
        }
        // Plain leaves — no window function hidden inside.
        Expr::Literal(_)
        | Expr::Column { .. }
        | Expr::BindParam(_)
        | Expr::Exists(_)
        | Expr::Subquery(_)
        | Expr::AggRef(_) => false,
    }
}

/// A located window-function call discovered during analysis: the function name, its argument
/// list (or `Star`), and the `OVER` clause's resolved window spec. Used by the future window
/// codegen path (M11.7) to emit the partition-sort + frame-step program.
#[derive(Clone, Debug)]
pub(crate) struct WindowCall {
    pub name: String,
    pub args: FunctionArgs,
    pub filter: Option<Box<Expr>>,
    pub window: Window,
}

/// Walk an expression, collecting every window-function call (any function call with an
/// `OVER (...)` clause) in evaluation order. The future window codegen path (M11.7) will use
/// this to allocate per-call accumulator registers and emit the partition-sort + frame-step
/// program. For now, [`compile`] rejects window queries via [`has_window_function_query`] with
/// "misuse of window function" / "window functions are not yet supported" until 11.7 lands.
fn collect_window_functions(e: &Expr, out: &mut Vec<WindowCall>) {
    match e {
        Expr::Function {
            name,
            distinct: _,
            args,
            filter,
            over,
        } => {
            if let Some(window) = over {
                out.push(WindowCall {
                    name: name.clone(),
                    args: args.clone(),
                    filter: filter.clone(),
                    window: window.clone(),
                });
            } else if let FunctionArgs::List(v) = args {
                for a in v {
                    collect_window_functions(a, out);
                }
            }
        }
        Expr::Unary { expr, .. } => collect_window_functions(expr, out),
        Expr::Binary { left, right, .. } => {
            collect_window_functions(left, out);
            collect_window_functions(right, out);
        }
        Expr::Between { expr, low, high, .. } => {
            collect_window_functions(expr, out);
            collect_window_functions(low, out);
            collect_window_functions(high, out);
        }
        Expr::In { expr, values, .. } => {
            collect_window_functions(expr, out);
            for v in values {
                collect_window_functions(v, out);
            }
        }
        Expr::InSubquery { expr, .. } => collect_window_functions(expr, out),
        Expr::Cast { expr, .. } => collect_window_functions(expr, out),
        Expr::Case {
            base,
            when_then,
            else_expr,
        } => {
            if let Some(b) = base {
                collect_window_functions(b, out);
            }
            for (w, t) in when_then {
                collect_window_functions(w, out);
                collect_window_functions(t, out);
            }
            if let Some(e) = else_expr {
                collect_window_functions(e, out);
            }
        }
        Expr::Collate { expr, .. } => collect_window_functions(expr, out),
        Expr::IsDistinctFrom { left, right, .. } => {
            collect_window_functions(left, out);
            collect_window_functions(right, out);
        }
        Expr::Row(es) => es.iter().for_each(|e| collect_window_functions(e, out)),
        Expr::Coalesce2 { left, right } => {
            collect_window_functions(left, out);
            collect_window_functions(right, out);
        }
        Expr::Literal(_) | Expr::Column { .. } | Expr::BindParam(_)
        | Expr::Exists(_) | Expr::Subquery(_) | Expr::AggRef(_) => {}
    }
}

/// `true` if this SELECT uses any window function (any function call with an `OVER` clause).
fn has_window_function_query(select: &SelectStmt, outputs: &[(Expr, String)]) -> bool {
    outputs.iter().any(|(e, _)| contains_window_function(e))
        || select
            .where_clause
            .as_ref()
            .is_some_and(|w| contains_window_function(w))
        || select
            .having
            .as_ref()
            .is_some_and(|h| contains_window_function(h))
        || select
            .order_by
            .iter()
            .any(|t| contains_window_function(&t.expr))
}

/// A located aggregate call discovered during analysis: the function name, its argument list
/// (or `Star`), and a stable identity used to deduplicate the same call appearing twice.
#[derive(Clone, Debug)]
struct AggCall {
    name: String,
    args: FunctionArgs,
}

/// Walk an expression, collecting every plain-aggregate function call in evaluation order
/// (left to right). The same call syntactically appearing twice is recorded once and both sites
/// reference the same accumulator register — matching upstream's `AggInfo` deduplication.
///
/// A call with an `OVER (...)` clause is a **window** function call, not a plain aggregate, and
/// is *not* collected here even if its name is one of the aggregate names (`count(*) OVER (...)`).
/// Window calls are collected separately by [`collect_window_functions`].
fn collect_aggregates(e: &Expr, out: &mut Vec<AggCall>) {
    match e {
        Expr::Function { name, args, over, .. }
            if over.is_none()
                && is_aggregate_call(
                    name,
                    match args {
                        FunctionArgs::Star => 0,
                        FunctionArgs::List(v) => v.len(),
                    },
                ) =>
        {
            // The arguments of an aggregate are NOT themselves walked for nested aggregates —
            // SQLite disallows nested aggregate calls (`sum(count(*))` is a parse error).
            out.push(AggCall {
                name: name.clone(),
                args: args.clone(),
            });
        }
        Expr::Function { args, .. } => {
            // A non-aggregate function call (or a window call) — recurse into its arguments so a
            // plain aggregate nested inside (e.g. `length(count(*))` — though that's actually
            // invalid SQL, the walker still descends) is collected. Window calls' arguments are
            // walked too so a plain aggregate inside a windowed call's args is still detected
            // for the outer query's aggregate analysis (rare but valid).
            if let FunctionArgs::List(v) = args {
                for a in v {
                    collect_aggregates(a, out);
                }
            }
        }
        Expr::Unary { expr, .. } => collect_aggregates(expr, out),
        Expr::Binary { left, right, .. } => {
            collect_aggregates(left, out);
            collect_aggregates(right, out);
        }
        Expr::Between { expr, low, high, .. } => {
            collect_aggregates(expr, out);
            collect_aggregates(low, out);
            collect_aggregates(high, out);
        }
        Expr::In { expr, values, .. } => {
            collect_aggregates(expr, out);
            for v in values {
                collect_aggregates(v, out);
            }
        }
        Expr::InSubquery { expr, .. } => collect_aggregates(expr, out),
        Expr::Cast { expr, .. } => collect_aggregates(expr, out),
        Expr::Case {
            base,
            when_then,
            else_expr,
        } => {
            if let Some(b) = base {
                collect_aggregates(b, out);
            }
            for (w, t) in when_then {
                collect_aggregates(w, out);
                collect_aggregates(t, out);
            }
            if let Some(e) = else_expr {
                collect_aggregates(e, out);
            }
        }
        Expr::Collate { expr, .. } => collect_aggregates(expr, out),
        Expr::IsDistinctFrom { left, right, .. } => {
            collect_aggregates(left, out);
            collect_aggregates(right, out);
        }
        Expr::Row(es) => es.iter().for_each(|e| collect_aggregates(e, out)),
        Expr::Literal(_) | Expr::Column { .. } | Expr::BindParam(_)
        | Expr::Exists(_) | Expr::Subquery(_) | Expr::AggRef(_) | Expr::Coalesce2 { .. } => {}
    }
}

/// Rewrite an expression tree so every plain-aggregate call is replaced by an `AggRef` to its
/// assigned accumulator register. Non-aggregate function calls are left untouched. The
/// `reg_of` closure maps a syntactic call (name + args) to its register; calls not present in
/// `reg_of` (which should not happen if analysis was complete) are left as-is.
///
/// A call with an `OVER (...)` clause is a window function call and is *not* rewritten here even
/// if its name is an aggregate name — the window codegen path (M11.7) handles windowed calls
/// separately.
fn rewrite_aggregates(e: &Expr, reg_of: &impl Fn(&AggCall) -> Option<i32>) -> Expr {
    match e {
        Expr::Function { name, args, over, .. }
            if over.is_none()
                && is_aggregate_call(
                    name,
                    match args {
                        FunctionArgs::Star => 0,
                        FunctionArgs::List(v) => v.len(),
                    },
                ) =>
        {
            let call = AggCall {
                name: name.clone(),
                args: args.clone(),
            };
            match reg_of(&call) {
                Some(reg) => Expr::AggRef(reg),
                None => e.clone(),
            }
        }
        Expr::Unary { op, expr } => Expr::Unary {
            op: *op,
            expr: Box::new(rewrite_aggregates(expr, reg_of)),
        },
        Expr::Binary { op, left, right } => Expr::Binary {
            op: *op,
            left: Box::new(rewrite_aggregates(left, reg_of)),
            right: Box::new(rewrite_aggregates(right, reg_of)),
        },
        Expr::Between { expr, low, high, negated } => Expr::Between {
            expr: Box::new(rewrite_aggregates(expr, reg_of)),
            low: Box::new(rewrite_aggregates(low, reg_of)),
            high: Box::new(rewrite_aggregates(high, reg_of)),
            negated: *negated,
        },
        Expr::In { expr, values, negated } => Expr::In {
            expr: Box::new(rewrite_aggregates(expr, reg_of)),
            values: values.iter().map(|v| rewrite_aggregates(v, reg_of)).collect(),
            negated: *negated,
        },
        Expr::InSubquery { expr, subquery, negated } => Expr::InSubquery {
            expr: Box::new(rewrite_aggregates(expr, reg_of)),
            subquery: subquery.clone(),
            negated: *negated,
        },
        Expr::Cast { expr, type_name } => Expr::Cast {
            expr: Box::new(rewrite_aggregates(expr, reg_of)),
            type_name: type_name.clone(),
        },
        Expr::Case { base, when_then, else_expr } => Expr::Case {
            base: base.as_ref().map(|b| Box::new(rewrite_aggregates(b, reg_of))),
            when_then: when_then
                .iter()
                .map(|(w, t)| (rewrite_aggregates(w, reg_of), rewrite_aggregates(t, reg_of)))
                .collect(),
            else_expr: else_expr.as_ref().map(|e| Box::new(rewrite_aggregates(e, reg_of))),
        },
        Expr::Collate { expr, collation } => Expr::Collate {
            expr: Box::new(rewrite_aggregates(expr, reg_of)),
            collation: collation.clone(),
        },
        Expr::IsDistinctFrom { left, right, negated } => Expr::IsDistinctFrom {
            left: Box::new(rewrite_aggregates(left, reg_of)),
            right: Box::new(rewrite_aggregates(right, reg_of)),
            negated: *negated,
        },
        Expr::Row(es) => Expr::Row(es.iter().map(|e| rewrite_aggregates(e, reg_of)).collect()),
        // Leaves: clone unchanged.
        other => other.clone(),
    }
}

/// A `VALUES` select body: emit one result row per literal/constant row. ORDER BY / LIMIT /
/// OFFSET are supported by wrapping the values in a sorter exactly like a constant SELECT does.
fn compile_values(
    select: &SelectStmt,
    outputs: &[(Expr, String)],
    limit: Option<i64>,
    offset: i64,
    subquery_resolver: Option<&dyn SubqueryResolver>,
) -> Result<Program> {
    // Validate arity: every row must have the same number of columns as the expansion result set.
    let ncol = outputs.len();
    for row in &select.values {
        if row.len() != ncol {
            return Err(Error::msg(format!(
                "all VALUES must have the same number of terms - {} vs {}",
                row.len(),
                ncol
            )));
        }
    }

    let empty = Table {
        name: String::new(),
        rootpage: 0,
        columns: Vec::new(),
        rowid_alias: None,
        without_rowid: false,
        pk_columns: Vec::new(),
    };
    let ctx = Ctx {
        table: &empty,
        cursor: -1,
        register_base: None, join_tables: None,
        index_read: None,
        subquery_resolver,
    };
    let ncol_i32 = ncol as i32;
    let mut b = ProgramBuilder::new();

    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    let after_init = b.cur_addr();

    let no_rows = limit == Some(0) || offset > 0;
    let limit_reg = match limit {
        Some(n) if n > 0 => Some(emit_int(&mut b, n)),
        _ => None,
    };
    let offset_reg = (offset > 0).then(|| emit_int(&mut b, offset));

    if !no_rows {
        let end = b.new_label();
        if select.order_by.is_empty() {
            for row in &select.values {
                if let Some(w) = &select.where_clause {
                    compile_jump(&mut b, w, end, false, true, ctx)?;
                }
                if let Some(oreg) = offset_reg {
                    b.emit_jump(Opcode::IfPos, oreg, end, 1);
                }
                let result_reg = b.alloc_regs(ncol_i32);
                for (j, expr) in row.iter().enumerate() {
                    compile_expr(&mut b, expr, result_reg + j as i32, ctx)?;
                }
                b.emit(Opcode::ResultRow, result_reg, ncol_i32, 0);
                if let Some(lreg) = limit_reg {
                    b.emit_jump(Opcode::DecrJumpZero, lreg, end, 0);
                }
            }
        } else {
            let sorter = 0i32;
            let keyinfo: Vec<KeyField> = select
                .order_by
                .iter()
                .map(|t| KeyField {
                    desc: t.desc,
                    collation: crate::types::Collation::Binary,
                })
                .collect();
            let nkey = select.order_by.len() as i32;
            let so = b.emit(Opcode::SorterOpen, sorter, nkey + ncol_i32, 0);
            b.set_p4(so, P4::KeyInfo(keyinfo));
            b.note_cursor(sorter);

            for row in &select.values {
                if let Some(w) = &select.where_clause {
                    compile_jump(&mut b, w, end, false, true, ctx)?;
                }
                let block = b.alloc_regs(nkey + ncol_i32);
                for (k, term) in select.order_by.iter().enumerate() {
                    let key_expr = resolve_order_term(term, outputs)?;
                    compile_expr(&mut b, &key_expr, block + k as i32, ctx)?;
                }
                for (j, expr) in row.iter().enumerate() {
                    compile_expr(&mut b, expr, block + nkey + j as i32, ctx)?;
                }
                let rec = b.alloc_reg();
                b.emit(Opcode::MakeRecord, block, nkey + ncol_i32, rec);
                b.emit(Opcode::SorterInsert, sorter, rec, 0);
            }

            // Output loop: sorted iteration with OFFSET/LIMIT.
            let end_out = b.new_label();
            b.emit_jump(Opcode::SorterSort, sorter, end_out, 0);
            let out_top = b.cur_addr();
            let sort_next = b.new_label();
            b.emit(Opcode::SorterData, sorter, 0, 0);
            if let Some(oreg) = offset_reg {
                b.emit_jump(Opcode::IfPos, oreg, sort_next, 1);
            }
            let result_reg = b.alloc_regs(ncol_i32);
            for j in 0..ncol_i32 {
                b.emit(Opcode::Column, sorter, nkey + j, result_reg + j);
            }
            b.emit(Opcode::ResultRow, result_reg, ncol_i32, 0);
            if let Some(lreg) = limit_reg {
                b.emit_jump(Opcode::DecrJumpZero, lreg, end_out, 0);
            }
            b.resolve(sort_next);
            b.emit(Opcode::SorterNext, sorter, out_top, 0);
            b.resolve(end_out);
        }
        b.resolve(end);
    }

    b.emit(Opcode::Halt, 0, 0, 0);
    b.resolve(setup);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

/// A table scan, optionally ordered, with LIMIT/OFFSET.
fn compile_scan(
    select: &SelectStmt,
    table: &Table,
    outputs: &[(Expr, String)],
    limit: Option<i64>,
    offset: i64,
    subquery_resolver: Option<&dyn SubqueryResolver>,
) -> Result<Program> {
    let cursor = 0i32;
    let ncol = outputs.len() as i32;
    let ctx = Ctx {
        table,
        cursor,
        register_base: None, join_tables: None,
        index_read: None,
        subquery_resolver,
    };
    let mut b = ProgramBuilder::new();

    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0); // addr 0
    let after_init = b.cur_addr(); // addr 1

    // LIMIT 0 → no rows at all.
    if limit == Some(0) {
        b.emit(Opcode::Halt, 0, 0, 0);
        b.resolve(setup);
        b.emit(Opcode::Transaction, 0, 0, 0);
        b.emit(Opcode::Goto, 0, after_init, 0);
        return Ok(b.finish());
    }

    // LIMIT / OFFSET counter registers.
    let limit_reg = match limit {
        Some(n) if n > 0 => Some(emit_int(&mut b, n)),
        _ => None,
    };
    let offset_reg = (offset > 0).then(|| emit_int(&mut b, offset));

    let open = b.emit(Opcode::OpenRead, cursor, table.rootpage as i32, 0);
    b.note_cursor(cursor);
    if table.without_rowid {
        // A WITHOUT ROWID table is an index b-tree keyed by the PK record; open it with the
        // table's KeyInfo so the IndexCursor compares correctly during ordered scans.
        b.set_p4(open, P4::KeyInfo(table.without_rowid_key_info()));
    } else {
        b.set_p4(open, P4::Int(table.columns.len() as i64));
    }

    if select.order_by.is_empty() {
        compile_scan_unordered(&mut b, select, ctx, outputs, ncol, limit_reg, offset_reg)?;
    } else {
        compile_scan_ordered(&mut b, select, ctx, outputs, ncol, limit_reg, offset_reg)?;
    }

    b.resolve(setup);
    b.emit(Opcode::Transaction, 0, 0, 0);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

#[allow(clippy::too_many_arguments)]
fn compile_scan_unordered(
    b: &mut ProgramBuilder,
    select: &SelectStmt,
    ctx: Ctx,
    outputs: &[(Expr, String)],
    ncol: i32,
    limit_reg: Option<i32>,
    offset_reg: Option<i32>,
) -> Result<()> {
    let cursor = ctx.cursor;
    // DISTINCT dedup cursor: an ephemeral index keyed by the result row record. Allocated
    // lazily so non-DISTINCT scans don't pay for it. The check runs *before* OFFSET/LIMIT
    // (matching upstream's `codeDistinct` preceding `codeOffset`): a row that's a duplicate
    // is skipped entirely and never counts toward the OFFSET/LIMIT counters.
    let distinct_cursor = select.distinct.then(|| {
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

    if let Some(w) = &select.where_clause {
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

#[allow(clippy::too_many_arguments)]
fn compile_scan_ordered(
    b: &mut ProgramBuilder,
    select: &SelectStmt,
    ctx: Ctx,
    outputs: &[(Expr, String)],
    ncol: i32,
    limit_reg: Option<i32>,
    offset_reg: Option<i32>,
) -> Result<()> {
    let cursor = ctx.cursor;
    let sorter = 1i32;
    let order = &select.order_by;
    let nkey = order.len() as i32;

    let keyinfo: Vec<KeyField> = order
        .iter()
        .map(|t| KeyField {
            desc: t.desc,
            collation: crate::types::Collation::Binary,
        })
        .collect();
    let so = b.emit(Opcode::SorterOpen, sorter, nkey + ncol, 0);
    b.set_p4(so, P4::KeyInfo(keyinfo));
    b.note_cursor(sorter);

    // --- scan loop: filter, build [keys..., outputs...] records, insert into the sorter ---
    let end_scan = b.new_label();
    b.emit_jump(Opcode::Rewind, cursor, end_scan, 0);
    let scan_top = b.cur_addr();
    let scan_next = b.new_label();

    if let Some(w) = &select.where_clause {
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

    // --- output loop: sorted iteration with OFFSET/LIMIT ---
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
        // Output column j lives at record index nkey+j.
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

/// A constant `SELECT` (no `FROM`) produces exactly one row (zero if `LIMIT 0` or `OFFSET > 0`).
fn compile_constant(
    select: &SelectStmt,
    outputs: &[(Expr, String)],
    limit: Option<i64>,
    offset: i64,
    subquery_resolver: Option<&dyn SubqueryResolver>,
) -> Result<Program> {
    // No table: column references resolve against an empty table and therefore error.
    let empty = Table {
        name: String::new(),
        rootpage: 0,
        columns: Vec::new(),
        rowid_alias: None,
        without_rowid: false,
        pk_columns: Vec::new(),
    };
    let ctx = Ctx {
        table: &empty,
        cursor: -1,
        register_base: None, join_tables: None,
        index_read: None,
        subquery_resolver,
    };
    let ncol = outputs.len() as i32;
    let mut b = ProgramBuilder::new();

    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    let after_init = b.cur_addr();

    let no_rows = limit == Some(0) || offset > 0;
    if !no_rows {
        let end = b.new_label();
        if let Some(w) = &select.where_clause {
            compile_jump(&mut b, w, end, false, true, ctx)?;
        }
        let result_reg = b.alloc_regs(ncol);
        for (j, (expr, _)) in outputs.iter().enumerate() {
            compile_expr(&mut b, expr, result_reg + j as i32, ctx)?;
        }
        b.emit(Opcode::ResultRow, result_reg, ncol, 0);
        b.resolve(end);
    }
    b.emit(Opcode::Halt, 0, 0, 0);
    b.resolve(setup);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

/// Expand `*` / `table.*` and resolve aliases into `(expression, column-name)` pairs.
pub(crate) fn expand_columns(select: &SelectStmt, table: Option<&Table>) -> Result<Vec<(Expr, String)>> {
    if !select.values.is_empty() {
        // VALUES rows provide unnamed output columns. Name them column1, column2, ... matching
        // the oracle. The arity was already validated by the caller; use the first row's length.
        let mut out = Vec::new();
        let ncols = select.values[0].len();
        for i in 0..ncols {
            out.push((select.values[0][i].clone(), format!("column{}", i + 1)));
        }
        return Ok(out);
    }
    let mut out = Vec::new();
    for rc in &select.columns {
        match rc {
            ResultColumn::Star | ResultColumn::TableStar(_) => {
                let t = table.ok_or_else(|| Error::msg("no tables specified"))?;
                for col in &t.columns {
                    out.push((column_expr(&col.name), col.name.clone()));
                }
            }
            ResultColumn::Expr { expr, alias } => {
                let name = alias.clone().unwrap_or_else(|| default_col_name(expr));
                out.push((expr.clone(), name));
            }
        }
    }
    if out.is_empty() {
        return Err(Error::msg("no result columns"));
    }
    Ok(out)
}

/// Like [`expand_columns`] but for a multi-table (join) FROM clause. `*` expands to all columns
/// of all tables in FROM order; `table.*` expands to the columns of the named table. Bare
/// column expressions are left as-is (the join codegen resolves them via `Ctx::join_tables`).
pub(crate) fn expand_columns_with_tables(
    select: &SelectStmt,
    tables: &[(&Table, &str)],
) -> Result<Vec<(Expr, String)>> {
    let mut out = Vec::new();
    for rc in &select.columns {
        match rc {
            ResultColumn::Star => {
                for (t, tname) in tables {
                    for col in &t.columns {
                        out.push((column_expr_for(tname, &col.name), col.name.clone()));
                    }
                }
            }
            ResultColumn::TableStar(qname) => {
                let (t, tname) = tables
                    .iter()
                    .find(|(_, name)| name.eq_ignore_ascii_case(qname))
                    .ok_or_else(|| Error::msg(format!("no such table: {qname}")))?;
                for col in &t.columns {
                    out.push((column_expr_for(tname, &col.name), col.name.clone()));
                }
            }
            ResultColumn::Expr { expr, alias } => {
                let name = alias.clone().unwrap_or_else(|| default_col_name(expr));
                out.push((expr.clone(), name));
            }
        }
    }
    if out.is_empty() {
        return Err(Error::msg("no result columns"));
    }
    Ok(out)
}

fn column_expr_for(table: &str, name: &str) -> Expr {
    Expr::Column {
        schema: None,
        table: Some(table.to_string()),
        name: name.to_string(),
    }
}

fn column_expr(name: &str) -> Expr {
    Expr::Column {
        schema: None,
        table: None,
        name: name.to_string(),
    }
}

/// Resolve an `ORDER BY` term: an integer ordinal selects an output column; a bare name that
/// matches an output alias uses that output's expression; otherwise the term is used as written.
pub(crate) fn resolve_order_term(term: &OrderingTerm, outputs: &[(Expr, String)]) -> Result<Expr> {
    if let Expr::Literal(Literal::Integer(n)) = &term.expr {
        let idx = *n;
        if idx >= 1 && (idx as usize) <= outputs.len() {
            return Ok(outputs[(idx - 1) as usize].0.clone());
        }
        return Err(Error::msg(format!(
            "ORDER BY term out of range - should be between 1 and {}",
            outputs.len()
        )));
    }
    if let Expr::Column {
        table: None, name, ..
    } = &term.expr
    {
        if let Some((expr, _)) = outputs.iter().find(|(_, n)| n.eq_ignore_ascii_case(name)) {
            return Ok(expr.clone());
        }
    }
    Ok(term.expr.clone())
}

/// Const-evaluate a literal-integer `LIMIT`/`OFFSET`. Returns `(limit, offset)` where `limit`
/// is `None` for "unlimited" (absent or negative) and `Some(n)` otherwise; `offset` is clamped
/// to `>= 0`.
pub(crate) fn eval_limit_offset(select: &SelectStmt) -> Result<(Option<i64>, i64)> {
    let limit = match &select.limit {
        None => None,
        Some(e) => {
            let n = const_int(e)
                .ok_or_else(|| Error::msg("only integer-literal LIMIT is supported in M3a"))?;
            (n >= 0).then_some(n)
        }
    };
    let offset = match &select.offset {
        None => 0,
        Some(e) => const_int(e)
            .ok_or_else(|| Error::msg("only integer-literal OFFSET is supported in M3a"))?
            .max(0),
    };
    Ok((limit, offset))
}

fn const_int(e: &Expr) -> Option<i64> {
    match e {
        Expr::Literal(Literal::Integer(n)) => Some(*n),
        Expr::Unary {
            op: UnaryOp::Negate,
            expr,
        } => const_int(expr).map(|n| -n),
        Expr::Unary {
            op: UnaryOp::Positive,
            expr,
        } => const_int(expr),
        _ => None,
    }
}

/// Emit a load of an `i64` constant into a fresh register, returning it.
pub(crate) fn emit_int(b: &mut ProgramBuilder, n: i64) -> i32 {
    let r = b.alloc_reg();
    match i32::try_from(n) {
        Ok(n32) => {
            b.emit(Opcode::Integer, n32, r, 0);
        }
        Err(_) => {
            let i = b.emit(Opcode::Int64, 0, r, 0);
            b.set_p4(i, P4::Int(n));
        }
    }
    r
}

/// A best-effort default column name for an unaliased non-column expression. SQLite uses the
/// expression's source text; without spans we reconstruct an approximation (only used for
/// header display — the result *rows* are unaffected).
pub(crate) fn default_col_name(expr: &Expr) -> String {
    match expr {
        Expr::Column { name, .. } => name.clone(),
        other => expr_to_text(other),
    }
}

/// Render an expression back into a SQL-like text form. This is intentionally public so that
/// other modules (e.g., schema index error messages) can build human-readable expression text.
pub fn expr_to_text(e: &Expr) -> String {
    use rustqlite_parser::FunctionArgs;
    match e {
        Expr::Literal(Literal::Null) => "NULL".to_string(),
        Expr::Literal(Literal::Integer(n)) => n.to_string(),
        Expr::Literal(Literal::Real(r)) => fp_to_text(*r),
        Expr::Literal(Literal::Text(s)) => format!("'{}'", s.replace('\'', "''")),
        Expr::Literal(Literal::Blob(_)) => "x'..'".to_string(),
        Expr::Literal(Literal::Bool(b)) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        Expr::Column {
            table: Some(t),
            name,
            ..
        } => format!("{t}.{name}"),
        Expr::Column { name, .. } => name.clone(),
        Expr::Unary { op, expr } => {
            let s = expr_to_text(expr);
            match op {
                UnaryOp::Negate => format!("-{s}"),
                UnaryOp::Positive => format!("+{s}"),
                UnaryOp::Not => format!("NOT {s}"),
                UnaryOp::BitNot => format!("~{s}"),
            }
        }
        Expr::Binary { op, left, right } => {
            let sym = binary_symbol(*op);
            format!("{}{}{}", expr_to_text(left), sym, expr_to_text(right))
        }
        Expr::Function { name, args, .. } => {
            let inner = match args {
                FunctionArgs::Star => "*".to_string(),
                FunctionArgs::List(v) => v.iter().map(expr_to_text).collect::<Vec<_>>().join(", "),
            };
            format!("{name}({inner})")
        }
        Expr::BindParam(s) => s.clone(),
        Expr::Between { .. } => "between".to_string(),
        Expr::In { .. } => "in".to_string(),
        Expr::InSubquery { .. } => "in".to_string(),
        Expr::Exists(_) => "exists".to_string(),
        Expr::Subquery(_) => "subquery".to_string(),
        Expr::Cast { .. } => "cast".to_string(),
        Expr::Case { .. } => "case".to_string(),
        Expr::Collate { expr, collation } => {
            let s = expr_to_text(expr);
            format!("{} COLLATE {}", s, collation)
        }
        Expr::IsDistinctFrom { .. } => "is_distinct".to_string(),
        Expr::Row(es) => {
            let inner = es.iter().map(expr_to_text).collect::<Vec<_>>().join(", ");
            format!("({inner})")
        }
        Expr::AggRef(r) => format!("agg#{r}"),
        Expr::Coalesce2 { left, right } => {
            format!("coalesce({}, {})", expr_to_text(left), expr_to_text(right))
        }
    }
}

fn binary_symbol(op: rustqlite_parser::BinaryOp) -> &'static str {
    use rustqlite_parser::BinaryOp::*;
    match op {
        Or => " OR ",
        And => " AND ",
        Eq => " = ",
        Ne => " <> ",
        Lt => " < ",
        Le => " <= ",
        Gt => " > ",
        Ge => " >= ",
        Add => " + ",
        Sub => " - ",
        Mul => " * ",
        Div => " / ",
        Mod => " % ",
        Concat => " || ",
        Is => " IS ",
        IsNot => " IS NOT ",
        Like => " LIKE ",
        Glob => " GLOB ",
        Regexp => " REGEXP ",
        Match => " MATCH ",
        BitAnd => " & ",
        BitOr => " | ",
        ShiftLeft => " << ",
        ShiftRight => " >> ",
        JsonExtract => " -> ",
        JsonExtractText => " ->> ",
    }
}

/// Helper for the golden codegen test: a readable disassembly of a program.
#[cfg(test)]
pub(crate) fn disassemble(p: &Program) -> Vec<String> {
    p.instructions
        .iter()
        .enumerate()
        .map(|(addr, i)| format_inst(addr, i))
        .collect()
}

#[cfg(test)]
fn format_inst(addr: usize, i: &crate::vdbe::program::Instruction) -> String {
    format!(
        "{addr} {} {} {} {} {:?} {}",
        i.opcode.name(),
        i.p1,
        i.p2,
        i.p3,
        i.p4,
        i.p5
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{SchemaObject, Table};
    use rustqlite_parser::{parse, Stmt};

    fn compile_sql(create: &str, select_sql: &str) -> (Program, Vec<String>) {
        let obj = SchemaObject {
            rowid: 1,
            obj_type: "table".into(),
            name: "t".into(),
            tbl_name: "t".into(),
            rootpage: 2,
            sql: Some(create.into()),
        };
        let table = Table::from_schema_object(&obj).unwrap();
        let Stmt::Select(s) = parse(select_sql).unwrap().into_iter().next().unwrap() else {
            panic!("expected SELECT")
        };
        compile(&s, Some(&table), &[], None).unwrap()
    }

    #[test]
    fn golden_select_a_b_where_a_gt_1() {
        let (prog, names) = compile_sql("CREATE TABLE t(a,b)", "SELECT a, b FROM t WHERE a > 1;");
        assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
        // The hand-verified canonical sequence for the scan. The WHERE `a > 1` is lowered to a
        // jump-if-false `Le` (the negation of `>`) with the JUMPIFNULL flag (0x10) and the
        // comparison affinity in the low bits (BLOB=0x01 → p5 = 0x11 = 17). Constant literals
        // are loaded inline (we do not yet hoist them into the init block as upstream does).
        let expected = vec![
            "0 Init 0 11 0 None 0",
            "1 OpenRead 0 2 0 Int(2) 0",
            "2 Rewind 0 10 0 None 0",
            "3 Column 0 0 1 None 0",
            "4 Integer 1 2 0 None 0",
            "5 Le 2 9 1 None 17",
            "6 Column 0 0 3 None 0",
            "7 Column 0 1 4 None 0",
            "8 ResultRow 3 2 0 None 0",
            "9 Next 0 3 0 None 0",
            "10 Halt 0 0 0 None 0",
            "11 Transaction 0 0 0 None 0",
            "12 Goto 0 1 0 None 0",
        ];
        assert_eq!(disassemble(&prog), expected);
    }
}
