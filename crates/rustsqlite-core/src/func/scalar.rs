//! Scalar built-in functions (mirrors the scalar entries in `func.c`).
//!
//! Each function is ported from the upstream C implementation and verified against the
//! edge cases the `sqlite3` binary exhibits (negative `substr` offsets, `abs` of the smallest
//! integer, `length` counting characters for TEXT but bytes for BLOB, `round` always REAL and
//! half-away-from-zero, etc.). The differential tests pin the behavior.

use std::cmp::Ordering;

use crate::error::{Error, Result};
use crate::types::{Collation, Value};
use crate::util::fp::fp_to_fixed;
use crate::vdbe::compare::mem_compare;

/// `abs(X)` — absolute value. NULL→NULL; the smallest integer overflows; non-numeric text/blob
/// becomes 0.0 (REAL).
pub fn abs(v: &Value) -> Result<Value> {
    match v {
        Value::Null => Ok(Value::Null),
        Value::Int(i) => {
            if *i == i64::MIN {
                Err(Error::msg("integer overflow"))
            } else {
                Ok(Value::Int(i.abs()))
            }
        }
        other => Ok(Value::Real(other.as_f64().abs())),
    }
}

/// `length(X)` — characters for TEXT, bytes for BLOB, length of the text rendering for numbers.
pub fn length(v: &Value) -> Value {
    match v {
        Value::Null => Value::Null,
        Value::Text(s) => Value::Int(s.chars().count() as i64),
        Value::Blob(b) => Value::Int(b.len() as i64),
        // INTEGER / REAL: the byte length of the value's text representation.
        other => Value::Int(other.to_text().map_or(0, |t| t.len()) as i64),
    }
}

/// `lower(X)` — ASCII lower-case fold (only A–Z are affected). NULL→NULL.
pub fn lower(v: &Value) -> Value {
    fold(v, |c| c.to_ascii_lowercase())
}

/// `upper(X)` — ASCII upper-case fold. NULL→NULL.
pub fn upper(v: &Value) -> Value {
    fold(v, |c| c.to_ascii_uppercase())
}

fn fold(v: &Value, f: impl Fn(char) -> char) -> Value {
    match v.to_text() {
        Some(s) => Value::Text(s.chars().map(f).collect()),
        None => Value::Null,
    }
}

/// `typeof(X)` — the storage-class name.
pub fn typeof_(v: &Value) -> Value {
    let name = match v {
        Value::Int(_) => "integer",
        Value::Real(_) => "real",
        Value::Text(_) => "text",
        Value::Blob(_) => "blob",
        Value::Null => "null",
    };
    Value::Text(name.to_string())
}

/// `coalesce(...)` — the first non-NULL argument, or NULL.
pub fn coalesce(args: &[Value]) -> Value {
    args.iter()
        .find(|v| !v.is_null())
        .cloned()
        .unwrap_or(Value::Null)
}

/// `ifnull(X, Y)` — `X` if non-NULL, else `Y`.
pub fn ifnull(x: &Value, y: &Value) -> Value {
    if x.is_null() {
        y.clone()
    } else {
        x.clone()
    }
}

/// `nullif(X, Y)` — NULL if `X == Y` (BINARY comparison), else `X`.
pub fn nullif(x: &Value, y: &Value) -> Value {
    if mem_compare(x, y, Collation::Binary) == Ordering::Equal {
        Value::Null
    } else {
        x.clone()
    }
}

/// `iif(C, A, B)` / `if(C, A, B)` — `A` when `C` is truthy, else `B`. A NULL or zero condition is
/// falsy. SQLite implements this as the equivalent `CASE WHEN C THEN A ELSE B END`.
pub fn iif(c: &Value, a: &Value, b: &Value) -> Value {
    if truthy(c) {
        a.clone()
    } else {
        b.clone()
    }
}

/// SQLite truth value of a register: NULL is false, otherwise the numeric value is non-zero.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Int(i) => *i != 0,
        Value::Real(r) => *r != 0.0,
        // TEXT/BLOB take NUMERIC affinity for a boolean test: their numeric value, else 0.
        other => other.as_f64() != 0.0,
    }
}

/// The scalar (variadic, ≥2-arg) `min(...)` / `max(...)`. These select the minimum / maximum
/// argument under SQLite's value ordering (NULL sorts lowest). Per SQLite, if *any* argument is
/// NULL the result is NULL. `want_max` picks `max`; otherwise `min`. Comparisons use the BINARY
/// collation, matching the oracle for these scalar forms.
pub fn min_max(args: &[Value], want_max: bool) -> Value {
    if args.iter().any(Value::is_null) {
        return Value::Null;
    }
    let mut best = &args[0];
    for cand in &args[1..] {
        let ord = mem_compare(cand, best, Collation::Binary);
        let take = if want_max {
            ord == Ordering::Greater
        } else {
            ord == Ordering::Less
        };
        if take {
            best = cand;
        }
    }
    best.clone()
}

/// `zeroblob(N)` — a BLOB of `N` zero bytes. A negative or NULL `N` produces an empty BLOB
/// (SQLite clamps a negative length to 0). Mirrors `zeroblobFunc`.
pub fn zeroblob(n: &Value) -> Result<Value> {
    let len = n.as_i64();
    if len <= 0 {
        return Ok(Value::Blob(Vec::new()));
    }
    // Guard against absurd sizes the way SQLite's SQLITE_MAX_LENGTH check does.
    if len > i64::from(i32::MAX) {
        return Err(Error::msg("string or blob too big"));
    }
    Ok(Value::Blob(vec![0u8; len as usize]))
}

/// `likely(X)` / `unlikely(X)` — optimizer hints; semantically the identity (return `X`).
pub fn likely(x: &Value) -> Value {
    x.clone()
}

/// `likelihood(X, Y)` — optimizer hint; returns `X` unchanged (the probability `Y` is advisory).
pub fn likelihood(x: &Value, _y: &Value) -> Value {
    x.clone()
}

/// `round(X)` / `round(X, N)` — always REAL, half-away-from-zero. A NULL `X` or `N` yields NULL.
pub fn round(x: &Value, n: Option<&Value>) -> Value {
    let mut n_digits: i64 = 0;
    if let Some(nv) = n {
        if nv.is_null() {
            return Value::Null;
        }
        n_digits = nv.as_i64().clamp(0, 30);
    }
    if x.is_null() {
        return Value::Null;
    }
    let r = x.as_f64();
    let result = if !(-4_503_599_627_370_496.0..=4_503_599_627_370_496.0).contains(&r) {
        // Magnitude too large to have a fractional part.
        r
    } else if n_digits == 0 {
        ((r + if r < 0.0 { -0.5 } else { 0.5 }) as i64) as f64
    } else {
        // SQLite renders with `%!.*f` then re-parses — reproduce that exactly.
        fp_to_fixed(r, n_digits as i32).parse::<f64>().unwrap_or(r)
    };
    Value::Real(result)
}

/// `substr(X, P)` / `substr(X, P, L)` — 1-based, with SQLite's negative/zero-offset handling.
/// Ported from `substrFunc`.
pub fn substr(x: &Value, p: &Value, l: Option<&Value>) -> Value {
    let is_blob = matches!(x, Value::Blob(_));
    let mut p1 = p.as_i64();

    // BLOB works in bytes; everything else works in characters of the text rendering.
    let blob_bytes: Vec<u8>;
    let chars: Vec<char>;
    let len: i64;
    if is_blob {
        blob_bytes = match x {
            Value::Blob(b) => b.clone(),
            _ => unreachable!(),
        };
        chars = Vec::new();
        len = blob_bytes.len() as i64; // only consulted in the blob path / for p1<0 below
    } else {
        let text = match x.to_text() {
            Some(t) => t,
            None => return Value::Null, // X is NULL
        };
        chars = text.chars().collect();
        blob_bytes = Vec::new();
        len = chars.len() as i64;
    }

    // Second argument handling.
    let mut p2: i64;
    match l {
        Some(lv) => {
            if lv.is_null() {
                return Value::Null;
            }
            p2 = lv.as_i64();
        }
        None => p2 = i64::from(i32::MAX), // unbounded (SQLITE_LIMIT_LENGTH stand-in)
    }

    if p1 == 0 && p.is_null() {
        return Value::Null;
    }

    if p1 < 0 {
        p1 += len;
        if p1 < 0 {
            if p2 < 0 {
                p2 = 0;
            } else {
                p2 += p1;
            }
            p1 = 0;
        }
    } else if p1 > 0 {
        p1 -= 1;
    } else if p2 > 0 {
        p2 -= 1;
    }

    if p2 < 0 {
        if p2 < -p1 {
            p2 = p1;
        } else {
            p2 = -p2;
        }
        p1 -= p2;
    }
    // p1, p2 are now >= 0.
    let start = p1.max(0) as usize;
    let take = p2.max(0) as usize;

    if is_blob {
        let total = blob_bytes.len();
        let (s, t) = if start >= total {
            (0, 0)
        } else {
            (start, take.min(total - start))
        };
        Value::Blob(blob_bytes[s..s + t].to_vec())
    } else {
        let total = chars.len();
        let s = start.min(total);
        let t = take.min(total - s);
        Value::Text(chars[s..s + t].iter().collect())
    }
}
