//! Code generator + query planner (mirrors `build.c`, `select.c`, `insert.c`, `update.c`,
//! `delete.c`, `expr.c`, `where*.c`, `attach.c`, `trigger.c`).
//!
//! Lowers an AST from [`rustqlite_parser`] to a VDBE [`crate::vdbe::program::Program`]. M3a
//! implements the read query path for a single-table (or constant) `SELECT`:
//! [`builder`] is the register allocator / instruction emitter, [`expr`] compiles expressions
//! (value and jump forms), and [`select`] lays out the scan/sorter/limit structure. The write
//! path and the planner's index selection arrive in later milestones.

pub mod builder;
pub mod compound;
pub mod create;
pub mod delete;
pub mod drop;
pub mod drop_index;
pub mod expr;
pub mod index;
pub mod index_planner;
pub mod insert;
pub mod join;
pub mod join_using;
pub mod returning;
pub mod select;
pub mod subquery;
pub mod update;

pub use expr::SubqueryResolver;

use crate::error::Result;
use crate::schema::{IndexObject, Table};
use crate::vdbe::Program;

use rustqlite_parser::{
    CreateIndex, CreateTable, DeleteStmt, DropIndexStmt, DropTableStmt, InsertStmt, SelectStmt,
    UpdateStmt,
};
/// Compile a single-table (or constant) `SELECT` into a VDBE program plus its result column
/// names. `table` is the resolved table for the lone `FROM` entry, or `None` for a `SELECT`
/// with no `FROM`. `indexes` is the list of indexes attached to `table`; the M5.1 first
/// slice uses them to route indexed-equality lookups (see [`index_planner`]) — an empty slice
/// is the M3a default.
///
/// `subquery_resolver`, when set, lets expression codegen compile scalar subqueries /
/// `EXISTS` / `IN (SELECT ...)` against the catalog. `None` leaves those expression kinds
/// raising "unsupported" (the pre-M8.7 behavior).
pub fn compile_select(
    select: &SelectStmt,
    table: Option<&Table>,
    indexes: &[IndexObject],
    subquery_resolver: Option<&dyn SubqueryResolver>,
) -> Result<(Program, Vec<String>)> {
    select::compile(select, table, indexes, subquery_resolver)
}

/// Compute the `EXPLAIN QUERY PLAN` index-plan summary for `select` against `table` with its
/// `indexes`, if the planner picks an index. Returns `None` when the plan is a table scan.
/// This recomputes the plan (the compile path already computed it once); the cost is cheap
/// (catalog read + a prefix match) and avoids threading the plan through every compile
/// return type.
pub fn select_index_plan_info(
    select: &SelectStmt,
    table: Option<&Table>,
    indexes: &[IndexObject],
) -> Option<crate::vdbe::explain::IndexPlanInfo> {
    table.and_then(|t| index_planner::pick_index(select, t, indexes)).map(|plan| {
        let equality_columns: Vec<String> = plan.equality.iter().map(|e| e.column.clone()).collect();
        let has_where_equality = !plan.equality.is_empty();
        crate::vdbe::explain::IndexPlanInfo {
            index_name: plan.index.name.clone(),
            covering: plan.covering,
            has_where_equality,
            equality_columns,
            order_by_satisfied: plan.order_by_satisfied,
        }
    })
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

/// Compile a `CREATE [UNIQUE] INDEX [IF NOT EXISTS] name ON tbl(col)` into a VDBE write program
/// that also populates the new index from the table's current rows. `table` is the catalog-
/// resolved table (the codegen verifies the indexed column exists); `sql_text` is the verbatim
/// `CREATE INDEX` source.
pub fn compile_create_index(
    ci: &CreateIndex,
    table: &Table,
    sql_text: &str,
    schema_cookie: u32,
) -> Result<Program> {
    index::compile_create_index(ci, table, sql_text, schema_cookie)
}

/// Compile an `INSERT` into a VDBE write program against the resolved `table`. The
/// `indexes` slice is the list of `IndexObject`s attached to `table` (the prepare path
/// resolves them from the catalog); the program emits per-row `IdxInsert` for each.
///
/// For `INSERT ... SELECT`, `source_table` is the catalog-resolved source table (or `None`
/// for a constant / `VALUES` source); `source_indexes` are the indexes attached to that source
/// table. These are required so the SELECT body can be compiled with column resolution and
/// indexed lookups.
pub fn compile_insert(
    ins: &InsertStmt,
    table: &Table,
    indexes: &[IndexObject],
    source_table: Option<&Table>,
    source_indexes: &[IndexObject],
) -> Result<Program> {
    insert::compile_insert(ins, table, indexes, source_table, source_indexes)
}

/// Compile a `DELETE FROM <table> [WHERE <expr>]` into a VDBE write program against the
/// resolved `table`. `indexes` is the list of indexes attached to `table`; the program
/// emits per-row `IdxDelete` for each (single-column) index.
pub fn compile_delete(del: &DeleteStmt, table: &Table, indexes: &[IndexObject]) -> Result<Program> {
    delete::compile_delete(del, table, indexes)
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

/// Compile a `DROP INDEX [IF EXISTS] [schema.]name` into a VDBE write program. `index` is the
/// catalog-resolved index (when `None`, the caller must be using `IF EXISTS` — the codegen
/// routes that to a no-op `Halt`). `current_schema_cookie` is the value before this statement
/// runs; the program bumps it by one. `schema_rowid` is the rowid of the matching
/// `sqlite_schema` row (so we can `Delete` it without scanning the b-tree).
pub fn compile_drop_index(
    drop: &DropIndexStmt,
    index: Option<&IndexObject>,
    current_schema_cookie: u32,
    schema_rowid: i64,
) -> Result<Program> {
    match index {
        Some(idx) => drop_index::compile_drop_index(drop, idx, current_schema_cookie, schema_rowid),
        None => Ok(drop_index::compile_drop_index_noop()),
    }
}

/// Compile an `UPDATE [OR action] tbl SET col = expr [, …] [WHERE expr]` into a VDBE write
/// program against the resolved `table`. The first M5.0 slice: single-table, no triggers /
/// FK / `OR action` other than ABORT (errors at codegen time). M5.1: `indexes` drives per-row
/// `IdxDelete` + `IdxInsert` maintenance for each single-column index on the table.
pub fn compile_update(upd: &UpdateStmt, table: &Table, indexes: &[IndexObject]) -> Result<Program> {
    update::compile_update(upd, table, indexes)
}

/// Compile a `SELECT ... FROM (subquery) AS alias [...]` by materializing the subquery into an
/// in-memory ephemeral table and then scanning it. `subquery` is the inner `SELECT`;
/// `subquery_table`/`subquery_indexes` describe the inner FROM table (or `None` for a
/// constant/VALUES subquery). Returns the program and the outer result column names.
#[allow(clippy::too_many_arguments)]
pub fn compile_from_subquery(
    outer: &SelectStmt,
    subquery: &SelectStmt,
    alias: &str,
    subquery_table: Option<&Table>,
    subquery_indexes: &[IndexObject],
) -> Result<(Program, Vec<String>)> {
    subquery::compile_from_subquery(outer, subquery, alias, subquery_table, subquery_indexes)
}
