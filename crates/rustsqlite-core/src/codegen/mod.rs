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
pub mod drop;
pub mod expr;
pub mod insert;
pub mod select;
pub mod update;

use crate::error::Result;
use crate::schema::Table;
use crate::vdbe::Program;

use rustqlite_parser::{CreateTable, DeleteStmt, DropTableStmt, InsertStmt, SelectStmt, UpdateStmt};

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

/// Compile a `DROP TABLE [IF EXISTS] <name>` into a VDBE write program. `current_schema_cookie`
/// is the value before this statement runs; the program bumps it by one. `resolved_table`
/// is `Some(table)` when the table exists in the catalog; the codegen errors with
/// "no such table" when it's `None` and the statement did not say `IF EXISTS`.
pub fn compile_drop_table(
    drop: &DropTableStmt,
    current_schema_cookie: u32,
    resolved_table: Option<&Table>,
) -> Result<Program> {
    drop::compile_drop_table(drop, drop.if_exists, current_schema_cookie, resolved_table)
}

/// Compile an `UPDATE [OR action] tbl SET col = expr [, …] [WHERE expr]` into a VDBE write
/// program against the resolved `table`. The first M5.0 slice: single-table, no triggers /
/// FK / indexes / UPSERT, `OR action` other than ABORT errors at codegen time.
pub fn compile_update(upd: &UpdateStmt, table: &Table) -> Result<Program> {
    update::compile_update(upd, table)
}
