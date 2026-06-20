//! Common Table Expression (CTE) rewriting — `WITH [RECURSIVE] name AS (…) SELECT …`
//! (mirrors the `searchWith` / `sqlite3WithPush` path in `select.c`).
//!
//! The first slice (M10.2–M10.5) handles **non-recursive** CTEs by rewriting the AST before
//! codegen: a FROM entry that names a CTE is replaced with a `TableOrJoin::Subquery` whose
//! body is the CTE's SELECT. The existing `codegen::subquery::compile_from_subquery`
//! infrastructure then materializes that subquery into an ephemeral table and scans it —
//! exactly the `SRT_EphemTab` shape upstream uses for non-recursive CTEs that are referenced
//! once (`tag-select-0488`).
//!
//! Recursive CTEs (`WITH RECURSIVE`) use the queue-based iterative algorithm
//! (`generateWithRecursiveQuery` in `select.c`) and have their own codegen (M10.3).
//!
//! What this module does NOT do:
//! * `MATERIALIZED` / `NOT MATERIALIZED` hints — the first slice always materializes (the
//!   default for a CTE referenced once). Reuse via `OP_OpenDup` (multiple references) lands
//!   with M7.12 / the CTE-optimization follow-up.
//! * Flattening of a non-recursive CTE into the outer query — lands with M8.12 subquery
//!   flattening.
//! * Correlated CTEs — CTEs are never correlated in SQLite (they're evaluated once).

use rustqlite_parser::{Expr, ResultColumn, SelectStmt, TableOrJoin, WithClause};

use crate::error::{Error, Result};

/// Whether a `WITH` clause is present and contains at actual CTE list.
pub fn has_ctes(select: &SelectStmt) -> bool {
    select
        .with_clause
        .as_ref()
        .is_some_and(|w| !w.ctes.is_empty())
}

/// Whether the WITH clause is recursive (carries the `RECURSIVE` keyword) OR any CTE body
/// is itself a compound SELECT whose second arm references the CTE name (the canonical
/// recursive-CTE shape). Upstream sets `SF_Recursive` only after name resolution; here we
/// approximate by checking the declared `recursive` flag plus a syntactic check for a
/// self-reference in a compound arm's FROM.
pub fn is_recursive(with: &WithClause) -> bool {
    if with.recursive {
        return true;
    }
    for cte in &with.ctes {
        if is_recursive_cte_body(&cte.query, &cte.name) {
            return true;
        }
    }
    false
}

/// Recursively scan a SELECT for a self-reference to `cte_name` in any FROM clause.
fn is_recursive_cte_body(select: &SelectStmt, cte_name: &str) -> bool {
    if from_references_name(&select.from, cte_name) {
        return true;
    }
    for (_, arm) in &select.compound {
        if from_references_name(&arm.from, cte_name) {
            return true;
        }
    }
    false
}

fn from_references_name(from: &[TableOrJoin], name: &str) -> bool {
    for item in from {
        match item {
            TableOrJoin::Table(t) => {
                if t.schema.is_none() && t.name.eq_ignore_ascii_case(name) {
                    return true;
                }
            }
            TableOrJoin::Subquery { query, .. } => {
                if is_recursive_cte_body(query, name) {
                    return true;
                }
            }
            TableOrJoin::Join(j) => {
                if from_references_name(std::slice::from_ref(&*j.left), name) {
                    return true;
                }
                if j.right.schema.is_none() && j.right.name.eq_ignore_ascii_case(name) {
                    return true;
                }
            }
        }
    }
    false
}

/// Rewrite the outer SELECT's FROM clause, replacing CTE references with subqueries. The
/// returned SELECT has its `with_clause` cleared so downstream codegen does not re-enter
/// this path. Each CTE body is itself rewritten against the prefix of earlier CTEs in the
/// same WITH clause (so a later CTE may reference an earlier one), and the CTE body's own
/// `with_clause` (a nested `WITH`) is processed recursively.
///
/// When the CTE declares an explicit column list (`name (col1, col2, …) AS (…)`), the
/// subquery's projection is wrapped so each output column carries the declared name as its
/// alias. This makes the synthesized subquery table's columns match the CTE's declared
/// header (M10.5).
pub fn rewrite_with_ctes(select: &SelectStmt) -> Result<SelectStmt> {
    let Some(with) = select.with_clause.clone() else {
        return Ok(select.clone());
    };
    if with.ctes.is_empty() {
        let mut s = select.clone();
        s.with_clause = None;
        return Ok(s);
    }
    if is_recursive(&with) {
        return Err(Error::msg(
            "recursive CTEs are not supported by this codegen path (M10.3 pending)",
        ));
    }

    // Rewrite the outer SELECT against the full set of CTEs (each CTE may be referenced by
    // the outer FROM). The CTE bodies themselves are rewritten first, in order, against the
    // growing prefix so a later CTE may reference an earlier one.
    let mut rewritten_ctes: Vec<(String, SelectStmt)> = Vec::with_capacity(with.ctes.len());
    for cte in &with.ctes {
        // Build the active scope: all earlier rewritten CTEs (in declared order) plus any
        // CTEs from an enclosing WITH (handled by recursion via `rewrite_with_ctes` on the
        // body). The body itself may carry a nested `WITH` clause; process that first.
        let mut body = cte.query.clone();
        if body.with_clause.is_some() {
            body = rewrite_with_ctes(&body)?;
        }
        // Rewrite the body's FROM against the prefix of earlier CTEs in this same WITH.
        let scope: Vec<(String, SelectStmt)> = rewritten_ctes.clone();
        body = rewrite_from_against_scope(body, &scope)?;
        // Wrap the body so its projection uses the CTE's explicit column list, if any.
        if !cte.columns.is_empty() {
            body = wrap_with_column_names(body, &cte.columns)?;
        }
        rewritten_ctes.push((cte.name.clone(), body));
    }

    // Now rewrite the outer SELECT's FROM (and its compound arms' FROMs) against the full
    // CTE scope.
    let mut outer = select.clone();
    outer = rewrite_from_against_scope(outer, &rewritten_ctes)?;
    outer.with_clause = None;
    Ok(outer)
}

/// Walk the SELECT's FROM clauses (the leading core and each compound arm) and replace any
/// `TableOrJoin::Table` whose name matches a CTE in `scope` with a `TableOrJoin::Subquery`
/// carrying a clone of that CTE's rewritten body. The subquery's alias is the CTE name (so
/// `table.*` and `Ctx::join_tables` resolution by alias still works). When the original
/// `TableRef` had an alias, that alias overrides the subquery alias (matching SQL: `FROM
/// cte AS x` makes `x` the name visible in the outer query).
fn rewrite_from_against_scope(mut select: SelectStmt, scope: &[(String, SelectStmt)]) -> Result<SelectStmt> {
    if scope.is_empty() {
        return Ok(select);
    }
    select.from = rewrite_from_list(select.from, scope)?;
    // Compound arms carry their own FROM clauses.
    let mut new_compound = Vec::with_capacity(select.compound.len());
    for (op, arm) in select.compound {
        let arm = rewrite_from_against_scope(arm, scope)?;
        new_compound.push((op, arm));
    }
    select.compound = new_compound;
    Ok(select)
}

fn rewrite_from_list(from: Vec<TableOrJoin>, scope: &[(String, SelectStmt)]) -> Result<Vec<TableOrJoin>> {
    let mut out = Vec::with_capacity(from.len());
    for item in from {
        out.push(rewrite_from_item(item, scope)?);
    }
    Ok(out)
}

fn rewrite_from_item(item: TableOrJoin, scope: &[(String, SelectStmt)]) -> Result<TableOrJoin> {
    match item {
        TableOrJoin::Table(t) => {
            if t.schema.is_some() {
                // A schema-qualified reference cannot be a CTE (matches upstream's
                // `searchWith` early-out when `zDatabase` is set).
                return Ok(TableOrJoin::Table(t));
            }
            let Some((_, cte_body)) = scope.iter().find(|(n, _)| n.eq_ignore_ascii_case(&t.name)) else {
                return Ok(TableOrJoin::Table(t));
            };
            // Replace with a subquery. The visible alias is the user-supplied alias if any,
            // else the CTE name.
            let alias = t.alias.clone().unwrap_or_else(|| t.name.clone());
            // The subquery body is a clone of the (already rewritten) CTE body. We must
            // strip any WITH clause from the cloned body — it has already been expanded.
            let mut body = cte_body.clone();
            body.with_clause = None;
            Ok(TableOrJoin::Subquery {
                query: Box::new(body),
                alias,
            })
        }
        TableOrJoin::Subquery { query, alias } => {
            // Recurse into the subquery's body so a CTE reference inside it resolves too.
            // (This handles `FROM (SELECT * FROM cte) AS x`.)
            let mut body = query.as_ref().clone();
            if body.with_clause.is_some() {
                body = rewrite_with_ctes(&body)?;
            }
            body = rewrite_from_against_scope(body, scope)?;
            Ok(TableOrJoin::Subquery {
                query: Box::new(body),
                alias,
            })
        }
        TableOrJoin::Join(j) => {
            let left = rewrite_from_item(*j.left, scope)?;
            Ok(TableOrJoin::Join(rustqlite_parser::Join {
                op: j.op,
                left: Box::new(left),
                right: j.right,
                constraint: j.constraint,
            }))
        }
    }
}

/// Wrap a CTE body so its projection uses the declared column names. Mirrors upstream's
/// behavior: when a CTE has `(col1, col2, …)`, the output columns are renamed to those
/// names regardless of the body's own projection aliases.
///
/// This is done by replacing each `ResultColumn::Expr` with one that carries the declared
/// name as its alias, and each `ResultColumn::Star` by an explicit list of the body's
/// output columns (so we can attach the declared names). Because we cannot know the body's
/// output column count without resolving it, we require the body to have an explicit
/// projection (no `*`) when an explicit CTE column list is present — matching upstream's
/// "table X has N values for M columns" error, which we approximate by raising a
/// codegen-time error if the counts disagree after expansion (deferred; here we just wrap
/// the explicit projection).
fn wrap_with_column_names(mut body: SelectStmt, names: &[String]) -> Result<SelectStmt> {
    if body.values.is_empty() && body.columns.len() == 1 {
        // `SELECT *` — we can't rename the columns without expanding. Defer the expansion
        // to the codegen's `expand_columns` by leaving the body as-is; the synthesized
        // table will use the body's own column names, and the explicit CTE column list is
        // then applied as a renaming outer shell. For now, error to surface the limitation.
        if matches!(body.columns[0], ResultColumn::Star) {
            // `SELECT * FROM t` with an explicit CTE column list: expand the star by
            // emitting a synthetic projection that aliases each output column to the
            // declared name. We don't know the source table's columns here, so instead we
            // wrap: build `SELECT <names> FROM (original body) AS __cte_inner`. The inner
            // body's star expands against its own FROM table at codegen time; this outer
            // shell projects the renamed columns.
            return Ok(wrap_star_cte_with_names(body, names));
        }
    }
    if body.values.is_empty() {
        // Replace each ResultColumn::Expr with one carrying the declared name as alias.
        // If the counts disagree, the codegen's `expand_columns` will surface a mismatch
        // when the outer query reads the synthesized table — we approximate the oracle's
        // "table X has N values for M columns" by checking here only when we can.
        let mut new_cols = Vec::with_capacity(body.columns.len());
        for (i, rc) in body.columns.iter().enumerate() {
            match rc {
                ResultColumn::Expr { expr, alias: _ } => {
                    let name = names.get(i).cloned().unwrap_or_default();
                    new_cols.push(ResultColumn::Expr {
                        expr: expr.clone(),
                        alias: Some(name),
                    });
                }
                ResultColumn::Star | ResultColumn::TableStar(_) => {
                    // Cannot attach a single name to a star; fall back to wrapping.
                    return Ok(wrap_star_cte_with_names(body, names));
                }
            }
        }
        body.columns = new_cols;
    }
    // For a VALUES body, the columns are `column1, column2, …` and the explicit CTE column
    // list just renames them — we wrap in the same way as a star.
    if !body.values.is_empty() {
        return Ok(wrap_star_cte_with_names(body, names));
    }
    Ok(body)
}

/// Wrap a CTE body whose projection is a `*` (or a VALUES) in an outer SELECT that
/// projects the declared CTE column names from the inner body. The inner body is compiled
/// as a subquery (its `*` expands against its own FROM table), and the outer shell
/// projects `inner.col1 AS name1, inner.col2 AS name2, …`. This requires knowing the inner
/// body's column count, which we get from the declared list (it must match).
fn wrap_star_cte_with_names(body: SelectStmt, names: &[String]) -> SelectStmt {
    // The inner body is referenced as a subquery in the outer shell's FROM. Each declared
    // name becomes `columnN AS name` — using the positional `columnN` names SQLite assigns
    // to a VALUES body or a star-expanded projection, which the inner body's ephemeral
    // table will expose.
    let inner_alias = "__cte_inner".to_string();
    let columns: Vec<ResultColumn> = names
        .iter()
        .enumerate()
        .map(|(i, n)| ResultColumn::Expr {
            expr: Expr::Column {
                schema: None,
                table: Some(inner_alias.clone()),
                name: format!("column{}", i + 1),
            },
            alias: Some(n.clone()),
        })
        .collect();
    SelectStmt {
        distinct: false,
        columns,
        from: vec![TableOrJoin::Subquery {
            query: Box::new(body),
            alias: inner_alias,
        }],
        where_clause: None,
        group_by: Vec::new(),
        having: None,
        compound: Vec::new(),
        order_by: Vec::new(),
        limit: None,
        offset: None,
        with_clause: None,
        window_clause: Vec::new(),
        values: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustqlite_parser::parse;

    fn select_of(sql: &str) -> SelectStmt {
        let stmts = parse(sql).expect("parse");
        match stmts.into_iter().next() {
            Some(rustqlite_parser::Stmt::Select(s)) => s,
            other => panic!("expected SELECT, got {other:?}"),
        }
    }

    #[test]
    fn non_recursive_cte_rewrites_from_table_to_subquery() {
        let s = select_of("WITH x AS (SELECT 1 AS a) SELECT * FROM x;");
        let r = rewrite_with_ctes(&s).expect("rewrite");
        assert!(r.with_clause.is_none());
        assert_eq!(r.from.len(), 1);
        match &r.from[0] {
            TableOrJoin::Subquery { alias, .. } => assert_eq!(alias, "x"),
            other => panic!("expected Subquery, got {other:?}"),
        }
    }

    #[test]
    fn cte_reference_keeps_user_alias() {
        let s = select_of("WITH x AS (SELECT 1 AS a) SELECT * FROM x AS y;");
        let r = rewrite_with_ctes(&s).expect("rewrite");
        match &r.from[0] {
            TableOrJoin::Subquery { alias, .. } => assert_eq!(alias, "y"),
            other => panic!("expected Subquery, got {other:?}"),
        }
    }

    #[test]
    fn schema_qualified_name_is_not_a_cte() {
        let s = select_of("WITH x AS (SELECT 1 AS a) SELECT * FROM main.x;");
        let r = rewrite_with_ctes(&s).expect("rewrite");
        // The reference stays a Table; the CTE is unused (and will fail at catalog lookup,
        // which is the correct behavior — schema-qualified names never match CTEs).
        assert!(matches!(r.from[0], TableOrJoin::Table(_)));
    }

    #[test]
    fn multiple_ctes_later_references_earlier() {
        let s = select_of(
            "WITH a AS (SELECT 1 AS x), b AS (SELECT x FROM a) SELECT * FROM b;",
        );
        let r = rewrite_with_ctes(&s).expect("rewrite");
        // Outer FROM references b → Subquery.
        let b_body = match &r.from[0] {
            TableOrJoin::Subquery { query, .. } => query.as_ref().clone(),
            other => panic!("expected Subquery, got {other:?}"),
        };
        // b's body references a → Subquery.
        match &b_body.from[0] {
            TableOrJoin::Subquery { alias, .. } => assert_eq!(alias, "a"),
            other => panic!("expected nested Subquery, got {other:?}"),
        }
    }

    #[test]
    fn explicit_column_list_wraps_projection() {
        let s = select_of(
            "WITH x (p, q) AS (SELECT 1, 2) SELECT p, q FROM x;",
        );
        let r = rewrite_with_ctes(&s).expect("rewrite");
        let body = match &r.from[0] {
            TableOrJoin::Subquery { query, .. } => query.as_ref().clone(),
            other => panic!("expected Subquery, got {other:?}"),
        };
        // The body's projection now carries aliases p, q.
        assert_eq!(body.columns.len(), 2);
        match &body.columns[0] {
            ResultColumn::Expr { alias, .. } => assert_eq!(alias.as_deref(), Some("p")),
            other => panic!("expected Expr, got {other:?}"),
        }
        match &body.columns[1] {
            ResultColumn::Expr { alias, .. } => assert_eq!(alias.as_deref(), Some("q")),
            other => panic!("expected Expr, got {other:?}"),
        }
    }

    #[test]
    fn recursive_cte_errors() {
        let s = select_of(
            "WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM x WHERE n<5) SELECT n FROM x;",
        );
        let r = rewrite_with_ctes(&s);
        assert!(r.is_err());
    }
}