//! `sqlite_schema` bootstrap (mirrors the hard-coded schema + schema-row writer in `build.c`).
//!
//! `sqlite_schema` is itself a table; its own definition is implicit (the b-tree at page 1). When a
//! `CREATE TABLE` runs, SQLite appends one row describing the new table to `sqlite_schema`. That
//! row has the five fixed columns `(type, name, tbl_name, rootpage, sql)` — see
//! [`SchemaObject`](super::catalog::SchemaObject) for the read side. This module builds the value
//! tuple for such a row so the code generator / executor can `encode_record` it and insert it into
//! page 1. M5.1 adds [`index_schema_row`] for the analogous `CREATE INDEX` writer.

use crate::types::Value;

/// Build the five-value `sqlite_schema` row for a `CREATE TABLE`:
/// `('table', name, name, rootpage, sql)`.
///
/// The `tbl_name` column equals `name` for an ordinary table (it differs only for objects attached
/// to another table, e.g. an index, which this slice does not create yet).
///
/// **Verbatim SQL rule:** `sql` stores the user's ORIGINAL `CREATE TABLE` text exactly as typed —
/// SQLite does not canonicalize or reformat it (`sqlite3EndTable` saves the source span). The
/// caller is therefore responsible for passing the exact source substring of the statement, so a
/// rustsqlite-written database round-trips byte-for-byte with what the C `sqlite3` shell stores.
pub fn table_schema_row(name: &str, rootpage: i64, sql: &str) -> Vec<Value> {
    vec![
        Value::Text("table".to_string()),
        Value::Text(name.to_string()),
        Value::Text(name.to_string()),
        Value::Int(rootpage),
        Value::Text(sql.to_string()),
    ]
}

/// Build the five-value `sqlite_schema` row for a `CREATE INDEX`:
/// `('index', name, tbl_name, rootpage, sql)`. The `name` (the index's own name) and `tbl_name`
/// (the underlying table) differ here; the rowid-tuple layout is the same as the table version.
pub fn index_schema_row(name: &str, tbl_name: &str, rootpage: i64, sql: &str) -> Vec<Value> {
    vec![
        Value::Text("index".to_string()),
        Value::Text(name.to_string()),
        Value::Text(tbl_name.to_string()),
        Value::Int(rootpage),
        Value::Text(sql.to_string()),
    ]
}
