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

    /// Integer interpretation, faithful to `sqlite3_value_int64`: INTEGER as itself, REAL
    /// truncated toward zero, TEXT/BLOB parsed via SQLite's **leading numeric prefix** (a BLOB is
    /// interpreted as its bytes-as-text first), and `0` when there is no numeric prefix or for
    /// NULL. (`'10x'` → 10, `'abc'` → 0, `x'2d35'` = `"-5"` → -5.)
    pub fn as_i64(&self) -> i64 {
        match self {
            Value::Int(i) => *i,
            Value::Real(r) => *r as i64,
            Value::Text(s) => prefix_i64(s),
            Value::Blob(b) => prefix_i64(&String::from_utf8_lossy(b)),
            Value::Null => 0,
        }
    }

    /// Floating-point interpretation, faithful to `sqlite3_value_double`: INTEGER/REAL directly,
    /// TEXT/BLOB via SQLite's **leading numeric prefix** (`sqlite3AtoF`-style, a BLOB read as its
    /// bytes-as-text), and `0.0` when there is no numeric prefix or for NULL. This is the
    /// coercion the math functions and boolean truthiness use; it does NOT require the *whole*
    /// string to be numeric (`'3.5x'` → 3.5, `'abc'` → 0.0).
    pub fn as_f64(&self) -> f64 {
        match self {
            Value::Int(i) => *i as f64,
            Value::Real(r) => *r,
            Value::Text(s) => prefix_f64(s),
            Value::Blob(b) => prefix_f64(&String::from_utf8_lossy(b)),
            Value::Null => 0.0,
        }
    }

    /// The storage-class code from `sqlite3_value_numeric_type`: like [`storage_class`], but a
    /// **TEXT** value whose *entire* content is a valid number reports `INTEGER`(1)/`FLOAT`(2)
    /// instead of `TEXT`(3). A BLOB is never promoted (it stays `BLOB`=4), matching upstream
    /// (`sqlite3_value_numeric_type` only applies numeric affinity to TEXT). The math functions
    /// (`math1`/`math2`/`ceil`/`floor`/`trunc`/`sign`/`log`) use this to gate a non-numeric
    /// argument to NULL — distinct from [`as_f64`], which parses a leading prefix.
    ///
    /// [`storage_class`]: Value::storage_class
    /// [`as_f64`]: Value::as_f64
    pub fn numeric_type(&self) -> i32 {
        match self {
            Value::Int(_) => 1,
            Value::Real(_) => 2,
            Value::Blob(_) => 4,
            Value::Null => 5,
            Value::Text(s) => match crate::util::numeric_prefix(s) {
                (Some(Value::Int(_)), true) => 1,
                (Some(Value::Real(_)), true) => 2,
                _ => 3, // not fully numeric → stays TEXT
            },
        }
    }

    /// True when [`numeric_type`](Value::numeric_type) is INTEGER or FLOAT — the gate the math
    /// functions apply (a non-numeric argument yields NULL).
    pub fn is_numeric(&self) -> bool {
        matches!(self.numeric_type(), 1 | 2)
    }
}

/// `f64` value of the leading numeric prefix of `s` (`sqlite3_value_double` text path): the
/// prefix value, or `0.0` if there is none.
fn prefix_f64(s: &str) -> f64 {
    match crate::util::numeric_prefix(s).0 {
        Some(Value::Int(i)) => i as f64,
        Some(Value::Real(r)) => r,
        _ => 0.0,
    }
}

/// `i64` value of the leading numeric prefix of `s` (`sqlite3_value_int64` text path): an integer
/// prefix as itself, a real-valued prefix truncated toward zero, else `0`.
fn prefix_i64(s: &str) -> i64 {
    match crate::util::numeric_prefix(s).0 {
        Some(Value::Int(i)) => i,
        Some(Value::Real(r)) => r as i64,
        _ => 0,
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
