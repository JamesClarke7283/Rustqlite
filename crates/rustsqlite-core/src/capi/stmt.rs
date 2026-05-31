//! The prepared-statement handle — `sqlite3_stmt *` (mirrors `vdbeapi.c` / `prepare.c`).
//!
//! `sqlite3_prepare_v2` runs the parser, resolves the (single) table from the catalog, compiles
//! the `SELECT` to a VDBE [`Program`], and builds a [`Vdbe`] that owns a cloned `Arc<Pager>` —
//! so the statement borrows nothing from the connection (mirroring how a C `sqlite3_stmt` holds
//! its own `db` pointer). `sqlite3_step` drives the async executor via the process-global
//! runtime; the column accessors read the current result row out of the VDBE registers.

use std::sync::Arc;

use rustqlite_parser::{parse, Stmt};

use crate::codegen;
use crate::error::{Error, Result, ResultCode};
use crate::schema::{read_catalog, Table};
use crate::types::Value;
use crate::vdbe::{Program, StepResult, Vdbe};

use super::connection::Sqlite3;
use super::runtime::block_on;

/// A compiled statement. The Rust analogue of `sqlite3_stmt *`.
pub struct Sqlite3Stmt {
    sql: String,
    program: Arc<Program>,
    column_names: Vec<String>,
    vdbe: Vdbe,
    last_error: Option<Error>,
}

/// `sqlite3_prepare_v2()` — compile the first SQL statement in `sql`.
///
/// Returns the statement and the unparsed tail (always empty for now — statement-boundary
/// tracking is not yet implemented, so the whole input is parsed at once).
pub fn sqlite3_prepare_v2<'a>(db: &mut Sqlite3, sql: &'a str) -> Result<(Sqlite3Stmt, &'a str)> {
    match prepare(db, sql) {
        Ok(stmt) => Ok((stmt, "")),
        Err(e) => {
            db.set_last_error(e.clone());
            Err(e)
        }
    }
}

fn prepare(db: &mut Sqlite3, sql: &str) -> Result<Sqlite3Stmt> {
    let ast = parse(sql).map_err(|e| Error::msg(format!("near syntax error: {e}")))?;
    let stmt = ast
        .into_iter()
        .next()
        .ok_or_else(|| Error::msg("no SQL statement"))?;
    let select = match stmt {
        Stmt::Select(s) => s,
        Stmt::CreateTable(_) | Stmt::Insert(_) => {
            return Err(Error::msg(
                "only SELECT is executable in M3a (the write path is pending)",
            ))
        }
    };

    // Resolve the single FROM table (if any) from the catalog.
    let (table, pager) = if let Some(table_ref) = select.from.first() {
        if select.from.len() > 1 {
            return Err(Error::msg("joins are not supported in M3a"));
        }
        let pager = db.pager_arc()?;
        let catalog = block_on(read_catalog(&pager))?;
        let obj = catalog
            .find_table(&table_ref.name)
            .ok_or_else(|| Error::msg(format!("no such table: {}", table_ref.name)))?;
        (Some(Table::from_schema_object(obj)?), Some(pager))
    } else {
        (None, None)
    };

    let (program, column_names) = codegen::compile_select(&select, table.as_ref())?;
    let program = Arc::new(program);
    let vdbe = Vdbe::new(Arc::clone(&program), pager);

    Ok(Sqlite3Stmt {
        sql: sql.to_string(),
        program,
        column_names,
        vdbe,
        last_error: None,
    })
}

impl Sqlite3Stmt {
    /// `sqlite3_step()` — advance the statement, returning `Row` (a result row is available),
    /// `Done`, or `Error`.
    pub fn step(&mut self) -> ResultCode {
        match block_on(self.vdbe.step()) {
            Ok(StepResult::Row) => ResultCode::Row,
            Ok(StepResult::Done) => ResultCode::Done,
            Err(e) => {
                self.last_error = Some(e);
                ResultCode::Error
            }
        }
    }

    /// The message of the most recent step error (or `"not an error"`).
    pub fn errmsg(&self) -> &str {
        match &self.last_error {
            Some(e) => &e.message,
            None => "not an error",
        }
    }

    /// `sqlite3_column_count()` — number of result columns.
    pub fn column_count(&self) -> usize {
        self.column_names.len()
    }

    /// `sqlite3_column_name()` — the name of result column `i`.
    pub fn column_name(&self, i: usize) -> Option<&str> {
        self.column_names.get(i).map(String::as_str)
    }

    /// `sqlite3_column_value()` — the value of result column `i` in the current row.
    pub fn column_value(&self, i: usize) -> Value {
        if i < self.column_names.len() {
            self.vdbe.result_value(i)
        } else {
            Value::Null
        }
    }

    /// `sqlite3_reset()` — reset to the start so the statement can be re-run.
    pub fn reset(&mut self) -> ResultCode {
        self.vdbe.reset();
        self.last_error = None;
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

    /// The compiled program (engine-internal; not part of the C API).
    pub fn program(&self) -> &Program {
        &self.program
    }
}
