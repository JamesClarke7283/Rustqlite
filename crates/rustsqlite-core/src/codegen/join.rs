//! Multi-table (join) codegen. Implements the two-table cross / inner / left / right / full
//! joins as a nested loop. Natural joins and `USING` arrive in later M7 tasks.
//!
//! The codegen shape for a two-table cross/inner join is:
//! ```text
//!   OpenRead  cur_a, root_a, 0
//!   OpenRead  cur_b, root_b, 0
//!   Rewind    cur_a, end
//!   loop_a:
//!     Rewind  cur_b, next_a
//!     loop_b:
//!       <ON predicate? jump to next_b on false>
//!       <WHERE predicate? jump to next_b on false>
//!       <project; ResultRow>
//!     next_b:
//!       Next    cur_b, loop_b
//!   next_a:
//!     Next      cur_a, loop_a
//!   end:
//!   Halt
//! ```
//! The projection / WHERE / ON expressions resolve column references across both tables via
//! `Ctx::join_tables`.

use rustqlite_parser::{Expr, JoinOp, SelectStmt, TableOrJoin};

use crate::error::{Error, Result};
use crate::schema::Table;
use crate::vdbe::program::{P4, Program};
use crate::vdbe::{KeyField, Opcode};

use super::builder::ProgramBuilder;
use super::expr::{compile_expr, compile_jump, Ctx, JoinTable};
use super::select::{eval_limit_offset, expand_columns_with_tables, resolve_order_term};

/// Compile a two-table cross / inner / left / right / full join. `tables` is in the JOIN
/// order (the first table is the outer/left loop, the second is the inner/right loop).
/// `from_order` is the ORIGINAL FROM order (for `SELECT *` expansion and bare-column
/// resolution). For a non-RIGHT join these are the same; for a RIGHT JOIN `tables` is swapped
/// while `from_order` keeps the original order. When `left_join` is true, a left-outer join is
/// emitted (NULL-fill the inner table on no match). When `full_join` is true, a second pass
/// scans the (original) right table and emits NULL-filled left rows for right rows that had no
/// left match; `left_join` is implied.
#[allow(clippy::too_many_arguments)]
pub fn compile_cross_join(
    select: &SelectStmt,
    tables: &[(&Table, &str); 2],
    from_order: &[(&Table, &str); 2],
    on_predicate: Option<&Expr>,
    left_join: bool,
    full_join: bool,
) -> Result<(Program, Vec<String>)> {
    let (limit, offset) = eval_limit_offset(select)?;
    let outputs = expand_columns_with_tables(select, from_order)?;
    let names: Vec<String> = outputs.iter().map(|(_, n)| n.clone()).collect();
    let ncol = outputs.len() as i32;

    // `join_tables` is in the JOIN order (`tables`), so column resolution maps each table to
    // its actual cursor (0 for the outer loop, 1 for the inner). For a RIGHT JOIN the tables
    // are swapped: the original right table is on cursor 0, the original left table on cursor
    // 1; `join_tables` reflects this so `t1.col` resolves to the right cursor.
    let join_tables: [JoinTable; 2] = [
        JoinTable {
            table: tables[0].0,
            cursor: 0,
            name: tables[0].1,
        },
        JoinTable {
            table: tables[1].0,
            cursor: 1,
            name: tables[1].1,
        },
    ];
    let ctx = Ctx {
        table: tables[0].0,
        cursor: 0,
        register_base: None,
        index_read: None,
        join_tables: Some(&join_tables),
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
        return Ok((b.finish(), names));
    }
    let limit_reg = match limit {
        Some(n) if n > 0 => Some(super::select::emit_int(&mut b, n)),
        _ => None,
    };
    let offset_reg = (offset > 0).then(|| super::select::emit_int(&mut b, offset));

    // Open both table cursors.
    for (i, (t, _)) in tables.iter().enumerate() {
        let open = b.emit(Opcode::OpenRead, i as i32, t.rootpage as i32, 0);
        if t.without_rowid {
            b.set_p4(open, P4::KeyInfo(t.without_rowid_key_info()));
        } else {
            b.set_p4(open, P4::Int(t.columns.len() as i64));
        }
    }

    // ORDER BY: the cross join doesn't use an index for ordering, so fall back to the sorter
    // when ORDER BY is present (mirroring `compile_scan_ordered`).
    let has_order_by = !select.order_by.is_empty();
    let norder = select.order_by.len() as i32;
    let sorter = 2i32;
    if has_order_by {
        let keyinfo: Vec<KeyField> = select
            .order_by
            .iter()
            .map(|t| KeyField {
                desc: t.desc,
                collation: crate::types::Collation::Binary,
            })
            .collect();
        let so = b.emit(Opcode::SorterOpen, sorter, norder + ncol, 0);
        b.set_p4(so, P4::KeyInfo(keyinfo));
    }

    // Outer loop over the left table.
    let end = b.new_label();
    b.emit_jump(Opcode::Rewind, 0, end, 0);
    let loop_a = b.new_label();
    b.resolve(loop_a);

    // For a LEFT JOIN, a per-outer-row flag tracks whether any inner row matched. When the
    // inner loop ends with no match, a NULL-filled right-table row is emitted.
    let match_flag = if left_join { Some(b.alloc_reg()) } else { None };
    if let Some(mf) = match_flag {
        b.emit(Opcode::Integer, 0, mf, 0);
    }

    // Inner loop over the right table.
    let next_a = b.new_label();
    // For a LEFT JOIN, the "no match" path lands here (after the inner loop). For a cross/
    // inner join, the inner loop exhaustion jumps straight to next_a.
    let no_match = b.new_label();
    b.emit_jump(Opcode::Rewind, 1, if left_join { no_match } else { next_a }, 0);
    let loop_b = b.new_label();
    b.resolve(loop_b);
    let next_b = b.new_label();

    // ON predicate (inner/left join): jump to next_b on false.
    if let Some(on) = on_predicate {
        compile_jump(&mut b, on, next_b, false, true, ctx)?;
    }

    // A match was found: set the flag for LEFT JOIN.
    if let Some(mf) = match_flag {
        b.emit(Opcode::Integer, 1, mf, 0);
    }

    // WHERE clause: jump to next_b on false. (For a LEFT JOIN, a WHERE on the right table's
    // columns filters out NULL-filled rows too — NULL comparisons yield UNKNOWN, which is
    // false, so the row is skipped. This matches SQLite's semantics.)
    if let Some(w) = &select.where_clause {
        compile_jump(&mut b, w, next_b, false, true, ctx)?;
    }

    // Project.
    let result_reg = b.alloc_regs(ncol);
    for (j, (expr, _)) in outputs.iter().enumerate() {
        compile_expr(&mut b, expr, result_reg + j as i32, ctx)?;
    }

    if has_order_by {
        // Build [order_keys..., projection...] and insert into the sorter.
        let block = b.alloc_regs(norder + ncol);
        for (k, term) in select.order_by.iter().enumerate() {
            let key_expr = resolve_order_term(term, &outputs)?;
            compile_expr(&mut b, &key_expr, block + k as i32, ctx)?;
        }
        for j in 0..ncol {
            b.emit(Opcode::SCopy, result_reg + j, block + norder + j, 0);
        }
        let rec = b.alloc_reg();
        b.emit(Opcode::MakeRecord, block, norder + ncol, rec);
        b.emit(Opcode::SorterInsert, sorter, rec, 0);
    } else {
        // Emit directly with OFFSET/LIMIT.
        if let Some(oreg) = offset_reg {
            b.emit_jump(Opcode::IfPos, oreg, next_b, 1);
        }
        b.emit(Opcode::ResultRow, result_reg, ncol, 0);
        if let Some(lreg) = limit_reg {
            b.emit_jump(Opcode::DecrJumpZero, lreg, end, 0);
        }
    }

    // Advance inner loop.
    b.resolve(next_b);
    b.emit_jump(Opcode::Next, 1, loop_b, 0);

    // LEFT JOIN no-match handler: after the inner loop ends with no match, set the right
    // cursor to a NULL row and emit one row. The WHERE clause is re-applied (a WHERE on the
    // right table's columns will filter this out since NULL comparisons are UNKNOWN).
    if let Some(mf) = match_flag {
        b.resolve(no_match);
        // If a match was found, skip the NULL-row emission.
        let skip = b.new_label();
        b.emit_jump(Opcode::If, mf, skip, 0);
        // Set the right cursor to a NULL row.
        b.emit(Opcode::NullRow, 1, 0, 0);
        // Re-apply the WHERE clause on the NULL-filled row.
        if let Some(w) = &select.where_clause {
            compile_jump(&mut b, w, skip, false, true, ctx)?;
        }
        // Project the NULL-filled row.
        let null_result_reg = b.alloc_regs(ncol);
        for (j, (expr, _)) in outputs.iter().enumerate() {
            compile_expr(&mut b, expr, null_result_reg + j as i32, ctx)?;
        }
        if has_order_by {
            let block = b.alloc_regs(norder + ncol);
            for (k, term) in select.order_by.iter().enumerate() {
                let key_expr = resolve_order_term(term, &outputs)?;
                compile_expr(&mut b, &key_expr, block + k as i32, ctx)?;
            }
            for j in 0..ncol {
                b.emit(Opcode::SCopy, null_result_reg + j, block + norder + j, 0);
            }
            let rec = b.alloc_reg();
            b.emit(Opcode::MakeRecord, block, norder + ncol, rec);
            b.emit(Opcode::SorterInsert, sorter, rec, 0);
        } else {
            if let Some(oreg) = offset_reg {
                b.emit_jump(Opcode::IfPos, oreg, skip, 1);
            }
            b.emit(Opcode::ResultRow, null_result_reg, ncol, 0);
            if let Some(lreg) = limit_reg {
                b.emit_jump(Opcode::DecrJumpZero, lreg, end, 0);
            }
        }
        b.resolve(skip);
    }

    // Advance outer loop.
    b.resolve(next_a);
    b.emit_jump(Opcode::Next, 0, loop_a, 0);

    // FULL JOIN second pass: scan the (original) right table again and emit NULL-filled left
    // rows for any right row that had no left match. The check is a nested loop: for each
    // right row, walk every left row and test the ON predicate; if none match, this right row
    // is "unmatched" and gets a NULL-filled left row.
    //
    // The WHERE clause is re-applied on the NULL-filled left row (a WHERE on left-table
    // columns will filter it out since NULL comparisons yield UNKNOWN). This mirrors SQLite's
    // FULL JOIN semantics.
    //
    // LIMIT applies globally to the FULL JOIN result, so the second pass decrements the same
    // limit register. When the limit is exhausted we jump to `end`, skipping the remaining
    // right rows (matching SQLite, which stops once the limit is reached).
    if full_join {
        // `right_cursor` is the cursor of the original right table — cursor 1 in the JOIN
        // order (for a non-RIGHT FULL JOIN the tables are not swapped).
        let right_cursor = 1i32;
        let left_cursor = 0i32;

        // The "found a match" flag for the current right row.
        let rj_match = b.alloc_reg();
        // Reuse the right-table scan cursor (already open). Rewind it again for the second
        // pass; jump to `end` if empty.
        let end_rj = b.new_label();
        b.emit_jump(Opcode::Rewind, right_cursor, end_rj, 0);
        let rj_outer = b.new_label();
        b.resolve(rj_outer);

        // For each right row, scan all left rows and test the ON predicate. If any left row
        // matches, set `rj_match=1`. The left cursor is rewound for each right row.
        b.emit(Opcode::Integer, 0, rj_match, 0);
        let left_empty = b.new_label();
        b.emit_jump(Opcode::Rewind, left_cursor, left_empty, 0);
        let rj_inner = b.new_label();
        b.resolve(rj_inner);
        // ON predicate: on match, set rj_match=1 and break out of the inner scan.
        if let Some(on) = on_predicate {
            let on_match = b.new_label();
            // `jump_if_null=false`: a NULL operand makes the comparison UNKNOWN, which is
            // neither true nor false. We don't jump on NULL — instead the next instruction
            // (Next) advances to the next left row. This matches SQL 3-valued logic for the
            // ON predicate so a right row whose ON comparison is always UNKNOWN (e.g. a
            // NULL join key) has no left match and is emitted as a NULL-filled left row.
            compile_jump(&mut b, on, on_match, true, false, ctx)?;
            // ON predicate false: advance to the next left row. If the left cursor is
            // exhausted, fall through to `left_empty` (no match for this right row).
            b.emit_jump(Opcode::Next, left_cursor, rj_inner, 0);
            b.emit_jump(Opcode::Goto, 0, left_empty, 0);
            b.resolve(on_match);
            b.emit(Opcode::Integer, 1, rj_match, 0);
            // Jump out of the inner scan after a match (no need to check remaining left rows).
            b.emit_jump(Opcode::Goto, 0, left_empty, 0);
        } else {
            // No ON predicate: every right row matches every left row (cross join), so all
            // right rows are matched. Mark matched and skip.
            b.emit(Opcode::Integer, 1, rj_match, 0);
            b.emit_jump(Opcode::Goto, 0, left_empty, 0);
        }
        b.resolve(left_empty);

        // If the right row matched at least one left row, skip NULL-row emission.
        let rj_skip = b.new_label();
        b.emit_jump(Opcode::If, rj_match, rj_skip, 0);

        // Emit a NULL-filled left row + the current right row. Set the left cursor to a NULL
        // row so reads from left-table columns return NULL.
        b.emit(Opcode::NullRow, left_cursor, 0, 0);

        // Re-apply the WHERE clause on the NULL-filled row.
        if let Some(w) = &select.where_clause {
            compile_jump(&mut b, w, rj_skip, false, true, ctx)?;
        }

        // Project the NULL-filled left + current right row.
        let rj_result_reg = b.alloc_regs(ncol);
        for (j, (expr, _)) in outputs.iter().enumerate() {
            compile_expr(&mut b, expr, rj_result_reg + j as i32, ctx)?;
        }
        if has_order_by {
            let block = b.alloc_regs(norder + ncol);
            for (k, term) in select.order_by.iter().enumerate() {
                let key_expr = resolve_order_term(term, &outputs)?;
                compile_expr(&mut b, &key_expr, block + k as i32, ctx)?;
            }
            for j in 0..ncol {
                b.emit(Opcode::SCopy, rj_result_reg + j, block + norder + j, 0);
            }
            let rec = b.alloc_reg();
            b.emit(Opcode::MakeRecord, block, norder + ncol, rec);
            b.emit(Opcode::SorterInsert, sorter, rec, 0);
        } else {
            if let Some(oreg) = offset_reg {
                b.emit_jump(Opcode::IfPos, oreg, rj_skip, 1);
            }
            b.emit(Opcode::ResultRow, rj_result_reg, ncol, 0);
            if let Some(lreg) = limit_reg {
                b.emit_jump(Opcode::DecrJumpZero, lreg, end, 0);
            }
        }
        b.resolve(rj_skip);

        // Advance to the next right row.
        b.emit_jump(Opcode::Next, right_cursor, rj_outer, 0);
        b.resolve(end_rj);
    }

    b.resolve(end);

    // ORDER BY sort tail.
    if has_order_by {
        let end_out = b.new_label();
        b.emit_jump(Opcode::SorterSort, sorter, end_out, 0);
        let out_top = b.cur_addr();
        let sort_next = b.new_label();
        b.emit(Opcode::SorterData, sorter, 0, 0);
        if let Some(oreg) = offset_reg {
            b.emit_jump(Opcode::IfPos, oreg, sort_next, 1);
        }
        let out_reg = b.alloc_regs(ncol);
        for j in 0..ncol {
            b.emit(Opcode::Column, sorter, norder + j, out_reg + j);
        }
        b.emit(Opcode::ResultRow, out_reg, ncol, 0);
        if let Some(lreg) = limit_reg {
            b.emit_jump(Opcode::DecrJumpZero, lreg, end_out, 0);
        }
        b.resolve(sort_next);
        b.emit(Opcode::SorterNext, sorter, out_top, 0);
        b.resolve(end_out);
    }

    b.emit(Opcode::Halt, 0, 0, 0);
    b.resolve(setup);
    b.emit(Opcode::Transaction, 0, 0, 0);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok((b.finish(), names))
}

/// Flatten a `FROM` clause into a list of `(TableRef, Option<JoinConstraint>)` for the
/// cross-join codegen. Returns `Some(list)` when the clause is a simple cross/comma join of
/// plain tables (no subqueries, no LEFT/RIGHT/FULL/NATURAL joins, no USING); `None` for
/// anything the M7 first slice doesn't handle.
pub fn flatten_cross_join(from: &[TableOrJoin]) -> Option<Vec<(&rustqlite_parser::TableRef, Option<&rustqlite_parser::JoinConstraint>)>> {
    let mut out = Vec::new();
    flatten_into(from, &mut out);
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn flatten_into<'a>(
    from: &'a [TableOrJoin],
    out: &mut Vec<(&'a rustqlite_parser::TableRef, Option<&'a rustqlite_parser::JoinConstraint>)>,
) {
    for item in from {
        match item {
            TableOrJoin::Table(t) => out.push((t, None)),
            TableOrJoin::Subquery { .. } => {
                out.clear();
                return;
            }
            TableOrJoin::Join(j) => {
                // Handle CROSS, INNER, LEFT, RIGHT, and FULL joins. NATURAL is rejected by
                // `validate_join`.
                match j.op {
                    JoinOp::Cross | JoinOp::Inner | JoinOp::Left | JoinOp::LeftOuter
                    | JoinOp::Right | JoinOp::RightOuter
                    | JoinOp::Full | JoinOp::FullOuter => {}
                    _ => {
                        out.clear();
                        return;
                    }
                }
                // Recurse into the left side.
                flatten_into(std::slice::from_ref(&*j.left), out);
                if out.is_empty() {
                    return;
                }
                out.push((&j.right, j.constraint.as_ref()));
            }
        }
    }
}

/// The `ON` predicate extracted from a join constraint (if any). `None` for a cross join or
/// a comma join without a constraint.
pub fn on_predicate(constraint: Option<&rustqlite_parser::JoinConstraint>) -> Option<&Expr> {
    match constraint? {
        rustqlite_parser::JoinConstraint::On(e) => Some(e),
        rustqlite_parser::JoinConstraint::Using(_) => None,
    }
}

/// True when the FROM clause's top-level join is a LEFT (OUTER) join. The M7 first slice
/// only handles a single join level; a chain of joins is deferred.
pub fn is_left_join(from: &[TableOrJoin]) -> bool {
    if let Some(TableOrJoin::Join(j)) = from.first() {
        matches!(j.op, JoinOp::Left | JoinOp::LeftOuter)
    } else {
        false
    }
}

/// True when the FROM clause's top-level join is a RIGHT (OUTER) join. The M7 first slice
/// implements RIGHT JOIN by swapping the tables and emitting a LEFT JOIN.
pub fn is_right_join(from: &[TableOrJoin]) -> bool {
    if let Some(TableOrJoin::Join(j)) = from.first() {
        matches!(j.op, JoinOp::Right | JoinOp::RightOuter)
    } else {
        false
    }
}

/// For a RIGHT JOIN, return the table list with the tables swapped (so the original right
/// table is first, becoming the outer/left loop of the LEFT JOIN emulation). For other joins,
/// return the list as-is.
pub fn swap_for_right_join<'a>(
    tables: Vec<(&'a Table, &'a str)>,
    from: &[TableOrJoin],
) -> Vec<(&'a Table, &'a str)> {
    if is_right_join(from) {
        let mut swapped = tables;
        swapped.reverse();
        swapped
    } else {
        tables
    }
}

/// Reject unsupported join features that `flatten_cross_join` accepts but the codegen can't
/// handle yet (USING, NATURAL, etc.). Returns an error message for the first unsupported
/// feature.
pub fn validate_join(from: &[TableOrJoin]) -> Result<()> {
    for item in from {
        if let TableOrJoin::Join(j) = item {
            if matches!(j.constraint, Some(rustqlite_parser::JoinConstraint::Using(_))) {
                return Err(Error::msg("USING clause is not supported yet (M7.10)"));
            }
            if matches!(j.op, JoinOp::Natural) {
                return Err(Error::msg("NATURAL joins are not supported yet (M7.10)"));
            }
            validate_join(std::slice::from_ref(&*j.left))?;
        }
    }
    Ok(())
}

/// True when the FROM clause's top-level join is a FULL (OUTER) join. A FULL JOIN is
/// implemented as a LEFT JOIN followed by a right anti-join pass (emit NULL-filled left rows
/// for right rows that had no left match).
pub fn is_full_join(from: &[TableOrJoin]) -> bool {
    if let Some(TableOrJoin::Join(j)) = from.first() {
        matches!(j.op, JoinOp::Full | JoinOp::FullOuter)
    } else {
        false
    }
}