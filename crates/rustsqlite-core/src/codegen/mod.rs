//! Code generator + query planner (mirrors `build.c`, `select.c`, `insert.c`, `update.c`,
//! `delete.c`, `expr.c`, `where*.c`, `attach.c`, `trigger.c`).
//!
//! Lowers an AST from [`rustqlite_parser`] to a VDBE [`crate::vdbe::program::Program`]. M3a
//! implements the read query path for a single-table (or constant) `SELECT`:
//! [`builder`] is the register allocator / instruction emitter, [`expr`] compiles expressions
//! (value and jump forms), and [`select`] lays out the scan/sorter/limit structure. The write
//! path and the planner's index selection arrive in later milestones.

pub mod builder;
pub mod expr;
pub mod select;

use crate::error::Result;
use crate::schema::Table;
use crate::vdbe::Program;

use rustqlite_parser::SelectStmt;

/// Compile a single-table (or constant) `SELECT` into a VDBE program plus its result column
/// names. `table` is the resolved table for the lone `FROM` entry, or `None` for a `SELECT`
/// with no `FROM`.
pub fn compile_select(
    select: &SelectStmt,
    table: Option<&Table>,
) -> Result<(Program, Vec<String>)> {
    select::compile(select, table)
}
