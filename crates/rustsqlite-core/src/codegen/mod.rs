//! Code generator + query planner (mirrors `build.c`, `select.c`, `insert.c`, `update.c`,
//! `delete.c`, `expr.c`, `where*.c`, `attach.c`, `trigger.c`).
//!
//! Lowers an AST from [`rustqlite_parser`] to a VDBE [`crate::vdbe::program::Program`]. M3a
//! implements the read query path for a single-table (or constant) `SELECT`:
//! [`builder`] is the register allocator / instruction emitter, [`expr`] compiles expressions
//! (value and jump forms), and [`select`] lays out the scan/sorter/limit structure. The write
//! path and the planner's index selection arrive in later milestones.

pub mod alter;
pub mod builder;
pub mod compound;
pub mod create;
pub mod cte;
pub mod delete;
pub mod drop;
pub mod drop_index;
pub mod expr;
pub mod index;
pub mod index_planner;
pub mod insert;
pub mod join;
pub mod join_using;
pub mod resolve;
pub mod returning;
pub mod select;
pub mod subquery;
pub mod transaction;
pub mod trigger;
pub mod update;
pub mod view;
pub mod window;

pub use expr::SubqueryResolver;

use crate::error::Result;
use crate::schema::{IndexObject, Table};
use crate::vdbe::Program;

use rustqlite_parser::{
    AlterTableStmt, CreateIndex, CreateTable, CreateTrigger, CreateView, DeleteStmt,
    DropIndexStmt, DropTableStmt, DropTriggerStmt, DropViewStmt, InsertStmt, SelectStmt,
    TransactionStmt, UpdateStmt,
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

/// Compile a transaction-control statement (`BEGIN`/`COMMIT`/`END`/`ROLLBACK`/`SAVEPOINT`/
/// `RELEASE`/`ROLLBACK TO SAVEPOINT`) into a tiny VDBE program. The M12 first slice handles
/// `BEGIN`/`COMMIT`/`END`/`ROLLBACK` via `OP_AutoCommit`; `SAVEPOINT`/`RELEASE`/`ROLLBACK TO`
/// are rejected (the pager savepoint stack is M12.4/M12.5).
pub fn compile_transaction(stmt: &TransactionStmt) -> Result<Program> {
    transaction::compile_transaction(stmt)
}

/// Compile `ALTER TABLE <name> RENAME TO <new>` into a VDBE write program that rewrites the
/// matching `sqlite_schema` rows. `current_schema_cookie` is the value before this DDL runs
/// (the program bumps it by one). `edits` is the resolved set of schema-row edits (the table
/// row + every associated index/trigger row whose `tbl_name` matches the old name).
pub fn compile_alter_rename_table(
    stmt: &AlterTableStmt,
    current_schema_cookie: u32,
    edits: &[alter::SchemaRowEdit],
) -> Result<Program> {
    alter::compile_alter_rename_table(stmt, current_schema_cookie, edits)
}

/// Compile `ALTER TABLE <name> ADD [COLUMN] <def>` into a VDBE write program that rewrites
/// the table's `sqlite_schema` row to include the new column in the CREATE TABLE text.
/// `current_schema_cookie` is the value before this DDL runs (the program bumps it by one).
/// `table_rowid` is the rowid of the table's `sqlite_schema` row. `old_sql` is the current
/// CREATE TABLE text. `col_def_text` is the verbatim column-definition text from the user's
/// ALTER TABLE statement.
pub fn compile_alter_add_column(
    stmt: &AlterTableStmt,
    current_schema_cookie: u32,
    table_rowid: i64,
    old_sql: &str,
    col_def_text: &str,
) -> Result<Program> {
    alter::compile_alter_add_column(stmt, current_schema_cookie, table_rowid, old_sql, col_def_text)
}

/// Compile `ALTER TABLE <name> DROP [COLUMN] <col>` into a VDBE write program that
/// rewrites the table's `sqlite_schema` row (removing the column from the CREATE TABLE
/// text) and rewrites every existing row in the table b-tree (removing the dropped
/// column's value). `table` is the catalog-resolved table. `drop_col_idx` is the
/// table-column index of the column being dropped.
pub fn compile_alter_drop_column(
    stmt: &AlterTableStmt,
    current_schema_cookie: u32,
    table: &Table,
    table_rowid: i64,
    old_sql: &str,
    drop_col_idx: usize,
    drop_col_name: &str,
) -> Result<Program> {
    alter::compile_alter_drop_column(
        stmt,
        current_schema_cookie,
        table,
        table_rowid,
        old_sql,
        drop_col_idx,
        drop_col_name,
    )
}

/// Compile `ALTER TABLE <name> RENAME [COLUMN] <old> TO <new>` into a VDBE write program
/// that rewrites the `sql` column of the table's `sqlite_schema` row (and any associated
/// index/trigger rows). `edits` is the resolved set of schema-row edits.
pub fn compile_alter_rename_column(
    stmt: &AlterTableStmt,
    current_schema_cookie: u32,
    edits: &[alter::SchemaRowEdit],
) -> Result<Program> {
    alter::compile_alter_rename_column(stmt, current_schema_cookie, edits)
}

/// Compile a `CREATE VIEW` into a VDBE write program. `sql_text` is stored verbatim in the
/// new `sqlite_schema` row's `sql` column; `schema_cookie` is the current cookie (the
/// program bumps it by one).
pub fn compile_create_view(cv: &CreateView, sql_text: &str, schema_cookie: u32) -> Result<Program> {
    view::compile_create_view(cv, sql_text, schema_cookie)
}

/// Compile a `DROP VIEW [IF EXISTS] [schema.]name` into a VDBE write program.
/// `schema_rowid` is the rowid of the view's `sqlite_schema` row (0 for a no-op
/// `IF EXISTS` against a missing view).
pub fn compile_drop_view(
    dv: &DropViewStmt,
    schema_cookie: u32,
    schema_rowid: i64,
) -> Result<Program> {
    if schema_rowid == 0 {
        Ok(view::compile_drop_view_noop())
    } else {
        view::compile_drop_view(dv, schema_cookie, schema_rowid)
    }
}

/// Compile a `CREATE TRIGGER` into a VDBE write program. `sql_text` is stored verbatim in
/// the new `sqlite_schema` row's `sql` column; `schema_cookie` is the current cookie.
/// Trigger firing (M16.9+) is deferred.
pub fn compile_create_trigger(
    ct: &CreateTrigger,
    sql_text: &str,
    schema_cookie: u32,
) -> Result<Program> {
    trigger::compile_create_trigger(ct, sql_text, schema_cookie)
}

/// Compile a `DROP TRIGGER [IF EXISTS] [schema.]name` into a VDBE write program.
/// `schema_rowid` is the rowid of the trigger's `sqlite_schema` row (0 for a no-op).
pub fn compile_drop_trigger(
    dt: &DropTriggerStmt,
    schema_cookie: u32,
    schema_rowid: i64,
) -> Result<Program> {
    if schema_rowid == 0 {
        Ok(trigger::compile_drop_trigger_noop())
    } else {
        trigger::compile_drop_trigger(dt, schema_cookie, schema_rowid)
    }
}
