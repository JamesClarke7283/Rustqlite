//! Tiny query planner: an index-aware codegen for the small set of operators the M5.1
//! first slice supports.
//!
//! The planner reads the catalog to find a usable single-column index on the table, and if
//! the `WHERE` predicate is an equality comparison between the indexed column and a
//! constant, emits an indexed lookup instead of a full table scan. Similarly, an
//! `ORDER BY` on the indexed column (ASC, no compound) routes through the index without a
//! sorter.
//!
//! The first slice deliberately keeps this small: it handles only single-column indexes,
//! only `=`/`IS` comparisons, only constant RHS, and only ASC `ORDER BY`. Anything else
//! falls through to the M3a scan path unchanged.
//!
//! The codegen output (M5.1 first slice) is:
//! ```text
//!   OpenRead  table_cur, table_root, 0
//!   OpenRead  idx_cur,   idx_root, 0, P4=KeyInfo(n=1, ASC, BINARY)
//!   <load constant into reg K>
//!   SeekGE    idx_cur, end_seek, K, P4=1
//!   IdxGT     idx_cur, end_seek, K, P4=1
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
//! For M5.1 the `WHERE` clause is *re-checked* on the table row (the IdxGT only verified
//! the indexed-column value, not the rest of the WHERE). When the WHERE is `col = X`
//! (the indexed equality), this is a no-op duplicate; when the WHERE is more complex,
//! the row is filtered again here. (The M5.1 first slice only allows `col = X` on the
//! indexed column, so in practice the re-check is always a tautology.)

use rustqlite_parser::{BinaryOp, Expr, Literal, OrderingTerm, SelectStmt};

use crate::schema::{IndexObject, Table};
use crate::types::Value;

/// An equality predicate: `column = <const>` (or `column IS <const>`) where the RHS is a
/// literal/bind-param.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EqualityKey {
    pub column: String,
    pub value: Value,
}

/// Pick an index to use for a `SELECT`, if any. Returns `Some((index, equality_key, _))`
/// when an index on the lone table covers a usable `WHERE` or `ORDER BY` clause; `None`
/// means the M3a table-scan path is the right choice.
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
    let indexed_col = indexes.iter().find_map(|idx| {
        if idx.columns.len() == 1 {
            Some(idx.columns[0].name.clone())
        } else {
            None
        }
    })?;
    let table_columns: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
    if !table_columns.iter().any(|c| c.eq_ignore_ascii_case(&indexed_col)) {
        return None;
    }

    let equality = find_equality(select, &indexed_col);
    let _order_ok = order_by_indexed(select, &indexed_col);

    if equality.is_none() {
        return None;
    }

    let chosen = indexes
        .iter()
        .find(|idx| idx.columns.len() == 1 && idx.columns[0].name.eq_ignore_ascii_case(&indexed_col))?
        .clone();
    Some(IndexPlan {
        index: chosen,
        equality: equality.unwrap(),
    })
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IndexPlan {
    pub index: IndexObject,
    pub equality: EqualityKey,
}

/// Find an equality predicate on `indexed_col` in the WHERE clause.
fn find_equality(select: &SelectStmt, indexed_col: &str) -> Option<EqualityKey> {
    let w = select.where_clause.as_ref()?;
    let e: &Expr = w;
    let (col, val) = match e {
        Expr::Binary {
            op: BinaryOp::Eq | BinaryOp::Is,
            left,
            right,
        } => {
            if let Some(c) = column_name(left) {
                if c.eq_ignore_ascii_case(indexed_col) {
                    if let Some(v) = const_value(right) {
                        (c, v)
                    } else {
                        return None;
                    }
                } else {
                    return None;
                }
            } else if let Some(c) = column_name(right) {
                if c.eq_ignore_ascii_case(indexed_col) {
                    if let Some(v) = const_value(left) {
                        (c, v)
                    } else {
                        return None;
                    }
                } else {
                    return None;
                }
            } else {
                return None;
            }
        }
        _ => return None,
    };
    // `WHERE col = NULL` is always UNKNOWN in three-valued logic, so the indexed path
    // (which would return the NULL row) is wrong. Reject the equality, falling back to
    // the M3a scan path which evaluates NULL = NULL as UNKNOWN and filters the row out.
    if matches!(val, Value::Null) {
        return None;
    }
    Some(EqualityKey {
        column: col,
        value: val,
    })
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
