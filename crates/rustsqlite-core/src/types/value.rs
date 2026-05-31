//! The dynamic value type — SQLite's five storage classes.
//!
//! Mirrors SQLite's `Mem`/`sqlite3_value` storage classes: NULL, INTEGER (i64), REAL (f64),
//! TEXT, and BLOB. The VDBE's register cell (`vdbe::mem::Mem`) will layer affinity flags and
//! cheaper sharing on top of this; this is the plain owned value used by the record codec,
//! the schema reader, and the C-API column accessors.

use std::fmt;

/// A SQLite value in one of the five storage classes.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    /// The SQLite storage-class code reported by `sqlite3_column_type`/`sqlite3_value_type`:
    /// `SQLITE_INTEGER`=1, `SQLITE_FLOAT`=2, `SQLITE_TEXT`=3, `SQLITE_BLOB`=4, `SQLITE_NULL`=5.
    pub fn storage_class(&self) -> i32 {
        match self {
            Value::Int(_) => 1,
            Value::Real(_) => 2,
            Value::Text(_) => 3,
            Value::Blob(_) => 4,
            Value::Null => 5,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// The TEXT representation produced by `sqlite3_column_text` / CAST-to-TEXT:
    /// INTEGER as decimal, REAL via the faithful `%!.17g` rendering, TEXT as itself, and BLOB
    /// as its raw bytes interpreted as (lossy) UTF-8. NULL has no text value (`None`).
    pub fn to_text(&self) -> Option<String> {
        match self {
            Value::Null => None,
            Value::Int(i) => Some(i.to_string()),
            Value::Real(r) => Some(crate::util::fp::fp_to_text(*r)),
            Value::Text(s) => Some(s.clone()),
            Value::Blob(b) => Some(String::from_utf8_lossy(b).into_owned()),
        }
    }

    /// Best-effort integer interpretation, as `sqlite3_value_int64`: INTEGER as itself, REAL
    /// truncated toward zero, TEXT parsed (0 if not numeric), BLOB/NULL as 0.
    pub fn as_i64(&self) -> i64 {
        match self {
            Value::Int(i) => *i,
            Value::Real(r) => *r as i64,
            Value::Text(s) => s.trim().parse().unwrap_or(0),
            _ => 0,
        }
    }

    /// Best-effort floating-point interpretation, as `sqlite3_value_double`.
    pub fn as_f64(&self) -> f64 {
        match self {
            Value::Int(i) => *i as f64,
            Value::Real(r) => *r,
            Value::Text(s) => s.trim().parse().unwrap_or(0.0),
            _ => 0.0,
        }
    }
}

impl fmt::Display for Value {
    /// A best-effort textual rendering. Note: this is NOT the shell's output formatting
    /// (quoting, NULL display, etc.) — that lives in the CLI's output modes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => Ok(()),
            Value::Int(i) => write!(f, "{i}"),
            Value::Real(r) => write!(f, "{r}"),
            Value::Text(s) => write!(f, "{s}"),
            Value::Blob(b) => {
                for byte in b {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
        }
    }
}
