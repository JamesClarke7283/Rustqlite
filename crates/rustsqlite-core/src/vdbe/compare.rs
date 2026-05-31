//! Value comparison and affinity coercion for the VDBE (mirrors `sqlite3MemCompare` and
//! `sqlite3IntFloatCompare` in `vdbeaux.c`, and `applyAffinity` in `vdbe.c`).
//!
//! Two pieces the comparison and sorter opcodes rely on:
//!
//! * [`mem_compare`] — the storage-class ordering `NULL < numbers < TEXT < BLOB`, with a
//!   precision-safe integer/real comparison (never blindly casting `i64`→`f64`).
//! * [`apply_affinity`] — the operand coercion SQLite applies before a typed comparison
//!   (TEXT-affinity stringifies numbers; NUMERIC-affinity parses numeric-looking text).

use std::cmp::Ordering;

use crate::types::{Affinity, Collation, Value};
use crate::util::fp::fp_to_text;

/// Compare two values using SQLite's storage-class ordering, exactly as `sqlite3MemCompare`:
///
/// 1. NULL sorts before everything; `NULL == NULL`.
/// 2. Numbers (INTEGER/REAL) sort before TEXT and BLOB, compared numerically (precision-safe).
/// 3. TEXT sorts before BLOB; two TEXT values compare under `coll`.
/// 4. Two BLOBs compare byte-wise.
///
/// This is the raw value ordering used by `ORDER BY` and by the comparison opcodes once they
/// have handled NULL operands; it never yields "unknown".
pub fn mem_compare(a: &Value, b: &Value, coll: Collation) -> Ordering {
    use Value::*;
    match (a, b) {
        (Null, Null) => Ordering::Equal,
        (Null, _) => Ordering::Less,
        (_, Null) => Ordering::Greater,

        (Int(x), Int(y)) => x.cmp(y),
        (Real(x), Real(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (Int(x), Real(y)) => int_float_compare(*x, *y),
        (Real(x), Int(y)) => int_float_compare(*y, *x).reverse(),

        // A number is always less than TEXT or BLOB.
        (Int(_) | Real(_), Text(_) | Blob(_)) => Ordering::Less,
        (Text(_) | Blob(_), Int(_) | Real(_)) => Ordering::Greater,

        (Text(x), Text(y)) => coll.compare(x, y),
        (Text(_), Blob(_)) => Ordering::Less,
        (Blob(_), Text(_)) => Ordering::Greater,
        (Blob(x), Blob(y)) => x.as_slice().cmp(y.as_slice()),
    }
}

/// Precision-safe comparison of an `i64` against an `f64` (port of `sqlite3IntFloatCompare`),
/// returning the ordering of the integer relative to the real.
fn int_float_compare(i: i64, r: f64) -> Ordering {
    if r.is_nan() {
        // SQLite treats NaN like NULL; every integer is greater than NULL.
        return Ordering::Greater;
    }
    if r < -9_223_372_036_854_775_808.0 {
        return Ordering::Greater; // i > r
    }
    if r >= 9_223_372_036_854_775_808.0 {
        return Ordering::Less; // i < r
    }
    let y = r as i64; // truncates toward zero; r is now in [-2^63, 2^63)
    match i.cmp(&y) {
        Ordering::Equal => {
            let di = i as f64;
            di.partial_cmp(&r).unwrap_or(Ordering::Equal)
        }
        other => other,
    }
}

/// Apply a column affinity to a value exactly as `applyAffinity` does before a typed
/// comparison:
///
/// * TEXT — stringify INTEGER/REAL operands (BLOB and NULL are left unchanged).
/// * INTEGER/REAL/NUMERIC — parse a numeric-looking TEXT operand into INTEGER or REAL.
/// * BLOB (a.k.a. NONE) — no coercion.
pub fn apply_affinity(v: Value, aff: Affinity) -> Value {
    match aff {
        Affinity::Text => to_text_affinity(v),
        Affinity::Integer | Affinity::Real | Affinity::Numeric => to_numeric_affinity(v),
        Affinity::Blob => v,
    }
}

fn to_text_affinity(v: Value) -> Value {
    match v {
        Value::Int(i) => Value::Text(i.to_string()),
        Value::Real(r) => Value::Text(fp_to_text(r)),
        other => other,
    }
}

fn to_numeric_affinity(v: Value) -> Value {
    match v {
        Value::Text(s) => match numeric_value_of(&s) {
            Some(n) => n,
            None => Value::Text(s),
        },
        other => other,
    }
}

/// SQLite's TEXT→numeric coercion for affinity (`applyNumericAffinity`): coerce only when the
/// **whole** string is a valid number, returning INTEGER (no fraction/exponent, fits `i64`) or
/// REAL. A non-numeric or only-partly-numeric string (e.g. `"10garbage"`, `"1e"`) yields `None`
/// and is left as text.
fn numeric_value_of(s: &str) -> Option<Value> {
    match crate::util::numeric_prefix(s) {
        (Some(v), true) => Some(v),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Collation::Binary;
    use std::cmp::Ordering::*;

    #[test]
    fn class_ordering() {
        // NULL < number < text < blob
        assert_eq!(mem_compare(&Value::Null, &Value::Int(0), Binary), Less);
        assert_eq!(
            mem_compare(&Value::Int(5), &Value::Text("a".into()), Binary),
            Less
        );
        assert_eq!(
            mem_compare(&Value::Text("a".into()), &Value::Blob(vec![0]), Binary),
            Less
        );
        assert_eq!(mem_compare(&Value::Null, &Value::Null, Binary), Equal);
    }

    #[test]
    fn numeric_cross_type() {
        assert_eq!(mem_compare(&Value::Int(2), &Value::Real(2.5), Binary), Less);
        assert_eq!(
            mem_compare(&Value::Real(2.5), &Value::Int(2), Binary),
            Greater
        );
        assert_eq!(
            mem_compare(&Value::Int(3), &Value::Real(3.0), Binary),
            Equal
        );
    }

    #[test]
    fn precision_safe_int_real() {
        // 2^53+1 is not representable as f64; the naive i64->f64 cast would lose the +1.
        let big = (1i64 << 53) + 1;
        assert_eq!(
            mem_compare(&Value::Int(big), &Value::Real((1i64 << 53) as f64), Binary),
            Greater
        );
        // i64::MAX vs a huge real beyond 2^63.
        assert_eq!(
            mem_compare(&Value::Int(i64::MAX), &Value::Real(1e30), Binary),
            Less
        );
        assert_eq!(
            mem_compare(&Value::Int(i64::MIN), &Value::Real(-1e30), Binary),
            Greater
        );
    }

    #[test]
    fn text_collation() {
        assert_eq!(
            mem_compare(
                &Value::Text("abc".into()),
                &Value::Text("abd".into()),
                Binary
            ),
            Less
        );
        assert_eq!(
            mem_compare(
                &Value::Text("ABC".into()),
                &Value::Text("abc".into()),
                Collation::NoCase
            ),
            Equal
        );
    }

    #[test]
    fn affinity_coercions() {
        // TEXT affinity stringifies numbers.
        assert_eq!(
            apply_affinity(Value::Int(5), Affinity::Text),
            Value::Text("5".into())
        );
        assert_eq!(
            apply_affinity(Value::Real(2.5), Affinity::Text),
            Value::Text("2.5".into())
        );
        // NUMERIC affinity parses numeric text.
        assert_eq!(
            apply_affinity(Value::Text("5".into()), Affinity::Numeric),
            Value::Int(5)
        );
        assert_eq!(
            apply_affinity(Value::Text("5.5".into()), Affinity::Numeric),
            Value::Real(5.5)
        );
        // Non-numeric text is left alone.
        assert_eq!(
            apply_affinity(Value::Text("abc".into()), Affinity::Numeric),
            Value::Text("abc".into())
        );
        // BLOB/NONE affinity coerces nothing.
        assert_eq!(apply_affinity(Value::Int(5), Affinity::Blob), Value::Int(5));
    }
}
