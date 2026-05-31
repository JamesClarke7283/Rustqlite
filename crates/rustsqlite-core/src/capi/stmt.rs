//! The prepared-statement handle — `sqlite3_stmt *` (mirrors `vdbeapi.c` / `prepare.c`).
//!
//! `sqlite3_prepare_v2` runs the parser, resolves the (single) table from the catalog, compiles
//! the `SELECT` to a VDBE [`Program`], and builds a [`Vdbe`] that owns a cloned `Arc<Pager>` —
//! so the statement borrows nothing from the connection (mirroring how a C `sqlite3_stmt` holds
//! its own `db` pointer). `sqlite3_step` drives the async executor via the process-global
//! runtime; the column accessors read the current result row out of the VDBE registers.

use std::sync::Arc;

use rustqlite_parser::{parse, ExplainKind, SelectStmt, Stmt};

use crate::codegen;
use crate::error::{Error, Result, ResultCode};
use crate::pager::Pager;
use crate::schema::{read_catalog, Table};
use crate::types::Value;
use crate::vdbe::{explain, Program, StepResult, Vdbe};

use super::connection::Sqlite3;
use super::runtime::block_on;

/// How a prepared statement produces its result rows.
enum Backing {
    /// A normal compiled `SELECT`: rows come from running the VDBE program.
    Vdbe(Vdbe),
    /// An `EXPLAIN` / `EXPLAIN QUERY PLAN`: rows are precomputed and replayed verbatim. `cur` is
    /// the index of the current row (for the column accessors), `pos` the next row to yield.
    Static {
        rows: Vec<Vec<Value>>,
        cur: Option<usize>,
        pos: usize,
    },
}

/// A compiled statement. The Rust analogue of `sqlite3_stmt *`.
pub struct Sqlite3Stmt {
    sql: String,
    /// The compiled program. For an `EXPLAIN` this is the INNER select's program (so `program()`
    /// stays meaningful — the golden bytecode test reads it).
    program: Arc<Program>,
    column_names: Vec<String>,
    backing: Backing,
    /// `sqlite3_stmt_isexplain()`: 0 = normal, 1 = `EXPLAIN`, 2 = `EXPLAIN QUERY PLAN`.
    explain: u8,
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

    match stmt {
        Stmt::Select(select) => {
            // A normal SELECT: compile and back it with a live VDBE.
            let compiled = compile_select(db, &select)?;
            let program = Arc::new(compiled.program);
            let vdbe = Vdbe::new(Arc::clone(&program), compiled.pager);
            Ok(Sqlite3Stmt {
                sql: sql.to_string(),
                program,
                column_names: compiled.column_names,
                backing: Backing::Vdbe(vdbe),
                explain: 0,
                last_error: None,
            })
        }
        Stmt::Explain(inner, kind) => prepare_explain(db, sql, *inner, kind),
        Stmt::CreateTable(_) | Stmt::Insert(_) => Err(Error::msg(
            "only SELECT is executable in M3a (the write path is pending)",
        )),
    }
}

/// Prepare an `EXPLAIN` / `EXPLAIN QUERY PLAN`. The inner statement must be a `SELECT` (the same
/// restriction the engine applies to plain statements — `EXPLAIN CREATE/INSERT` is rejected with
/// the identical "only SELECT" error). The inner select is compiled and INSPECTED, never executed;
/// the resulting explain rows are replayed from a [`Backing::Static`].
fn prepare_explain(
    db: &mut Sqlite3,
    sql: &str,
    inner: Stmt,
    kind: ExplainKind,
) -> Result<Sqlite3Stmt> {
    let select = match inner {
        Stmt::Select(s) => s,
        _ => {
            return Err(Error::msg(
                "only SELECT is executable in M3a (the write path is pending)",
            ))
        }
    };

    let compiled = compile_select(db, &select)?;
    let table_name = compiled.table.as_ref().map(|t| t.name.as_str());
    let (rows, headers): (Vec<Vec<Value>>, Vec<String>) = match kind {
        ExplainKind::Bytecode => (
            explain::bytecode_rows(&compiled.program),
            explain::BYTECODE_HEADER
                .iter()
                .map(|s| s.to_string())
                .collect(),
        ),
        ExplainKind::QueryPlan => (
            explain::query_plan_rows(&select, table_name),
            explain::QUERY_PLAN_HEADER
                .iter()
                .map(|s| s.to_string())
                .collect(),
        ),
    };

    // Keep the inner select's program around so `program()` stays meaningful for the golden test.
    let program = Arc::new(compiled.program);
    let explain = match kind {
        ExplainKind::Bytecode => 1,
        ExplainKind::QueryPlan => 2,
    };
    Ok(Sqlite3Stmt {
        sql: sql.to_string(),
        program,
        column_names: headers,
        backing: Backing::Static {
            rows,
            cur: None,
            pos: 0,
        },
        explain,
        last_error: None,
    })
}

/// A compiled SELECT plus everything the prepare path needs from it: the program, the result
/// column names, the owned `Arc<Pager>` (for a live VDBE), and the resolved table (for EXPLAIN
/// QUERY PLAN's `SCAN <name>` detail).
struct CompiledSelect {
    program: Program,
    column_names: Vec<String>,
    pager: Option<Arc<Pager>>,
    table: Option<Table>,
}

/// Resolve the single FROM table (if any) from the catalog and compile the SELECT. Shared by the
/// normal SELECT path and the EXPLAIN path.
fn compile_select(db: &mut Sqlite3, select: &SelectStmt) -> Result<CompiledSelect> {
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

    let (program, column_names) = codegen::compile_select(select, table.as_ref())?;
    Ok(CompiledSelect {
        program,
        column_names,
        pager,
        table,
    })
}

impl Sqlite3Stmt {
    /// `sqlite3_step()` — advance the statement, returning `Row` (a result row is available),
    /// `Done`, or `Error`.
    pub fn step(&mut self) -> ResultCode {
        match &mut self.backing {
            Backing::Vdbe(vdbe) => match block_on(vdbe.step()) {
                Ok(StepResult::Row) => ResultCode::Row,
                Ok(StepResult::Done) => ResultCode::Done,
                Err(e) => {
                    self.last_error = Some(e);
                    ResultCode::Error
                }
            },
            Backing::Static { rows, cur, pos } => {
                if *pos < rows.len() {
                    *cur = Some(*pos);
                    *pos += 1;
                    ResultCode::Row
                } else {
                    ResultCode::Done
                }
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
        if i >= self.column_names.len() {
            return Value::Null;
        }
        match &self.backing {
            Backing::Vdbe(vdbe) => vdbe.result_value(i),
            Backing::Static { rows, cur, .. } => cur
                .and_then(|c| rows.get(c))
                .and_then(|row| row.get(i))
                .cloned()
                .unwrap_or(Value::Null),
        }
    }

    /// `sqlite3_reset()` — reset to the start so the statement can be re-run.
    pub fn reset(&mut self) -> ResultCode {
        match &mut self.backing {
            Backing::Vdbe(vdbe) => vdbe.reset(),
            Backing::Static { cur, pos, .. } => {
                *cur = None;
                *pos = 0;
            }
        }
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

    /// `sqlite3_stmt_isexplain()` — 0 for a normal statement, 1 for `EXPLAIN`, 2 for
    /// `EXPLAIN QUERY PLAN`. The shell uses this to choose between the bytecode table and the
    /// query-plan tree rendering.
    pub fn explain_kind(&self) -> u8 {
        self.explain
    }
}
