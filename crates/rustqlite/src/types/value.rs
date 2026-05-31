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
