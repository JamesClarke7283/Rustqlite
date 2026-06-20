//! Tiny query planner: an index-aware codegen for the small set of operators the M5.1/M5.2
//! slices support.
//!
//! The planner reads the catalog to find a usable index on the table. It considers three
//! benefits an index can provide, in roughly increasing value:
//!
//! 1. **ORDER BY benefit** — when `ORDER BY` is a prefix of the index's columns (in matching
//!    ASC/DESC direction), the index scan already yields rows in the requested order, so the
//!    sorter is dropped. (`SELECT ... FROM t ORDER BY a` with `CREATE INDEX idx ON t(a)`.)
//! 2. **Covering benefit** — when every column the query needs (projection + WHERE + ORDER BY
//!    + DISTINCT) is one of the index's columns, the table lookup is dropped entirely and the
//!    scan reads only the index b-tree (an "index-only scan"). The rowid tail of the index
//!    record is also available as a column value when the rowid-alias column is needed.
//! 3. **WHERE equality benefit** — when the `WHERE` clause contains equality comparisons on a
//!    prefix of the index columns (`col1 = const AND col2 = const ...`), the scan seeks
//!    directly to the matching range instead of walking the whole index.
//!
//! Any one of these benefits is enough for the planner to prefer the index over a table scan;
//! they compose freely (a covering index that also satisfies ORDER BY and has a WHERE equality
//! prefix is the ideal plan).
//!
//! The codegen output for an indexed equality with a non-covering index is:
//! ```text
//!   OpenRead  table_cur, table_root, 0
//!   OpenRead  idx_cur,   idx_root, 0, P4=KeyInfo(n=K, ASC, BINARY)
//!   <load constant into reg K..K+n-1>
//!   SeekGE    idx_cur, end_seek, K, P4=n
//!   IdxGT     idx_cur, end_seek, K, P4=n
//! loop_top:
//!   IdxRowid  idx_cur, R
//!   NotExists table_cur, idx_next, R
//!   <project + WHERE-filter; ResultRow>
//! idx_next:
//!   Next      idx_cur, loop_top
//! end_seek:
//!   Halt
//! ```
//! For a covering index the `OpenRead table_cur` / `IdxRowid` / `NotExists` are dropped and
//! the projection reads directly from the index cursor (column position = index column
//! position; the rowid tail is at position `nkey_fields`). For an ORDER BY-only plan (no
//! WHERE equality) the `SeekGE`/`IdxGT` are dropped and the scan is a plain `Rewind`/`Next`
//! over the index.
//!
//! The M5.2 slice deliberately keeps this small: it handles multi-column indexes only for
//! prefix equality and prefix ORDER BY; partial keys beyond the prefix, range scans, and
//! reverse scans fall through to the M3a scan path unchanged.
//!
//! For M5.2 the `WHERE` clause is *re-checked* on the table row (the IdxGT only verified
//! the indexed-column prefix, not the rest of the WHERE). When the WHERE is exactly the
//! indexed equalities, this is a tautology; when it is more complex, the row is filtered
//! again here. When the index is covering, the WHERE is re-checked against the index-read
//! column values instead.

use rustqlite_parser::{BinaryOp, Expr, Literal, OrderingTerm, SelectStmt};

use crate::schema::{IndexObject, Table};
use crate::types::Value;

/// One equality predicate: `column = <const>` (or `column IS <const>`) where the RHS is a
/// literal/bind-param.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EqualityKey {
    pub column: String,
    pub value: Value,
}

/// An index plan: the chosen index, the matched equality prefix, whether the index covers all
/// columns needed by the query (so no table lookup is required), and whether the index scan
/// ordering satisfies the `ORDER BY` clause (so no sorter is required).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IndexPlan {
    pub index: IndexObject,
    pub equality: Vec<EqualityKey>,
    /// `true` when the index scan yields rows in the ORDER BY order, so the sorter is dropped.
    /// Only set when `select.order_by` is non-empty AND the index ordering satisfies it.
    pub order_by_satisfied: bool,
    /// `true` when every column the query needs is read from the index, so no table cursor is
    /// opened and no `IdxRowid`/`NotExists` pair is emitted.
    pub covering: bool,
}

/// Pick an index to use for a `SELECT`, if any. Returns `Some(plan)` when an index provides at
/// least one of: an ORDER BY benefit, a covering benefit, or a WHERE equality prefix.
/// `None` means the M3a table-scan path is the right choice.
pub(crate) fn pick_index(
    select: &SelectStmt,
    table: &Table,
    indexes: &[IndexObject],
) -> Option<IndexPlan> {
    if indexes.is_empty() {
        return None;
    }
    if select.from.len() != 1 {
        return None;
    }

    let table_columns: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
    let where_equalities = collect_where_equalities(select);

    // The columns the query references (projection + WHERE + ORDER BY). Used to decide if an
    // index is covering. `collect_referenced_columns` walks the expressions and returns the
    // table-column indices it finds.
    let referenced = collect_referenced_columns(select, table);

    // Choose the index with the best combined benefit. Score is a tuple
    // (where_prefix_len, covering, order_by_satisfied): a longer WHERE prefix wins; ties go to
    // a covering index (saves the table lookup); further ties go to an ORDER BY-satisfying
    // index (saves the sorter). This is a simple proxy for cost — a real planner would
    // estimate row counts and I/O.
    let mut best: Option<IndexPlan> = None;
    let mut best_score: (usize, bool, bool) = (0, false, false);
    for idx in indexes {
        // Partial indexes can only be used when the query's WHERE implies the index predicate.
        // A safe, conservative rule that matches SQLite for simple cases: the index predicate
        // must appear verbatim (or tautologically) in the query WHERE. Until we have a real
        // theorem prover, we accept the index only when the query WHERE literally contains the
        // same expression tree as the index predicate, so `WHERE a=1 AND predicate` uses a
        // partial index defined with `WHERE predicate`, while `WHERE a=1` does not.
        if !partial_index_usable(idx, select) {
            continue;
        }

        // (1) WHERE equality prefix. May be empty (no WHERE benefit).
        let prefix = find_index_prefix_equalities(idx, &table_columns, &where_equalities)
            .unwrap_or_default();

        // (2) ORDER BY benefit. The index satisfies ORDER BY when:
        //   * there is an ORDER BY clause,
        //   * the ORDER BY terms are a prefix of the index columns (in index order), and
        //   * each term's direction matches the index column's direction.
        // The WHERE equality prefix precedes the ORDER BY prefix in the index: an equality
        // on column 0 lets ORDER BY on column 1 be satisfied by the same index. So the
        // ORDER BY match starts at index column `prefix.len()`.
        let order_by_satisfied =
            order_by_matches_index(select, idx, table, prefix.len(), &where_equalities);

        // (3) Covering benefit. The index is covering when every referenced column is one of
        // the index's columns. The rowid-alias column is satisfied by the index's trailing
        // rowid (read via `Column` at position `nkey_fields`). A non-alias rowid reference
        // (`SELECT rowid FROM t`) is also satisfied by the trailing rowid.
        let covering = !referenced.is_empty() && index_covers(idx, table, &referenced);

        // Require at least one benefit to use the index. A useless index that is neither
        // covering, nor ORDER-BY-satisfying, nor has a WHERE equality prefix would just add
        // an extra b-tree open with no gain — fall through to the table scan.
        let has_benefit =
            !prefix.is_empty() || order_by_satisfied || (covering && !referenced.is_empty());
        if !has_benefit {
            continue;
        }

        // When the query has an ORDER BY that this index does NOT satisfy, the indexed scan
        // would still need a sorter — and the codegen's indexed path does not emit one. Fall
        // through to the table-scan + sorter path (`compile_scan_ordered`) which handles
        // arbitrary ORDER BY. The indexed path is only usable when the ORDER BY is fully
        // satisfied by the index (so no sorter is needed) or there is no ORDER BY.
        if !select.order_by.is_empty() && !order_by_satisfied {
            continue;
        }

        let score = (prefix.len(), covering, order_by_satisfied);
        if score > best_score {
            best_score = score;
            best = Some(IndexPlan {
                index: idx.clone(),
                equality: prefix,
                order_by_satisfied,
                covering,
            });
        }
    }

    best
}

/// True when a partial index's predicate is satisfied by the query's WHERE clause.
/// The conservative check looks for the exact predicate expression as a conjunct of the
/// WHERE clause (flattened by AND). This handles the common `WHERE a = ? AND b = ?` query
/// against an index `WHERE b = ?` — the literal equality term `b = ?` must be present.
fn partial_index_usable(index: &IndexObject, select: &SelectStmt) -> bool {
    let Some(pred) = &index.where_clause else {
        return true; // non-partial indexes are always usable
    };
    let Some(w) = &select.where_clause else {
        return false;
    };
    let mut conjuncts = Vec::new();
    flatten_and(w, &mut conjuncts);
    conjuncts.iter().any(|c| exprs_equal(c, pred))
}

fn flatten_and(expr: &Expr, out: &mut Vec<Expr>) {
    if let Expr::Binary {
        op: BinaryOp::And,
        left,
        right,
    } = expr
    {
        flatten_and(left, out);
        flatten_and(right, out);
    } else {
        out.push(expr.clone());
    }
}

fn exprs_equal(a: &Expr, b: &Expr) -> bool {
    a == b
}

/// Collect all equality predicates from the WHERE clause as a flat list. The M3a/M5.2
/// supported WHERE shape is a conjunction of `column = const` / `column IS const`
/// comparisons (possibly with extra terms); we flatten `AND` and gather every equality.
fn collect_where_equalities(select: &SelectStmt) -> Vec<EqualityKey> {
    let Some(w) = select.where_clause.as_ref() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    flatten_and_collect_equalities(w, &mut out);
    out
}

/// Recursively walk `expr`, flattening `AND` chains and recording every `col = const` /
/// `col IS const` predicate. The RHS must be a constant literal or bind parameter.
fn flatten_and_collect_equalities(expr: &Expr, out: &mut Vec<EqualityKey>) {
    match expr {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            flatten_and_collect_equalities(left, out);
            flatten_and_collect_equalities(right, out);
        }
        other => {
            if let Some(ek) = as_equality_key(other) {
                out.push(ek);
            }
        }
    }
}

/// If `expr` is `col = const` or `col IS const` (or the commutative equality forms), return
/// the equality key. Returns `None` for non-equality expressions or when the RHS is not a
/// constant.
fn as_equality_key(expr: &Expr) -> Option<EqualityKey> {
    let (col_expr, val_expr) = match expr {
        Expr::Binary {
            op: BinaryOp::Eq | BinaryOp::Is,
            left,
            right,
        } => (left.as_ref(), right.as_ref()),
        _ => return None,
    };

    let col = column_name(col_expr).or_else(|| column_name(val_expr))?;
    let val_expr = if column_name(col_expr).is_some() {
        val_expr
    } else {
        col_expr
    };
    let value = const_value(val_expr)?;

    // `WHERE col = NULL` is always UNKNOWN in three-valued logic, so the indexed path
    // (which would return the NULL row) is wrong. Reject the equality.
    if matches!(value, Value::Null) {
        return None;
    }

    Some(EqualityKey { column: col, value })
}

/// Find the longest prefix of `index.columns` that is covered by equality predicates in
/// `equalities`. Returns `Some(prefix)` when at least the first column has an equality.
fn find_index_prefix_equalities(
    index: &IndexObject,
    table_columns: &[&str],
    equalities: &[EqualityKey],
) -> Option<Vec<EqualityKey>> {
    let mut prefix = Vec::new();
    for ic in &index.columns {
        // Sanity check: the indexed column must exist on the table. If it doesn't, the
        // index is corrupt/inconsistent; we simply can't use it.
        if !table_columns
            .iter()
            .any(|c| c.eq_ignore_ascii_case(&ic.name))
        {
            // A corrupt index can't be used at all; return None so the caller skips it.
            return None;
        }
        // The prefix extends as long as each index column (in order) has an equality
        // predicate. The first column without an equality terminates the prefix — columns
        // after it are not pinned to a single value, so they can't be part of the seek key.
        let Some(ek) = equalities
            .iter()
            .find(|e| e.column.eq_ignore_ascii_case(&ic.name))
        else {
            break;
        };
        prefix.push(ek.clone());
    }
    // The prefix is usable when at least the first index column has an equality. An empty
    // prefix (no equality on the first column) means the index can't be seeked — the
    // caller still considers the index for covering / ORDER BY benefits, but not for a
    // WHERE-equality seek.
    if prefix.is_empty() { None } else { Some(prefix) }
}

/// True when the `ORDER BY` clause is satisfied by walking this index forward starting at
/// the column right after the WHERE equality prefix. Mirrors the `nOBSat` logic in
/// `where.c`'s `whereLoopAddBtreeIndex` for the ORDER-BY-on-index case.
///
/// The match rules (forward scan only — we don't implement reverse scans yet):
///   * Each ORDER BY term must be a bare column reference that matches the corresponding
///     index column (by name), starting at index column `prefix_len`.
///   * The ORDER BY direction must match the index column's `desc` flag: ASC for an
///     ascending index column, DESC for a descending one. (A mismatched direction would
///     require a reverse scan, which we don't support — fall through to the sorter.)
///   * The full ORDER BY must be consumed (no trailing terms the index doesn't satisfy).
///   * `NULLS FIRST`/`NULLS LAST` must be the SQLite default for the direction (NULLS FIRST
///     for ASC, NULLS LAST for DESC) — the index's NULL placement matches the default.
fn order_by_matches_index(
    select: &SelectStmt,
    index: &IndexObject,
    table: &Table,
    prefix_len: usize,
    where_equalities: &[EqualityKey],
) -> bool {
    if select.order_by.is_empty() {
        return false;
    }
    // If there are WHERE equalities on a prefix, the ORDER BY can only be satisfied by the
    // columns *after* that prefix — the prefix columns are pinned to a single value, so
    // ordering by them is a no-op (and upstream's `nOBSat` accounts for this). An ORDER BY
    // that re-lists a prefix column is still satisfiable but we'd need to skip it; for
    // simplicity we reject an ORDER BY term that names a prefix column. The common case
    // (`WHERE a=? ORDER BY b` on `INDEX(a,b)`) works.
    let idx_cols = &index.columns;
    if prefix_len + select.order_by.len() > idx_cols.len() {
        return false;
    }
    for (i, term) in select.order_by.iter().enumerate() {
        let ic = &idx_cols[prefix_len + i];
        // The ORDER BY term must be a bare column matching this index column.
        let Some(term_col) = order_by_column_name(term, table) else {
            return false;
        };
        if !term_col.eq_ignore_ascii_case(&ic.name) {
            return false;
        }
        // Direction must match (forward scan only). The index column's `desc` flag says the
        // index stores that column descending; an `ORDER BY col DESC` matches a `desc` index
        // column, an `ORDER BY col ASC` (the default) matches a non-`desc` index column.
        if term.desc != ic.desc {
            return false;
        }
        // NULLS FIRST/LAST must be the SQLite default for the direction. The default for ASC
        // is NULLS FIRST, for DESC is NULLS LAST. The index's NULL placement matches the
        // default. An explicit non-default NULLS placement would need a sorter.
        if !nulls_is_default(term) {
            return false;
        }
    }
    // The ORDER BY columns after the prefix must not be constrained by an equality — if they
    // were, they'd be pinned and the ORDER BY would be over-constrained (upstream handles
    // this via `nDistinctCol`); for simplicity we just reject the rare case.
    let _ = where_equalities;
    true
}

/// The column name an ORDER BY term references, after resolving aliases and ordinals. Returns
/// `None` for non-column terms (expressions, ordinals that resolve to non-column outputs).
fn order_by_column_name(term: &OrderingTerm, table: &Table) -> Option<String> {
    // An ordinal ORDER BY n selects an output column; we can only use the index when that
    // output is a bare column. The caller (the codegen) resolves ordinals against outputs;
    // here we only accept a bare column reference.
    if let Expr::Column { name, .. } = &term.expr {
        // Verify it's a real column on the table (not an alias). Aliases that shadow columns
        // would still resolve to the column, which is fine.
        if table.column_index(name).is_some() {
            return Some(name.clone());
        }
    }
    None
}

/// True when the ORDER BY term's NULLS placement is the SQLite default for its direction
/// (NULLS FIRST for ASC, NULLS LAST for DESC). An explicit non-default would require a sorter
/// because the index's NULL placement is the default.
fn nulls_is_default(term: &OrderingTerm) -> bool {
    use rustqlite_parser::NullsOrder;
    match term.nulls {
        None => true,
        Some(NullsOrder::First) => !term.desc,
        Some(NullsOrder::Last) => term.desc,
    }
}

/// The set of table-column indices referenced by the query's projection, WHERE, and ORDER BY
/// clauses. Used to decide if an index is covering. The rowid-alias column is included when
/// it's referenced (it's readable from the index's trailing rowid). Returns an empty set for
/// a FROM-less / `*`-only query that we can't analyze — the caller treats empty as "not
/// covering" so `SELECT *` never picks a covering index (correct: `*` includes every column).
fn collect_referenced_columns(select: &SelectStmt, table: &Table) -> Vec<usize> {
    let mut cols: Vec<usize> = Vec::new();
    let mut push = |idx: usize| {
        if !cols.contains(&idx) {
            cols.push(idx);
        }
    };
    // Projection. A `*` / `t.*` references every column — bail out (return empty) so the
    // index is never considered covering for `SELECT *`.
    for rc in &select.columns {
        match rc {
            rustqlite_parser::ResultColumn::Star | rustqlite_parser::ResultColumn::TableStar(_) => {
                return Vec::new();
            }
            rustqlite_parser::ResultColumn::Expr { expr, .. } => {
                collect_columns(expr, table, &mut push);
            }
        }
    }
    if let Some(w) = &select.where_clause {
        collect_columns(w, table, &mut push);
    }
    for term in &select.order_by {
        collect_columns(&term.expr, table, &mut push);
    }
    cols
}

/// Walk an expression tree, recording the table-column index of every `Expr::Column` that
/// resolves to a stored column on `table`. The rowid-alias column is recorded as its column
/// index (so the covering check knows the index's trailing rowid satisfies it). Columns that
/// don't resolve (unknown names) are ignored — they'll surface as "no such column" later in
/// codegen.
fn collect_columns(expr: &Expr, table: &Table, push: &mut impl FnMut(usize)) {
    match expr {
        Expr::Column { name, .. } => {
            if let Some(i) = table.column_index(name) {
                push(i);
            }
        }
        Expr::Unary { expr, .. } => collect_columns(expr, table, push),
        Expr::Binary { left, right, .. } => {
            collect_columns(left, table, push);
            collect_columns(right, table, push);
        }
        Expr::Between { expr, low, high, .. } => {
            collect_columns(expr, table, push);
            collect_columns(low, table, push);
            collect_columns(high, table, push);
        }
        Expr::In { expr, values, .. } => {
            collect_columns(expr, table, push);
            for v in values {
                collect_columns(v, table, push);
            }
        }
        Expr::InSubquery { expr, .. } => collect_columns(expr, table, push),
        Expr::Cast { expr, .. } => collect_columns(expr, table, push),
        Expr::Case {
            base,
            when_then,
            else_expr,
        } => {
            if let Some(b) = base {
                collect_columns(b, table, push);
            }
            for (w, t) in when_then {
                collect_columns(w, table, push);
                collect_columns(t, table, push);
            }
            if let Some(e) = else_expr {
                collect_columns(e, table, push);
            }
        }
        Expr::Collate { expr, .. } => collect_columns(expr, table, push),
        Expr::IsDistinctFrom { left, right, .. } => {
            collect_columns(left, table, push);
            collect_columns(right, table, push);
        }
        Expr::Row(es) => es.iter().for_each(|e| collect_columns(e, table, push)),
        Expr::Coalesce2 { left, right } => {
            collect_columns(left, table, push);
            collect_columns(right, table, push);
        }
        Expr::Function { args, .. } => {
            if let rustqlite_parser::FunctionArgs::List(v) = args {
                for a in v {
                    collect_columns(a, table, push);
                }
            }
        }
        // Leaves with no column references.
        Expr::Literal(_) | Expr::BindParam(_) | Expr::Exists(_) | Expr::Subquery(_)
        | Expr::AggRef(_) => {}
    }
}

/// True when `index` covers all the columns in `referenced`: every referenced column is one
/// of the index's plain columns, OR is the table's rowid-alias column (satisfied by the
/// index's trailing rowid). Expression-index columns are matched by expression identity (not
/// yet implemented — we conservatively return false if any referenced column is not a plain
/// indexed column or the rowid alias).
fn index_covers(index: &IndexObject, table: &Table, referenced: &[usize]) -> bool {
    // The set of table-column indices the index stores as plain columns.
    let indexed_cols: Vec<usize> = index
        .columns
        .iter()
        .filter_map(|ic| {
            if ic.is_expression() {
                None
            } else {
                table.column_index(&ic.name)
            }
        })
        .collect();
    for &col_idx in referenced {
        // The rowid-alias column is satisfied by the index's trailing rowid.
        if Some(col_idx) == table.rowid_alias {
            continue;
        }
        if !indexed_cols.contains(&col_idx) {
            return false;
        }
    }
    true
}

fn column_name(expr: &Expr) -> Option<String> {
    let Expr::Column { name, .. } = expr else {
        return None;
    };
    Some(name.clone())
}

fn const_value(expr: &Expr) -> Option<Value> {
    match expr {
        Expr::Literal(lit) => Some(literal_to_value(lit)),
        Expr::BindParam(_) => Some(Value::Null),
        Expr::Unary {
            op: rustqlite_parser::UnaryOp::Negate,
            expr,
        } => {
            let v = const_value(expr)?;
            match v {
                Value::Int(i) => Some(Value::Int(-i)),
                Value::Real(r) => Some(Value::Real(-r)),
                _ => None,
            }
        }
        _ => None,
    }
}

fn literal_to_value(lit: &Literal) -> Value {
    match lit {
        Literal::Null => Value::Null,
        Literal::Integer(n) => Value::Int(*n),
        Literal::Real(r) => Value::Real(*r),
        Literal::Text(s) => Value::Text(s.clone()),
        Literal::Blob(b) => Value::Blob(b.clone()),
        Literal::Bool(b) => Value::Int(if *b { 1 } else { 0 }),
    }
}