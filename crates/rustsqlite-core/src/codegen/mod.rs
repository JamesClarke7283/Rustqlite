//! Code generator + query planner (mirrors `build.c`, `select.c`, `insert.c`, `update.c`,
//! `delete.c`, `expr.c`, `where*.c`, `attach.c`, `trigger.c`).
//!
//! Lowers an AST from [`rustqlite_parser`] to a VDBE [`crate::vdbe::program::Program`]. M3a
//! implements the read query path for a single-table (or constant) `SELECT`:
//! [`builder`] is the register allocator / instruction emitter, [`expr`] compiles expressions
//! (value and jump forms), and [`select`] lays out the scan/sorter/limit structure. The write
//! path and the planner's index selection arrive in later milestones.

pub mod builder;
pub mod create;
pub mod delete;
pub mod expr;
pub mod insert;
pub mod select;

use crate::error::Result;
use crate::schema::Table;
use crate::vdbe::Program;

use rustqlite_parser::{CreateTable, DeleteStmt, InsertStmt, SelectStmt};

/// Compile a single-table (or constant) `SELECT` into a VDBE program plus its result column
/// names. `table` is the resolved table for the lone `FROM` entry, or `None` for a `SELECT`
/// with no `FROM`.
pub fn compile_select(
    select: &SelectStmt,
    table: Option<&Table>,
) -> Result<(Program, Vec<String>)> {
    select::compile(select, table)
}

/// Compile a `CREATE TABLE` into a VDBE write program. `sql_text` is stored verbatim in the new
/// `sqlite_schema` row; `schema_cookie` is the current cookie (the program bumps it by one).
pub fn compile_create_table(
    ct: &CreateTable,
    sql_text: &str,
    schema_cookie: u32,
) -> Result<Program> {
    create::compile_create_table(ct, sql_text, schema_cookie)
}

/// Compile an `INSERT ... VALUES` into a VDBE write program against the resolved `table`.
pub fn compile_insert(ins: &InsertStmt, table: &Table) -> Result<Program> {
    insert::compile_insert(ins, table)
}

/// Compile a `DELETE FROM <table> [WHERE <expr>]` into a VDBE write program against the
/// resolved `table`.
pub fn compile_delete(del: &DeleteStmt, table: &Table) -> Result<Program> {
    delete::compile_delete(del, table)
}
