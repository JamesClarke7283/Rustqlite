//! The prepared-statement handle — `sqlite3_stmt *` (mirrors `vdbeapi.c` / `prepare.c`).
//!
//! At M1, `sqlite3_prepare_v2` runs the real parser (so syntax errors are reported
//! faithfully) and stores the AST, but there is no code generator or VDBE yet, so
//! `sqlite3_step` reports that execution is not implemented. The accessor and bind methods
//! exist with their C-API names so callers can be written against the final surface.

use rustqlite_parser::{parse, Stmt};

use crate::error::{Error, Result, ResultCode};
use crate::types::Value;

use super::connection::Sqlite3;

/// A compiled (parsed) statement. The Rust analogue of `sqlite3_stmt *`.
pub struct Sqlite3Stmt {
    sql: String,
    ast: Vec<Stmt>,
}

/// `sqlite3_prepare_v2()` — compile the first SQL statement in `sql`.
///
/// Returns the statement and the unparsed tail. NOTE: statement-boundary tracking is not yet
/// implemented, so the tail is currently always empty (the whole input is parsed at once).
pub fn sqlite3_prepare_v2<'a>(_db: &mut Sqlite3, sql: &'a str) -> Result<(Sqlite3Stmt, &'a str)> {
    let ast = parse(sql).map_err(|e| Error::msg(format!("near syntax error: {e}")))?;
    Ok((
        Sqlite3Stmt {
            sql: sql.to_string(),
            ast,
        },
        "",
    ))
}

impl Sqlite3Stmt {
    /// `sqlite3_step()` — advance the statement. Not yet implemented (pending the VDBE in M3).
    pub fn step(&mut self) -> ResultCode {
        ResultCode::Error
    }

    /// The error explaining why [`step`](Self::step) cannot run yet.
    pub fn step_error(&self) -> Error {
        Error::msg("statement execution is not implemented yet (pending the VDBE, M3)")
    }

    /// `sqlite3_column_count()` — number of result columns (0 until codegen lands).
    pub fn column_count(&self) -> usize {
        0
    }

    /// `sqlite3_column_value()` — the value of result column `i` in the current row. Always
    /// NULL until the VDBE produces rows.
    pub fn column_value(&self, _i: usize) -> Value {
        Value::Null
    }

    /// `sqlite3_reset()` — reset to the start. No-op until execution exists.
    pub fn reset(&mut self) -> ResultCode {
        ResultCode::Ok
    }

    /// `sqlite3_finalize()` — destroy the statement. Resources free on drop.
    pub fn finalize(self) -> ResultCode {
        ResultCode::Ok
    }

    /// The original SQL text.
    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// The parsed statement list (engine-internal; not part of the C API).
    pub fn ast(&self) -> &[Stmt] {
        &self.ast
    }
}
