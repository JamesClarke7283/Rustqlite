//! Shared utilities (mirrors `util.c`, `hash.c`, `utf.c`, `global.c`).
//!
//! Small cross-cutting helpers accrete here rather than being invented up front. Currently:
//! [`fp`], the faithful floating-point → text rendering ported from `util.c`/`printf.c`, and
//! [`numeric_prefix`], SQLite's text→number recognizer.

pub mod fp;

pub use fp::{fp_to_fixed, fp_to_text};

use crate::types::Value;

/// Parse the leading numeric prefix of `s` the way SQLite's `sqlite3AtoF`/`sqlite3Atoi64` do,
/// returning the value and whether the **entire** string (ignoring surrounding ASCII
/// whitespace) was the number.
///
/// A sign, integer digits, an optional fraction, and an optional *complete* exponent
/// (`e`/`E` followed by digits) are consumed. The value is INTEGER when it has no fraction and
/// no exponent and fits an `i64`, otherwise REAL. `(None, false)` is returned when there is no
/// valid numeric prefix at all (e.g. `"abc"`, `""`, `"+"`, `"."`).
///
/// The two callers differ in how they use the `full` flag:
/// * arithmetic coercion uses the prefix value regardless (`"10x"` → 10, `"abc"` → treat as 0);
/// * NUMERIC-affinity coercion only converts when `full` is true (`"10x"` stays TEXT).
pub fn numeric_prefix(s: &str) -> (Option<Value>, bool) {
    let b = s.as_bytes();
    let n = b.len();
    let mut i = 0;
    while i < n && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let tok_start = i;
    if i < n && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let int_start = i;
    while i < n && b[i].is_ascii_digit() {
        i += 1;
    }
    let has_int = i > int_start;

    let mut has_dot = false;
    let mut has_frac = false;
    if i < n && b[i] == b'.' {
        has_dot = true;
        i += 1;
        let frac_start = i;
        while i < n && b[i].is_ascii_digit() {
            i += 1;
        }
        has_frac = i > frac_start;
    }
    if !has_int && !has_frac {
        return (None, false);
    }

    // Optional exponent, consumed only if it has digits.
    let mut has_exp = false;
    if i < n && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < n && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        let exp_start = j;
        while j < n && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > exp_start {
            has_exp = true;
            i = j;
        }
    }

    let tok = &s[tok_start..i];
    let value = if !has_dot && !has_exp {
        match tok.parse::<i64>() {
            Ok(v) => Value::Int(v),
            Err(_) => Value::Real(tok.parse::<f64>().unwrap_or(0.0)), // i64 overflow → real
        }
    } else {
        Value::Real(tok.parse::<f64>().unwrap_or(0.0))
    };

    // Whole-string check: only trailing whitespace may remain.
    let mut j = i;
    while j < n && b[j].is_ascii_whitespace() {
        j += 1;
    }
    (Some(value), j == n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_and_full_parsing() {
        use Value::*;
        assert_eq!(numeric_prefix("10garbage"), (Some(Int(10)), false));
        assert_eq!(numeric_prefix("1e"), (Some(Int(1)), false)); // incomplete exponent → "1"
        assert_eq!(numeric_prefix("1.5e"), (Some(Real(1.5)), false));
        assert_eq!(numeric_prefix("5e2"), (Some(Real(500.0)), true));
        assert_eq!(numeric_prefix("5."), (Some(Real(5.0)), true));
        assert_eq!(numeric_prefix(".5"), (Some(Real(0.5)), true));
        assert_eq!(numeric_prefix("  +5  "), (Some(Int(5)), true));
        assert_eq!(numeric_prefix("1.2.3"), (Some(Real(1.2)), false));
        assert_eq!(numeric_prefix("0x10"), (Some(Int(0)), false)); // hex not recognized
        assert_eq!(numeric_prefix("abc"), (None, false));
        assert_eq!(numeric_prefix(""), (None, false));
        assert_eq!(numeric_prefix("+"), (None, false));
        assert_eq!(numeric_prefix("."), (None, false));
        assert_eq!(numeric_prefix("10"), (Some(Int(10)), true));
    }
}
