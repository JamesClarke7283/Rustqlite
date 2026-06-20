//! Expression code generation (mirrors `sqlite3ExprCode` / `sqlite3ExprIfTrue` /
//! `sqlite3ExprIfFalse` in `expr.c`).
//!
//! Two entry points:
//! * [`compile_expr`] writes an expression's value into a target register.
//! * [`compile_jump`] compiles a boolean expression as a conditional jump, short-circuiting
//!   `AND`/`OR` and treating NULL as false (the form a `WHERE` clause needs).

use rustqlite_parser::{BinaryOp, Expr, FunctionArgs, Literal, UnaryOp};

use crate::error::{Error, Result};
use crate::func;
use crate::schema::{ColumnRef, Table};
use crate::types::Affinity;
use crate::vdbe::program::{aff_to_p5, P4, P5_JUMPIFNULL, P5_NULLEQ, P5_STOREP2};
use crate::vdbe::Opcode;

use super::builder::{Label, ProgramBuilder};

/// Code-generation context for expressions over a single table scan.
#[derive(Clone, Copy)]
pub struct Ctx<'a> {
    pub table: &'a Table,
    /// The cursor number the table is open on.
    pub cursor: i32,
    /// When set, column references read from this register base instead of the table cursor.
    /// Used for partial-index predicate evaluation during INSERT/UPDATE index maintenance,
    /// where the row values already sit in a contiguous register block.
    pub register_base: Option<i32>,
    /// When set, column references read from the index cursor at the mapped record position
    /// instead of the table cursor. Used for covering-index scans, where the projection /
    /// WHERE / ORDER BY are satisfied entirely by the index b-tree and no table cursor is
    /// opened. The map translates a table-column index to the position of that column's
    /// value in the index key record (`[indexed cols..., rowid]`); the rowid-alias column
    /// maps to `nkey_fields` (the trailing rowid).
    pub index_read: Option<IndexRead<'a>>,
    /// When set, column references resolve across multiple tables (a join). A bare `col`
    /// searches the join tables in order; a `table.col` resolves to the named table. When
    /// `None`, the single `table`/`cursor` fields are used. When set, `table`/`cursor` are
    /// still populated (with the first table) for backward compatibility with code that
    /// reads them directly.
    pub join_tables: Option<&'a [JoinTable<'a>]>,
}

/// One table in a join: the resolved `Table`, the cursor number it's open on, and the name
/// used to reference it (the table name or its alias). Used by [`Ctx::join_tables`] for
/// multi-table column resolution.
#[derive(Clone, Copy)]
pub struct JoinTable<'a> {
    pub table: &'a Table,
    pub cursor: i32,
    /// The name used to qualify columns: the alias if present, otherwise the table name.
    pub name: &'a str,
}

/// Covering-index read context: the cursor number of the open index and a map from
/// table-column index to position in the index key record. The map is dense over the
/// table's columns (every column the query might reference has an entry); unmapped columns
/// (which should not occur for a validated covering plan) read as NULL.
#[derive(Clone, Copy)]
pub struct IndexRead<'a> {
    /// The index cursor number to read from.
    pub cursor: i32,
    /// `table_column_index -> position in the index key record`. Length == table.columns.len();
    /// an entry of `usize::MAX` means "not in the index" (should not happen for a covering plan
    /// but defensive).
    pub column_positions: &'a [usize],
    /// The position of the trailing rowid in the index key record (`nkey_fields`).
    pub rowid_position: usize,
}

/// Emit code computing `e` into register `target`.
pub fn compile_expr(b: &mut ProgramBuilder, e: &Expr, target: i32, ctx: Ctx) -> Result<()> {
    match e {
        Expr::Literal(lit) => compile_literal(b, lit, target),
        Expr::Column { table, name, .. } => compile_column(b, table.as_deref(), name, target, ctx)?,
        Expr::Unary { op, expr } => compile_unary(b, *op, expr, target, ctx)?,
        Expr::Binary { op, left, right } => compile_binary(b, *op, left, right, target, ctx)?,
        Expr::Function {
            name,
            distinct,
            args,
            filter: _,
            over: _,
        } => {
            if *distinct {
                return Err(Error::msg(
                    "DISTINCT in function arguments is not supported in M3a",
                ));
            }
            let arg_exprs = match args {
                FunctionArgs::List(v) => v.as_slice(),
                FunctionArgs::Star => {
                    return Err(Error::msg(format!("{name}(*) is not supported in M3a")))
                }
            };
            func::check(name, arg_exprs.len())?;
            let start = b.alloc_regs(arg_exprs.len() as i32);
            for (k, a) in arg_exprs.iter().enumerate() {
                compile_expr(b, a, start + k as i32, ctx)?;
            }
            let idx = b.emit(Opcode::Function, 0, start, target);
            b.set_p4(idx, P4::Symbol(name.clone()));
            b.set_p5(idx, arg_exprs.len() as u8);
        }
        Expr::BindParam(_) => return Err(Error::msg("bind parameters are not supported in M3a")),
        Expr::Between { .. } => {
            return Err(Error::msg("BETWEEN is not supported by the executor yet"))
        }
        Expr::In { .. } => return Err(Error::msg("IN is not supported by the executor yet")),
        Expr::InSubquery { .. } => {
            return Err(Error::msg("IN subquery is not supported by the executor yet"))
        }
        Expr::Exists(_) => return Err(Error::msg("EXISTS is not supported by the executor yet")),
        Expr::Subquery(_) => {
            return Err(Error::msg(
                "subqueries are not supported by the executor yet",
            ))
        }
        Expr::Cast { .. } => return Err(Error::msg("CAST is not supported by the executor yet")),
        Expr::Case { .. } => return Err(Error::msg("CASE is not supported by the executor yet")),
        Expr::Collate { expr, collation } => {
            compile_expr(b, expr, target, ctx)?;
            // The COLLATE operator only matters to the comparison that consumes it. For an
            // index key we would need to thread the collation into the key-info; that is
            // handled by the caller (index codegen) which reads the IndexedColumn's collation.
            // Here we simply evaluate the underlying expression.
            let _ = collation;
        }
        Expr::IsDistinctFrom { .. } => {
            return Err(Error::msg(
                "IS DISTINCT FROM is not supported by the executor yet",
            ))
        }
        Expr::Row(_) => return Err(Error::msg(
            "row-value expressions are not supported by the executor yet",
        )),
        Expr::AggRef(reg) => {
            // A synthetic reference emitted by the aggregate codegen path: copy the
            // accumulator's result register into the target. The accumulator register was
            // filled by `AggFinal` during the per-group output pass.
            b.emit(Opcode::SCopy, *reg, target, 0);
        }
        Expr::Coalesce2 { left, right } => {
            // `IF left IS NOT NULL THEN left ELSE right`. Emit the left value, test it
            // for NULL, and on NULL overwrite the target with the right value.
            compile_expr(b, left, target, ctx)?;
            let not_null = b.new_label();
            b.emit_jump(Opcode::NotNull, target, not_null, 0);
            compile_expr(b, right, target, ctx)?;
            b.resolve(not_null);
        }
    }
    Ok(())
}

fn compile_literal(b: &mut ProgramBuilder, lit: &Literal, target: i32) {
    match lit {
        Literal::Null => {
            b.emit(Opcode::Null, 0, target, 0);
        }
        Literal::Integer(n) => match i32::try_from(*n) {
            Ok(n32) => {
                b.emit(Opcode::Integer, n32, target, 0);
            }
            Err(_) => {
                let i = b.emit(Opcode::Int64, 0, target, 0);
                b.set_p4(i, P4::Int(*n));
            }
        },
        Literal::Real(r) => {
            let i = b.emit(Opcode::Real, 0, target, 0);
            b.set_p4(i, P4::Real(*r));
        }
        Literal::Text(s) => {
            let i = b.emit(Opcode::String8, 0, target, 0);
            b.set_p4(i, P4::Text(s.clone()));
        }
        Literal::Blob(bytes) => {
            let i = b.emit(Opcode::Blob, 0, target, 0);
            b.set_p4(i, P4::Blob(bytes.clone()));
        }
        Literal::Bool(bl) => {
            b.emit(Opcode::Integer, i32::from(*bl), target, 0);
        }
    }
}

fn compile_column(
    b: &mut ProgramBuilder,
    qualifier: Option<&str>,
    name: &str,
    target: i32,
    ctx: Ctx,
) -> Result<()> {
    // Multi-table (join) column resolution. When `join_tables` is set, a `table.col` reference
    // resolves to the named table; a bare `col` searches the join tables in order and resolves
    // to the first table that has it. This delegates to the single-table path below with a
    // sub-`Ctx` pointing at the resolved table/cursor.
    if let Some(jt) = ctx.join_tables {
        let resolved: Option<(Ctx, &str)> = if let Some(q) = qualifier {
            jt.iter().find(|t| t.name.eq_ignore_ascii_case(q)).map(|t| {
                let sub = Ctx {
                    table: t.table,
                    cursor: t.cursor,
                    register_base: ctx.register_base,
                    index_read: None,
                    join_tables: None,
                };
                (sub, t.name)
            })
        } else {
            // Bare column: search tables in FROM order. An ambiguous column (present in
            // multiple tables) resolves to the first one — matching SQLite's behavior for
            // comma joins without USING. (SQLite actually raises an "ambiguous column name"
            // error, but that's a name-resolution check we defer to M2.74; for now we pick
            // the first table so the cross-join codegen works.)
            jt.iter().find(|t| t.table.resolve_column(name).is_some()).map(|t| {
                let sub = Ctx {
                    table: t.table,
                    cursor: t.cursor,
                    register_base: ctx.register_base,
                    index_read: None,
                    join_tables: None,
                };
                (sub, t.name)
            })
        };
        let Some((sub_ctx, _)) = resolved else {
            let disp = match qualifier {
                Some(q) => format!("{q}.{name}"),
                None => name.to_string(),
            };
            return Err(Error::msg(format!("no such column: {disp}")));
        };
        return compile_column(b, qualifier, name, target, sub_ctx);
    }

    // Column references against a VALUES-derived subquery use synthesized columnN names
    // rather than the underlying table. Treat an empty/no-column table as a VALUES scope:
    // only column1..columnN are resolvable.
    if ctx.table.columns.is_empty() {
        let col_name = qualifier.map_or_else(|| name.to_string(), |q| format!("{q}.{name}"));
        let idx: usize = if col_name.starts_with("column") {
            col_name["column".len()..].parse().unwrap_or(0)
        } else {
            0
        };
        if idx == 0 {
            return Err(Error::msg(format!("no such column: {col_name}")));
        }
        let reg = ctx.register_base.unwrap_or(0) + idx as i32 - 1;
        b.emit(Opcode::SCopy, reg, target, 0);
        return Ok(());
    }

    match ctx.table.resolve_column(name) {
        Some(ColumnRef::Rowid) => {
            if let Some(base) = ctx.register_base {
                // When the rowid is staged in a register block (RETURNING path), there is no
                // dedicated rowid register in the block. Instead, look up the rowid alias column
                // and copy its value; a non-alias table gets the rowid via Rowid.
                if let Some(alias_idx) = ctx.table.rowid_alias {
                    b.emit(Opcode::SCopy, base + alias_idx as i32, target, 0);
                } else {
                    b.emit(Opcode::Rowid, ctx.cursor, target, 0);
                }
            } else if let Some(ir) = ctx.index_read {
                // Covering-index scan: the rowid is the trailing value of the index key record.
                b.emit(Opcode::Column, ir.cursor, ir.rowid_position as i32, target);
            } else {
                b.emit(Opcode::Rowid, ctx.cursor, target, 0);
            }
        }
        Some(ColumnRef::Index(i)) => {
            if let Some(base) = ctx.register_base {
                b.emit(Opcode::SCopy, base + i as i32, target, 0);
            } else if let Some(ir) = ctx.index_read {
                // Covering-index scan: read the column from the index key record at the
                // mapped position. The position was computed by the planner from the index's
                // column list; the rowid-alias column maps to the trailing rowid.
                let pos = ir.column_positions.get(i).copied().unwrap_or(usize::MAX);
                if pos == usize::MAX {
                    // Not in the index — a covering plan should never reach here. Emit a NULL
                    // so the program is still well-formed if it does.
                    b.emit(Opcode::Null, 0, target, 0);
                } else {
                    b.emit(Opcode::Column, ir.cursor, pos as i32, target);
                    // REAL-affinity columns stored via the index keep their on-disk type; realify
                    // so integer-valued REAL columns read back as REAL (same as the table path).
                    if ctx.table.columns[i].affinity == Affinity::Real {
                        b.emit(Opcode::RealAffinity, target, 0, 0);
                    }
                }
            } else {
                // A WITHOUT ROWID table is stored as an index b-tree keyed by the PK record;
                // the on-disk column position is the storage index, not the table column index.
                let col_pos = if ctx.table.without_rowid {
                    ctx.table
                        .without_rowid_storage_index(i)
                        .expect("column exists on WITHOUT ROWID table") as i32
                } else {
                    i as i32
                };
                b.emit(Opcode::Column, ctx.cursor, col_pos, target);
                // A REAL-affinity column may store integer-valued rows as integers on disk; realify
                // so they read back as REAL (matches upstream's OP_RealAffinity after OP_Column).
                if ctx.table.columns[i].affinity == Affinity::Real {
                    b.emit(Opcode::RealAffinity, target, 0, 0);
                }
            }
        }
        None => {
            let disp = match qualifier {
                Some(q) => format!("{q}.{name}"),
                None => name.to_string(),
            };
            return Err(Error::msg(format!("no such column: {disp}")));
        }
    }
    Ok(())
}

fn compile_unary(
    b: &mut ProgramBuilder,
    op: UnaryOp,
    expr: &Expr,
    target: i32,
    ctx: Ctx,
) -> Result<()> {
    match op {
        UnaryOp::Negate => {
            if let Some(lit) = const_negate(expr) {
                compile_literal(b, &lit, target);
            } else {
                let tmp = b.alloc_reg();
                compile_expr(b, expr, tmp, ctx)?;
                let zero = b.alloc_reg();
                b.emit(Opcode::Integer, 0, zero, 0);
                // r[target] = r[zero] - r[tmp] = 0 - tmp
                b.emit(Opcode::Subtract, tmp, zero, target);
            }
        }
        UnaryOp::Positive => compile_expr(b, expr, target, ctx)?,
        UnaryOp::Not => {
            let tmp = b.alloc_reg();
            compile_expr(b, expr, tmp, ctx)?;
            b.emit(Opcode::Not, tmp, target, 0);
        }
        UnaryOp::BitNot => {
            let tmp = b.alloc_reg();
            compile_expr(b, expr, tmp, ctx)?;
            b.emit(Opcode::BitNot, tmp, target, 0);
        }
    }
    Ok(())
}

/// Fold `-<literal-number>` so `-5` is a single load (and so the negation matches SQLite).
fn const_negate(expr: &Expr) -> Option<Literal> {
    match expr {
        Expr::Literal(Literal::Integer(n)) => n.checked_neg().map(Literal::Integer),
        Expr::Literal(Literal::Real(r)) => Some(Literal::Real(-r)),
        _ => None,
    }
}

fn compile_binary(
    b: &mut ProgramBuilder,
    op: BinaryOp,
    left: &Expr,
    right: &Expr,
    target: i32,
    ctx: Ctx,
) -> Result<()> {
    // Arithmetic / concatenation.
    if let Some(opcode) = arith_opcode(op) {
        let rl = b.alloc_reg();
        compile_expr(b, left, rl, ctx)?;
        let rr = b.alloc_reg();
        compile_expr(b, right, rr, ctx)?;
        // r[target] = r[p2] OP r[p1] = r[rl] OP r[rr]  (p2 = left, p1 = right)
        b.emit(opcode, rr, rl, target);
        return Ok(());
    }

    // Comparisons (value form: store the boolean result in `target`).
    if let Some((opcode, nulleq)) = cmp_opcode(op) {
        let rl = b.alloc_reg();
        compile_expr(b, left, rl, ctx)?;
        let rr = b.alloc_reg();
        compile_expr(b, right, rr, ctx)?;
        let aff = comparison_affinity(left, right, ctx);
        let mut p5 = aff_to_p5(aff) | P5_STOREP2;
        if nulleq {
            p5 |= P5_NULLEQ;
        }
        // test r[p3] OP r[p1] = r[rl] OP r[rr]; store into p2 = target.
        let idx = b.emit(opcode, rr, target, rl);
        b.set_p5(idx, p5);
        return Ok(());
    }

    // Three-valued logic (value form).
    match op {
        BinaryOp::And | BinaryOp::Or => {
            let rl = b.alloc_reg();
            compile_expr(b, left, rl, ctx)?;
            let rr = b.alloc_reg();
            compile_expr(b, right, rr, ctx)?;
            let opcode = if op == BinaryOp::And {
                Opcode::And
            } else {
                Opcode::Or
            };
            // r[target] = r[p1] OP r[p2] = r[rl] OP r[rr]
            b.emit(opcode, rl, rr, target);
            Ok(())
        }
        BinaryOp::Like | BinaryOp::Glob => {
            // Lower `X LIKE Y` to `like(Y, X)` and `X GLOB Y` to `glob(Y, X)` — upstream passes
            // the pattern first. Mirror the `Expr::Function` lowering above: a contiguous arg
            // block, then one `Opcode::Function` with `p4 = Symbol(name)`, `p5 = nArg`.
            let name = if op == BinaryOp::Like { "like" } else { "glob" };
            let start = b.alloc_regs(2);
            compile_expr(b, right, start, ctx)?; // pattern (Y) → first arg
            compile_expr(b, left, start + 1, ctx)?; // value (X) → second arg
            let idx = b.emit(Opcode::Function, 0, start, target);
            b.set_p4(idx, P4::Symbol(name.to_string()));
            b.set_p5(idx, 2);
            Ok(())
        }
        BinaryOp::JsonExtract | BinaryOp::JsonExtractText => Err(Error::msg(
            "JSON -> / ->> operators are not supported by the executor yet",
        )),
        BinaryOp::Regexp | BinaryOp::Match => Err(Error::msg(
            "REGEXP / MATCH operators are not supported by the executor yet",
        )),
        _ => unreachable!("binary op already handled"),
    }
}

/// Emit code that jumps to `label` when `e` is true (`jump_if_true`) or false (else). `jump_if_null`
/// controls whether a NULL result also takes the jump — it is threaded through `AND`/`OR`/`NOT`
/// exactly as in `sqlite3ExprIfTrue`/`sqlite3ExprIfFalse` (note the XOR flip into the
/// short-circuit operand). A `WHERE` clause is compiled as `jump_if_true = false`,
/// `jump_if_null = true` (a NULL predicate skips the row).
pub fn compile_jump(
    b: &mut ProgramBuilder,
    e: &Expr,
    label: Label,
    jump_if_true: bool,
    jump_if_null: bool,
    ctx: Ctx,
) -> Result<()> {
    match e {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            if jump_if_true {
                // ExprIfTrue(AND): IfFalse(L, d2, !jn); IfTrue(R, dest, jn); d2:
                let d2 = b.new_label();
                compile_jump(b, left, d2, false, !jump_if_null, ctx)?;
                compile_jump(b, right, label, true, jump_if_null, ctx)?;
                b.resolve(d2);
            } else {
                // ExprIfFalse(AND): IfFalse(L, dest, jn); IfFalse(R, dest, jn)
                compile_jump(b, left, label, false, jump_if_null, ctx)?;
                compile_jump(b, right, label, false, jump_if_null, ctx)?;
            }
            Ok(())
        }
        Expr::Binary {
            op: BinaryOp::Or,
            left,
            right,
        } => {
            if jump_if_true {
                // ExprIfTrue(OR): IfTrue(L, dest, jn); IfTrue(R, dest, jn)
                compile_jump(b, left, label, true, jump_if_null, ctx)?;
                compile_jump(b, right, label, true, jump_if_null, ctx)?;
            } else {
                // ExprIfFalse(OR): IfTrue(L, d2, !jn); IfFalse(R, dest, jn); d2:
                let d2 = b.new_label();
                compile_jump(b, left, d2, true, !jump_if_null, ctx)?;
                compile_jump(b, right, label, false, jump_if_null, ctx)?;
                b.resolve(d2);
            }
            Ok(())
        }
        Expr::Unary {
            op: UnaryOp::Not,
            expr,
        } => compile_jump(b, expr, label, !jump_if_true, jump_if_null, ctx),
        Expr::Binary { op, left, right } if cmp_opcode(*op).is_some() => {
            let (opcode, nulleq) = cmp_opcode(*op).unwrap();
            let rl = b.alloc_reg();
            compile_expr(b, left, rl, ctx)?;
            let rr = b.alloc_reg();
            compile_expr(b, right, rr, ctx)?;
            let aff = comparison_affinity(left, right, ctx);
            // For a jump-when-FALSE, emit the negated comparison (which jumps when TRUE).
            let emit_op = if jump_if_true {
                opcode
            } else {
                negate_cmp(opcode)
            };
            let mut p5 = aff_to_p5(aff);
            if nulleq {
                p5 |= P5_NULLEQ; // IS / IS NOT: NULL is comparable, never "unknown"
            } else if jump_if_null {
                p5 |= P5_JUMPIFNULL;
            }
            // test r[p3] OP r[p1] = r[rl] OP r[rr]; jump target is p2.
            let idx = b.emit_jump(emit_op, rr, label, rl);
            b.set_p5(idx, p5);
            Ok(())
        }
        other => {
            let r = b.alloc_reg();
            compile_expr(b, other, r, ctx)?;
            let on_null = i32::from(jump_if_null);
            if jump_if_true {
                b.emit_jump(Opcode::If, r, label, on_null);
            } else {
                b.emit_jump(Opcode::IfNot, r, label, on_null);
            }
            Ok(())
        }
    }
}

fn arith_opcode(op: BinaryOp) -> Option<Opcode> {
    Some(match op {
        BinaryOp::Add => Opcode::Add,
        BinaryOp::Sub => Opcode::Subtract,
        BinaryOp::Mul => Opcode::Multiply,
        BinaryOp::Div => Opcode::Divide,
        BinaryOp::Mod => Opcode::Remainder,
        BinaryOp::Concat => Opcode::Concat,
        BinaryOp::BitAnd => Opcode::BitAnd,
        BinaryOp::BitOr => Opcode::BitOr,
        BinaryOp::ShiftLeft => Opcode::ShiftLeft,
        BinaryOp::ShiftRight => Opcode::ShiftRight,
        _ => return None,
    })
}

/// The comparison opcode and whether NULL-equality (`IS`) semantics apply.
fn cmp_opcode(op: BinaryOp) -> Option<(Opcode, bool)> {
    Some(match op {
        BinaryOp::Eq => (Opcode::Eq, false),
        BinaryOp::Ne => (Opcode::Ne, false),
        BinaryOp::Lt => (Opcode::Lt, false),
        BinaryOp::Le => (Opcode::Le, false),
        BinaryOp::Gt => (Opcode::Gt, false),
        BinaryOp::Ge => (Opcode::Ge, false),
        BinaryOp::Is => (Opcode::Eq, true),
        BinaryOp::IsNot => (Opcode::Ne, true),
        _ => return None,
    })
}

fn negate_cmp(op: Opcode) -> Opcode {
    match op {
        Opcode::Eq => Opcode::Ne,
        Opcode::Ne => Opcode::Eq,
        Opcode::Lt => Opcode::Ge,
        Opcode::Ge => Opcode::Lt,
        Opcode::Le => Opcode::Gt,
        Opcode::Gt => Opcode::Le,
        other => other,
    }
}

/// The affinity SQLite applies to both sides of a comparison (`sqlite3CompareAffinity`):
/// NUMERIC if either side is a numeric-affinity column, else the lone column's affinity, else
/// none (literal-vs-literal applies no affinity).
fn comparison_affinity(left: &Expr, right: &Expr, ctx: Ctx) -> Option<Affinity> {
    let l = expr_affinity(left, ctx);
    let r = expr_affinity(right, ctx);
    match (l, r) {
        (Some(a), Some(b)) => {
            if is_numeric(a) || is_numeric(b) {
                Some(Affinity::Numeric)
            } else {
                None // two non-numeric columns → no coercion
            }
        }
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// The affinity of an expression for comparison purposes: a column's declared affinity, or
/// `None` for anything that is not a column (the rowid alias has INTEGER affinity).
fn expr_affinity(e: &Expr, ctx: Ctx) -> Option<Affinity> {
    match e {
        Expr::Column { name, .. } => match ctx.table.resolve_column(name) {
            Some(ColumnRef::Index(i)) => Some(ctx.table.columns[i].affinity),
            Some(ColumnRef::Rowid) => Some(Affinity::Integer),
            None => None,
        },
        _ => None,
    }
}

fn is_numeric(a: Affinity) -> bool {
    matches!(a, Affinity::Integer | Affinity::Real | Affinity::Numeric)
}
