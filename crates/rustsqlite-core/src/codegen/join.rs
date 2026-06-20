//! Multi-table (join) codegen. The M7 first slice implements the cross join (cartesian
//! product) and the inner join with an `ON` predicate — both as a simple nested loop.
//! Left/right/full joins, natural joins, and `USING` arrive in later M7 tasks.
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

/// Compile a two-table cross / inner join. `tables` is the resolved pair (left, right) with
/// their cursor numbers (0, 1). The `ON` predicate (if any) is evaluated inside the inner
/// loop before the projection; the `WHERE` clause is evaluated after the `ON`.
#[allow(clippy::too_many_arguments)]
pub fn compile_cross_join(
    select: &SelectStmt,
    tables: &[(&Table, &str); 2],
    on_predicate: Option<&Expr>,
) -> Result<(Program, Vec<String>)> {
    let (limit, offset) = eval_limit_offset(select)?;
    let outputs = expand_columns_with_tables(select, tables)?;
    let names: Vec<String> = outputs.iter().map(|(_, n)| n.clone()).collect();
    let ncol = outputs.len() as i32;

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
    // Inner loop over the right table.
    let next_a = b.new_label();
    b.emit_jump(Opcode::Rewind, 1, next_a, 0);
    let loop_b = b.new_label();
    b.resolve(loop_b);
    let next_b = b.new_label();

    // ON predicate (inner join only): jump to next_b on false.
    if let Some(on) = on_predicate {
        compile_jump(&mut b, on, next_b, false, true, ctx)?;
    }
    // WHERE clause: jump to next_b on false.
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
    // Advance outer loop.
    b.resolve(next_a);
    b.emit_jump(Opcode::Next, 0, loop_a, 0);
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
                // Only handle CROSS and plain INNER joins with an ON constraint.
                match j.op {
                    JoinOp::Cross | JoinOp::Inner => {}
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

/// Reject unsupported join features that `flatten_cross_join` accepts but the codegen can't
/// handle yet (USING, etc.). Returns an error message for the first unsupported feature.
pub fn validate_join(from: &[TableOrJoin]) -> Result<()> {
    for item in from {
        if let TableOrJoin::Join(j) = item {
            if matches!(j.constraint, Some(rustqlite_parser::JoinConstraint::Using(_))) {
                return Err(Error::msg("USING clause is not supported yet (M7.10)"));
            }
            if matches!(j.op, JoinOp::Left | JoinOp::LeftOuter | JoinOp::Right | JoinOp::RightOuter | JoinOp::Full | JoinOp::FullOuter | JoinOp::Natural) {
                return Err(Error::msg("OUTER/NATURAL joins are not supported yet (M7.6-M7.10)"));
            }
            validate_join(std::slice::from_ref(&*j.left))?;
        }
    }
    Ok(())
}