//! Built-in function registry (mirrors `sqlite3FindFunction` / the builtin tables in `func.c`).
//!
//! Maps a `(name, arg-count)` pair, case-insensitively, to a scalar implementation. M3a ships
//! the common scalar starter set; the full table and the aggregates arrive later.

use crate::error::{Error, Result};
use crate::types::Value;

use super::like;
use super::math;
use super::scalar;
use super::string;
use super::string::TrimSide;
use super::json;

/// Call a scalar function by name (case-insensitive) over already-evaluated arguments.
pub fn call_scalar(name: &str, args: &[Value]) -> Result<Value> {
    let lname = name.to_ascii_lowercase();
    match (lname.as_str(), args.len()) {
        // ---- core scalars (M3a) ----
        ("abs", 1) => scalar::abs(&args[0]),
        ("length", 1) => Ok(scalar::length(&args[0])),
        ("lower", 1) => Ok(scalar::lower(&args[0])),
        ("upper", 1) => Ok(scalar::upper(&args[0])),
        ("typeof", 1) => Ok(scalar::typeof_(&args[0])),
        ("substr" | "substring", 2) => Ok(scalar::substr(&args[0], &args[1], None)),
        ("substr" | "substring", 3) => Ok(scalar::substr(&args[0], &args[1], Some(&args[2]))),
        ("round", 1) => Ok(scalar::round(&args[0], None)),
        ("round", 2) => Ok(scalar::round(&args[0], Some(&args[1]))),
        ("coalesce", n) if n >= 2 => Ok(scalar::coalesce(args)),
        ("ifnull", 2) => Ok(scalar::ifnull(&args[0], &args[1])),
        ("nullif", 2) => Ok(scalar::nullif(&args[0], &args[1])),

        // ---- misc scalars (M3b) ----
        // 2-arg `iif(X, Y)` is shorthand for `iif(X, Y, NULL)` (upstream accepts both arities).
        ("iif" | "if", 2) => Ok(scalar::iif(&args[0], &args[1], &Value::Null)),
        ("iif" | "if", 3) => Ok(scalar::iif(&args[0], &args[1], &args[2])),
        ("min", n) if n >= 2 => Ok(scalar::min_max(args, false)),
        ("max", n) if n >= 2 => Ok(scalar::min_max(args, true)),
        ("zeroblob", 1) => scalar::zeroblob(&args[0]),
        ("likely" | "unlikely", 1) => Ok(scalar::likely(&args[0])),
        ("likelihood", 2) => Ok(scalar::likelihood(&args[0], &args[1])),

        // ---- string functions (M3b) ----
        ("instr", 2) => Ok(string::instr(&args[0], &args[1])),
        ("replace", 3) => Ok(string::replace(&args[0], &args[1], &args[2])),
        ("trim", 1) => Ok(string::trim(TrimSide::Both, &args[0], None)),
        ("trim", 2) => Ok(string::trim(TrimSide::Both, &args[0], Some(&args[1]))),
        ("ltrim", 1) => Ok(string::trim(TrimSide::Left, &args[0], None)),
        ("ltrim", 2) => Ok(string::trim(TrimSide::Left, &args[0], Some(&args[1]))),
        ("rtrim", 1) => Ok(string::trim(TrimSide::Right, &args[0], None)),
        ("rtrim", 2) => Ok(string::trim(TrimSide::Right, &args[0], Some(&args[1]))),
        ("char", _) => Ok(string::char_(args)),
        ("unicode", 1) => Ok(string::unicode(&args[0])),
        ("hex", 1) => Ok(string::hex(&args[0])),
        ("unhex", 1) => Ok(string::unhex(&args[0], None)),
        ("unhex", 2) => Ok(string::unhex(&args[0], Some(&args[1]))),
        ("concat", n) if n >= 1 => Ok(string::concat(args)),
        ("concat_ws", n) if n >= 2 => Ok(string::concat_ws(args)),
        ("quote", 1) => Ok(string::quote(&args[0])),
        ("octet_length", 1) => Ok(string::octet_length(&args[0])),
        ("like", 2) => Ok(like::like(&args[0], &args[1], None)?),
        ("like", 3) => Ok(like::like(&args[0], &args[1], Some(&args[2]))?),
        ("glob", 2) => Ok(like::glob(&args[0], &args[1])),

        // ---- math functions (M3b) ----
        ("sqrt", 1) => Ok(math::sqrt(&args[0])),
        ("exp", 1) => Ok(math::exp(&args[0])),
        ("ln", 1) => Ok(math::ln(&args[0])),
        ("log" | "log10", 1) => Ok(math::log10(&args[0])),
        ("log", 2) => Ok(math::log_base(&args[0], &args[1])),
        ("log2", 1) => Ok(math::log2(&args[0])),
        ("ceil" | "ceiling", 1) => Ok(math::ceil(&args[0])),
        ("floor", 1) => Ok(math::floor(&args[0])),
        ("trunc", 1) => Ok(math::trunc(&args[0])),
        ("pow" | "power", 2) => Ok(math::pow(&args[0], &args[1])),
        ("mod", 2) => Ok(math::mod_(&args[0], &args[1])),
        ("sign", 1) => Ok(math::sign(&args[0])),
        ("pi", 0) => Ok(math::pi()),
        ("sin", 1) => Ok(math::sin(&args[0])),
        ("cos", 1) => Ok(math::cos(&args[0])),
        ("tan", 1) => Ok(math::tan(&args[0])),
        ("asin", 1) => Ok(math::asin(&args[0])),
        ("acos", 1) => Ok(math::acos(&args[0])),
        ("atan", 1) => Ok(math::atan(&args[0])),
        ("atan2", 2) => Ok(math::atan2(&args[0], &args[1])),
        ("sinh", 1) => Ok(math::sinh(&args[0])),
        ("cosh", 1) => Ok(math::cosh(&args[0])),
        ("tanh", 1) => Ok(math::tanh(&args[0])),
        ("asinh", 1) => Ok(math::asinh(&args[0])),
        ("acosh", 1) => Ok(math::acosh(&args[0])),
        ("atanh", 1) => Ok(math::atanh(&args[0])),
        ("radians", 1) => Ok(math::radians(&args[0])),
        ("degrees", 1) => Ok(math::degrees(&args[0])),

        // ---- JSON functions (M24) ----
        ("json", 1) => json::json_fn(&args[0]),
        ("jsonb", 1) => json::jsonb_fn(&args[0]),
        ("json_array", _) => json::json_array_fn(args),
        ("json_object", _) => json::json_object_fn(args),
        ("json_extract" | "jsonb_extract", n) if n >= 2 => json::json_extract_fn(args),
        ("json_type", n) if n >= 1 && n <= 2 => json::json_type_fn(args),
        ("json_valid", n) if n >= 1 && n <= 2 => json::json_valid_fn(args),
        ("json_quote", 1) => json::json_quote_fn(&args[0]),
        ("json_array_length" | "jsonb_array_length", n) if n >= 1 && n <= 2 => {
            json::json_array_length_fn(args)
        }

        // Should not happen: codegen validates with `check` before emitting a Function opcode.
        _ => Err(no_such_function(name, args.len())),
    }
}

/// Validate at code-generation time that `name` is a known scalar function callable with
/// `n_arg` arguments, returning the same error SQLite would (`no such function` / `wrong number
/// of arguments`).
pub fn check(name: &str, n_arg: usize) -> Result<()> {
    // Aggregate names are accepted by `check` so the scalar `Function` codegen arm does not
    // reject them prematurely; the aggregate codegen path resolves them via
    // [`aggregate::AggregateKind::from_name`] before reaching `check`. This keeps the scalar
    // `min(X,Y,...)` / `max(X,Y,...)` forms working alongside the 1-arg aggregate forms.
    if super::aggregate::AggregateKind::from_name(name, n_arg).is_some() {
        return Ok(());
    }
    let lname = name.to_ascii_lowercase();
    let arity_ok = match lname.as_str() {
        // core scalars (M3a)
        "abs" | "length" | "lower" | "upper" | "typeof" => Some(n_arg == 1),
        "substr" | "substring" => Some(n_arg == 2 || n_arg == 3),
        "round" => Some(n_arg == 1 || n_arg == 2),
        "coalesce" => Some(n_arg >= 2),
        "ifnull" | "nullif" => Some(n_arg == 2),

        // misc scalars (M3b) — `iif(X,Y)` ≡ `iif(X,Y,NULL)`, so arity 2 or 3.
        "iif" | "if" => Some(n_arg == 2 || n_arg == 3),
        "min" | "max" => Some(n_arg >= 2),
        "zeroblob" => Some(n_arg == 1),
        "likely" | "unlikely" => Some(n_arg == 1),
        "likelihood" => Some(n_arg == 2),

        // string functions (M3b)
        "instr" => Some(n_arg == 2),
        "replace" => Some(n_arg == 3),
        "trim" | "ltrim" | "rtrim" => Some(n_arg == 1 || n_arg == 2),
        "char" => Some(true), // any arity, including zero
        "unicode" => Some(n_arg == 1),
        "hex" => Some(n_arg == 1),
        "unhex" => Some(n_arg == 1 || n_arg == 2),
        "concat" => Some(n_arg >= 1),
        "concat_ws" => Some(n_arg >= 2),
        "quote" => Some(n_arg == 1),
        "octet_length" => Some(n_arg == 1),
        "like" => Some(n_arg == 2 || n_arg == 3),
        "glob" => Some(n_arg == 2),

        // math functions (M3b)
        "sqrt" | "exp" | "ln" | "log10" | "log2" | "ceil" | "ceiling" | "floor" | "trunc"
        | "sign" | "sin" | "cos" | "tan" | "asin" | "acos" | "atan" | "sinh" | "cosh" | "tanh"
        | "asinh" | "acosh" | "atanh" | "radians" | "degrees" => Some(n_arg == 1),
        "log" => Some(n_arg == 1 || n_arg == 2),
        "pow" | "power" | "mod" | "atan2" => Some(n_arg == 2),
        "pi" => Some(n_arg == 0),

        // JSON functions (M24)
        "json" | "jsonb" => Some(n_arg == 1),
        "json_array" => Some(true), // any arity including zero
        "json_object" => Some(n_arg % 2 == 0),
        "json_extract" | "jsonb_extract" => Some(n_arg >= 2),
        "json_type" => Some(n_arg == 1 || n_arg == 2),
        "json_valid" => Some(n_arg == 1 || n_arg == 2),
        "json_quote" => Some(n_arg == 1),
        "json_array_length" | "jsonb_array_length" => Some(n_arg == 1 || n_arg == 2),

        // volatile / connection-state functions (M3b): handled in the VDBE executor's Function
        // arm (they need runtime state), so `check` only learns their arities as the codegen
        // gatekeeper — they intentionally have no `call_scalar` entry.
        "random" | "changes" | "total_changes" | "last_insert_rowid" | "sqlite_version" => {
            Some(n_arg == 0)
        }
        "randomblob" => Some(n_arg == 1),

        // date/time functions (M23): handled in the VDBE executor's Function arm (they need
        // the per-statement DateCtx for `now`/`current_*`/`localtime`/`utc`), so `check` only
        // validates arity as the codegen gatekeeper. Arity -1 = variadic.
        "date" | "datetime" | "julianday" | "unixepoch" | "strftime" => Some(true),
        "time" => Some(true),
        "timediff" => Some(n_arg == 2),
        "current_date" | "current_time" | "current_timestamp" => Some(n_arg == 0),

        _ => None,
    };
    match arity_ok {
        None => Err(Error::msg(format!("no such function: {name}"))),
        Some(false) => Err(Error::msg(format!(
            "wrong number of arguments to function {lname}()"
        ))),
        Some(true) => Ok(()),
    }
}

fn no_such_function(name: &str, _n: usize) -> Error {
    Error::msg(format!("no such function: {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Value {
        Value::Text(s.to_string())
    }

    /// Functions whose names the current parser reserves as keywords (`replace`, `if`) cannot be
    /// reached from SQL yet, so the differential harness can't cover them. Pin their behavior
    /// here by calling `call_scalar` directly. Expected values were taken from the `sqlite3`
    /// oracle while developing (e.g. `sqlite3 :memory: "SELECT replace('abcabc','bc','XY');"`).
    #[test]
    fn replace_via_call_scalar() {
        let cs = |a: &[Value]| call_scalar("replace", a).unwrap();
        assert_eq!(cs(&[t("abcabc"), t("bc"), t("XY")]), t("aXYaXY"));
        // Empty pattern returns X unchanged (preserving storage class).
        assert_eq!(cs(&[t("abc"), t(""), t("X")]), t("abc"));
        assert_eq!(cs(&[Value::Int(123), t("2"), t("X")]), t("1X3"));
        assert_eq!(cs(&[t("aaa"), t("a"), t("bb")]), t("bbbbbb"));
        // Any NULL argument yields NULL.
        assert_eq!(cs(&[t("abc"), Value::Null, t("X")]), Value::Null);
        assert_eq!(cs(&[t("abc"), t("b"), Value::Null]), Value::Null);
        assert_eq!(cs(&[Value::Null, t("b"), t("X")]), Value::Null);
    }

    #[test]
    fn iif_and_if_alias_via_call_scalar() {
        for name in ["iif", "if"] {
            let cs = |a: &[Value]| call_scalar(name, a).unwrap();
            assert_eq!(cs(&[Value::Int(1), t("a"), t("b")]), t("a"));
            assert_eq!(cs(&[Value::Int(0), t("a"), t("b")]), t("b"));
            assert_eq!(cs(&[Value::Null, t("a"), t("b")]), t("b"));
            // TEXT/REAL truthiness uses the numeric value.
            assert_eq!(cs(&[t("x"), t("a"), t("b")]), t("b"));
            assert_eq!(cs(&[t("0"), t("a"), t("b")]), t("b"));
            assert_eq!(cs(&[Value::Real(2.5), t("a"), t("b")]), t("a"));
            // The 2-arg form `iif(X, Y)` ≡ `iif(X, Y, NULL)`: Y when truthy, else NULL.
            assert_eq!(cs(&[Value::Int(1), t("a")]), t("a"));
            assert_eq!(cs(&[Value::Int(0), t("a")]), Value::Null);
            assert_eq!(cs(&[Value::Null, t("a")]), Value::Null);
        }
        // `check` accepts both names at arity 2 and 3 and rejects other arities.
        assert!(check("iif", 2).is_ok());
        assert!(check("iif", 3).is_ok());
        assert!(check("if", 2).is_ok());
        assert!(check("if", 3).is_ok());
        assert!(check("if", 1).is_err());
        assert!(check("iif", 4).is_err());
    }

    #[test]
    fn check_accepts_and_rejects_arities() {
        // Known names at valid arities.
        assert!(check("concat", 1).is_ok());
        assert!(check("concat_ws", 2).is_ok());
        assert!(check("char", 0).is_ok());
        assert!(check("pi", 0).is_ok());
        assert!(check("log", 1).is_ok());
        assert!(check("log", 2).is_ok());
        // Wrong arities.
        assert!(check("concat_ws", 1).is_err()); // needs the separator + ≥1 value
        // `min`/`max` at arity 1 are the aggregate forms (accepted now that M6 is landing);
        // only the scalar forms (arity ≥2) go through `call_scalar`.
        assert!(check("min", 1).is_ok());
        assert!(check("max", 1).is_ok());
        assert!(check("pi", 1).is_err());
        assert!(check("sqrt", 2).is_err());
        // Unknown name.
        assert!(check("no_such_fn", 1).is_err());
    }
}
