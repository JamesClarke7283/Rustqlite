//! Expression code generation (mirrors `sqlite3ExprCode` / `sqlite3ExprIfTrue` /
//! `sqlite3ExprIfFalse` in `expr.c`).
//!
//! Two entry points:
//! * [`compile_expr`] writes an expression's value into a target register.
//! * [`compile_jump`] compiles a boolean expression as a conditional jump, short-circuiting
//!   `AND`/`OR` and treating NULL as false (the form a `WHERE` clause needs).

use rustqlite_parser::{BinaryOp, Expr, FunctionArgs, Literal, SelectStmt, UnaryOp};

use crate::error::{Error, Result};
use crate::func;
use crate::schema::{ColumnRef, IndexObject, Table};
use crate::types::{Affinity, Collation};
use crate::vdbe::program::{aff_to_p5, P4, P5_JUMPIFNULL, P5_NULLEQ, P5_STOREP2};
use crate::vdbe::Opcode;

use super::builder::{Label, ProgramBuilder};

/// Resolves the source table (and its indexes) for a subquery's `FROM` clause, so that a
/// scalar subquery / `EXISTS` / `IN (SELECT ...)` expression encountered inside
/// [`compile_expr`] can compile its body against the catalog. The C-API implementation reads
/// the catalog via the pager; the codegen itself stays pager-free.
///
/// Returns `(None, [])` for a constant / `VALUES` subquery (no `FROM`). The `SelectStmt` is
/// the subquery body; the resolver returns the resolved `Table` (owned, so the subquery
/// codegen can hold a stable reference to it for the lifetime of the compile) and the indexes
/// attached to that table.
pub trait SubqueryResolver {
    fn resolve(&self, subquery: &SelectStmt) -> Result<(Option<Table>, Vec<IndexObject>)>;
}

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
    /// When set, scalar subquery / `EXISTS` / `IN (SELECT ...)` expressions encountered inside
    /// [`compile_expr`] compile their body against the catalog via this resolver. When `None`,
    /// those expression kinds raise an "unsupported" error (matching the pre-M8.7 behavior).
    pub subquery_resolver: Option<&'a dyn SubqueryResolver>,
    /// The enclosing query's scope, for correlated subquery column resolution. When a column
    /// reference does not resolve against the local `table`/`join_tables`, [`compile_column`]
    /// walks this scope chain and, on a match, emits a `Column`/`Rowid` against the outer
    /// table's **sentinel** cursor number (assigned by the subquery codegen, see
    /// [`OuterTable::cursor`]). The subquery inliner's [`rebase_operands`] rewrites those
    /// sentinel cursors back to the real outer-program cursor numbers. `None` at the outermost
    /// query (no enclosing scope).
    pub outer: Option<&'a OuterScope<'a>>,
}

/// The enclosing query's table scope for correlated subquery resolution — the Rust analogue
/// of upstream's `NameContext.pNext` chain. Carries the outer query's FROM tables (each with
/// a sentinel cursor number) and a link to the next enclosing scope.
pub struct OuterScope<'a> {
    pub tables: &'a [OuterTable<'a>],
    pub parent: Option<&'a OuterScope<'a>>,
}

/// One table in an enclosing query's scope. `cursor` is a **sentinel** cursor number unique
/// within the subquery's compile (assigned by the subquery codegen from
/// [`OUTER_CURSOR_BASE`]); the inliner's [`rebase_operands`] rewrites it to the real
/// outer-program cursor number. `name` is the alias if present, else the table name — the form
/// a `alias.col` or `table.col` reference matches against.
#[derive(Clone, Copy)]
pub struct OuterTable<'a> {
    pub table: &'a Table,
    pub cursor: i32,
    pub name: &'a str,
}

/// Sentinel cursor numbers for outer-scope tables start here, well clear of any real cursor
/// number a subquery body opens (0, 1, 2 — offset to `cursor_offset..cursor_offset+3` during
/// inlining). [`rebase_operands`] treats any cursor operand `>= OUTER_CURSOR_BASE` as a
/// sentinel and rewrites it via the outer-cursor map.
pub const OUTER_CURSOR_BASE: i32 = 10_000;

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
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            // `expr BETWEEN low AND high` lowers to `expr >= low AND expr <= high` (and NOT
            // BETWEEN to its negation). The value form stores 1/0/NULL — NULL when the LHS or
            // any comparison is UNKNOWN (3-valued logic). Mirrors upstream's
            // `sqlite3ExprCodeBetween` (the `SQLITE_JUMPIFNULL` path).
            let r = b.alloc_reg();
            compile_expr(b, expr, r, ctx)?;
            let lo = b.alloc_reg();
            compile_expr(b, low, lo, ctx)?;
            let hi = b.alloc_reg();
            compile_expr(b, high, hi, ctx)?;
            let aff = comparison_affinity(expr, low, ctx)
                .or_else(|| comparison_affinity(expr, high, ctx));
            let p5 = aff_to_p5(aff) | P5_JUMPIFNULL;
            let dest_true = b.new_label();
            let dest_false = b.new_label();
            let dest_null = b.new_label();
            let dest_end = b.new_label();
            if *negated {
                // NOT BETWEEN: TRUE when (r < lo) OR (r > hi); FALSE when (r >= lo) AND (r <= hi);
                // NULL when r IS NULL or any comparison is UNKNOWN.
                b.emit_jump(Opcode::IsNull, r, dest_null, 0);
                let lt = b.emit_jump(Opcode::Lt, lo, dest_true, r);
                b.set_p5(lt, p5 & !P5_JUMPIFNULL);
                let gt = b.emit_jump(Opcode::Gt, hi, dest_true, r);
                b.set_p5(gt, p5 & !P5_JUMPIFNULL);
                // Fall-through: in range → FALSE.
                b.emit(Opcode::Integer, 0, target, 0);
                b.emit_jump(Opcode::Goto, 0, dest_end, 0);
                b.resolve(dest_true);
                b.emit(Opcode::Integer, 1, target, 0);
                b.emit_jump(Opcode::Goto, 0, dest_end, 0);
                b.resolve(dest_null);
                b.emit(Opcode::Null, target, 0, 0);
            } else {
                // BETWEEN: TRUE when (r >= lo) AND (r <= hi); FALSE otherwise; NULL when r IS NULL.
                b.emit_jump(Opcode::IsNull, r, dest_null, 0);
                let lt = b.emit_jump(Opcode::Lt, lo, dest_false, r);
                b.set_p5(lt, p5 & !P5_JUMPIFNULL);
                let gt = b.emit_jump(Opcode::Gt, hi, dest_false, r);
                b.set_p5(gt, p5 & !P5_JUMPIFNULL);
                // Fall-through: in range → TRUE.
                b.emit(Opcode::Integer, 1, target, 0);
                b.emit_jump(Opcode::Goto, 0, dest_end, 0);
                b.resolve(dest_false);
                b.emit(Opcode::Integer, 0, target, 0);
                b.emit_jump(Opcode::Goto, 0, dest_end, 0);
                b.resolve(dest_null);
                b.emit(Opcode::Null, target, 0, 0);
            }
            b.resolve(dest_end);
        }
        Expr::In { .. } => return Err(Error::msg("IN is not supported by the executor yet")),
        Expr::InSubquery { expr, subquery, negated } => {
            // `X [NOT] IN (SELECT …)`: evaluate the membership test and store the boolean
            // result (1/0/NULL) into `target`. Mirrors `sqlite3ExprCodeIN` in `expr.c` for the
            // `ExprUseXSelect` case — the subquery is materialized into an ephemeral index
            // (wrapped in `OP_Once` so a non-correlated subquery runs only once), then the LHS
            // is probed against it. See [`super::subquery::compile_in_subquery`] for the jump
            // form; here we wrap it with FALSE/NULL labels and store the 3-valued result.
            let Some(resolver) = ctx.subquery_resolver else {
                return Err(Error::msg(
                    "subqueries are not supported by the executor yet",
                ));
            };
            let (sub_table, sub_indexes) = resolver.resolve(subquery)?;
            // Allocate three labels: false (LHS not in set), null (indeterminate), and truth
            // (fall-through = member). The value form stores 1/NULL/0 into `target` based on
            // which label is taken.
            let dest_false = b.new_label();
            let dest_null = b.new_label();
            let dest_true = b.new_label();
            super::subquery::compile_in_subquery(
                b,
                expr,
                subquery,
                *negated,
                sub_table.as_ref(),
                &sub_indexes,
                dest_false,
                dest_null,
                ctx,
            )?;
            // Fall-through = IN-is-TRUE. For `NOT IN`, TRUE becomes FALSE (store 0); for `IN`,
            // TRUE becomes 1. The FALSE/NULL cases are stored the same way (NOT IN of FALSE is
            // TRUE only when there are no NULLs — but the FALSE destination here means "LHS not
            // in set AND no NULL ambiguity", so NOT IN of that is TRUE; however we route FALSE
            // to store 0 to keep the value-form simple and let `compile_jump` handle the
            // negation's jump routing). To keep the value form correct for both, we store:
            //   * fall-through (IN TRUE):  `negated ? 0 : 1`
            //   * dest_false (IN FALSE):  `negated ? 1 : 0`
            //   * dest_null  (IN NULL):   NULL  (NOT IN of NULL is NULL)
            b.emit(Opcode::Integer, if *negated { 0 } else { 1 }, target, 0);
            b.emit_jump(Opcode::Goto, 0, dest_true, 0);
            b.resolve(dest_false);
            b.emit(Opcode::Integer, if *negated { 1 } else { 0 }, target, 0);
            b.emit_jump(Opcode::Goto, 0, dest_true, 0);
            // NULL: store NULL (both IN and NOT IN yield NULL when the result is indeterminate).
            b.resolve(dest_null);
            b.emit(Opcode::Null, 0, target, 0);
            b.resolve(dest_true);
        }
        Expr::Exists(s) => {
            // `EXISTS (SELECT …)`: evaluates to 1 if the subquery returns at least one row,
            // 0 otherwise. Mirrors `sqlite3CodeSubselect` for the `TK_EXISTS` case: the
            // subquery body is inlined as a subroutine wrapped in `OP_Once` (the M8.8 first
            // slice assumes the subquery is non-correlated — `Once` caches the result across
            // encounters). See [`super::subquery::compile_scalar_subquery`] for the same
            // shape with `SRT_Mem` instead of `SRT_Exists`.
            let Some(resolver) = ctx.subquery_resolver else {
                return Err(Error::msg(
                    "subqueries are not supported by the executor yet",
                ));
            };
            let (sub_table, sub_indexes) = resolver.resolve(s)?;
            let result_reg = super::subquery::compile_exists_subquery(
                b,
                s,
                sub_table.as_ref(),
                &sub_indexes,
                Some(ctx),
            )?;
            b.emit(Opcode::SCopy, result_reg, target, 0);
        }
        Expr::Subquery(s) => {
            // Scalar subquery `(SELECT …)`: evaluate to the first column of the first row, or
            // NULL if the subquery returns no rows. The subquery body is compiled via
            // [`super::subquery::compile_scalar_subquery`], which inlines it as a subroutine
            // (`Gosub`/`Return`) wrapped in `OP_Once` (the M8.7 first slice assumes the
            // subquery is non-correlated — `Once` caches the result across encounters; outer
            // column references inside the subquery will fail column resolution with "no such
            // column", which is the right error for unsupported correlation until M8.11/M8.13).
            let Some(resolver) = ctx.subquery_resolver else {
                return Err(Error::msg(
                    "subqueries are not supported by the executor yet",
                ));
            };
            let (sub_table, sub_indexes) = resolver.resolve(s)?;
            // Stash the resolved table/indexes in a place that outlives the subroutine call.
            // The resolver returns owned values; we keep them on the stack here.
            let result_reg = super::subquery::compile_scalar_subquery(
                b,
                s,
                sub_table.as_ref(),
                &sub_indexes,
                Some(ctx),
            )?;
            // Move the scalar result into the caller's target register.
            b.emit(Opcode::SCopy, result_reg, target, 0);
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
                    subquery_resolver: ctx.subquery_resolver,
                    outer: ctx.outer,
                };
                (sub, t.name)
            })
        } else {
            // Bare column: search tables in FROM order. An ambiguous column (present
            // in multiple tables) resolves to the first one — the ambiguous-column
            // check is enforced up front by the `codegen::resolve` pass (M2.74),
            // which raises "ambiguous column name" before codegen runs, so by the
            // time we reach here the first match is the uniquely-correct one.
            jt.iter().find(|t| t.table.resolve_column(name).is_some()).map(|t| {
                let sub = Ctx {
                    table: t.table,
                    cursor: t.cursor,
                    register_base: ctx.register_base,
                    index_read: None,
                    join_tables: None,
                    subquery_resolver: ctx.subquery_resolver,
                    outer: ctx.outer,
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
            // Not a `columnN` reference — try the enclosing scope (correlated subquery
            // inside a VALUES-derived outer scope). If no outer match, raise "no such column".
            if let Some(outer) = ctx.outer {
                if let Some((ot, col_ref)) = lookup_outer(outer, qualifier, name) {
                    return emit_outer_column(b, ot, col_ref, target);
                }
            }
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
            // No match against the local table. Try the enclosing scope (correlated
            // subquery). When the outer scope matches, emit a `Column`/`Rowid` against
            // the outer table's sentinel cursor; the subquery inliner's `rebase_operands`
            // rewrites the sentinel back to the real outer-program cursor number.
            if let Some(outer) = ctx.outer {
                if let Some((ot, col_ref)) = lookup_outer(outer, qualifier, name) {
                    return emit_outer_column(b, ot, col_ref, target);
                }
            }
            let disp = match qualifier {
                Some(q) => format!("{q}.{name}"),
                None => name.to_string(),
            };
            return Err(Error::msg(format!("no such column: {disp}")));
        }
    }
    Ok(())
}

/// Walk the [`OuterScope`] chain looking for a column match, mirroring the
/// `do { … } while (pNC = pNC->pNext)` loop in `lookupName` (`resolve.c:341`). On a match
/// returns the outer table and the `ColumnRef` within it. A qualified ref pins the scope:
/// if the qualifier matches a table here but not the column, returns `None` (no outward
/// fallthrough). A bare ref with no local match falls through to the parent scope.
pub(crate) fn lookup_outer<'a>(
    scope: &'a OuterScope<'a>,
    qualifier: Option<&str>,
    name: &str,
) -> Option<(&'a OuterTable<'a>, ColumnRef)> {
    let mut local_cnt = 0;
    let mut matched: Option<(&OuterTable, ColumnRef)> = None;
    let mut qualifier_matched_a_table = false;
    for t in scope.tables {
        if let Some(q) = qualifier {
            if !t.name.eq_ignore_ascii_case(q) {
                continue;
            }
            qualifier_matched_a_table = true;
            if let Some(cr) = t.table.resolve_column(name) {
                local_cnt += 1;
                matched = Some((t, cr));
            }
        } else if let Some(cr) = t.table.resolve_column(name) {
            local_cnt += 1;
            matched = Some((t, cr));
        }
    }
    if local_cnt > 1 {
        // Ambiguous in this scope — upstream raises "ambiguous column name"; the resolve
        // pass (M2.74) catches it first. Return the first match defensively.
        return matched;
    }
    if local_cnt == 1 {
        return matched;
    }
    // No local match. A qualified ref whose qualifier matched a table here but had no
    // column: pin the scope (no outward fallthrough).
    if qualifier.is_some() && qualifier_matched_a_table {
        return None;
    }
    // A qualified ref whose qualifier didn't match any table here: upstream returns "no
    // such column" without falling outward. Match that.
    if qualifier.is_some() && !qualifier_matched_a_table {
        return None;
    }
    // Bare ref, no local match: fall through to the parent scope.
    scope.parent.and_then(|p| lookup_outer(p, qualifier, name))
}

/// Emit a `Column`/`Rowid` against an outer table's sentinel cursor. The sentinel is
/// rewritten to the real outer-program cursor by `rebase_operands` during subquery inlining.
fn emit_outer_column(
    b: &mut ProgramBuilder,
    ot: &OuterTable,
    col_ref: ColumnRef,
    target: i32,
) -> Result<()> {
    match col_ref {
        ColumnRef::Rowid => {
            b.emit(Opcode::Rowid, ot.cursor, target, 0);
        }
        ColumnRef::Index(i) => {
            let col_pos = if ot.table.without_rowid {
                ot.table
                    .without_rowid_storage_index(i)
                    .expect("column exists on WITHOUT ROWID table") as i32
            } else {
                i as i32
            };
            b.emit(Opcode::Column, ot.cursor, col_pos, target);
            if ot.table.columns[i].affinity == Affinity::Real {
                b.emit(Opcode::RealAffinity, target, 0, 0);
            }
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
        // M26.6: thread the comparison's collation into the opcode's P4 (see the jump
        // form above for the precedence rule).
        if let Some(coll) = comparison_collation(left, right, ctx) {
            let name = match coll {
                Collation::Binary => "BINARY",
                Collation::NoCase => "NOCASE",
                Collation::RTrim => "RTRIM",
            };
            b.set_p4(idx, P4::Symbol(name.to_string()));
        }
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
        BinaryOp::JsonExtract | BinaryOp::JsonExtractText => {
            // `X -> P` / `X ->> P` — JSON extraction. `->` returns the JSON representation
            // (always JSON text, even for scalars); `->>` returns the SQL representation
            // (NULL/INTEGER/REAL/TEXT, like a single-path `json_extract`). The right operand
            // is a path (`'$.x'`), a bare object label (`'x'` → `'$.x'`), or an integer
            // array index (`3` → `'$[3]'`; `-K` → `'$[#-K]'`). Mirrors `jsonExtractFunc`
            // / the `JSON` operator handling in `expr.c`.
            compile_json_arrow(b, op, left, right, target, ctx)
        }
        BinaryOp::Regexp | BinaryOp::Match => Err(Error::msg(
            "REGEXP / MATCH operators are not supported by the executor yet",
        )),
        _ => unreachable!("binary op already handled"),
    }
}

/// Compile `X -> P` (`JsonExtract`) or `X ->> P` (`JsonExtractText`). The right operand `P`
/// may be a TEXT path/label or an INTEGER array index. Per the JSON1 docs (§4.10):
/// * a TEXT right operand that starts with `$` is a full path;
/// * a TEXT right operand `X` that doesn't start with `$` is treated as `'$.X'` (a bare
///   object label);
/// * a non-negative INTEGER right operand `N` is treated as `'$[N]'`;
/// * a negative INTEGER right operand `-K` is treated as `'$[#-K]'` (the K-th from the end).
///
/// `->` always returns a JSON representation (a scalar is JSON-encoded — `5` → `"5"`,
/// `"x"` → `"\"x\""`; an array/object is its canonical JSON text). `->>` returns the SQL
/// representation (NULL/INTEGER/REAL/TEXT, like a single-path `json_extract`).
///
/// The implementation lowers to a `Function` opcode call: `->` calls an internal
/// `_json_arrow` function that renders JSON; `->>` calls `json_extract`. Both take the
/// resolved path string as the second argument.
fn compile_json_arrow(
    b: &mut ProgramBuilder,
    op: BinaryOp,
    left: &Expr,
    right: &Expr,
    target: i32,
    ctx: Ctx,
) -> Result<()> {
    // Resolve the path argument: a literal TEXT/INTEGER is folded into a path string at
    // compile time (so `col -> 'a'` becomes `json_extract(col, '$.a')`). A non-literal
    // right operand is evaluated at runtime and the path normalization is deferred to the
    // function body (we emit a call to the internal `_json_arrow` / `_json_arrow_text`
    // helpers which mirror the compile-time normalization).
    let json_op = op == BinaryOp::JsonExtract;
    // Try to fold a literal right operand into a path string.
    if let Some(path_str) = json_arrow_path_literal(right) {
        // Folded path: lower to a direct `json_extract` call (or the JSON-rendering variant
        // for `->`). The 2-arg `json_extract` returns the SQL representation; for `->` we
        // need the JSON representation, which is `json_quote` of the extract for scalars
        // and the raw extract text for arrays/objects. The cleanest lowering is to call
        // the internal `_json_arrow` / `_json_arrow_text` helpers, which take (X, P) and
        // do the right thing.
        let fn_name = if json_op { "_json_arrow" } else { "_json_arrow_text" };
        let start = b.alloc_regs(2);
        compile_expr(b, left, start, ctx)?; // X → first arg
        // The path string is a literal — emit it as a String8 into the second arg slot.
        let path_idx = b.emit(Opcode::String8, 0, start + 1, 0);
        b.set_p4(path_idx, P4::Text(path_str));
        let idx = b.emit(Opcode::Function, 0, start, target);
        b.set_p4(idx, P4::Symbol(fn_name.to_string()));
        b.set_p5(idx, 2);
        return Ok(());
    }
    // Non-literal right operand: evaluate it at runtime and let the function do the
    // normalization.
    let fn_name = if json_op { "_json_arrow" } else { "_json_arrow_text" };
    let start = b.alloc_regs(2);
    compile_expr(b, left, start, ctx)?;
    compile_expr(b, right, start + 1, ctx)?;
    let idx = b.emit(Opcode::Function, 0, start, target);
    b.set_p4(idx, P4::Symbol(fn_name.to_string()));
    b.set_p5(idx, 2);
    Ok(())
}

/// If `right` is a literal TEXT or INTEGER, fold it into the corresponding JSON path string
/// per the `->`/`->>` operator's right-operand rules:
/// * TEXT starting with `$` → used verbatim;
/// * TEXT not starting with `$` → `'$.<text>'` (a bare object label);
/// * non-negative INTEGER N → `'$[N]'`;
/// * negative INTEGER -K → `'$[#-K]'`.
/// Returns `None` for non-literal right operands (the path is resolved at runtime).
fn json_arrow_path_literal(right: &Expr) -> Option<String> {
    match right {
        Expr::Literal(Literal::Text(s)) => {
            if s.starts_with('$') {
                Some(s.clone())
            } else {
                Some(format!("$.{s}"))
            }
        }
        Expr::Literal(Literal::Integer(n)) => {
            if *n >= 0 {
                Some(format!("$[{n}]"))
            } else {
                // Negative index: `$[#-K]` where K = -n.
                let k = n.checked_neg()?;
                Some(format!("$[#-{k}]"))
            }
        }
        Expr::Unary {
            op: UnaryOp::Negate,
            expr,
        } => {
            // `-K` for a literal K — fold to `$[#-K]`.
            if let Expr::Literal(Literal::Integer(k)) = expr.as_ref() {
                Some(format!("$[#-{k}]"))
            } else {
                None
            }
        }
        _ => None,
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
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            // `expr BETWEEN low AND high` is `expr >= low AND expr <= high`. The jump form
            // mirrors upstream's `ExprIfTrue`/`ExprIfFalse` for a BETWEEN term: evaluate the
            // LHS once, then two comparisons joined by AND (for BETWEEN) or OR (for NOT
            // BETWEEN). `NOT BETWEEN` is `(expr < low OR expr > high)`.
            let r = b.alloc_reg();
            compile_expr(b, expr, r, ctx)?;
            let lo = b.alloc_reg();
            compile_expr(b, low, lo, ctx)?;
            let hi = b.alloc_reg();
            compile_expr(b, high, hi, ctx)?;
            let aff = comparison_affinity(expr, low, ctx)
                .or_else(|| comparison_affinity(expr, high, ctx));
            let p5 = aff_to_p5(aff) | if jump_if_null { P5_JUMPIFNULL } else { 0 };
            if *negated {
                // NOT BETWEEN = `(r < lo) OR (r > hi)`. TRUE when out of range, FALSE when in
                // range, NULL when r is NULL. The OR pattern threads `!jn` into the
                // short-circuit operand (mirroring `ExprIfTrue`/`ExprIfFalse` for OR).
                let p5_short = aff_to_p5(aff) | if !jump_if_null { P5_JUMPIFNULL } else { 0 };
                if jump_if_true {
                    // jump-when-TRUE: IfTrue(L, dest, jn); IfTrue(R, dest, jn).
                    let lo_cmp = b.emit_jump(Opcode::Lt, lo, label, r);
                    b.set_p5(lo_cmp, p5);
                    let hi_cmp = b.emit_jump(Opcode::Gt, hi, label, r);
                    b.set_p5(hi_cmp, p5);
                } else {
                    // jump-when-FALSE (WHERE skip): IfTrue(L, d2, !jn); IfFalse(R, dest, jn); d2:.
                    let d2 = b.new_label();
                    let lt = b.emit_jump(Opcode::Lt, lo, d2, r);
                    b.set_p5(lt, p5_short);
                    let le = b.emit_jump(Opcode::Le, hi, label, r);
                    b.set_p5(le, p5);
                    b.resolve(d2);
                }
            } else {
                // BETWEEN = `(r >= lo) AND (r <= hi)`. TRUE when in range, FALSE when out of
                // range, NULL when r is NULL. The AND pattern threads `!jn` into the
                // short-circuit operand.
                let p5_short = aff_to_p5(aff) | if !jump_if_null { P5_JUMPIFNULL } else { 0 };
                if jump_if_true {
                    // jump-when-TRUE: IfFalse(L, d2, !jn); IfTrue(R, dest, jn); d2:.
                    let d2 = b.new_label();
                    let ge = b.emit_jump(Opcode::Lt, lo, d2, r);
                    b.set_p5(ge, p5_short);
                    let le = b.emit_jump(Opcode::Le, hi, label, r);
                    b.set_p5(le, p5);
                    b.resolve(d2);
                } else {
                    // jump-when-FALSE (WHERE skip): IfFalse(L, dest, jn); IfFalse(R, dest, jn).
                    let lt = b.emit_jump(Opcode::Lt, lo, label, r);
                    b.set_p5(lt, p5);
                    let gt = b.emit_jump(Opcode::Gt, hi, label, r);
                    b.set_p5(gt, p5);
                }
            }
            Ok(())
        }
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
            // M26.6: thread the comparison's collation into the opcode's P4 so the VDBE
            // compares TEXT under the right sequence (NOCASE/RTRIM/explicit COLLATE). The
            // precedence is explicit COLLATE > column default > BINARY; BINARY is encoded
            // by the absence of a P4 (the VDBE defaults to BINARY).
            if let Some(coll) = comparison_collation(left, right, ctx) {
                let name = match coll {
                    Collation::Binary => "BINARY",
                    Collation::NoCase => "NOCASE",
                    Collation::RTrim => "RTRIM",
                };
                b.set_p4(idx, P4::Symbol(name.to_string()));
            }
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

/// The collation of an expression for comparison purposes, mirroring upstream's
/// `sqlite3CompareCollation` (the `p4` collation attached to a comparison opcode). The
/// precedence is: an explicit `expr COLLATE name` clause wins; otherwise a column's
/// declared collation (searching both sides — at most one side should carry a declared
/// collation in a typical comparison); otherwise `BINARY`. Returns `None` when no collation
/// is needed (BINARY is the default and is encoded by the absence of a `P4::Symbol`).
fn comparison_collation(left: &Expr, right: &Expr, ctx: Ctx) -> Option<Collation> {
    let lc = expr_collation(left, ctx);
    let rc = expr_collation(right, ctx);
    match (lc, rc) {
        (Some(c), _) | (_, Some(c)) => {
            if c == Collation::Binary {
                None
            } else {
                Some(c)
            }
        }
        (None, None) => None,
    }
}

/// The collation carried by a single expression: an explicit `COLLATE` clause, or a column's
/// declared collation. Returns `None` for expressions with no collation (literals, computed
/// values, columns with the default BINARY collation — `None` lets `comparison_collation`
/// fall through to the other side or to BINARY).
pub(crate) fn expr_collation(e: &Expr, ctx: Ctx) -> Option<Collation> {
    match e {
        Expr::Collate { expr, collation } => {
            // An explicit COLLATE wins. Resolve by name; an unknown collation falls through
            // to the underlying expression (upstream raises "no such collation" lazily; we
            // treat it as BINARY here so the comparison still runs).
            Collation::from_name(collation)
                .or_else(|| expr_collation(expr, ctx))
        }
        Expr::Column { table, name, .. } => {
            resolve_column_collation(table.as_deref(), name, ctx)
        }
        _ => None,
    }
}

/// Look up a column's declared collation, searching `Ctx::join_tables` first (for joins),
/// then the single `Ctx::table`, then the outer scope (for correlated subqueries). Returns
/// `None` for the default BINARY collation (so it falls through in `comparison_collation`).
fn resolve_column_collation(qualifier: Option<&str>, name: &str, ctx: Ctx) -> Option<Collation> {
    // Joins: resolve via the named table (qualifier matches the table name or its alias), or
    // the first table that has the column for a bare ref.
    if let Some(jt) = ctx.join_tables {
        let found: Option<(&Table, ColumnRef)> = if let Some(q) = qualifier {
            jt.iter().find(|t| t.name.eq_ignore_ascii_case(q)).and_then(|t| {
                t.table.resolve_column(name).map(|cr| (t.table, cr))
            })
        } else {
            jt.iter().find_map(|t| {
                t.table.resolve_column(name).map(|cr| (t.table, cr))
            })
        };
        if let Some((table, cr)) = found {
            return column_collation(table, cr);
        }
    }
    // Single table. For a qualified ref (`tbl.col`), only match the named table; for a bare
    // ref, the single table is the only candidate.
    if let Some(q) = qualifier {
        if !ctx.table.name.eq_ignore_ascii_case(q) {
            return None;
        }
    }
    if let Some(cr) = ctx.table.resolve_column(name) {
        return column_collation(ctx.table, cr);
    }
    // Outer scope (correlated subquery). The collation is read-only metadata; the sentinel
    // cursor is rewritten at inline time, but the column's declared collation is stable.
    if let Some(outer) = ctx.outer {
        if let Some((ot, cr)) = lookup_outer(outer, qualifier, name) {
            return column_collation(ot.table, cr);
        }
    }
    None
}

/// A column's declared collation, or `None` when it's the default BINARY (so it falls
/// through in `comparison_collation`). The rowid alias has no declared collation.
fn column_collation(table: &Table, cr: ColumnRef) -> Option<Collation> {
    match cr {
        ColumnRef::Index(i) => {
            let c = table.columns[i].collation;
            if c == Collation::Binary { None } else { Some(c) }
        }
        ColumnRef::Rowid => None,
    }
}

fn is_numeric(a: Affinity) -> bool {
    matches!(a, Affinity::Integer | Affinity::Real | Affinity::Numeric)
}
