//! Value accessors for the C API (mirrors `vdbeapi.c`'s `sqlite3_value_*` /
//! `sqlite3_column_*`).
//!
//! The storage type is [`crate::types::Value`]; this module provides the C-API-shaped
//! accessors over it (`*_type`, `*_int64`, `*_double`, `*_text`, `*_blob`). SQLite applies
//! type conversions in these accessors (e.g. `column_int` on a TEXT value parses it); those
//! conversions are added as the query path matures.

use crate::types::Value;

/// `SQLITE_INTEGER` storage-class code.
pub const SQLITE_INTEGER: i32 = 1;
/// `SQLITE_FLOAT` storage-class code.
pub const SQLITE_FLOAT: i32 = 2;
/// `SQLITE_TEXT` storage-class code.
pub const SQLITE_TEXT: i32 = 3;
/// `SQLITE_BLOB` storage-class code.
pub const SQLITE_BLOB: i32 = 4;
/// `SQLITE_NULL` storage-class code.
pub const SQLITE_NULL: i32 = 5;

/// `sqlite3_value_type()` — the storage class of a value.
pub fn value_type(v: &Value) -> i32 {
    v.storage_class()
}

/// `sqlite3_value_int64()` — best-effort integer view of a value.
pub fn value_int64(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        Value::Real(r) => *r as i64,
        Value::Text(s) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// `sqlite3_value_double()` — best-effort floating-point view of a value.
pub fn value_double(v: &Value) -> f64 {
    match v {
        Value::Int(i) => *i as f64,
        Value::Real(r) => *r,
        Value::Text(s) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// `sqlite3_value_text()` — the text of a value, if it is TEXT.
pub fn value_text(v: &Value) -> Option<&str> {
    match v {
        Value::Text(s) => Some(s),
        _ => None,
    }
}

/// `sqlite3_value_blob()` — the bytes of a value, if it is a BLOB.
pub fn value_blob(v: &Value) -> Option<&[u8]> {
    match v {
        Value::Blob(b) => Some(b),
        _ => None,
    }
}
