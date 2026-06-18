//! Tiny query planner: an index-aware codegen for the small set of operators the M5.1/M5.2
//! slices support.
//!
//! The planner reads the catalog to find a usable index on the table. If the `WHERE`
//! predicate contains equality comparisons on a prefix of the index columns
//! (`col1 = const AND col2 = const ...`), it emits an indexed lookup instead of a full
//! table scan. Similarly, an `ORDER BY` on the first indexed column (ASC, no compound)
//! routes through the index without a sorter.
//!
//! The M5.2 slice deliberately keeps this small: it handles multi-column indexes only for
//! prefix equality; partial keys, range scans, and multi-column `ORDER BY` fall through to
//! the M3a scan path unchanged.
//!
//! The codegen output for an indexed equality is:
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
//!
//! For M5.2 the `WHERE` clause is *re-checked* on the table row (the IdxGT only verified
//! the indexed-column prefix, not the rest of the WHERE). When the WHERE is exactly the
//! indexed equalities, this is a tautology; when it is more complex, the row is filtered
//! again here.

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

/// An index plan: the chosen index plus the matched equality prefix (one entry per indexed
/// column that has an equality predicate, in index order).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IndexPlan {
    pub index: IndexObject,
    pub equality: Vec<EqualityKey>,
}

/// Pick an index to use for a `SELECT`, if any. Returns `Some(plan)` when an index on the
/// lone table covers a usable `WHERE` equality prefix; `None` means the M3a table-scan path
/// is the right choice.
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

    // Choose the index with the longest usable equality prefix. For now we simply take the
    // first index that yields the longest prefix; later slices can add cost estimation.
    let mut best: Option<IndexPlan> = None;
    let mut best_len = 0usize;
    for idx in indexes {
        let prefix = match find_index_prefix_equalities(idx, &table_columns, &where_equalities) {
            Some(p) if !p.is_empty() => p,
            _ => continue,
        };
        if prefix.len() > best_len {
            best_len = prefix.len();
            best = Some(IndexPlan {
                index: idx.clone(),
                equality: prefix,
            });
        }
    }

    // If we have a usable prefix but no ORDER BY benefit, we still use the index when the
    // prefix has at least one equality. (M5.1 already did this for single-column indexes.)
    best
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
            return None;
        }
        let ek = equalities
            .iter()
            .find(|e| e.column.eq_ignore_ascii_case(&ic.name))?;
        prefix.push(ek.clone());
    }
    Some(prefix)
}

fn order_by_indexed(select: &SelectStmt, indexed_col: &str) -> bool {
    if select.order_by.len() != 1 {
        return false;
    }
    let term: &OrderingTerm = &select.order_by[0];
    if term.desc {
        return false;
    }
    let Expr::Column { name, .. } = &term.expr else {
        return false;
    };
    name.eq_ignore_ascii_case(indexed_col)
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
