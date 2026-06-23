//! Subquery flattening (mirrors `flattenSubquery` in upstream `select.c`).
//!
//! When the outer `SELECT` has a `FROM (subquery) AS alias` entry and the subquery
//! is a simple non-aggregate single-core SELECT, the subquery's FROM entries can be
//! spliced directly into the outer FROM and the outer expressions rewritten to
//! reference the substituted projection expressions. This avoids materializing
//! the subquery into an ephemeral table and scanning it again, and lets the
//! query planner use indexes on the inner tables.
//!
//! Only the simplest shape is handled here: a single FROM entry that is a subquery,
//! no joins in the outer FROM, no compound arms on either side, no recursive CTE.
//! The full upstream flattener handles joins, compound subqueries, and
//! RIGHT/FULL JOIN — those land with later follow-ups. The restriction numbers in
//! the comments refer to the block above `flattenSubquery` in `select.c`.

use rustqlite_parser::{
    BinaryOp, Expr, FunctionArgs, OrderingTerm, ResultColumn, SelectStmt, TableOrJoin,
};

use crate::error::Result;

/// `true` if `e` contains a function call with an `OVER` clause (a window function).
fn contains_window_function(e: &Expr) -> bool {
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
        Expr::Row(v) => v.iter().any(contains_window_function),
        Expr::Coalesce2 { left, right } => {
            contains_window_function(left) || contains_window_function(right)
        }
        Expr::Exists(_) | Expr::Subquery(_) => false,
        Expr::Literal(_) | Expr::Column { .. } | Expr::BindParam(_) | Expr::AggRef(_) => false,
    }
}

fn query_has_window(select: &SelectStmt) -> bool {
    let cols_have_window = select.columns.iter().any(|rc| match rc {
        ResultColumn::Expr { expr, .. } => contains_window_function(expr),
        _ => false,
    });
    let order_has_window = select.order_by.iter().any(|t| contains_window_function(&t.expr));
    cols_have_window || order_has_window
}

fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "count" | "sum" | "total" | "avg" | "min" | "max" | "group_concat" | "string_agg"
    )
}

/// `true` if `e` contains an aggregate function call (a function call without an `OVER`
/// clause whose name is a known aggregate). A conservative check used only to skip
/// flattening aggregate subqueries (matches the upstream decision to never flatten
/// aggregate subqueries). The real aggregate decision for codegen lives in `select.rs`.
fn contains_aggregate_call(e: &Expr) -> bool {
    match e {
        Expr::Function { name, over, .. } => over.is_none() && is_aggregate_name(name),
        Expr::Unary { expr, .. } => contains_aggregate_call(expr),
        Expr::Binary { left, right, .. } => {
            contains_aggregate_call(left) || contains_aggregate_call(right)
        }
        Expr::Between { expr, low, high, .. } => {
            contains_aggregate_call(expr)
                || contains_aggregate_call(low)
                || contains_aggregate_call(high)
        }
        Expr::In { expr, values, .. } => {
            contains_aggregate_call(expr) || values.iter().any(contains_aggregate_call)
        }
        Expr::InSubquery { expr, .. } => contains_aggregate_call(expr),
        Expr::Cast { expr, .. } => contains_aggregate_call(expr),
        Expr::Case {
            base,
            when_then,
            else_expr,
        } => {
            base.as_ref().is_some_and(|b| contains_aggregate_call(b))
                || when_then
                    .iter()
                    .any(|(w, t)| contains_aggregate_call(w) || contains_aggregate_call(t))
                || else_expr.as_ref().is_some_and(|e| contains_aggregate_call(e))
        }
        Expr::Collate { expr, .. } => contains_aggregate_call(expr),
        Expr::IsDistinctFrom { left, right, .. } => {
            contains_aggregate_call(left) || contains_aggregate_call(right)
        }
        Expr::Row(v) => v.iter().any(contains_aggregate_call),
        Expr::Coalesce2 { left, right } => {
            contains_aggregate_call(left) || contains_aggregate_call(right)
        }
        Expr::Exists(_) | Expr::Subquery(_) => false,
        Expr::Literal(_) | Expr::Column { .. } | Expr::BindParam(_) | Expr::AggRef(_) => false,
    }
}

fn select_has_aggregate(select: &SelectStmt) -> bool {
    !select.group_by.is_empty()
        || select.having.is_some()
        || select.columns.iter().any(|rc| match rc {
            ResultColumn::Expr { expr, .. } => contains_aggregate_call(expr),
            _ => false,
        })
}

/// Check the upstream flattening restrictions on `outer` (whose single FROM entry is the
/// subquery to flatten). Returns the cloned subquery and its alias when flattening is
/// permitted; `None` when not flattenable.
fn restrictions_pass(outer: &SelectStmt) -> Option<(SelectStmt, String)> {
    if !outer.compound.is_empty() || !outer.values.is_empty() {
        return None;
    }
    if outer.from.len() != 1 {
        return None;
    }
    let (subquery, alias) = match &outer.from[0] {
        TableOrJoin::Subquery { query, alias } => (query.as_ref(), alias.clone()),
        _ => return None,
    };
    // Restriction (25): neither outer nor subquery may use window functions.
    if query_has_window(outer) || query_has_window(subquery) {
        return None;
    }
    // The subquery must not be a compound SELECT (the compound-subquery flattening is a
    // separate upstream path we do not implement here) or a VALUES body.
    if !subquery.compound.is_empty() || !subquery.values.is_empty() {
        return None;
    }
    // Restriction (4): subquery not DISTINCT.
    if subquery.distinct {
        return None;
    }
    // Restriction (7): subquery must have a FROM clause.
    if subquery.from.is_empty() {
        return None;
    }
    // No longer flatten aggregate subqueries (matches the upstream decision).
    if select_has_aggregate(subquery) {
        return None;
    }
    // Restriction (13): not both have LIMIT.
    if subquery.limit.is_some() && outer.limit.is_some() {
        return None;
    }
    // Restriction (14): subquery may not use OFFSET.
    if subquery.offset.is_some() {
        return None;
    }
    // Restriction (11): not both have ORDER BY.
    if !subquery.order_by.is_empty() && !outer.order_by.is_empty() {
        return None;
    }
    // Restriction (16): if outer is aggregate, subquery may not use ORDER BY.
    if select_has_aggregate(outer) && !subquery.order_by.is_empty() {
        return None;
    }
    // Restriction (19): if subquery uses LIMIT, outer may not have a WHERE clause.
    if subquery.limit.is_some() && outer.where_clause.is_some() {
        return None;
    }
    // Restriction (21): if subquery uses LIMIT, outer may not be DISTINCT.
    if subquery.limit.is_some() && outer.distinct {
        return None;
    }
    // Restriction (9): if subquery uses LIMIT, outer may not be aggregate.
    if subquery.limit.is_some() && select_has_aggregate(outer) {
        return None;
    }
    // Restriction (8) is implicit: we only flatten a single-entry outer FROM, so the
    // outer is never a join.
    Some((subquery.clone(), alias))
}

/// A substitution entry: the subquery output column name (as seen by the outer query) and
/// the expression to substitute for references to it.
struct Subst {
    name: String,
    expr: Expr,
}

/// Build the substitution map for a subquery's projection. Returns `None` when the
/// projection contains a `*` or `table.*` that this slice does not expand (the caller falls
/// back to materialization in that case).
fn build_substitution(subquery: &SelectStmt) -> Option<Vec<Subst>> {
    let mut out = Vec::new();
    for rc in &subquery.columns {
        match rc {
            ResultColumn::Star | ResultColumn::TableStar(_) => return None,
            ResultColumn::Expr { expr, alias } => {
                let name = alias.clone().unwrap_or_else(|| default_col_name(expr));
                out.push(Subst { name, expr: expr.clone() });
            }
        }
    }
    Some(out)
}

/// Expand the outer SELECT's projection against the subquery's output columns. `SELECT *`
/// becomes the subquery's projection expressions (one per output column); `alias.*` becomes
/// the same; `table.*` referencing a table inside the subquery's FROM is left as-is (it will
/// be expanded by the downstream codegen against the spliced-in inner FROM). A bare
/// expression column is substituted via `subst_expr`.
fn subst_result_column_expanded(
    rc: &ResultColumn,
    alias: &str,
    subst: &[Subst],
) -> Vec<ResultColumn> {
    match rc {
        ResultColumn::Star => {
            // `SELECT *` expands to all subquery output columns, in order.
            subst
                .iter()
                .map(|s| ResultColumn::Expr {
                    expr: s.expr.clone(),
                    alias: None,
                })
                .collect()
        }
        ResultColumn::TableStar(t) => {
            if t.eq_ignore_ascii_case(alias) {
                // `alias.*` expands to all subquery output columns, in order.
                subst
                    .iter()
                    .map(|s| ResultColumn::Expr {
                        expr: s.expr.clone(),
                        alias: None,
                    })
                    .collect()
            } else {
                // A table-qualified `*` referencing a table inside the subquery's FROM is
                // left as-is; the downstream codegen expands it against the spliced-in
                // inner FROM.
                vec![ResultColumn::TableStar(t.clone())]
            }
        }
        ResultColumn::Expr { expr, alias: a } => vec![ResultColumn::Expr {
            expr: subst_expr(expr, alias, subst),
            alias: a.clone(),
        }],
    }
}

/// The default column name for an expression. Matches `select::default_col_name` for the
/// bare-column case (the common case); for other expressions the exact text does not matter
/// for correctness — it only affects whether a bare reference in the outer query matches,
/// and the common flattenable case uses an explicit `AS alias`.
fn default_col_name(expr: &Expr) -> String {
    match expr {
        Expr::Column { name, .. } => name.clone(),
        _ => expr_to_text(expr),
    }
}

fn expr_to_text(e: &Expr) -> String {
    match e {
        Expr::Literal(l) => match l {
            rustqlite_parser::Literal::Null => "NULL".to_string(),
            rustqlite_parser::Literal::Integer(n) => n.to_string(),
            rustqlite_parser::Literal::Real(f) => f.to_string(),
            rustqlite_parser::Literal::Text(s) => format!("'{}'", s),
            rustqlite_parser::Literal::Blob(b) => {
                format!("X'{}'", b.iter().map(|x| format!("{:02x}", x)).collect::<String>())
            }
            rustqlite_parser::Literal::Bool(b) => b.to_string(),
        },
        Expr::Column { table: Some(t), name, .. } => format!("{}.{}", t, name),
        Expr::Column { table: None, name, .. } => name.clone(),
        Expr::BindParam(s) => s.clone(),
        Expr::Unary { op, expr } => format!("{:?}({})", op, expr_to_text(expr)),
        Expr::Binary { op, left, right } => {
            format!("{} {:?} {}", expr_to_text(left), op, expr_to_text(right))
        }
        _ => "?".to_string(),
    }
}

/// Substitute references in `expr` that point to the subquery's output columns with the
/// subquery's projection expressions. A bare `col` matching a substitution name is replaced
/// by the corresponding expression. An `alias.col` reference is replaced when `alias`
/// matches the subquery's alias and `col` matches a substitution name. Other column
/// references are left as-is.
fn subst_expr(expr: &Expr, alias: &str, subst: &[Subst]) -> Expr {
    match expr {
        Expr::Column {
            table: None,
            name,
            ..
        } => {
            if let Some(s) = subst.iter().find(|s| s.name.eq_ignore_ascii_case(name)) {
                s.expr.clone()
            } else {
                expr.clone()
            }
        }
        Expr::Column {
            table: Some(t),
            name,
            ..
        } => {
            if t.eq_ignore_ascii_case(alias) {
                if let Some(s) = subst.iter().find(|s| s.name.eq_ignore_ascii_case(name)) {
                    return s.expr.clone();
                }
            }
            expr.clone()
        }
        Expr::Unary { op, expr: inner } => Expr::Unary {
            op: *op,
            expr: Box::new(subst_expr(inner, alias, subst)),
        },
        Expr::Binary { op, left, right } => Expr::Binary {
            op: *op,
            left: Box::new(subst_expr(left, alias, subst)),
            right: Box::new(subst_expr(right, alias, subst)),
        },
        Expr::Function {
            name: fn_name,
            distinct,
            args,
            filter,
            over,
        } => {
            let new_args = match args {
                FunctionArgs::Star => FunctionArgs::Star,
                FunctionArgs::List(v) => {
                    FunctionArgs::List(v.iter().map(|a| subst_expr(a, alias, subst)).collect())
                }
            };
            Expr::Function {
                name: fn_name.clone(),
                distinct: *distinct,
                args: new_args,
                filter: filter.as_ref().map(|f| Box::new(subst_expr(f, alias, subst))),
                over: over.clone(),
            }
        }
        Expr::BindParam(_) | Expr::Literal(_) | Expr::AggRef(_) => expr.clone(),
        Expr::Between {
            expr: e,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: Box::new(subst_expr(e, alias, subst)),
            low: Box::new(subst_expr(low, alias, subst)),
            high: Box::new(subst_expr(high, alias, subst)),
            negated: *negated,
        },
        Expr::In {
            expr: e,
            values,
            negated,
        } => Expr::In {
            expr: Box::new(subst_expr(e, alias, subst)),
            values: values.iter().map(|v| subst_expr(v, alias, subst)).collect(),
            negated: *negated,
        },
        Expr::InSubquery {
            expr: e,
            subquery,
            negated,
        } => Expr::InSubquery {
            expr: Box::new(subst_expr(e, alias, subst)),
            subquery: subquery.clone(),
            negated: *negated,
        },
        Expr::Exists(s) => Expr::Exists(s.clone()),
        Expr::Subquery(s) => Expr::Subquery(s.clone()),
        Expr::Row(v) => Expr::Row(v.iter().map(|e| subst_expr(e, alias, subst)).collect()),
        Expr::Cast { expr: e, type_name } => Expr::Cast {
            expr: Box::new(subst_expr(e, alias, subst)),
            type_name: type_name.clone(),
        },
        Expr::Case {
            base,
            when_then,
            else_expr,
        } => Expr::Case {
            base: base.as_ref().map(|b| Box::new(subst_expr(b, alias, subst))),
            when_then: when_then
                .iter()
                .map(|(w, t)| (subst_expr(w, alias, subst), subst_expr(t, alias, subst)))
                .collect(),
            else_expr: else_expr.as_ref().map(|e| Box::new(subst_expr(e, alias, subst))),
        },
        Expr::Collate { expr: e, collation } => Expr::Collate {
            expr: Box::new(subst_expr(e, alias, subst)),
            collation: collation.clone(),
        },
        Expr::IsDistinctFrom {
            left,
            right,
            negated,
        } => Expr::IsDistinctFrom {
            left: Box::new(subst_expr(left, alias, subst)),
            right: Box::new(subst_expr(right, alias, subst)),
            negated: *negated,
        },
        Expr::Coalesce2 { left, right } => Expr::Coalesce2 {
            left: Box::new(subst_expr(left, alias, subst)),
            right: Box::new(subst_expr(right, alias, subst)),
        },
    }
}

fn subst_ordering_term(t: &OrderingTerm, alias: &str, subst: &[Subst]) -> OrderingTerm {
    OrderingTerm {
        expr: subst_expr(&t.expr, alias, subst),
        desc: t.desc,
        nulls: t.nulls,
    }
}

/// Attempt to flatten the single `FROM (subquery) AS alias` entry of `outer` into the outer
/// query. Returns `Some(rewritten_outer)` when flattening was applied, or `None` when the
/// restrictions are not satisfied (the caller falls back to materialization).
pub fn try_flatten_subquery(outer: &SelectStmt) -> Result<Option<SelectStmt>> {
    let (subquery, alias) = match restrictions_pass(outer) {
        Some(v) => v,
        None => return Ok(None),
    };
    let subst = match build_substitution(&subquery) {
        Some(s) => s,
        None => return Ok(None),
    };

    let mut new_outer = outer.clone();
    // 1. Splice the subquery's FROM entries into the outer FROM.
    new_outer.from = subquery.from.clone();

    // 2. Combine WHERE: subquery's WHERE AND outer's rewritten WHERE.
    let sub_where = subquery.where_clause.clone();
    let outer_where = outer
        .where_clause
        .as_ref()
        .map(|w| subst_expr(w, &alias, &subst));
    new_outer.where_clause = match (sub_where, outer_where) {
        (Some(s), Some(o)) => Some(Expr::Binary {
            op: BinaryOp::And,
            left: Box::new(s),
            right: Box::new(o),
        }),
        (Some(s), None) => Some(s),
        (None, Some(o)) => Some(o),
        (None, None) => None,
    };

    // 3. Rewrite the outer projection. `SELECT *` and `alias.*` expand to the subquery's
    //    output columns (matching the upstream flattener's `substSelect` which replaces the
    //    parent's `pEList` with the subquery's `pEList`).
    new_outer.columns = outer
        .columns
        .iter()
        .flat_map(|rc| subst_result_column_expanded(rc, &alias, &subst))
        .collect();

    // 4. Rewrite GROUP BY / HAVING.
    new_outer.group_by = outer
        .group_by
        .iter()
        .map(|e| subst_expr(e, &alias, &subst))
        .collect();
    new_outer.having = outer
        .having
        .as_ref()
        .map(|e| subst_expr(e, &alias, &subst));

    // 5. ORDER BY: if the outer has none and the subquery has one, transfer the subquery's
    //    ORDER BY (restriction (11) already ensured they are not both present). Otherwise
    //    rewrite the outer's ORDER BY.
    if outer.order_by.is_empty() && !subquery.order_by.is_empty() {
        new_outer.order_by = subquery.order_by.clone();
    } else {
        new_outer.order_by = outer
            .order_by
            .iter()
            .map(|t| subst_ordering_term(t, &alias, &subst))
            .collect();
    }

    // 6. LIMIT: if the subquery has one and the outer does not, transfer it (restriction
    //    (13) already ensured they are not both present).
    if outer.limit.is_none() && subquery.limit.is_some() {
        new_outer.limit = subquery.limit.clone();
        new_outer.offset = subquery.offset.clone();
    }

    Ok(Some(new_outer))
}