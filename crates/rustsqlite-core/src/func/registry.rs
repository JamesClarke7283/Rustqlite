//! Built-in function registry (mirrors `sqlite3FindFunction` / the builtin tables in `func.c`).
//!
//! Maps a `(name, arg-count)` pair, case-insensitively, to a scalar implementation. M3a ships
//! the common scalar starter set; the full table and the aggregates arrive later.

use crate::error::{Error, Result};
use crate::types::Value;

use super::scalar;

/// Call a scalar function by name (case-insensitive) over already-evaluated arguments.
pub fn call_scalar(name: &str, args: &[Value]) -> Result<Value> {
    let lname = name.to_ascii_lowercase();
    match (lname.as_str(), args.len()) {
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
        // Should not happen: codegen validates with `check` before emitting a Function opcode.
        _ => Err(no_such_function(name, args.len())),
    }
}

/// Validate at code-generation time that `name` is a known scalar function callable with
/// `n_arg` arguments, returning the same error SQLite would (`no such function` / `wrong number
/// of arguments`).
pub fn check(name: &str, n_arg: usize) -> Result<()> {
    let lname = name.to_ascii_lowercase();
    let arity_ok = match lname.as_str() {
        "abs" | "length" | "lower" | "upper" | "typeof" => Some(n_arg == 1),
        "substr" | "substring" => Some(n_arg == 2 || n_arg == 3),
        "round" => Some(n_arg == 1 || n_arg == 2),
        "coalesce" => Some(n_arg >= 2),
        "ifnull" | "nullif" => Some(n_arg == 2),
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
