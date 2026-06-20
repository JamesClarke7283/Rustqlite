//! `USING (cols)` and `NATURAL JOIN` rewriting (M7.10 / M7.14).
//!
//! SQLite handles `USING`/`NATURAL` at the name-resolution layer (`select.c` /
//! `resolve.c`): the join's `ON` predicate is the AND of `<left>.col = <right>.col`
//! for each shared column, and a bare reference to a shared column (in the
//! projection, WHERE, ORDER BY, etc.) resolves to a COALESCE of both sides — the
//! preserved side first (so a LEFT JOIN's bare `col` is the left value unless it
//! is NULL, then the right value), and `SELECT *` suppresses the second copy of
//! each shared column. See `@docs/using-and-natural-join.md`.
//!
//! Rustqlite implements this by *rewriting the AST* before the join codegen runs:
//! the rewrite produces a plain inner/left/right/full join with a synthetic
//! `ON` predicate, a deduplicated projection, and bare shared-column references
//! replaced by a coalesce pseudo-expression. The downstream `compile_cross_join`
//! then handles the result without needing to know about `USING` at all.
//!
//! The coalesce is uniformly `IF outer.col IS NOT NULL THEN outer.col ELSE inner.col`
//! (in JOIN order, not FROM order), which matches SQLite's preserved-side-first
//! rule for LEFT/RIGHT/FULL joins and is observationally identical for INNER
//! joins (where both sides are equal on a match).

use rustqlite_parser::{
    BinaryOp, Expr, FunctionArgs, JoinConstraint, JoinOp, OrderingTerm, ResultColumn, SelectStmt,
    TableOrJoin,
};

use crate::error::{Error, Result};
use crate::schema::Table;

/// Resolve a column name to its index in the table's column list (case-insensitive).
fn column_index(table: &Table, name: &str) -> Option<usize> {
    table.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name))
}

/// Compute the USING column list for a join between `left` and `right`:
/// - `USING(cols)`: the explicit list, validated to exist in both tables.
/// - `NATURAL` (any variant): the columns of `left` (in declared order) that also exist in `right`.
/// - `None` for an `ON`/no-constraint join.
///
/// Returns `(using_cols, is_natural)`. Errors mirror SQLite's wording.
pub fn resolve_using_cols(
    left: &Table,
    right: &Table,
    constraint: Option<&JoinConstraint>,
    op: JoinOp,
) -> Result<Option<Vec<String>>> {
    // NATURAL may not carry an ON or USING clause (parser allows it; we reject here).
    let is_natural = matches!(
        op,
        JoinOp::Natural | JoinOp::NaturalLeft | JoinOp::NaturalRight | JoinOp::NaturalFull
    );
    if is_natural && constraint.is_some() {
        return Err(Error::msg(
            "a NATURAL join may not have an ON or USING clause",
        ));
    }
    match constraint {
        Some(JoinConstraint::Using(cols)) => {
            for c in cols {
                if column_index(left, c).is_none() || column_index(right, c).is_none() {
                    return Err(Error::msg(format!(
                        "cannot join using column {c} - column not present in both tables"
                    )));
                }
            }
            Ok(Some(cols.clone()))
        }
        Some(JoinConstraint::On(_)) => Ok(None),
        None => {
            if is_natural {
                let mut shared = Vec::new();
                for col in &left.columns {
                    if column_index(right, &col.name).is_some()
                        && !shared.iter().any(|s: &String| s.eq_ignore_ascii_case(&col.name))
                    {
                        shared.push(col.name.clone());
                    }
                }
                Ok(Some(shared))
            } else {
                Ok(None)
            }
        }
    }
}

/// Build the synthetic `ON` predicate for a USING/NATURAL join: `l.col = r.col AND ...`.
/// `outer_name`/`inner_name` are the names used to qualify columns on each side (the
/// alias or table name in JOIN order — outer first). Returns `None` for an empty list
/// (a NATURAL join with no shared columns is a cross join).
pub fn synthetic_on(
    using_cols: &[String],
    outer_name: &str,
    inner_name: &str,
) -> Option<Expr> {
    let mut acc: Option<Expr> = None;
    for c in using_cols {
        let term = Expr::Binary {
            op: BinaryOp::Eq,
            left: Box::new(qualified_col(outer_name, c)),
            right: Box::new(qualified_col(inner_name, c)),
        };
        acc = Some(match acc {
            Some(prev) => Expr::Binary {
                op: BinaryOp::And,
                left: Box::new(prev),
                right: Box::new(term),
            },
            None => term,
        });
    }
    acc
}

fn qualified_col(table: &str, name: &str) -> Expr {
    Expr::Column {
        schema: None,
        table: Some(table.to_string()),
        name: name.to_string(),
    }
}

/// A coalesce expression: `IF outer.col IS NOT NULL THEN outer.col ELSE inner.col`,
/// modeled as a synthetic [`Expr::Coalesce2`] node lowered by the expression codegen.
pub fn coalesce_expr(outer_name: &str, inner_name: &str, col: &str) -> Expr {
    Expr::Coalesce2 {
        left: Box::new(qualified_col(outer_name, col)),
        right: Box::new(qualified_col(inner_name, col)),
    }
}

/// True if `name` is in `using_cols` (case-insensitive).
fn is_using_col(using_cols: &[String], name: &str) -> bool {
    using_cols.iter().any(|c| c.eq_ignore_ascii_case(name))
}

/// Walk an expression tree, rewriting bare column references that match a USING
/// column into a coalesce of both sides. Table-qualified references are left
/// untouched (the user explicitly chose a side). Returns the rewritten expression.
///
/// `outer_name`/`inner_name` are the join-order side names (outer first). Bare
/// column references that are NOT in `using_cols` and are present in BOTH tables
/// are an "ambiguous column name" error — raised here, matching SQLite.
pub fn rewrite_expr(
    expr: &Expr,
    using_cols: &[String],
    outer_name: &str,
    inner_name: &str,
    outer_table: &Table,
    inner_table: &Table,
) -> Result<Expr> {
    Ok(match expr {
        Expr::Column {
            schema: _,
            table: None,
            name,
        } => {
            if is_using_col(using_cols, name) {
                coalesce_expr(outer_name, inner_name, name)
            } else {
                let in_outer = column_index(outer_table, name).is_some();
                let in_inner = column_index(inner_table, name).is_some();
                if in_outer && in_inner {
                    return Err(Error::msg(format!(
                        "ambiguous column name: {name}"
                    )));
                }
                expr.clone()
            }
        }
        Expr::Column { table: Some(_), .. } => expr.clone(),
        Expr::Unary { op, expr } => Expr::Unary {
            op: *op,
            expr: Box::new(rewrite_expr(
                expr,
                using_cols,
                outer_name,
                inner_name,
                outer_table,
                inner_table,
            )?),
        },
        Expr::Binary { op, left, right } => Expr::Binary {
            op: *op,
            left: Box::new(rewrite_expr(
                left,
                using_cols,
                outer_name,
                inner_name,
                outer_table,
                inner_table,
            )?),
            right: Box::new(rewrite_expr(
                right,
                using_cols,
                outer_name,
                inner_name,
                outer_table,
                inner_table,
            )?),
        },
        Expr::Function {
            name: fn_name,
            distinct,
            args,
            filter,
            over,
        } => {
            let new_args = match args {
                FunctionArgs::List(v) => FunctionArgs::List(
                    v.iter()
                        .map(|a| {
                            rewrite_expr(
                                a,
                                using_cols,
                                outer_name,
                                inner_name,
                                outer_table,
                                inner_table,
                            )
                        })
                        .collect::<Result<Vec<_>>>()?,
                ),
                FunctionArgs::Star => FunctionArgs::Star,
            };
            Expr::Function {
                name: fn_name.clone(),
                distinct: *distinct,
                args: new_args,
                filter: filter
                    .as_ref()
                    .map(|f| {
                        Box::new(rewrite_expr(
                            f,
                            using_cols,
                            outer_name,
                            inner_name,
                            outer_table,
                            inner_table,
                        ).unwrap_or_else(|_| (**f).clone()))
                    }),
                over: over.clone(),
            }
        }
        Expr::BindParam(_) => expr.clone(),
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: Box::new(rewrite_expr(
                expr,
                using_cols,
                outer_name,
                inner_name,
                outer_table,
                inner_table,
            )?),
            low: Box::new(rewrite_expr(
                low,
                using_cols,
                outer_name,
                inner_name,
                outer_table,
                inner_table,
            )?),
            high: Box::new(rewrite_expr(
                high,
                using_cols,
                outer_name,
                inner_name,
                outer_table,
                inner_table,
            )?),
            negated: *negated,
        },
        Expr::In {
            expr,
            values,
            negated,
        } => Expr::In {
            expr: Box::new(rewrite_expr(
                expr,
                using_cols,
                outer_name,
                inner_name,
                outer_table,
                inner_table,
            )?),
            values: values
                .iter()
                .map(|v| {
                    rewrite_expr(
                        v,
                        using_cols,
                        outer_name,
                        inner_name,
                        outer_table,
                        inner_table,
                    )
                })
                .collect::<Result<Vec<_>>>()?,
            negated: *negated,
        },
        Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => Expr::InSubquery {
            expr: Box::new(rewrite_expr(
                expr,
                using_cols,
                outer_name,
                inner_name,
                outer_table,
                inner_table,
            )?),
            subquery: subquery.clone(),
            negated: *negated,
        },
        Expr::Exists(s) => Expr::Exists(s.clone()),
        Expr::Subquery(s) => Expr::Subquery(s.clone()),
        Expr::Row(items) => Expr::Row(
            items
                .iter()
                .map(|e| {
                    rewrite_expr(
                        e,
                        using_cols,
                        outer_name,
                        inner_name,
                        outer_table,
                        inner_table,
                    )
                })
                .collect::<Result<Vec<_>>>()?,
        ),
        Expr::Cast { expr, type_name } => Expr::Cast {
            expr: Box::new(rewrite_expr(
                expr,
                using_cols,
                outer_name,
                inner_name,
                outer_table,
                inner_table,
            )?),
            type_name: type_name.clone(),
        },
        Expr::Case {
            base,
            when_then,
            else_expr,
        } => {
            let new_base = base
                .as_ref()
                .map(|b| {
                    Box::new(rewrite_expr(
                        b,
                        using_cols,
                        outer_name,
                        inner_name,
                        outer_table,
                        inner_table,
                    ).unwrap_or_else(|_| (**b).clone()))
                });
            let new_when: Vec<(Expr, Expr)> = when_then
                .iter()
                .map(|(w, t)| {
                    let nw = rewrite_expr(
                        w,
                        using_cols,
                        outer_name,
                        inner_name,
                        outer_table,
                        inner_table,
                    )?;
                    let nt = rewrite_expr(
                        t,
                        using_cols,
                        outer_name,
                        inner_name,
                        outer_table,
                        inner_table,
                    )?;
                    Ok((nw, nt))
                })
                .collect::<Result<Vec<_>>>()?;
            let new_else = else_expr
                .as_ref()
                .map(|e| {
                    Box::new(rewrite_expr(
                        e,
                        using_cols,
                        outer_name,
                        inner_name,
                        outer_table,
                        inner_table,
                    ).unwrap_or_else(|_| (**e).clone()))
                });
            Expr::Case {
                base: new_base,
                when_then: new_when,
                else_expr: new_else,
            }
        }
        Expr::Collate { expr, collation } => Expr::Collate {
            expr: Box::new(rewrite_expr(
                expr,
                using_cols,
                outer_name,
                inner_name,
                outer_table,
                inner_table,
            )?),
            collation: collation.clone(),
        },
        Expr::IsDistinctFrom {
            left,
            right,
            negated,
        } => Expr::IsDistinctFrom {
            left: Box::new(rewrite_expr(
                left,
                using_cols,
                outer_name,
                inner_name,
                outer_table,
                inner_table,
            )?),
            right: Box::new(rewrite_expr(
                right,
                using_cols,
                outer_name,
                inner_name,
                outer_table,
                inner_table,
            )?),
            negated: *negated,
        },
        Expr::AggRef(_) => expr.clone(),
        Expr::Literal(_) => expr.clone(),
        // A pre-existing Coalesce2 (from a nested rewrite, or already-synthetic) is
        // walked so its inner column references get rewritten too.
        Expr::Coalesce2 { left, right } => Expr::Coalesce2 {
            left: Box::new(rewrite_expr(
                left,
                using_cols,
                outer_name,
                inner_name,
                outer_table,
                inner_table,
            )?),
            right: Box::new(rewrite_expr(
                right,
                using_cols,
                outer_name,
                inner_name,
                outer_table,
                inner_table,
            )?),
        },
    })
}

/// Rewrite the projection (`SELECT` column list) for a USING/NATURAL join:
/// - `*` expands to all left-table columns (FROM order), then all right-table
///   columns except those in `using_cols`.
/// - `table.*` is left as-is (the codegen expands it against the named table).
/// - Bare expressions are rewritten via [`rewrite_expr`].
pub fn rewrite_projection(
    select: &SelectStmt,
    using_cols: &[String],
    outer_name: &str,
    inner_name: &str,
    outer_table: &Table,
    inner_table: &Table,
    from_left_name: &str,
    from_right_name: &str,
) -> Result<Vec<ResultColumn>> {
    let _ = (outer_name, inner_name);
    let mut out = Vec::new();
    for rc in &select.columns {
        match rc {
            ResultColumn::Star => {
                // FROM order: left table's columns, then right table's columns
                // minus the using cols. For a non-swapped (INNER/LEFT/FULL) join
                // FROM order == join order; for a RIGHT JOIN the FROM order is
                // still left-then-right while join order is right-then-left. The
                // dedup suppresses the using cols from the SECOND table in FROM
                // order (the right/inner-from-side). The using col itself
                // appears once, coalesced across both sides (preserved side
                // first via `outer_name`/`inner_name`). Non-using columns are
                // table-qualified so they resolve unambiguously even when both
                // tables share a column name.
                for c in &outer_table.columns {
                    if is_using_col(using_cols, &c.name) {
                        out.push(ResultColumn::Expr {
                            expr: coalesce_expr(outer_name, inner_name, &c.name),
                            alias: None,
                        });
                    } else {
                        out.push(ResultColumn::Expr {
                            expr: qualified_col(from_left_name, &c.name),
                            alias: None,
                        });
                    }
                }
                for c in &inner_table.columns {
                    if is_using_col(using_cols, &c.name) {
                        continue;
                    }
                    out.push(ResultColumn::Expr {
                        expr: qualified_col(from_right_name, &c.name),
                        alias: None,
                    });
                }
            }
            ResultColumn::TableStar(_) => out.push(rc.clone()),
            ResultColumn::Expr { expr, alias } => {
                let new_expr = rewrite_expr(
                    expr,
                    using_cols,
                    outer_name,
                    inner_name,
                    outer_table,
                    inner_table,
                )?;
                out.push(ResultColumn::Expr {
                    expr: new_expr,
                    alias: alias.clone(),
                });
            }
        }
    }
    if out.is_empty() {
        return Err(Error::msg("no result columns"));
    }
    Ok(out)
}

/// Rewrite the WHERE clause, ORDER BY terms, HAVING, and GROUP BY of `select` in
/// place via [`rewrite_expr`].
pub fn rewrite_select_clauses(
    select: &mut SelectStmt,
    using_cols: &[String],
    outer_name: &str,
    inner_name: &str,
    outer_table: &Table,
    inner_table: &Table,
) -> Result<()> {
    if let Some(w) = &select.where_clause {
        select.where_clause = Some(rewrite_expr(
            w,
            using_cols,
            outer_name,
            inner_name,
            outer_table,
            inner_table,
        )?);
    }
    if let Some(h) = &select.having {
        select.having = Some(rewrite_expr(
            h,
            using_cols,
            outer_name,
            inner_name,
            outer_table,
            inner_table,
        )?);
    }
    select.group_by = select
        .group_by
        .iter()
        .map(|e| {
            rewrite_expr(
                e,
                using_cols,
                outer_name,
                inner_name,
                outer_table,
                inner_table,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    select.order_by = select
        .order_by
        .iter()
        .map(|t| {
            Ok::<_, Error>(OrderingTerm {
                expr: rewrite_expr(
                    &t.expr,
                    using_cols,
                    outer_name,
                    inner_name,
                    outer_table,
                    inner_table,
                )?,
                desc: t.desc,
                nulls: t.nulls,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(())
}

/// Returns the using-column list when the top-level FROM join is a USING/NATURAL
/// join (and `None` otherwise). Callers then call [`rewrite_select_clauses`] and
/// [`rewrite_projection`] with the resolved sides and tables in FROM order.
///
/// `from_order` is `(left, right)` in original FROM order; `join_order` is the
/// outer/inner loop order. Returns `None` if the top-level join has no
/// USING/NATURAL constraint.
pub fn using_cols_for(
    from: &[TableOrJoin],
    left: &Table,
    right: &Table,
) -> Result<Option<Vec<String>>> {
    let Some(TableOrJoin::Join(j)) = from.first() else {
        return Ok(None);
    };
    resolve_using_cols(left, right, j.constraint.as_ref(), j.op)
}