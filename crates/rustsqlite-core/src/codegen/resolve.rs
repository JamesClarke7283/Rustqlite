//! Name resolution — the Rust analogue of upstream's `resolve.c`.
//!
//! Upstream's `resolve.c` walks the AST (via `walker.c`) and, for every `Expr::Column`
//! node, calls `lookupName` to find the matching entry in the current `NameContext`'s
//! `SrcList` (or an enclosing context, via the `pNext` chain). It mutates the `Expr` in
//! place (`pExpr->iTable = pItem->iCursor`, `pExpr->iColumn = j`) and raises
//! `"no such column"` / `"ambiguous column name"` when the match count is 0 or >1.
//!
//! Our `Expr` is an owned immutable enum with no cursor/index slots, so this pass is a
//! **validation pre-pass**: it walks the SELECT, builds a per-core `NameContext` from
//! the FROM clause, and verifies that every column reference resolves uniquely. The
//! actual cursor/column-index binding still happens at codegen time in
//! [`super::expr::compile_column`] — this pass just enforces the error parity upstream
//! enforces at resolve time, so a query like `SELECT a FROM t1, t2` (where both tables
//! have a column `a`) raises `"ambiguous column name: a"` before codegen, matching the
//! oracle, instead of silently resolving to the first table.
//!
//! ## Scope
//!
//! This first slice handles the cases the codegen currently defers:
//!
//! - **Ambiguous bare column** in a multi-table FROM (raised here, not at codegen).
//! - **No-such-column** with a table qualifier that doesn't exist as a FROM alias/name.
//! - **No-such-column** for a bare column that no FROM table exposes.
//!
//! Correlation (a column reference resolved in an *enclosing* `NameContext`) is
//! supported via the [`NameContext::parent`] chain, matching upstream's
//! `do { … } while (pNC = pNC->pNext)` loop in `lookupName` (`resolve.c:341`). A
//! correlated reference does NOT raise "ambiguous" even if the same name appears in
//! an inner scope — the innermost match wins, exactly like upstream.
//!
//! What this pass does **not** yet do (and upstream does):
//!
//! - Resolve result-column aliases for `ORDER BY` (handled by
//!   [`super::select::resolve_order_term`] at codegen time).
//! - Bind parameters / function lookup / type validation (those are codegen-time today).
//! - `NC_*` flag enforcement (`NC_AllowAgg`, `NC_IsCheck`, `NC_PartIdx`, …) — the
//!   flag-specific checks (e.g. "aggregate functions are not allowed in the WHERE
//!   clause") remain at codegen time in `select.rs`.
//! - Annotate the `Expr` with cursor/index (Rust's immutable AST has no slot for that;
//!   codegen re-resolves via [`crate::schema::Table::resolve_column`]).
//! - Recurse into FROM-subquery bodies or compound-arm FROM clauses — those need
//!   catalog access and are validated by the subquery/compound codegen paths, which
//!   raise "no such column" via `compile_column` as before. The pass prunes any
//!   subquery (via `visit_select`) to avoid double-resolution.

use rustqlite_parser::walker::{Visitor, WalkControl, walk_select_expr};
use rustqlite_parser::{Expr, SelectStmt};

use crate::error::{Error, Result};
use crate::schema::Table;

/// A resolved FROM-table entry in a [`NameContext`] — the Rust analogue of one
/// `SrcItem` in upstream's `SrcList`. The `name` is the alias if present, else the
/// table name — the form a bare `col` or `alias.col` reference matches against.
#[derive(Clone, Copy)]
pub struct ResolveTable<'a> {
    pub table: &'a Table,
    /// The alias if one was supplied (`FROM t AS x` → `"x"`), else the table name.
    pub name: &'a str,
}

/// The name-resolution scope for a single `SELECT` core — the Rust analogue of
/// upstream's `NameContext` (`sqliteInt.h:3499`). Carries the FROM tables and a link
/// to the enclosing scope for correlated subqueries.
pub struct NameContext<'a, 'p> {
    pub tables: &'a [ResolveTable<'a>],
    pub parent: Option<&'p NameContext<'a, 'p>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LookupOutcome {
    /// Exactly one match in this context.
    Found,
    /// Two or more matches in this context (ambiguous).
    Ambiguous,
    /// No match here, but an enclosing context matched (correlation).
    FoundOuter,
    /// No match here; an enclosing context was itself ambiguous.
    AmbiguousOuter,
    /// No match anywhere in the scope chain.
    NoSuchColumn,
}

impl<'a, 'p> NameContext<'a, 'p> {
    /// Walk the scope chain (this context, then `parent`, then its parent, …) looking
    /// for a column match. Returns the number of matches found in *this* context
    /// (for the ambiguous check) and whether any enclosing context matched (for
    /// correlation — which suppresses the "no such column" error). Mirrors the
    /// `do { … } while (pNC = pNC->pNext)` loop in `lookupName` (`resolve.c:341`).
    fn lookup(&self, schema: Option<&str>, qualifier: Option<&str>, name: &str) -> LookupOutcome {
        // A schema-qualified reference (`main.t.col`) is not resolvable in single-DB
        // mode — we don't track the schema name on `ResolveTable`. Upstream's
        // `lookupName` checks `zDatabase` against `pTab->zName` only. Match the
        // oracle's "no such column: schema.tbl.col" for an unknown schema.
        if schema.is_some() {
            return LookupOutcome::NoSuchColumn;
        }
        let mut local_cnt = 0i32;
        let mut qualifier_matched_a_table = false;
        for t in self.tables {
            if let Some(q) = qualifier {
                if !t.name.eq_ignore_ascii_case(q) {
                    continue;
                }
                qualifier_matched_a_table = true;
                if t.table.resolve_column(name).is_some() {
                    local_cnt += 1;
                }
            } else {
                if t.table.resolve_column(name).is_some() {
                    local_cnt += 1;
                }
            }
        }
        if local_cnt > 1 {
            return LookupOutcome::Ambiguous;
        }
        if local_cnt == 1 {
            return LookupOutcome::Found;
        }
        // local_cnt == 0.
        // A qualified ref whose qualifier matched a table here but had no column:
        // the qualifier pinned the scope, so don't look outward — "no such column".
        if qualifier.is_some() && qualifier_matched_a_table {
            return LookupOutcome::NoSuchColumn;
        }
        // A qualified ref whose qualifier didn't match any table here: upstream's
        // `lookupName` does NOT fall through to outer contexts for an unknown `zTab`
        // — it returns "no such column" immediately. Match that.
        if qualifier.is_some() && !qualifier_matched_a_table {
            return LookupOutcome::NoSuchColumn;
        }
        // Unqualified ref with no local match: try enclosing scope (correlation).
        if let Some(parent) = self.parent {
            return match parent.lookup(schema, qualifier, name) {
                LookupOutcome::Found | LookupOutcome::FoundOuter => LookupOutcome::FoundOuter,
                LookupOutcome::Ambiguous | LookupOutcome::AmbiguousOuter => {
                    LookupOutcome::AmbiguousOuter
                }
                LookupOutcome::NoSuchColumn => LookupOutcome::NoSuchColumn,
            };
        }
        LookupOutcome::NoSuchColumn
    }
}

/// The visitor that walks a SELECT and validates every `Expr::Column` against the
/// current [`NameContext`]. On the first error, it aborts the walk carrying the
/// `Error` back to the caller.
struct Resolver<'a, 'p> {
    nc: &'a NameContext<'a, 'p>,
    err: Option<Error>,
}

impl<'a, 'p> Resolver<'a, 'p> {
    fn new(nc: &'a NameContext<'a, 'p>) -> Self {
        Self { nc, err: None }
    }

    fn fail(&mut self, e: Error) -> WalkControl<Error> {
        self.err = Some(e.clone());
        WalkControl::Abort(e)
    }
}

impl<'a, 'p> Visitor for Resolver<'a, 'p> {
    type Break = Error;

    fn visit_expr(&mut self, e: &Expr) -> WalkControl<Error> {
        match e {
            Expr::Column { schema, table, name } => {
                let outcome = self.nc.lookup(schema.as_deref(), table.as_deref(), name);
                match outcome {
                    LookupOutcome::Found | LookupOutcome::FoundOuter => WalkControl::Continue,
                    LookupOutcome::Ambiguous | LookupOutcome::AmbiguousOuter => {
                        let disp = display_col(schema.as_deref(), table.as_deref(), name);
                        self.fail(Error::msg(format!("ambiguous column name: {disp}")))
                    }
                    LookupOutcome::NoSuchColumn => {
                        let disp = display_col(schema.as_deref(), table.as_deref(), name);
                        self.fail(Error::msg(format!("no such column: {disp}")))
                    }
                }
            }
            // Codegen-only synthetic nodes; never produced by the parser, so a resolve
            // pass never encounters them. Treat as opaque (no descent).
            Expr::AggRef(_) => WalkControl::Prune,
            Expr::Coalesce2 { .. } => WalkControl::Prune,
            // Subqueries: the walk descends into them via `walk_select`, which calls
            // `visit_select`. `visit_select` prunes subqueries with their own FROM
            // (their column refs are resolved by the subquery codegen paths) and
            // continues into FROM-less / VALUES-only subqueries (so their bare
            // column refs resolve against the enclosing scope — correlation).
            _ => WalkControl::Continue,
        }
    }

    fn visit_select(&mut self, select: &SelectStmt) -> WalkControl<Error> {
        // Prune any subquery that has its own FROM clause — its column references
        // are resolved against the catalog by the subquery codegen paths
        // (`codegen::subquery`), which raise "no such column" via `compile_column`.
        // A FROM-less or VALUES-only subquery is walked normally so its bare column
        // refs resolve against the enclosing scope (correlation).
        //
        // The top-level SELECT is walked via `walk_select_expr` (not `walk_select`),
        // so this method is only invoked for subqueries encountered during the
        // expression walk.
        if !select.from.is_empty() && select.values.is_empty() {
            WalkControl::Prune
        } else {
            WalkControl::Continue
        }
    }
}

fn display_col(schema: Option<&str>, table: Option<&str>, name: &str) -> String {
    match (schema, table) {
        (Some(s), Some(t)) => format!("{s}.{t}.{name}"),
        (None, Some(t)) => format!("{t}.{name}"),
        (Some(s), None) => format!("{s}.{name}"),
        (None, None) => name.to_string(),
    }
}

/// Validate that every column reference in `select` resolves uniquely against the
/// FROM tables in `tables`. `parent` is the enclosing scope (for correlated
/// subqueries), or `None` for the outermost query.
///
/// This is the entry point mirroring `sqlite3ResolveSelectNames` (`resolve.c:2265`).
/// It walks the SELECT's result columns, WHERE, GROUP BY, HAVING, ORDER BY, LIMIT,
/// OFFSET, and VALUES rows — but NOT the FROM clause's own subquery bodies (those
/// are resolved against the catalog by the subquery codegen paths, which raise
/// "no such column" via `compile_column` when a reference doesn't resolve).
///
/// Compound SELECT arms (`UNION`/`UNION ALL`/`INTERSECT`/`EXCEPT`) each carry their
/// own FROM scope (independent of the leading arm's), so they need catalog
/// resolution and are validated by the compound codegen path; this pass validates
/// the leading core only.
pub fn resolve_select<'a, 'p>(
    select: &SelectStmt,
    tables: &'a [ResolveTable<'a>],
    parent: Option<&'p NameContext<'a, 'p>>,
) -> Result<()> {
    let nc = NameContext { tables, parent };
    let mut v = Resolver::new(&nc);
    // Walk only this core's expressions (result columns, WHERE, GROUP BY, HAVING,
    // ORDER BY, LIMIT, OFFSET, VALUES rows) — NOT the FROM clause's subquery bodies
    // (those are resolved by the subquery codegen paths). Subqueries encountered
    // inside expressions are pruned in `visit_expr` when they have their own FROM.
    walk_select_expr(&mut v, select)?;
    if let Some(e) = v.err.take() {
        return Err(e);
    }
    // Compound SELECT arms: each arm has its own FROM scope (independent of the
    // leading arm's). For the first slice we only validate the leading core
    // against `tables`; the compound arms' FROM tables need catalog resolution and
    // are validated by the compound codegen path.
    Ok(())
}