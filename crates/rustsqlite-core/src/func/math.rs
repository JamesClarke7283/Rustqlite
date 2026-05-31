//! Math built-in functions (mirrors the `SQLITE_ENABLE_MATH_FUNCTIONS` block in `func.c`).
//!
//! These mirror the system `sqlite3` built with math functions enabled (the oracle for this
//! crate). The faithfulness-critical behaviors, all verified against that oracle:
//!
//! * `log(X)` is **base-10** (NOT natural), `ln(X)` is the natural log, `log(B, X)` is base-`B`,
//!   and `log2`/`log10` are explicit. SQLite's `logFunc` shares one C function for all of these,
//!   with explicit domain guards (`X<=0`, `B<=1` → result is NULL, computed *before* the call).
//! * `NaN` results become **NULL** (SQLite stores a NaN double as NULL), but `±Inf` is kept and
//!   renders as `Inf`/`-Inf` (e.g. `exp(1000)`, `pow(10,1000)`, `atanh(1)`). The
//!   `math1Func`/`math2Func` wrappers in `func.c` do NO domain checking — they hand back whatever
//!   C produced — so `sqrt(-1)`→NULL (NaN), `acos(2)`→NULL (NaN), but `atanh(1)`→`Inf`.
//! * A NULL argument yields NULL.
//! * Almost everything returns REAL. `ceil`/`floor`/`trunc` preserve an INTEGER argument as
//!   INTEGER but return REAL for a REAL argument; `sign` returns INTEGER.

use crate::types::Value;

/// Wrap an `f64` result the way SQLite stores a math-function result: `NaN` becomes NULL (SQLite
/// converts NaN doubles to NULL), while finite values and `±Inf` are kept as REAL.
fn real_or_null(r: f64) -> Value {
    if r.is_nan() {
        Value::Null
    } else {
        Value::Real(r)
    }
}

/// Apply a unary `f64 -> f64` to `X`; a NaN result becomes NULL, `±Inf` is kept. Mirrors
/// `math1Func`, which gates on `sqlite3_value_numeric_type`: a non-numeric argument (NULL, a
/// non-fully-numeric string, or any BLOB) yields NULL *before* the function runs.
fn math1(x: &Value, f: impl Fn(f64) -> f64) -> Value {
    if !x.is_numeric() {
        return Value::Null;
    }
    real_or_null(f(x.as_f64()))
}

/// Apply a binary `f64 -> f64` to `(X, Y)`; a NaN result becomes NULL, `±Inf` is kept. Mirrors
/// `math2Func`, which gates *each* argument on `sqlite3_value_numeric_type` (any non-numeric
/// argument → NULL).
fn math2(x: &Value, y: &Value, f: impl Fn(f64, f64) -> f64) -> Value {
    if !x.is_numeric() || !y.is_numeric() {
        return Value::Null;
    }
    real_or_null(f(x.as_f64(), y.as_f64()))
}

// ---- roots, exponentials, logs ----

/// `sqrt(X)` — square root; `sqrt(-1)` → NULL.
pub fn sqrt(x: &Value) -> Value {
    math1(x, f64::sqrt)
}

/// `exp(X)` — e^X.
pub fn exp(x: &Value) -> Value {
    math1(x, f64::exp)
}

/// `ln(X)` — natural logarithm. SQLite's `logFunc` guards `X<=0` → NULL *before* computing, so
/// `ln(0)`/`ln(-1)` are NULL (not `-Inf`/NaN). NULL arg → NULL.
pub fn ln(x: &Value) -> Value {
    log_one(x, f64::ln)
}

/// `log10(X)` and the one-argument `log(X)` — base-10 logarithm. NOTE: in SQLite `log(X)` is
/// base-10, not natural. `X<=0` → NULL.
pub fn log10(x: &Value) -> Value {
    log_one(x, f64::log10)
}

/// `log2(X)` — base-2 logarithm. `X<=0` → NULL.
pub fn log2(x: &Value) -> Value {
    log_one(x, f64::log2)
}

/// Shared one-argument log shape: `logFunc` gates the argument on `sqlite3_value_numeric_type`
/// (non-numeric → NULL), then applies SQLite's `if(x<=0.0) return;` domain guard (so `0`/negatives
/// are NULL, never `-Inf`/NaN), otherwise the computed REAL.
fn log_one(x: &Value, f: impl Fn(f64) -> f64) -> Value {
    if !x.is_numeric() {
        return Value::Null;
    }
    let xx = x.as_f64();
    if xx > 0.0 {
        Value::Real(f(xx))
    } else {
        Value::Null
    }
}

/// Two-argument `log(B, X)` — logarithm of `X` to base `B`. SQLite's `logFunc` computes this as
/// `log(X)/log(B)` using the **natural** log (C `log`), and bails to NULL when `X<=0`, `B<=0`, or
/// `log(B)<=0` (i.e. `B<=1`). Using `ln` (not `log10`) is required to reproduce the exact
/// floating-point rounding, e.g. `log(10,1000)` → `2.9999999999999996`, not `3.0`.
///
/// Coercion asymmetry, faithful to `logFunc`: the **base** `B` is gated on
/// `sqlite3_value_numeric_type` (a non-numeric base → NULL), but the **argument** `X` is read with
/// `sqlite3_value_double` — its *leading numeric prefix* — so `log(10, '10x')` is `1.0` and
/// `log(10, x'31303030')` (the blob `"1000"`) is `~3`, while `log('2x', 8)` is NULL.
pub fn log_base(b: &Value, x: &Value) -> Value {
    if !b.is_numeric() {
        return Value::Null;
    }
    let bb = b.as_f64();
    if bb <= 0.0 {
        return Value::Null; // first `if(x<=0.0) return;` (x == base here)
    }
    let ln_b = bb.ln();
    if ln_b <= 0.0 {
        return Value::Null; // `b = log(x_base); if( b<=0.0 ) return;` → base <= 1
    }
    let xx = x.as_f64(); // value_double(argv[1]) — leading prefix, no numeric_type gate
    if xx <= 0.0 {
        return Value::Null;
    }
    real_or_null(xx.ln() / ln_b)
}

// ---- rounding family ----

/// `ceil(X)`/`ceiling(X)` — round toward +∞. INTEGER in → INTEGER out; REAL in → REAL out.
pub fn ceil(x: &Value) -> Value {
    round_like(x, f64::ceil)
}

/// `floor(X)` — round toward -∞. INTEGER in → INTEGER out; REAL in → REAL out.
pub fn floor(x: &Value) -> Value {
    round_like(x, f64::floor)
}

/// `trunc(X)` — round toward zero. INTEGER in → INTEGER out; REAL in → REAL out.
pub fn trunc(x: &Value) -> Value {
    round_like(x, f64::trunc)
}

/// Shared shape for ceil/floor/trunc (`ceilingFunc`): gate on `sqlite3_value_numeric_type` — an
/// INTEGER (including whole-numeric text such as `'5'`) passes through as INTEGER, a FLOAT (or
/// fully-numeric float text) is rounded and returned as REAL, and anything non-numeric (a
/// non-fully-numeric string, any BLOB, or NULL) yields NULL.
fn round_like(x: &Value, f: impl Fn(f64) -> f64) -> Value {
    match x.numeric_type() {
        1 => Value::Int(x.as_i64()),
        2 => Value::Real(f(x.as_f64())),
        _ => Value::Null,
    }
}

// ---- powers / modulo / sign ----

/// `pow(X, Y)`/`power(X, Y)` — X^Y. A non-finite result (e.g. `pow(-1, 0.5)`) → NULL.
pub fn pow(x: &Value, y: &Value) -> Value {
    math2(x, y, f64::powf)
}

/// `mod(X, Y)` — floating-point remainder (C `fmod`). A `math2Func`: each argument is gated on
/// `sqlite3_value_numeric_type` (non-numeric → NULL), and `mod(X, 0)` → NULL.
pub fn mod_(x: &Value, y: &Value) -> Value {
    if !x.is_numeric() || !y.is_numeric() {
        return Value::Null;
    }
    let yy = y.as_f64();
    if yy == 0.0 {
        return Value::Null;
    }
    real_or_null(x.as_f64() % yy)
}

/// `sign(X)` — `-1`, `0`, or `1` as INTEGER. Mirrors `signFunc`, which gates on
/// `sqlite3_value_numeric_type`: INTEGER/FLOAT (including fully-numeric text such as `'5'` or
/// `'-2.5'`) yield the sign, while NULL, a non-fully-numeric string, or any BLOB yield NULL.
pub fn sign(x: &Value) -> Value {
    if !x.is_numeric() {
        return Value::Null;
    }
    let r = x.as_f64();
    if r > 0.0 {
        Value::Int(1)
    } else if r < 0.0 {
        Value::Int(-1)
    } else {
        // 0.0 and -0.0 → 0; NaN can't occur here for a value that passed the numeric gate.
        Value::Int(0)
    }
}

/// `pi()` — the constant π.
pub fn pi() -> Value {
    Value::Real(std::f64::consts::PI)
}

// ---- trigonometry ----

pub fn sin(x: &Value) -> Value {
    math1(x, f64::sin)
}
pub fn cos(x: &Value) -> Value {
    math1(x, f64::cos)
}
pub fn tan(x: &Value) -> Value {
    math1(x, f64::tan)
}
pub fn asin(x: &Value) -> Value {
    math1(x, f64::asin)
}
pub fn acos(x: &Value) -> Value {
    math1(x, f64::acos)
}
pub fn atan(x: &Value) -> Value {
    math1(x, f64::atan)
}
/// `atan2(Y, X)` — angle of the point `(X, Y)`.
pub fn atan2(y: &Value, x: &Value) -> Value {
    math2(y, x, f64::atan2)
}
pub fn sinh(x: &Value) -> Value {
    math1(x, f64::sinh)
}
pub fn cosh(x: &Value) -> Value {
    math1(x, f64::cosh)
}
pub fn tanh(x: &Value) -> Value {
    math1(x, f64::tanh)
}
pub fn asinh(x: &Value) -> Value {
    math1(x, f64::asinh)
}
/// `acosh(X)` — for `X<1` the result is NaN → NULL (no explicit guard; `math1Func` semantics).
pub fn acosh(x: &Value) -> Value {
    math1(x, f64::acosh)
}
/// `atanh(X)` — `atanh(±1)` is `±Inf` (kept; renders as `Inf`/`-Inf`); `|X|>1` is NaN → NULL.
pub fn atanh(x: &Value) -> Value {
    math1(x, f64::atanh)
}

/// `radians(X)` — degrees → radians.
pub fn radians(x: &Value) -> Value {
    math1(x, f64::to_radians)
}
/// `degrees(X)` — radians → degrees.
pub fn degrees(x: &Value) -> Value {
    math1(x, f64::to_degrees)
}
