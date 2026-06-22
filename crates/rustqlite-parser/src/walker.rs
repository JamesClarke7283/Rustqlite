//! AST tree-walking infrastructure — the Rust analogue of upstream's `walker.c`.
//!
//! SQLite's `walker.c` provides a small, generic tree-walk engine: a `Walker` struct
//! holding two function-pointer callbacks (`xExprCallback`, `xSelectCallback`) and a
//! `WRC_Continue`/`WRC_Prune`/`WRC_Abort` return-code protocol. The walk is pre-order —
//! the callback is invoked *before* descending into children — and the visitor decides
//! per-node whether to descend (`WRC_Continue`), skip the children but continue with
//! siblings (`WRC_Prune`), or unwind the whole walk (`WRC_Abort`).
//!
//! This module mirrors that API idiomatically in Rust: instead of function pointers and
//! a tagged-union `Walker.u` payload, we expose a [`Visitor`] trait with generic state
//! and a [`WalkControl`] enum that matches the C return codes. Free functions
//! [`walk_expr`], [`walk_expr_list`], [`walk_select`], [`walk_select_expr`],
//! [`walk_select_from`], and [`walk_stmt`] correspond to upstream's `sqlite3WalkExpr` /
//! `sqlite3WalkExprList` / `sqlite3WalkSelect` / `sqlite3WalkSelectExpr` /
//! `sqlite3WalkSelectFrom` (plus a top-level statement walker that has no direct C
//! counterpart — SQLite dispatches on the statement kind at each call site).
//!
//! As in the C version, the visitor methods are invoked *before* descending into
//! children. Override [`Visitor::visit_expr`] and/or [`Visitor::visit_select`] to
//! inspect nodes; the default implementations are no-ops that return
//! [`WalkControl::Continue`] (descend), matching `sqlite3ExprWalkNoop` /
//! `sqlite3SelectWalkNoop`.
//!
//! The walk is read-only (it borrows `&Expr` / `&SelectStmt`). Mutating passes use the
//! separate [`Rewriter`] trait, which consumes and produces owned nodes — the equivalent
//! of upstream's `xTreeRewrite`-style passes that rebuild the tree. Upstream's walker
//! is also used for in-place mutation via casts; Rust makes that explicit with a
//! distinct trait.
//!
//! ## Example
//!
//! ```
//! use rustqlite_parser::{Stmt, parse};
//! use rustqlite_parser::walker::{Visitor, WalkControl, walk_stmt};
//!
//! struct CountColumns(usize);
//! impl Visitor for CountColumns {
//!     type Break = ();
//!     // No custom visit_expr; the walk descends into every expression.
//! }
//! // Count the result columns of every SELECT in the statement:
//! // (illustrative — visit_select is called on each SELECT encountered)
//! ```

use crate::ast::*;

/// What a [`Visitor`] callback wants the walk to do next.
///
/// Mirrors the `WRC_Continue` / `WRC_Prune` / `WRC_Abort` constants in
/// `walker.c`:
///
/// - [`WalkControl::Continue`] — descend into this node's children, then continue
///   with its siblings.
/// - [`WalkControl::Prune`] — skip this node's children, but continue with its
///   siblings. Upstream's `WRC_Prune`.
/// - [`WalkControl::Abort`] — stop the walk immediately and unwind, returning `B`
///   from the top-level call. Upstream's `WRC_Abort` (which carries no value; the
///   `Break` type parameter lets Rust visitors short-circuit with a result).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkControl<B> {
    Continue,
    Prune,
    Abort(B),
}

/// A read-only pre-order visitor over the AST.
///
/// Override [`Visitor::visit_expr`] and/or [`Visitor::visit_select`] to inspect
/// nodes. The default implementations are no-ops that return
/// [`WalkControl::Continue`] (descend), matching upstream's
/// `sqlite3ExprWalkNoop` / `sqlite3SelectWalkNoop`.
///
/// The associated type [`Break`](Self::Break) is the value carried by an
/// [`WalkControl::Abort`]; use `()` if the walk never aborts with data.
pub trait Visitor {
    type Break;

    /// Invoked once per expression node, before descending into its children.
    fn visit_expr(&mut self, _expr: &Expr) -> WalkControl<Self::Break> {
        WalkControl::Continue
    }

    /// Invoked once per `SELECT` core, before descending into its expressions and
    /// FROM clause. Mirrors `xSelectCallback` (the *first* SELECT callback in
    /// upstream — the second, `xSelectCallback2`, runs after the descend and has no
    /// abort semantics; it is not modelled here as no current consumer needs it).
    fn visit_select(&mut self, _select: &SelectStmt) -> WalkControl<Self::Break> {
        WalkControl::Continue
    }
}

/// The outcome of a walk. `Ok(())` means the walk completed fully; `Err(b)` means a
/// visitor returned [`WalkControl::Abort(b)`](WalkControl::Abort) and the walk unwound.
///
/// This mirrors the C convention where `sqlite3Walk*` returns `WRC_Abort` (non-zero) or
/// `WRC_Continue` (zero). `Prune` is resolved locally inside the walk and never escapes.
pub type WalkResult<B> = Result<(), B>;

/// Walk a single expression tree, invoking [`Visitor::visit_expr`] on the root and
/// (unless pruned) every descendant, pre-order. Mirrors `sqlite3WalkExpr`.
///
/// `None` (a NULL expression pointer in C) is a no-op, matching `sqlite3WalkExpr`'s
/// null-pointer early return.
pub fn walk_expr<V: Visitor + ?Sized>(v: &mut V, expr: &Expr) -> WalkResult<V::Break> {
    match v.visit_expr(expr) {
        WalkControl::Continue => {}
        WalkControl::Prune => return Ok(()),
        WalkControl::Abort(b) => return Err(b),
    }
    walk_expr_children(v, expr)
}

/// Descend into an expression's children without re-visiting the node itself. Used
/// internally by [`walk_expr`] after the visitor returns `Continue`; also useful for
/// visitors that have already handled the parent node and want to recurse into the
/// children directly.
pub fn walk_expr_children<V: Visitor + ?Sized>(
    v: &mut V,
    expr: &Expr,
) -> WalkResult<V::Break> {
    match expr {
        Expr::Literal(_)
        | Expr::Column { .. }
        | Expr::BindParam(_)
        | Expr::AggRef(_) => Ok(()),
        Expr::Unary { expr, .. } => walk_expr(v, expr),
        Expr::Binary { left, right, .. } => {
            walk_expr(v, left)?;
            walk_expr(v, right)
        }
        Expr::Function { args, filter, over, .. } => {
            walk_function_args(v, args)?;
            if let Some(f) = filter {
                walk_expr(v, f)?;
            }
            if let Some(w) = over {
                walk_window(v, w)?;
            }
            Ok(())
        }
        Expr::Between { expr, low, high, .. } => {
            walk_expr(v, expr)?;
            walk_expr(v, low)?;
            walk_expr(v, high)
        }
        Expr::In { expr, values, .. } => {
            walk_expr(v, expr)?;
            for v_ in values {
                walk_expr(v, v_)?;
            }
            Ok(())
        }
        Expr::InSubquery { expr, subquery, .. } => {
            walk_expr(v, expr)?;
            walk_select(v, subquery)
        }
        Expr::Exists(s) => walk_select(v, s),
        Expr::Subquery(s) => walk_select(v, s),
        Expr::Row(es) => {
            for e in es {
                walk_expr(v, e)?;
            }
            Ok(())
        }
        Expr::Cast { expr, .. } => walk_expr(v, expr),
        Expr::Case { base, when_then, else_expr } => {
            if let Some(b) = base {
                walk_expr(v, b)?;
            }
            for (w, t) in when_then {
                walk_expr(v, w)?;
                walk_expr(v, t)?;
            }
            if let Some(e) = else_expr {
                walk_expr(v, e)?;
            }
            Ok(())
        }
        Expr::Collate { expr, .. } => walk_expr(v, expr),
        Expr::IsDistinctFrom { left, right, .. } => {
            walk_expr(v, left)?;
            walk_expr(v, right)
        }
        Expr::Coalesce2 { left, right } => {
            walk_expr(v, left)?;
            walk_expr(v, right)
        }
    }
}

/// Walk a slice of expressions (an `ExprList` in C). Mirrors `sqlite3WalkExprList`.
pub fn walk_expr_list<V: Visitor + ?Sized>(
    v: &mut V,
    list: &[Expr],
) -> WalkResult<V::Break> {
    for e in list {
        walk_expr(v, e)?;
    }
    Ok(())
}

/// Walk a window spec: its PARTITION BY, ORDER BY, and frame-bound expressions.
/// Mirrors `walkWindowList` (one window at a time; the named-window reference case
/// — `Window::name` — has no expressions of its own).
pub fn walk_window<V: Visitor + ?Sized>(v: &mut V, w: &Window) -> WalkResult<V::Break> {
    // A named-window reference (`OVER name`) carries no expressions of its own; it
    // is resolved against the trailing `WINDOW` clause at codegen time.
    if w.name.is_some() {
        return Ok(());
    }
    for e in &w.partition_by {
        walk_expr(v, e)?;
    }
    for t in &w.order_by {
        walk_expr(v, &t.expr)?;
    }
    if let Some(f) = &w.frame {
        walk_frame_bound(v, &f.start)?;
        if let Some(end) = &f.end {
            walk_frame_bound(v, end)?;
        }
    }
    Ok(())
}

fn walk_frame_bound<V: Visitor + ?Sized>(
    v: &mut V,
    b: &FrameBound,
) -> WalkResult<V::Break> {
    match b {
        FrameBound::UnboundedPreceding | FrameBound::UnboundedFollowing
        | FrameBound::CurrentRow => Ok(()),
        FrameBound::Preceding(e) | FrameBound::Following(e) => walk_expr(v, e),
    }
}

fn walk_function_args<V: Visitor + ?Sized>(
    v: &mut V,
    args: &FunctionArgs,
) -> WalkResult<V::Break> {
    match args {
        FunctionArgs::Star => Ok(()),
        FunctionArgs::List(list) => walk_expr_list(v, list),
    }
}

/// Walk a `SELECT` core: invoke the select callback on `select`, then descend into
/// its expressions and FROM clause, then recurse into the compound arms and (for a
/// FROM-subquery) the subquery body. Mirrors `sqlite3WalkSelect`.
///
/// The compound arms (`select.compound`) are walked in order, each invoking the
/// select callback. This matches upstream's `p->pPrior` chain traversal in
/// `sqlite3WalkSelect`.
pub fn walk_select<V: Visitor + ?Sized>(
    v: &mut V,
    select: &SelectStmt,
) -> WalkResult<V::Break> {
    walk_select_core(v, select)?;
    for (_, arm) in &select.compound {
        walk_select_core(v, arm)?;
    }
    Ok(())
}

/// Walk a single SELECT core (no compound-arm recursion). Invokes the select
/// callback, then walks the expressions and FROM clause. Mirrors the body of
/// `sqlite3WalkSelect`'s `do { ... } while ((p = p->pPrior) != 0)` loop for one
/// iteration.
fn walk_select_core<V: Visitor + ?Sized>(
    v: &mut V,
    select: &SelectStmt,
) -> WalkResult<V::Break> {
    match v.visit_select(select) {
        WalkControl::Continue => {}
        WalkControl::Prune => return Ok(()),
        WalkControl::Abort(b) => return Err(b),
    }
    walk_select_expr(v, select)?;
    walk_select_from(v, select)?;
    // The trailing WINDOW clause's named windows carry their own expressions
    // (partition/order/frame), independent of any `OVER name` reference.
    for nw in &select.window_clause {
        walk_window(v, &nw.spec)?;
    }
    // A WITH clause's CTE bodies are themselves SELECTs.
    if let Some(with) = &select.with_clause {
        for cte in &with.ctes {
            walk_select(v, &cte.query)?;
        }
    }
    Ok(())
}

/// Walk the expressions of a SELECT core (result columns, WHERE, GROUP BY, HAVING,
/// ORDER BY, LIMIT, OFFSET, and `VALUES` rows), but not its FROM clause. Mirrors
/// `sqlite3WalkSelectExpr`.
pub fn walk_select_expr<V: Visitor + ?Sized>(
    v: &mut V,
    select: &SelectStmt,
) -> WalkResult<V::Break> {
    for c in &select.columns {
        if let ResultColumn::Expr { expr, .. } = c {
            walk_expr(v, expr)?;
        }
    }
    if let Some(w) = &select.where_clause {
        walk_expr(v, w)?;
    }
    for e in &select.group_by {
        walk_expr(v, e)?;
    }
    if let Some(h) = &select.having {
        walk_expr(v, h)?;
    }
    for t in &select.order_by {
        walk_expr(v, &t.expr)?;
    }
    if let Some(l) = &select.limit {
        walk_expr(v, l)?;
    }
    if let Some(o) = &select.offset {
        walk_expr(v, o)?;
    }
    for row in &select.values {
        for e in row {
            walk_expr(v, e)?;
        }
    }
    Ok(())
}

/// Walk the FROM clause of a SELECT core: for each table reference, recurse into any
/// subquery body and any table-valued-function arguments. Mirrors
/// `sqlite3WalkSelectFrom`.
pub fn walk_select_from<V: Visitor + ?Sized>(
    v: &mut V,
    select: &SelectStmt,
) -> WalkResult<V::Break> {
    for tj in &select.from {
        walk_table_or_join(v, tj)?;
    }
    Ok(())
}

fn walk_table_or_join<V: Visitor + ?Sized>(
    v: &mut V,
    tj: &TableOrJoin,
) -> WalkResult<V::Break> {
    match tj {
        TableOrJoin::Table(t) => {
            if let Some(args) = &t.args {
                walk_expr_list(v, args)?;
            }
            Ok(())
        }
        TableOrJoin::Subquery { query, .. } => walk_select(v, query),
        TableOrJoin::Join(j) => {
            walk_table_or_join(v, &j.left)?;
            if let Some(JoinConstraint::On(e)) = &j.constraint {
                walk_expr(v, e)?;
            }
            Ok(())
        }
    }
}

/// Walk a top-level statement, dispatching on its kind. There is no direct C
/// counterpart — upstream dispatches on the statement kind at each call site (e.g.
/// `sqlite3Select` walks a `Select*`, `sqlite3Insert` walks the `INSERT`'s columns
/// and SELECT source, etc.). This function provides a single entry point that
/// covers the statement shapes the parser can produce.
pub fn walk_stmt<V: Visitor + ?Sized>(v: &mut V, stmt: &Stmt) -> WalkResult<V::Break> {
    match stmt {
        Stmt::Select(s) => walk_select(v, s),
        Stmt::CreateTable(ct) => {
            for c in &ct.columns {
                walk_column_def(v, c)?;
            }
            for c in &ct.constraints {
                walk_table_constraint(v, c)?;
            }
            if let Some(s) = &ct.as_select {
                walk_select(v, s)?;
            }
            Ok(())
        }
        Stmt::Insert(i) => {
            walk_insert_source(v, &i.source)?;
            for u in &i.upsert {
                walk_upsert(v, u)?;
            }
            if let Some(r) = &i.returning {
                walk_result_columns(v, r)?;
            }
            Ok(())
        }
        Stmt::Delete(d) => {
            if let Some(w) = &d.where_clause {
                walk_expr(v, w)?;
            }
            for t in &d.order_by {
                walk_expr(v, &t.expr)?;
            }
            if let Some(l) = &d.limit {
                walk_expr(v, l)?;
            }
            if let Some(o) = &d.offset {
                walk_expr(v, o)?;
            }
            if let Some(r) = &d.returning {
                walk_result_columns(v, r)?;
            }
            Ok(())
        }
        Stmt::DropTable(_) => Ok(()),
        Stmt::Update(u) => {
            for a in &u.assignments {
                walk_expr(v, &a.value)?;
            }
            for tj in &u.from {
                walk_table_or_join(v, tj)?;
            }
            if let Some(w) = &u.where_clause {
                walk_expr(v, w)?;
            }
            for t in &u.order_by {
                walk_expr(v, &t.expr)?;
            }
            if let Some(l) = &u.limit {
                walk_expr(v, l)?;
            }
            if let Some(o) = &u.offset {
                walk_expr(v, o)?;
            }
            if let Some(r) = &u.returning {
                walk_result_columns(v, r)?;
            }
            Ok(())
        }
        Stmt::CreateIndex(ci) => {
            for c in &ci.columns {
                if let Some(e) = &c.expr {
                    walk_expr(v, e)?;
                }
            }
            if let Some(w) = &ci.where_clause {
                walk_expr(v, w)?;
            }
            Ok(())
        }
        Stmt::DropIndex(_) => Ok(()),
        Stmt::AlterTable(a) => walk_alter_table(v, a),
        Stmt::CreateView(cv) => walk_select(v, &cv.select),
        Stmt::DropView(_) => Ok(()),
        Stmt::CreateTrigger(t) => {
            if let Some(w) = &t.when_clause {
                walk_expr(v, w)?;
            }
            for step in &t.body {
                walk_trigger_step(v, step)?;
            }
            Ok(())
        }
        Stmt::DropTrigger(_) => Ok(()),
        Stmt::Pragma(_) => Ok(()),
        Stmt::Transaction(_) => Ok(()),
        Stmt::Attach(a) => {
            walk_expr(v, &a.filename)?;
            walk_expr(v, &a.schema_name)?;
            if let Some(k) = &a.key {
                walk_expr(v, k)?;
            }
            Ok(())
        }
        Stmt::Detach(d) => walk_expr(v, &d.schema_name),
        Stmt::Vacuum(vac) => {
            if let Some(i) = &vac.into {
                walk_expr(v, i)?;
            }
            Ok(())
        }
        Stmt::Analyze(_) => Ok(()),
        Stmt::Reindex(_) => Ok(()),
        Stmt::CreateVirtualTable(_) => Ok(()),
        Stmt::Explain(inner, _) => walk_stmt(v, inner),
    }
}

fn walk_column_def<V: Visitor + ?Sized>(
    v: &mut V,
    c: &ColumnDef,
) -> WalkResult<V::Break> {
    for con in &c.constraints {
        match con {
            ColumnConstraint::Default(e) => walk_expr(v, e)?,
            ColumnConstraint::Generated { expr, .. } => walk_expr(v, expr)?,
            _ => {}
        }
    }
    Ok(())
}

fn walk_table_constraint<V: Visitor + ?Sized>(
    v: &mut V,
    c: &TableConstraint,
) -> WalkResult<V::Break> {
    match &c.body {
        TableConstraintBody::Check { expr, .. } => walk_expr(v, expr),
        _ => Ok(()),
    }
}

fn walk_insert_source<V: Visitor + ?Sized>(
    v: &mut V,
    src: &InsertSource,
) -> WalkResult<V::Break> {
    match src {
        InsertSource::Values(rows) => {
            for row in rows {
                walk_expr_list(v, row)?;
            }
            Ok(())
        }
        InsertSource::Select(s) => walk_select(v, s),
        InsertSource::DefaultValues => Ok(()),
    }
}

fn walk_upsert<V: Visitor + ?Sized>(
    v: &mut V,
    u: &UpsertClause,
) -> WalkResult<V::Break> {
    if let Some(t) = &u.target {
        if let Some(w) = &t.where_clause {
            walk_expr(v, w)?;
        }
        for c in &t.columns {
            if let UpsertTargetColumn::Expr(e) = c {
                walk_expr(v, e)?;
            }
        }
    }
    if let UpsertAction::Update { assignments, where_clause } = &u.action {
        for a in assignments {
            walk_expr(v, &a.value)?;
        }
        if let Some(w) = where_clause {
            walk_expr(v, w)?;
        }
    }
    Ok(())
}

fn walk_result_columns<V: Visitor + ?Sized>(
    v: &mut V,
    cols: &[ResultColumn],
) -> WalkResult<V::Break> {
    for c in cols {
        if let ResultColumn::Expr { expr, .. } = c {
            walk_expr(v, expr)?;
        }
    }
    Ok(())
}

fn walk_alter_table<V: Visitor + ?Sized>(
    v: &mut V,
    a: &AlterTableStmt,
) -> WalkResult<V::Break> {
    match &a.action {
        AlterTableAction::AddColumn(c) => walk_column_def(v, c),
        AlterTableAction::AddCheckConstraint { expr, .. } => walk_expr(v, expr),
        _ => Ok(()),
    }
}

fn walk_trigger_step<V: Visitor + ?Sized>(
    v: &mut V,
    step: &TriggerStep,
) -> WalkResult<V::Break> {
    match step {
        TriggerStep::Insert(i) => {
            walk_insert_source(v, &i.source)?;
            if let Some(r) = &i.returning {
                walk_result_columns(v, r)?;
            }
            Ok(())
        }
        TriggerStep::Update(u) => {
            for a in &u.assignments {
                walk_expr(v, &a.value)?;
            }
            if let Some(w) = &u.where_clause {
                walk_expr(v, w)?;
            }
            if let Some(r) = &u.returning {
                walk_result_columns(v, r)?;
            }
            Ok(())
        }
        TriggerStep::Delete(d) => {
            if let Some(w) = &d.where_clause {
                walk_expr(v, w)?;
            }
            if let Some(r) = &d.returning {
                walk_result_columns(v, r)?;
            }
            Ok(())
        }
        TriggerStep::Select(s) => walk_select(v, s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    /// A visitor that counts every expression node it sees.
    struct CountExprs(usize);
    impl Visitor for CountExprs {
        type Break = ();
        fn visit_expr(&mut self, _expr: &Expr) -> WalkControl<()> {
            self.0 += 1;
            WalkControl::Continue
        }
    }

    /// A visitor that aborts as soon as it sees a `Function` node.
    struct FindFunction;
    impl Visitor for FindFunction {
        type Break = &'static str;
        fn visit_expr(&mut self, expr: &Expr) -> WalkControl<&'static str> {
            if matches!(expr, Expr::Function { .. }) {
                WalkControl::Abort("found function")
            } else {
                WalkControl::Continue
            }
        }
    }

    /// A visitor that prunes at `Binary` nodes (so their children are skipped).
    struct PruneBinary(usize);
    impl Visitor for PruneBinary {
        type Break = ();
        fn visit_expr(&mut self, expr: &Expr) -> WalkControl<()> {
            self.0 += 1;
            if matches!(expr, Expr::Binary { .. }) {
                WalkControl::Prune
            } else {
                WalkControl::Continue
            }
        }
    }

    /// A visitor that counts every SELECT core it sees.
    struct CountSelects(usize);
    impl Visitor for CountSelects {
        type Break = ();
        fn visit_select(&mut self, _select: &SelectStmt) -> WalkControl<()> {
            self.0 += 1;
            WalkControl::Continue
        }
    }

    #[test]
    fn counts_all_expression_nodes() {
        let stmt = parse("SELECT a + b * c FROM t WHERE x = 1;").unwrap();
        let mut v = CountExprs(0);
        walk_stmt(&mut v, &stmt[0]).unwrap();
        // a, b, c, (b*c), (a + b*c), x, 1, (x = 1) = 8
        assert_eq!(v.0, 8);
    }

    #[test]
    fn abort_short_circuits() {
        let stmt = parse("SELECT a + b FROM t;").unwrap();
        let mut v = FindFunction;
        let r = walk_stmt(&mut v, &stmt[0]);
        assert!(r.is_ok(), "no function to abort on");
        let stmt = parse("SELECT abs(a) FROM t;").unwrap();
        let mut v = FindFunction;
        let r = walk_stmt(&mut v, &stmt[0]);
        assert_eq!(r, Err("found function"));
    }

    #[test]
    fn prune_skips_children() {
        let stmt = parse("SELECT a + b * c FROM t;").unwrap();
        let mut v = PruneBinary(0);
        walk_stmt(&mut v, &stmt[0]).unwrap();
        // The projection `a + b * c` is one Binary whose children are pruned, so only
        // the top-level Binary node is visited: 1. (No WHERE/ORDER BY/etc.)
        assert_eq!(v.0, 1);
    }

    #[test]
    fn counts_selects_including_subqueries() {
        let stmt = parse("SELECT * FROM (SELECT 1) AS s WHERE x IN (SELECT 2);").unwrap();
        let mut v = CountSelects(0);
        walk_stmt(&mut v, &stmt[0]).unwrap();
        // Outer SELECT + FROM-subquery + IN-subquery = 3.
        assert_eq!(v.0, 3);
    }

    #[test]
    fn walks_compound_select_arms() {
        let stmt = parse("SELECT 1 UNION SELECT 2 UNION ALL SELECT 3;").unwrap();
        let mut v = CountSelects(0);
        walk_stmt(&mut v, &stmt[0]).unwrap();
        assert_eq!(v.0, 3);
    }

    #[test]
    fn walks_cte_bodies() {
        let stmt = parse("WITH t AS (SELECT 1) SELECT * FROM t;").unwrap();
        let mut v = CountSelects(0);
        walk_stmt(&mut v, &stmt[0]).unwrap();
        // CTE body + outer SELECT = 2.
        assert_eq!(v.0, 2);
    }

    #[test]
    fn walks_window_clause_expressions() {
        let stmt = parse(
            "SELECT count(*) OVER (PARTITION BY a ORDER BY b) FROM t;",
        )
        .unwrap();
        let mut v = CountExprs(0);
        walk_stmt(&mut v, &stmt[0]).unwrap();
        // count(*) (Function), a, b = 3.
        assert_eq!(v.0, 3);
    }

    #[test]
    fn walks_insert_values() {
        let stmt = parse("INSERT INTO t VALUES (1, 2 + 3);").unwrap();
        let mut v = CountExprs(0);
        walk_stmt(&mut v, &stmt[0]).unwrap();
        // 1, 2, 3, (2+3) = 4.
        assert_eq!(v.0, 4);
    }

    #[test]
    fn walks_update_assignments_and_where() {
        let stmt = parse("UPDATE t SET a = 1, b = 2 + 3 WHERE c = 4;").unwrap();
        let mut v = CountExprs(0);
        walk_stmt(&mut v, &stmt[0]).unwrap();
        // 1, 2, 3, (2+3), c, 4, (c=4) = 7.
        assert_eq!(v.0, 7);
    }

    #[test]
    fn walks_create_table_check_constraints() {
        let stmt = parse("CREATE TABLE t (a INTEGER, CHECK (a > 0));").unwrap();
        let mut v = CountExprs(0);
        walk_stmt(&mut v, &stmt[0]).unwrap();
        // a, 0, (a > 0) = 3.
        assert_eq!(v.0, 3);
    }

    #[test]
    fn walks_create_index_expression() {
        let stmt = parse("CREATE INDEX idx ON t (a, (b + c)) WHERE d > 0;").unwrap();
        let mut v = CountExprs(0);
        walk_stmt(&mut v, &stmt[0]).unwrap();
        // The parser stores each plain indexed column as both a `name` and an
        // `Expr::Column` (so plain and expression indexes can be evaluated uniformly):
        // a, b, c, (b+c), d, 0, (d>0) = 7.
        assert_eq!(v.0, 7);
    }

    #[test]
    fn walks_explain_inner_statement() {
        let stmt = parse("EXPLAIN SELECT a + b FROM t;").unwrap();
        let mut v = CountExprs(0);
        walk_stmt(&mut v, &stmt[0]).unwrap();
        // a, b, (a+b) = 3.
        assert_eq!(v.0, 3);
    }

    #[test]
    fn walks_trigger_body() {
        let stmt = parse(
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET a = 1; END;",
        )
        .unwrap();
        let mut v = CountExprs(0);
        walk_stmt(&mut v, &stmt[0]).unwrap();
        // 1 = 1.
        assert_eq!(v.0, 1);
    }

    #[test]
    fn walk_expr_directly() {
        let stmt = parse("SELECT a + b * c FROM t;").unwrap();
        let Stmt::Select(s) = &stmt[0] else { panic!("expected select") };
        let ResultColumn::Expr { expr, .. } = &s.columns[0] else { panic!() };
        let mut v = CountExprs(0);
        walk_expr(&mut v, expr).unwrap();
        // a, b, c, (b*c), (a + b*c) = 5.
        assert_eq!(v.0, 5);
    }
}
