//! The prepared-statement handle — `sqlite3_stmt *` (mirrors `vdbeapi.c` / `prepare.c`).
//!
//! `sqlite3_prepare_v2` runs the parser, resolves the (single) table from the catalog, compiles
//! the `SELECT` to a VDBE [`Program`], and builds a [`Vdbe`] that owns a cloned `Arc<Pager>` —
//! so the statement borrows nothing from the connection (mirroring how a C `sqlite3_stmt` holds
//! its own `db` pointer). `sqlite3_step` drives the async executor via the process-global
//! runtime; the column accessors read the current result row out of the VDBE registers.

use std::sync::{Arc, Mutex};

use rustqlite_parser::{
    parse, AlterTableAction, AlterTableStmt, DropIndexStmt, DropTableStmt, ExplainKind,
    InsertSource, Literal, PragmaStmt, PragmaValue, PragmaValueKind, SelectStmt, Stmt,
};

use crate::codegen;
use crate::codegen::returning::Returning;
use crate::codegen::SubqueryResolver;
use crate::error::{Error, Result, ResultCode};
use crate::pager::Pager;
use crate::schema::{
    dequote_ident, read_catalog, schema_cookie, IndexObject, Table,
};
use crate::types::Value;
use crate::vdbe::{explain, Instruction, Opcode, Program, StepResult, Vdbe};

use super::connection::{ChangeCounts, Sqlite3};
use super::runtime::block_on;

/// A [`SubqueryResolver`] that reads the catalog via the pager. Used by `compile_select` to
/// give the expression codegen the table/index info it needs to compile scalar subqueries /
/// `EXISTS` / `IN (SELECT ...)` against the database. The pager is held as `Arc` so the
/// resolver can outlive the borrow of the `Sqlite3` connection (the codegen pass mutates the
/// `ProgramBuilder`, not the connection, so the pager clone is safe).
struct CatalogSubqueryResolver {
    pager: Arc<Pager>,
}

impl SubqueryResolver for CatalogSubqueryResolver {
    fn resolve(&self, subquery: &SelectStmt) -> Result<(Option<Table>, Vec<IndexObject>)> {
        // Mirrors `resolve_subquery_source` but without returning the pager (the codegen
        // expression path only needs the table + indexes; the pager is already wired into the
        // outer VDBE for cursor access).
        if !subquery.values.is_empty() || subquery.from.is_empty() {
            // A VALUES or constant SELECT subquery has no real FROM table.
            return Ok((None, Vec::new()));
        }
        if subquery.from.len() > 1 {
            return Err(Error::msg("joins inside a scalar subquery are not supported yet"));
        }
        let Some(table_ref) = subquery.from[0].table() else {
            return Err(Error::msg("nested FROM subqueries are not supported yet"));
        };
        if table_ref.name.eq_ignore_ascii_case("sqlite_schema")
            || table_ref.name.eq_ignore_ascii_case("sqlite_master")
        {
            let table = resolve_sqlite_schema(&self.pager)?;
            return Ok((Some(table), Vec::new()));
        }
        let catalog = block_on(read_catalog(&self.pager))?;
        let obj = catalog
            .find_table(&table_ref.name)
            .ok_or_else(|| Error::msg(format!("no such table: {}", table_ref.name)))?;
        let table = Table::from_schema_object(obj)?;
        let mut indexes = Vec::new();
        for obj in catalog.indexes() {
            if obj.tbl_name.eq_ignore_ascii_case(&table_ref.name) {
                indexes.push(IndexObject::from_schema_object(obj)?);
            }
        }
        Ok((Some(table), indexes))
    }
}

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

/// Construct a Vdbe for `program` and install the connection's shared autocommit flag and
/// `is_transaction_savepoint` flag so `OP_AutoCommit`, `OP_Halt`, and `OP_Savepoint` can
/// consult/mutate them. Mirrors how `sqlite3VdbeMakeReady` in `vdbeaux.c` copies `db->autoCommit`
/// (and related state) into the VDBE before running it. Every Vdbe constructed by the prepare
/// path should go through this helper so transaction semantics are honored.
fn vdbe_for(program: Arc<Program>, pager: Option<Arc<Pager>>, db: &Sqlite3) -> Vdbe {
    let mut v = Vdbe::new(program, pager);
    v.set_autocommit_handle(db.autocommit_handle());
    v.set_is_transaction_savepoint_handle(db.is_transaction_savepoint_handle());
    v
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
    /// For a write statement (`CREATE TABLE`/`INSERT`), the connection's shared change counters to
    /// publish into when the program finishes. `None` for read-only statements.
    counts: Option<Arc<Mutex<ChangeCounts>>>,
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
            let pager = compiled.pager;
            let vdbe = vdbe_for(Arc::clone(&program), pager, db);
            Ok(Sqlite3Stmt {
                sql: sql.to_string(),
                program,
                column_names: compiled.column_names,
                backing: Backing::Vdbe(vdbe),
                explain: 0,
                counts: None,
                last_error: None,
            })
        }
        Stmt::Explain(inner, kind) => prepare_explain(db, sql, *inner, kind),
        Stmt::CreateTable(ct) => {
            // CREATE TABLE: ensure the database file exists (create page 1 on an empty file),
            // then compile a write program. The verbatim CREATE text is the original SQL.
            let pager = db.ensure_pager()?;
            let schema_cookie = pager.header().schema_cookie;
            let sql_text = create_table_text(sql);
            let program = Arc::new(codegen::compile_create_table(&ct, sql_text, schema_cookie)?);
            let vdbe = vdbe_for(Arc::clone(&program), Some(pager), db);
            Ok(Sqlite3Stmt {
                sql: sql.to_string(),
                program,
                column_names: Vec::new(),
                backing: Backing::Vdbe(vdbe),
                explain: 0,
                counts: Some(db.counts_handle()),
                last_error: None,
            })
        }
        Stmt::Insert(ins) => {
            // INSERT: resolve the target table from the catalog, plus the source table for
            // `INSERT ... SELECT` so the SELECT body compiles with real column resolution.
            let pager = db.pager_arc()?;
            let (table, indexes) = resolve_table_and_indexes(&pager, &ins.table)?;
            let (source_table, source_indexes) =
                resolve_insert_source(&pager, &ins.source)?.unwrap_or_default();
            let source_table_ref = (!source_table.name.is_empty()).then_some(&source_table);
            let column_names = ins
                .returning
                .as_deref()
                .map(|r| Returning::new(r, &table))
                .transpose()?
                .map(|ret| ret.column_names())
                .unwrap_or_default();
            let program = Arc::new(codegen::compile_insert(
                &ins,
                &table,
                &indexes,
                source_table_ref,
                &source_indexes,
            )?);
            let vdbe = vdbe_for(Arc::clone(&program), Some(pager), db);
            Ok(Sqlite3Stmt {
                sql: sql.to_string(),
                program,
                column_names,
                backing: Backing::Vdbe(vdbe),
                explain: 0,
                counts: Some(db.counts_handle()),
                last_error: None,
            })
        }
        Stmt::Delete(del) => {
            // DELETE: resolve the target table from the catalog and compile a write program.
            let pager = db.pager_arc()?;
            let (table, indexes) = resolve_table_and_indexes(&pager, &del.table)?;
            let column_names = del
                .returning
                .as_deref()
                .map(|r| Returning::new(r, &table))
                .transpose()?
                .map(|ret| ret.column_names())
                .unwrap_or_default();
            let program = Arc::new(codegen::compile_delete(&del, &table, &indexes)?);
            let vdbe = vdbe_for(Arc::clone(&program), Some(pager), db);
            Ok(Sqlite3Stmt {
                sql: sql.to_string(),
                program,
                column_names,
                backing: Backing::Vdbe(vdbe),
                explain: 0,
                counts: Some(db.counts_handle()),
                last_error: None,
            })
        }
        Stmt::DropTable(drop) => {
            // DROP TABLE: resolve the table (None if missing AND IF EXISTS), then compile
            // a write program that destroys the b-tree and removes the schema row.
            let pager = db.pager_arc()?;
            let (table_opt, schema_cookie) = resolve_drop_target(&pager, &drop)?;
            let program = Arc::new(codegen::compile_drop_table(
                &drop,
                schema_cookie,
                table_opt.as_ref(),
            )?);
            let vdbe = vdbe_for(Arc::clone(&program), Some(pager), db);
            Ok(Sqlite3Stmt {
                sql: sql.to_string(),
                program,
                column_names: Vec::new(),
                backing: Backing::Vdbe(vdbe),
                explain: 0,
                counts: Some(db.counts_handle()),
                last_error: None,
            })
        }
        Stmt::Update(upd) => {
            // UPDATE: resolve the target table from the catalog and compile the two-pass
            // (sorter-as-rowset) write program. The codegen rejects OR actions other than
            // ABORT, schema-qualified names, unknown columns, and (defensively) updates of
            // the rowid-alias column.
            let pager = db.pager_arc()?;
            let (table, indexes) = resolve_table_and_indexes(&pager, &upd.table)?;
            let column_names = upd
                .returning
                .as_deref()
                .map(|r| Returning::new(r, &table))
                .transpose()?
                .map(|ret| ret.column_names())
                .unwrap_or_default();
            let program = Arc::new(codegen::compile_update(&upd, &table, &indexes)?);
            let vdbe = vdbe_for(Arc::clone(&program), Some(pager), db);
            Ok(Sqlite3Stmt {
                sql: sql.to_string(),
                program,
                column_names,
                backing: Backing::Vdbe(vdbe),
                explain: 0,
                counts: Some(db.counts_handle()),
                last_error: None,
            })
        }
        Stmt::CreateIndex(ci) => {
            // CREATE INDEX: ensure the database is open, resolve the target table, then
            // compile a write program that creates the index b-tree, populates it from the
            // table's current rows, and inserts a row into `sqlite_schema`.
            let pager = db.ensure_pager()?;
            let catalog = block_on(read_catalog(&pager))?;
            // `IF NOT EXISTS` against a pre-existing index of the same shape is a no-op.
            if ci.if_not_exists && catalog.find_index(&ci.name).is_some() {
                return Ok(Sqlite3Stmt {
                    sql: sql.to_string(),
                    program: Arc::new(Program::empty()),
                    column_names: Vec::new(),
                    backing: Backing::Vdbe(vdbe_for(Arc::new(Program::empty()), None, db)),
                    explain: 0,
                    counts: Some(db.counts_handle()),
                    last_error: None,
                });
            }
            let table_obj = catalog
                .find_table(&ci.table)
                .ok_or_else(|| Error::msg(format!("no such table: {}", ci.table)))?;
            let table = Table::from_schema_object(table_obj)?;
            let schema_cookie = schema_cookie(&pager);
            let sql_text = create_table_text(sql);
            let program = Arc::new(codegen::compile_create_index(
                &ci,
                &table,
                sql_text,
                schema_cookie,
            )?);
            let vdbe = vdbe_for(Arc::clone(&program), Some(pager), db);
            Ok(Sqlite3Stmt {
                sql: sql.to_string(),
                program,
                column_names: Vec::new(),
                backing: Backing::Vdbe(vdbe),
                explain: 0,
                counts: Some(db.counts_handle()),
                last_error: None,
            })
        }
        Stmt::DropIndex(di) => {
            // DROP INDEX: resolve the target index from the catalog. `IF EXISTS` against
            // a missing index is a no-op; otherwise the codegen errors at compile time.
            let pager = db.pager_arc()?;
            let catalog = block_on(read_catalog(&pager))?;
            let (index, schema_rowid) = resolve_drop_index_target(&pager, &catalog, &di)?;
            let schema_cookie = schema_cookie(&pager);
            let program = Arc::new(codegen::compile_drop_index(
                &di,
                index.as_ref(),
                schema_cookie,
                schema_rowid,
            )?);
            let vdbe = vdbe_for(Arc::clone(&program), Some(pager), db);
            Ok(Sqlite3Stmt {
                sql: sql.to_string(),
                program,
                column_names: Vec::new(),
                backing: Backing::Vdbe(vdbe),
                explain: 0,
                counts: Some(db.counts_handle()),
                last_error: None,
            })
        }
        Stmt::AlterTable(alter) => {
            // ALTER TABLE: resolve the target table from the catalog and dispatch on the
            // action. M14.5 implements `RENAME TO`; M14.6 implements `ADD COLUMN`. The
            // other actions (DROP/RENAME COLUMN, ALTER COLUMN, ADD/DROP CONSTRAINT) are
            // deferred.
            let pager = db.pager_arc()?;
            match &alter.action {
                AlterTableAction::RenameTo(_) => {
                    let (edits, schema_cookie) = resolve_alter_rename_target(&pager, &alter)?;
                    let program = Arc::new(codegen::compile_alter_rename_table(
                        &alter,
                        schema_cookie,
                        &edits,
                    )?);
                    let vdbe = vdbe_for(Arc::clone(&program), Some(pager), db);
                    Ok(Sqlite3Stmt {
                        sql: sql.to_string(),
                        program,
                        column_names: Vec::new(),
                        backing: Backing::Vdbe(vdbe),
                        explain: 0,
                        counts: Some(db.counts_handle()),
                        last_error: None,
                    })
                }
                AlterTableAction::AddColumn(col_def) => {
                    let (table_rowid, old_sql, schema_cookie) =
                        resolve_alter_add_column_target(&pager, &alter, col_def)?;
                    let col_def_text = codegen::alter::extract_add_column_text(sql)
                        .ok_or_else(|| {
                            Error::msg(
                                "cannot extract column definition text from ALTER TABLE statement",
                            )
                        })?;
                    let program = Arc::new(codegen::compile_alter_add_column(
                        &alter,
                        schema_cookie,
                        table_rowid,
                        &old_sql,
                        &col_def_text,
                    )?);
                    let vdbe = vdbe_for(Arc::clone(&program), Some(pager), db);
                    Ok(Sqlite3Stmt {
                        sql: sql.to_string(),
                        program,
                        column_names: Vec::new(),
                        backing: Backing::Vdbe(vdbe),
                        explain: 0,
                        counts: Some(db.counts_handle()),
                        last_error: None,
                    })
                }
                AlterTableAction::DropColumn(col_name) => {
                    let (table, table_rowid, old_sql, schema_cookie) =
                        resolve_alter_drop_column_target(&pager, &alter, &col_name)?;
                    let drop_col_idx = codegen::alter::validate_drop_column(&table, &col_name)?;
                    let drop_col_name_dequoted = codegen::alter::dequote_ident(&col_name);
                    let program = Arc::new(codegen::compile_alter_drop_column(
                        &alter,
                        schema_cookie,
                        &table,
                        table_rowid,
                        &old_sql,
                        drop_col_idx,
                        &drop_col_name_dequoted,
                    )?);
                    let vdbe = vdbe_for(Arc::clone(&program), Some(pager), db);
                    Ok(Sqlite3Stmt {
                        sql: sql.to_string(),
                        program,
                        column_names: Vec::new(),
                        backing: Backing::Vdbe(vdbe),
                        explain: 0,
                        counts: Some(db.counts_handle()),
                        last_error: None,
                    })
                }
                AlterTableAction::RenameColumn { old, new } => {
                    let (edits, schema_cookie) =
                        resolve_alter_rename_column_target(&pager, &alter, &old, &new)?;
                    let program = Arc::new(codegen::compile_alter_rename_column(
                        &alter,
                        schema_cookie,
                        &edits,
                    )?);
                    let vdbe = vdbe_for(Arc::clone(&program), Some(pager), db);
                    Ok(Sqlite3Stmt {
                        sql: sql.to_string(),
                        program,
                        column_names: Vec::new(),
                        backing: Backing::Vdbe(vdbe),
                        explain: 0,
                        counts: Some(db.counts_handle()),
                        last_error: None,
                    })
                }
                _ => Err(Error::msg(format!(
                    "ALTER TABLE action {:?} is not supported yet",
                    alter.action
                ))),
            }
        }
        Stmt::CreateView(cv) => {
            // CREATE VIEW: write a sqlite_schema row with type='view', rootpage=0, and the
            // verbatim CREATE VIEW text. View expansion (M15.5) is deferred.
            let pager = db.ensure_pager()?;
            let catalog = block_on(read_catalog(&pager))?;
            // IF NOT EXISTS against a pre-existing view is a no-op.
            if cv.if_not_exists && catalog.find_view(&cv.name).is_some() {
                return Ok(Sqlite3Stmt {
                    sql: sql.to_string(),
                    program: Arc::new(Program::empty()),
                    column_names: Vec::new(),
                    backing: Backing::Vdbe(vdbe_for(Arc::new(Program::empty()), None, db)),
                    explain: 0,
                    counts: Some(db.counts_handle()),
                    last_error: None,
                });
            }
            // Reject if a table, view, or index with this name already exists.
            if catalog.find_object(&cv.name).is_some() {
                return Err(Error::msg(format!(
                    "there is already another table or index with this name: {}",
                    cv.name
                )));
            }
            let schema_cookie = schema_cookie(&pager);
            let sql_text = create_table_text(sql);
            let program = Arc::new(codegen::compile_create_view(&cv, sql_text, schema_cookie)?);
            let vdbe = vdbe_for(Arc::clone(&program), Some(pager), db);
            Ok(Sqlite3Stmt {
                sql: sql.to_string(),
                program,
                column_names: Vec::new(),
                backing: Backing::Vdbe(vdbe),
                explain: 0,
                counts: Some(db.counts_handle()),
                last_error: None,
            })
        }
        Stmt::DropView(dv) => {
            // DROP VIEW: resolve the view's sqlite_schema rowid, then compile a write
            // program that deletes it. IF EXISTS against a missing view is a no-op.
            let pager = db.pager_arc()?;
            let catalog = block_on(read_catalog(&pager))?;
            let mut schema_rowid = 0i64;
            let mut found = false;
            for obj in &catalog.objects {
                if obj.obj_type == "view" && dequote_ident(&obj.name).eq_ignore_ascii_case(&dequote_ident(&dv.name)) {
                    schema_rowid = obj.rowid;
                    found = true;
                    break;
                }
            }
            if !found && !dv.if_exists {
                return Err(Error::msg(format!("no such view: {}", dv.name)));
            }
            let schema_cookie = schema_cookie(&pager);
            let program = Arc::new(codegen::compile_drop_view(&dv, schema_cookie, schema_rowid)?);
            let vdbe = vdbe_for(Arc::clone(&program), Some(pager), db);
            Ok(Sqlite3Stmt {
                sql: sql.to_string(),
                program,
                column_names: Vec::new(),
                backing: Backing::Vdbe(vdbe),
                explain: 0,
                counts: Some(db.counts_handle()),
                last_error: None,
            })
        }
        Stmt::CreateTrigger(ct) => {
            // CREATE TRIGGER: write a sqlite_schema row with type='trigger', rootpage=0,
            // and the verbatim CREATE TRIGGER text. Trigger firing (M16.9+) is deferred.
            let pager = db.ensure_pager()?;
            let catalog = block_on(read_catalog(&pager))?;
            // The target table must exist.
            if catalog.find_table(&ct.table).is_none() {
                return Err(Error::msg(format!("no such table: {}", ct.table)));
            }
            // IF NOT EXISTS against a pre-existing trigger is a no-op.
            if ct.if_not_exists && catalog.find_object(&ct.name).is_some() {
                return Ok(Sqlite3Stmt {
                    sql: sql.to_string(),
                    program: Arc::new(Program::empty()),
                    column_names: Vec::new(),
                    backing: Backing::Vdbe(vdbe_for(Arc::new(Program::empty()), None, db)),
                    explain: 0,
                    counts: Some(db.counts_handle()),
                    last_error: None,
                });
            }
            // Reject if an object with this name already exists.
            if catalog.find_object(&ct.name).is_some() {
                return Err(Error::msg(format!(
                    "there is already another table or index with this name: {}",
                    ct.name
                )));
            }
            let schema_cookie = schema_cookie(&pager);
            let sql_text = create_table_text(sql);
            let program = Arc::new(codegen::compile_create_trigger(&ct, sql_text, schema_cookie)?);
            let vdbe = vdbe_for(Arc::clone(&program), Some(pager), db);
            Ok(Sqlite3Stmt {
                sql: sql.to_string(),
                program,
                column_names: Vec::new(),
                backing: Backing::Vdbe(vdbe),
                explain: 0,
                counts: Some(db.counts_handle()),
                last_error: None,
            })
        }
        Stmt::DropTrigger(dt) => {
            // DROP TRIGGER: resolve the trigger's sqlite_schema rowid, then compile a write
            // program that deletes it. IF EXISTS against a missing trigger is a no-op.
            let pager = db.pager_arc()?;
            let catalog = block_on(read_catalog(&pager))?;
            let mut schema_rowid = 0i64;
            let mut found = false;
            for obj in &catalog.objects {
                if obj.obj_type == "trigger"
                    && dequote_ident(&obj.name).eq_ignore_ascii_case(&dequote_ident(&dt.name))
                {
                    schema_rowid = obj.rowid;
                    found = true;
                    break;
                }
            }
            if !found && !dt.if_exists {
                return Err(Error::msg(format!("no such trigger: {}", dt.name)));
            }
            let schema_cookie = schema_cookie(&pager);
            let program = Arc::new(codegen::compile_drop_trigger(&dt, schema_cookie, schema_rowid)?);
            let vdbe = vdbe_for(Arc::clone(&program), Some(pager), db);
            Ok(Sqlite3Stmt {
                sql: sql.to_string(),
                program,
                column_names: Vec::new(),
                backing: Backing::Vdbe(vdbe),
                explain: 0,
                counts: Some(db.counts_handle()),
                last_error: None,
            })
        }
        Stmt::Pragma(pragma) => {
            // PRAGMA codegen is implemented inline here for the auto_vacuum family
            // (M5.3.7). Other pragmas remain deferred to M20.
            compile_pragma(db, sql, &pragma)
        }
        Stmt::Transaction(tx) => {
            // BEGIN / COMMIT / END / ROLLBACK / SAVEPOINT / RELEASE / ROLLBACK TO SAVEPOINT.
            // The M12 first slice handles BEGIN/COMMIT/END/ROLLBACK via OP_AutoCommit; the
            // SAVEPOINT family is rejected at codegen time (the pager savepoint stack is
            // M12.4/M12.5). The program is one OP_AutoCommit instruction (terminal — it halts
            // the VDBE itself, no trailing Halt needed).
            let program = Arc::new(codegen::compile_transaction(&tx)?);
            // The pager is only needed for COMMIT/ROLLBACK (which commit/rollback a pending
            // write transaction in the pager). For BEGIN/SAVEPOINT/RELEASE the program never
            // touches the pager, but passing it is harmless. If the database has no pages
            // yet (no DDL has ever run), COMMIT/ROLLBACK against an empty DB is a no-op
            // (there's no write transaction to commit). For BEGIN against an empty DB we
            // pass `None` so OP_AutoCommit doesn't try to consult a missing pager.
            let pager = db.pager_arc().ok();
            let vdbe = vdbe_for(Arc::clone(&program), pager, db);
            Ok(Sqlite3Stmt {
                sql: sql.to_string(),
                program,
                column_names: Vec::new(),
                backing: Backing::Vdbe(vdbe),
                explain: 0,
                counts: None,
                last_error: None,
            })
        }
        Stmt::Attach(_) => {
            // ATTACH parsing is implemented (M2.38) but codegen is deferred to M21.
            Err(Error::msg("ATTACH is not supported yet"))
        }
        Stmt::Detach(_) => {
            // DETACH parsing is implemented (M2.39) but codegen is deferred to M21.
            Err(Error::msg("DETACH is not supported yet"))
        }
        Stmt::Vacuum(_) => {
            // VACUUM parsing is implemented (M2.40) but codegen is deferred to M22.
            Err(Error::msg("VACUUM is not supported yet"))
        }
        Stmt::Analyze(_) => {
            // ANALYZE parsing is implemented (M2.41) but codegen is deferred to M22.
            Err(Error::msg("ANALYZE is not supported yet"))
        }
        Stmt::Reindex(_) => {
            // REINDEX parsing is implemented (M2.42) but codegen is deferred to M22.
            Err(Error::msg("REINDEX is not supported yet"))
        }
        Stmt::CreateVirtualTable(_) => {
            // CREATE VIRTUAL TABLE parsing is implemented (M2.43) but codegen is deferred to M31.
            Err(Error::msg("CREATE VIRTUAL TABLE is not supported yet"))
        }
    }
}

/// Resolve the source table and its indexes for an `INSERT ... SELECT`. Returns `None` for
/// `VALUES` or constant SELECT sources (no real FROM table), otherwise the resolved table plus
/// its attached indexes.
fn resolve_insert_source(
    pager: &Arc<Pager>,
    source: &InsertSource,
) -> Result<Option<(Table, Vec<IndexObject>)>> {
    let select = match source {
        InsertSource::Values(_) | InsertSource::DefaultValues => return Ok(None),
        InsertSource::Select(s) => s,
    };
    // A constant SELECT has no FROM clause.
    let first = match select.from.first() {
        Some(f) => f,
        None => return Ok(None),
    };
    if select.from.len() > 1 {
        return Err(Error::msg("joins are not supported"));
    }
    let table_ref = match first.table() {
        Some(t) => t,
        None => {
            return Err(Error::msg(
                "subqueries are not supported in INSERT ... SELECT",
            ))
        }
    };
    let catalog = block_on(read_catalog(pager))?;
    let table_obj = catalog
        .find_table(&table_ref.name)
        .ok_or_else(|| Error::msg(format!("no such table: {}", table_ref.name)))?;
    let table = Table::from_schema_object(table_obj)?;
    let mut indexes = Vec::new();
    for obj in catalog.indexes() {
        if obj.tbl_name.eq_ignore_ascii_case(&table_ref.name) {
            indexes.push(IndexObject::from_schema_object(obj)?);
        }
    }
    Ok(Some((table, indexes)))
}

/// Resolve the table a `DELETE` targets from the current catalog, plus the list of indexes
/// attached to that table. Used by INSERT / UPDATE / DELETE to drive the index maintenance
/// that the codegen emits.
fn resolve_table_and_indexes(
    pager: &Arc<Pager>,
    table_name: &str,
) -> Result<(Table, Vec<IndexObject>)> {
    let catalog = block_on(read_catalog(pager))?;
    let table_obj = catalog
        .find_table(table_name)
        .ok_or_else(|| Error::msg(format!("no such table: {table_name}")))?;
    let table = Table::from_schema_object(table_obj)?;
    let mut indexes = Vec::new();
    for obj in catalog.indexes() {
        if obj.tbl_name.eq_ignore_ascii_case(table_name) {
            indexes.push(IndexObject::from_schema_object(obj)?);
        }
    }
    Ok((table, indexes))
}

/// Resolve the implicit `sqlite_schema` (alias `sqlite_master`) table for a `SELECT` against
/// the catalog. The page-1 b-tree IS the `sqlite_schema` table; we synthesize a
/// [`Table`] directly (no catalog row is required) so the scan can open rootpage 1 with the
/// known 5-column schema (`type, name, tbl_name, rootpage, sql`).
fn resolve_sqlite_schema(pager: &Arc<Pager>) -> Result<Table> {
    use crate::types::Affinity;
    let _ = pager; // the pager is the source of truth; the table shape is hard-coded
    Ok(Table {
        name: "sqlite_schema".to_string(),
        rootpage: 1,
        columns: vec![
            crate::schema::Column {
                name: "type".to_string(),
                affinity: Affinity::Text,
                collation: crate::types::Collation::Binary,
                notnull: false,
                pk: false,
                default: None,
                notnull_oe: crate::vdbe::oe::OeAction::None,
            },
            crate::schema::Column {
                name: "name".to_string(),
                affinity: Affinity::Text,
                collation: crate::types::Collation::Binary,
                notnull: false,
                pk: false,
                default: None,
                notnull_oe: crate::vdbe::oe::OeAction::None,
            },
            crate::schema::Column {
                name: "tbl_name".to_string(),
                affinity: Affinity::Text,
                collation: crate::types::Collation::Binary,
                notnull: false,
                pk: false,
                default: None,
                notnull_oe: crate::vdbe::oe::OeAction::None,
            },
            crate::schema::Column {
                name: "rootpage".to_string(),
                affinity: Affinity::Integer,
                collation: crate::types::Collation::Binary,
                notnull: false,
                pk: false,
                default: None,
                notnull_oe: crate::vdbe::oe::OeAction::None,
            },
            crate::schema::Column {
                name: "sql".to_string(),
                affinity: Affinity::Text,
                collation: crate::types::Collation::Binary,
                notnull: false,
                pk: false,
                default: None,
                notnull_oe: crate::vdbe::oe::OeAction::None,
            },
        ],
        rowid_alias: None,
        without_rowid: false,
        pk_columns: Vec::new(),
    })
}

/// Resolve a `DROP TABLE` target: returns the table when present in the catalog (else
/// `None`, which the codegen turns into either an error or a no-op depending on the
/// `IF EXISTS` flag), and the current schema cookie for the codegen to bump.
fn resolve_drop_target(pager: &Arc<Pager>, drop: &DropTableStmt) -> Result<(Option<Table>, u32)> {
    let catalog = block_on(read_catalog(pager))?;
    let cookie = schema_cookie(pager);
    let table = catalog
        .find_table(&drop.name)
        .map(|obj| Table::from_schema_object(obj))
        .transpose()?;
    Ok((table, cookie))
}

/// Resolve an `ALTER TABLE … RENAME TO` target: validates the table exists, checks the new
/// name doesn't collide, and produces the list of `sqlite_schema` row edits the codegen
/// should perform. Returns `(edits, schema_cookie)`.
///
/// `RENAME TO new_name` collects:
///   * the table row itself — `name` and `tbl_name` set to `new_name`, `sql` rewritten,
///   * every associated index/trigger row whose `tbl_name` matches the old name — `tbl_name`
///     set to `new_name`, `sql` rewritten (when the rewrite succeeds).
fn resolve_alter_rename_target(
    pager: &Arc<Pager>,
    alter: &AlterTableStmt,
) -> Result<(Vec<codegen::alter::SchemaRowEdit>, u32)> {
    if alter.schema.is_some() {
        return Err(Error::msg(
            "schema-qualified ALTER TABLE is not yet supported",
        ));
    }
    let catalog = block_on(read_catalog(pager))?;
    let cookie = schema_cookie(pager);
    let new_name = match &alter.action {
        AlterTableAction::RenameTo(n) => n.clone(),
        _ => {
            return Err(Error::msg(format!(
                "ALTER TABLE action {:?} is not supported yet",
                alter.action
            )));
        }
    };
    // Reject renaming a system table (sqlite_*) — upstream's `isAlterableTable`.
    if alter.table.starts_with("sqlite_") {
        return Err(Error::msg(format!("table {} may not be altered", alter.table)));
    }
    // The table must exist.
    let table_obj = catalog
        .find_table(&alter.table)
        .ok_or_else(|| Error::msg(format!("no such table: {}", alter.table)))?;
    // The new name must not collide with an existing table or index.
    if catalog.find_table(&new_name).is_some()
        || catalog.find_index(&new_name).is_some()
    {
        return Err(Error::msg(format!(
            "there is already another table or index with this name: {}",
            new_name
        )));
    }
    // Reject reserved-name targets (sqlite_*).
    if new_name.starts_with("sqlite_") {
        return Err(Error::msg(format!(
            "object name reserved for internal use: {}",
            new_name
        )));
    }

    let old_name = &alter.table;
    // Dequote the new name: SQLite stores the *dequoted* form in the `name`/`tbl_name`
    // columns of `sqlite_schema` (the parser keeps the quote characters in the AST string,
    // which is correct for the `sql` column text but not for the name columns).
    let new_name_dequoted = codegen::alter::dequote_ident(&new_name);
    let old_name_dequoted = codegen::alter::dequote_ident(old_name);
    let mut edits: Vec<codegen::alter::SchemaRowEdit> = Vec::new();

    // The table row: update name, tbl_name, and sql.
    let table_sql_rewrite = table_obj
        .sql
        .as_deref()
        .and_then(|s| {
            codegen::alter::rewrite_table_name_in_sql(s, &old_name_dequoted, &new_name_dequoted)
        });
    let table_edit = codegen::alter::SchemaRowEdit {
        rowid: table_obj.rowid,
        new_name: Some(new_name_dequoted.clone()),
        new_tbl_name: Some(new_name_dequoted.clone()),
        new_sql: table_sql_rewrite,
    };
    edits.push(table_edit);

    // Associated rows (indexes, triggers) whose tbl_name matches the old name: update
    // tbl_name and rewrite sql. The `name` column of these rows is NOT changed (the index
    // keeps its own name; only the table-association changes).
    for obj in &catalog.objects {
        if obj.rowid == table_obj.rowid {
            continue; // already handled above
        }
        if !dequote_ident(&obj.tbl_name).eq_ignore_ascii_case(&old_name_dequoted) {
            continue;
        }
        // Only rewrite index/trigger rows whose tbl_name matches. (Views are separate.)
        if !(obj.is_index() || obj.obj_type == "trigger") {
            continue;
        }
        let sql_rewrite = obj
            .sql
            .as_deref()
            .and_then(|s| {
                codegen::alter::rewrite_table_name_in_sql(s, &old_name_dequoted, &new_name_dequoted)
            });
        edits.push(codegen::alter::SchemaRowEdit {
            rowid: obj.rowid,
            new_name: None,
            new_tbl_name: Some(new_name_dequoted.clone()),
            new_sql: sql_rewrite,
        });
    }

    Ok((edits, cookie))
}

/// Resolve an `ALTER TABLE … ADD [COLUMN] <def>` target: validates the table exists,
/// validates the new column is legal for ADD COLUMN, and returns
/// `(table_rowid, old_sql, schema_cookie)` for the codegen.
fn resolve_alter_add_column_target(
    pager: &Arc<Pager>,
    alter: &AlterTableStmt,
    col_def: &rustqlite_parser::ColumnDef,
) -> Result<(i64, String, u32)> {
    if alter.schema.is_some() {
        return Err(Error::msg(
            "schema-qualified ALTER TABLE is not yet supported",
        ));
    }
    if alter.table.starts_with("sqlite_") {
        return Err(Error::msg(format!("table {} may not be altered", alter.table)));
    }
    // Validate the column def is legal for ADD COLUMN.
    codegen::alter::validate_add_column(col_def)?;
    let catalog = block_on(read_catalog(pager))?;
    let cookie = schema_cookie(pager);
    let table_obj = catalog
        .find_table(&alter.table)
        .ok_or_else(|| Error::msg(format!("no such table: {}", alter.table)))?;
    let old_sql = table_obj
        .sql
        .as_ref()
        .ok_or_else(|| Error::msg(format!("table \"{}\" has no CREATE statement", alter.table)))?
        .clone();
    Ok((table_obj.rowid, old_sql, cookie))
}

/// Resolve an `ALTER TABLE … DROP [COLUMN] <name>` target: validates the table exists,
/// validates the column can be dropped, and returns `(table, table_rowid, old_sql,
/// schema_cookie)` for the codegen.
fn resolve_alter_drop_column_target(
    pager: &Arc<Pager>,
    alter: &AlterTableStmt,
    _col_name: &str,
) -> Result<(Table, i64, String, u32)> {
    if alter.schema.is_some() {
        return Err(Error::msg(
            "schema-qualified ALTER TABLE is not yet supported",
        ));
    }
    if alter.table.starts_with("sqlite_") {
        return Err(Error::msg(format!("table {} may not be altered", alter.table)));
    }
    let catalog = block_on(read_catalog(pager))?;
    let cookie = schema_cookie(pager);
    let table_obj = catalog
        .find_table(&alter.table)
        .ok_or_else(|| Error::msg(format!("no such table: {}", alter.table)))?;
    let table = Table::from_schema_object(table_obj)?;
    let old_sql = table_obj
        .sql
        .as_ref()
        .ok_or_else(|| Error::msg(format!("table \"{}\" has no CREATE statement", alter.table)))?
        .clone();
    Ok((table, table_obj.rowid, old_sql, cookie))
}

/// Resolve an `ALTER TABLE … RENAME [COLUMN] <old> TO <new>` target: validates the table
/// and column exist, and produces the list of `sqlite_schema` row edits (the table row +
/// every associated index/trigger row whose `sql` references the column). Returns
/// `(edits, schema_cookie)`.
fn resolve_alter_rename_column_target(
    pager: &Arc<Pager>,
    alter: &AlterTableStmt,
    old_name: &str,
    new_name: &str,
) -> Result<(Vec<codegen::alter::SchemaRowEdit>, u32)> {
    if alter.schema.is_some() {
        return Err(Error::msg(
            "schema-qualified ALTER TABLE is not yet supported",
        ));
    }
    if alter.table.starts_with("sqlite_") {
        return Err(Error::msg(format!("table {} may not be altered", alter.table)));
    }
    let catalog = block_on(read_catalog(pager))?;
    let cookie = schema_cookie(pager);
    let table_obj = catalog
        .find_table(&alter.table)
        .ok_or_else(|| Error::msg(format!("no such table: {}", alter.table)))?;
    let table = Table::from_schema_object(table_obj)?;
    let old_name_dequoted = codegen::alter::dequote_ident(old_name);
    let new_name_dequoted = codegen::alter::dequote_ident(new_name);
    // The column must exist.
    if table.column_index(&old_name_dequoted).is_none() {
        return Err(Error::msg(format!("no such column: {}", old_name)));
    }
    // The new name must not collide with an existing column.
    if table.column_index(&new_name_dequoted).is_some() {
        return Err(Error::msg(format!(
            "duplicate column name: {}",
            new_name
        )));
    }

    let mut edits: Vec<codegen::alter::SchemaRowEdit> = Vec::new();

    // The table row: rewrite the `sql` column.
    let table_sql_rewrite = table_obj
        .sql
        .as_deref()
        .and_then(|s| {
            codegen::alter::rewrite_column_name_in_sql(s, &old_name_dequoted, &new_name_dequoted)
        });
    edits.push(codegen::alter::SchemaRowEdit {
        rowid: table_obj.rowid,
        new_name: None,
        new_tbl_name: None,
        new_sql: table_sql_rewrite,
    });

    // Associated index/trigger rows whose `tbl_name` matches: rewrite their `sql` to
    // replace the column name.
    for obj in &catalog.objects {
        if obj.rowid == table_obj.rowid {
            continue;
        }
        if !dequote_ident(&obj.tbl_name).eq_ignore_ascii_case(&dequote_ident(&alter.table)) {
            continue;
        }
        if !(obj.is_index() || obj.obj_type == "trigger") {
            continue;
        }
        let sql_rewrite = obj
            .sql
            .as_deref()
            .and_then(|s| {
                codegen::alter::rewrite_column_name_in_sql(s, &old_name_dequoted, &new_name_dequoted)
            });
        if sql_rewrite.is_some() {
            edits.push(codegen::alter::SchemaRowEdit {
                rowid: obj.rowid,
                new_name: None,
                new_tbl_name: None,
                new_sql: sql_rewrite,
            });
        }
    }

    Ok((edits, cookie))
}

/// Resolve the index a `DROP INDEX` targets from the current catalog. Returns
/// `(Some(IndexObject), rowid)` when found, `(None, 0)` when missing and `IF EXISTS` was
/// given. Errors with `no such index` when missing and `IF EXISTS` was not given.
fn resolve_drop_index_target(
    _pager: &Arc<Pager>,
    catalog: &crate::schema::Catalog,
    di: &DropIndexStmt,
) -> Result<(Option<IndexObject>, i64)> {
    // Use the actual b-tree rowid (preserved on each `SchemaObject` by the catalog reader) so
    // the `Delete` opcode targets the right row even when other rows have been deleted.
    for obj in catalog.objects.iter() {
        if obj.is_index()
            && dequote_ident(&obj.name)
                .eq_ignore_ascii_case(&dequote_ident(&di.name))
        {
            let idx = IndexObject::from_schema_object(obj)?;
            return Ok((Some(idx), obj.rowid));
        }
    }
    if di.if_exists {
        Ok((None, 0))
    } else {
        Err(Error::msg(format!("no such index: {}", di.name)))
    }
}

/// Extract the verbatim `CREATE TABLE` text to store in `sqlite_schema.sql`. SQLite stores the
/// user's original statement text (minus a trailing `;` and surrounding whitespace), not a
/// canonicalized form. The first prepared statement is the whole input today (no multi-statement
/// boundary tracking yet), so we trim the buffer and strip one trailing semicolon.
fn create_table_text(sql: &str) -> &str {
    let trimmed = sql.trim();
    trimmed.strip_suffix(';').unwrap_or(trimmed).trim_end()
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
                "EXPLAIN of a non-SELECT statement is not supported",
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
            explain::query_plan_rows(&select, table_name, compiled.index_plan_info.as_ref()),
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
        counts: None,
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
    /// The index-plan summary for `EXPLAIN QUERY PLAN`. `None` for a table scan / VALUES /
    /// constant SELECT.
    index_plan_info: Option<crate::vdbe::explain::IndexPlanInfo>,
}

/// Apply the USING/NATURAL JOIN rewrite to `select` if its top-level FROM join is a
/// USING/NATURAL join. Returns `(rewritten_select, on_predicate)` where `on_predicate`
/// is the existing ON clause (if any) possibly combined with the synthetic
/// `l.col = r.col AND ...` predicate. For a plain ON/cross join, returns the select
/// unchanged and the original ON predicate.
///
/// `flat` is the flattened FROM clause (top-level first). `from_order` is the canonical
/// FROM order (left, right); `join_order` is the outer/inner loop order (which may be
/// swapped for a RIGHT JOIN). The rewrite uses JOIN-order names for the coalesce (so
/// the preserved side wins) and FROM-order tables for the `SELECT *` dedup.
fn rewrite_using_or_natural(
    select: &SelectStmt,
    flat: &[(&rustqlite_parser::TableRef, Option<&rustqlite_parser::JoinConstraint>)],
    from_order: &[(&Table, &str); 2],
    join_order: &[(&Table, &str); 2],
) -> Result<(SelectStmt, Option<rustqlite_parser::Expr>)> {
    use rustqlite_parser::{JoinConstraint, TableOrJoin};
    let _ = flat;

    // Extract the top-level join's op and constraint.
    let (op, constraint) = match select.from.first() {
        Some(TableOrJoin::Join(j)) => (j.op, j.constraint.as_ref()),
        _ => return Ok((select.clone(), None)),
    };
    let on_expr = match constraint {
        Some(JoinConstraint::On(e)) => Some(e.clone()),
        _ => None,
    };
    let using_cols = codegen::join_using::resolve_using_cols(
        join_order[0].0,
        join_order[1].0,
        constraint,
        op,
    )?;
    let Some(using_cols) = using_cols else {
        // Plain ON or cross join — no rewrite.
        return Ok((select.clone(), on_expr));
    };
    if on_expr.is_some() && matches!(constraint, Some(JoinConstraint::Using(_))) {
        // USING and ON together is a syntax error in SQLite (parser catches the ordering);
        // we never reach here because the parser already accepted the shape, but guard
        // anyway.
        return Err(Error::msg("ON clause may not be used with USING"));
    }

    let outer_name = join_order[0].1;
    let inner_name = join_order[1].1;
    let outer_t = join_order[0].0;
    let inner_t = join_order[1].0;

    let mut sel = select.clone();
    // Projection `*` dedup uses FROM-order tables (left, right) — the SECOND table in
    // FROM order loses the using cols.
    sel.columns = codegen::join_using::rewrite_projection(
        &sel,
        &using_cols,
        outer_name,
        inner_name,
        from_order[0].0,
        from_order[1].0,
        from_order[0].1,
        from_order[1].1,
    )?;
    codegen::join_using::rewrite_select_clauses(
        &mut sel,
        &using_cols,
        outer_name,
        inner_name,
        outer_t,
        inner_t,
    )?;
    let synthetic_on = codegen::join_using::synthetic_on(&using_cols, outer_name, inner_name);
    Ok((sel, synthetic_on.or(on_expr)))
}

/// Resolve the single FROM table (if any) from the catalog and compile the SELECT. Shared by the
/// Resolve the single FROM table (if any) from the catalog and compile the SELECT. Shared by the
/// normal SELECT path and the EXPLAIN path.
fn compile_select(db: &mut Sqlite3, select: &SelectStmt) -> Result<CompiledSelect> {
    // CTE rewriting: a `WITH …` clause on the outer SELECT is expanded by rewriting each
    // CTE reference in the FROM clause into a `TableOrJoin::Subquery` (M10.2–M10.5). The
    // rewritten SELECT has its `with_clause` cleared, so this is a one-shot rewrite and
    // downstream codegen sees a plain `FROM (subquery) AS alias` shape that the existing
    // `codegen::subquery::compile_from_subquery` infrastructure handles. Recursive CTEs
    // (M10.3) use a dedicated queue-based codegen path (`codegen::cte::compile_recursive`).
    if let Some(with) = select.with_clause.as_ref() {
        if codegen::cte::is_recursive(with) {
            let cte = codegen::cte::compile_recursive(db, select)?;
            return Ok(CompiledSelect {
                program: cte.program,
                column_names: cte.column_names,
                pager: cte.pager,
                table: None,
                index_plan_info: None,
            });
        }
    }
    let select_owned: SelectStmt;
    let select: &SelectStmt = if codegen::cte::has_ctes(select) {
        select_owned = codegen::cte::rewrite_with_ctes(select)?;
        &select_owned
    } else {
        select
    };

    // `FROM (subquery) AS alias` path: materialize the subquery into an ephemeral table and
    // scan it. Single-entry FROM with a subquery (no joins). M8.6.
    if !select.from.is_empty() && select.values.is_empty() && select.from.len() == 1 {
        if let rustqlite_parser::TableOrJoin::Subquery { query, alias } = &select.from[0] {
            // Resolve the subquery's own FROM table (if any) so the inner body can be compiled.
            // A subquery with a join in its own FROM is not yet supported (M7+).
            let (sub_table, sub_indexes, pager) = resolve_subquery_source(db, query)?;
            let (program, column_names) = codegen::compile_from_subquery(
                select,
                query,
                alias,
                sub_table.as_ref(),
                &sub_indexes,
            )?;
            return Ok(CompiledSelect {
                program,
                column_names,
                pager,
                table: None,
                index_plan_info: None,
            });
        }
    }

    // Multi-table (join) path: when the FROM clause is a cross/inner join of plain tables,
    // resolve each table from the catalog and compile via the join codegen. This returns
    // early; the single-table path below handles the rest.
    if !select.from.is_empty() && !select.values.is_empty() == false {
        if let Some(flat) = codegen::join::flatten_cross_join(&select.from) {
            if flat.len() >= 2 {
                codegen::join::validate_join(&select.from)?;
                let pager = db.pager_arc()?;
                let catalog = block_on(read_catalog(&pager))?;
                let mut resolved: Vec<(Table, String)> = Vec::new();
                for (tref, _constraint) in &flat {
                    let obj = catalog
                        .find_table(&tref.name)
                        .ok_or_else(|| Error::msg(format!("no such table: {}", tref.name)))?;
                    let table = Table::from_schema_object(obj)?;
                    let name = tref.alias.clone().unwrap_or_else(|| tref.name.clone());
                    resolved.push((table, name));
                }
                // The ON predicate for the first join level (the M7 first slice handles one
                // ON predicate; a chain of joins with multiple ONs is deferred). The
                // USING/NATURAL rewrite may replace or augment this with a synthetic ON.
                let _on_predicate = flat
                    .iter()
                    .find_map(|(_, c)| codegen::join::on_predicate(*c));
                let table_refs: Vec<(&Table, &str)> =
                    resolved.iter().map(|(t, n)| (t, n.as_str())).collect();
                let right_join = codegen::join::is_right_join(&select.from);
                let full_join = codegen::join::is_full_join(&select.from);
                // A FULL JOIN is LEFT JOIN (first pass) + a right anti-join (second pass). A
                // RIGHT JOIN is emulated as a LEFT JOIN with swapped tables. `left_join` is
                // true for LEFT/RIGHT/FULL joins (all need NULL-fill on no-match).
                let left_join = codegen::join::is_left_join(&select.from) || right_join || full_join;
                // For a FULL JOIN we do NOT swap tables (it is symmetric). For a RIGHT JOIN we
                // swap so the original right table becomes the outer/left loop.
                let join_order = if full_join {
                    table_refs.clone()
                } else {
                    codegen::join::swap_for_right_join(table_refs.clone(), &select.from)
                };
                let from_order: [(&Table, &str); 2] = table_refs[..2].try_into().unwrap();
                let join_order_arr: [(&Table, &str); 2] = join_order[..2].try_into().unwrap();

                // USING/NATURAL: rewrite the SELECT's projection, WHERE, ORDER BY, GROUP BY,
                // and HAVING to coalesce bare shared-column refs and dedup `SELECT *`. The
                // synthetic ON predicate (`l.col = r.col AND ...`) replaces the USING clause.
                // The rewrite runs against the JOIN-order tables (outer first, inner second)
                // so the coalesce picks the preserved side first (matching SQLite).
                let (select_for_codegen, on_for_codegen) =
                    rewrite_using_or_natural(select, &flat, &from_order, &join_order_arr)?;

                // Name resolution (M2.74): validate every column reference in the
                // (possibly rewritten) SELECT resolves uniquely against the FROM
                // tables. Raises "ambiguous column name" and "no such column" matching
                // the oracle, before codegen emits opcodes. The resolver uses the
                // FROM-order tables (alias if present, else table name) as the scope.
                let resolve_tables: Vec<codegen::resolve::ResolveTable<'_>> = table_refs
                    .iter()
                    .map(|(t, n)| codegen::resolve::ResolveTable {
                        table: *t,
                        name: *n,
                    })
                    .collect();
                codegen::resolve::resolve_select(&select_for_codegen, &resolve_tables, None)?;

                let (program, column_names) = codegen::join::compile_cross_join(
                    &select_for_codegen,
                    &join_order_arr,
                    &from_order,
                    on_for_codegen.as_ref(),
                    left_join,
                    full_join,
                )?;
                return Ok(CompiledSelect {
                    program,
                    column_names,
                    pager: Some(pager),
                    table: Some(resolved[0].0.clone()),
                    index_plan_info: None,
                });
            }
        }
    }

    let (table, pager, indexes) = if !select.values.is_empty() {
        // VALUES select bodies never have a real FROM table; run them without a pager/database.
        (None, None, Vec::new())
    } else if let Some(table_or_join) = select.from.first() {
        if select.from.len() > 1 {
            return Err(Error::msg("joins are not supported"));
        }
        let Some(table_ref) = table_or_join.table() else {
            // Subqueries in FROM (including parenthesised VALUES) are not yet executable;
            // reject with the same message as joins until M8 materializes them.
            return Err(Error::msg("joins are not supported"));
        };
        // The implicit `sqlite_schema` / `sqlite_master` table lives at page 1 and is not
        // listed in the catalog (it IS the catalog); synthesize a `Table` for it directly.
        if table_ref.name.eq_ignore_ascii_case("sqlite_schema")
            || table_ref.name.eq_ignore_ascii_case("sqlite_master")
        {
            let pager = db.pager_arc()?;
            let table = resolve_sqlite_schema(&pager)?;
            (Some(table), Some(pager), Vec::new())
        } else {
            let pager = db.pager_arc()?;
            let catalog = block_on(read_catalog(&pager))?;
            let obj = catalog
                .find_table(&table_ref.name)
                .ok_or_else(|| Error::msg(format!("no such table: {}", table_ref.name)))?;
            let table = Table::from_schema_object(obj)?;
            let mut indexes = Vec::new();
            for obj in catalog.indexes() {
                if obj.tbl_name.eq_ignore_ascii_case(&table_ref.name) {
                    indexes.push(IndexObject::from_schema_object(obj)?);
                }
            }
            (Some(table), Some(pager), indexes)
        }
    } else {
        (None, None, Vec::new())
    };

    // Name resolution (M2.74): validate every column reference in the SELECT resolves
    // uniquely against the FROM table. Raises "no such column" matching the oracle,
    // before codegen emits opcodes. For a single-table SELECT there can be no
    // ambiguous-column error (only one FROM table), so this only catches
    // `no-such-column` and qualified-`tbl.col` mistakes here — the ambiguous-column
    // path is exercised by the join path above. Skip when the FROM is a subquery
    // (the subquery codegen paths resolve their own column refs).
    if let Some(t) = table.as_ref() {
        if let Some(tref) = select.from.first().and_then(|tj| tj.table()) {
            let alias = tref.alias.as_deref().unwrap_or(t.name.as_str());
            let resolve_tables = vec![codegen::resolve::ResolveTable {
                table: t,
                name: alias,
            }];
            // Skip when the FROM is a subquery (no `table()`); the subquery paths
            // resolve their own refs. The `tref` here is guaranteed to be the FROM
            // table because we're in the single-table branch.
            codegen::resolve::resolve_select(select, &resolve_tables, None)?;
        }
    }

    // Build a subquery resolver over the connection's pager so expression codegen can compile
    // scalar subqueries / EXISTS / IN (SELECT ...) against the catalog. Even a FROM-less outer
    // SELECT (e.g. `SELECT (SELECT a FROM t)`) may have a subquery that scans a real table, so
    // we obtain the pager even when the outer SELECT itself has no FROM. Only fall back to the
    // no-DB resolver when the connection truly has no open database (a `:memory:` that was
    // never written to, or a real `Sqlite3` with no pager — both rare).
    let (resolver, resolver_pager): (Box<dyn SubqueryResolver>, Option<Arc<Pager>>) =
        match db.pager_arc() {
            Ok(p) => (Box::new(CatalogSubqueryResolver { pager: p.clone() }), Some(p)),
            Err(_) => (Box::new(NoDbSubqueryResolver), None),
        };
    let (program, column_names) =
        codegen::compile_select(select, table.as_ref(), &indexes, Some(resolver.as_ref()))?;
    let index_plan_info = codegen::select_index_plan_info(select, table.as_ref(), &indexes);
    // If the outer SELECT itself has no FROM, its `pager` is `None` — but a scalar subquery in
    // its projection may have needed the pager to scan a real table. The inlined subquery body
    // emits `OpenRead` opcodes that the VDBE must be able to satisfy, so when the resolver
    // obtained a pager, attach it to the VDBE even when the outer SELECT's own `pager` is
    // `None`. (When both are `Some`, they are the same pager; when both are `None`, the
    // statement truly needs no database.)
    let pager = pager.or(resolver_pager);
    Ok(CompiledSelect {
        program,
        column_names,
        pager,
        table,
        index_plan_info,
    })
}

/// A [`SubqueryResolver`] for a FROM-less (constant) outer SELECT — the only subqueries it can
/// resolve are themselves constant / `VALUES` (no `FROM`), so it returns `(None, [])` for those
/// and errors with "no such table" for anything that tries to reference a real table. This
/// keeps `SELECT (SELECT 1)` working without an open database.
struct NoDbSubqueryResolver;

impl SubqueryResolver for NoDbSubqueryResolver {
    fn resolve(&self, subquery: &SelectStmt) -> Result<(Option<Table>, Vec<IndexObject>)> {
        if !subquery.values.is_empty() || subquery.from.is_empty() {
            return Ok((None, Vec::new()));
        }
        // A subquery that references a real table can't be resolved without a pager. Surface
        // a clear error rather than panicking.
        Err(Error::msg("no such table: database is not open"))
    }
}

/// Resolve the inner FROM table (if any) for a `FROM (subquery)` subquery, so the subquery
/// body can be compiled. Returns `(table, indexes, pager)` where `table` is `None` for a
/// constant / `VALUES` subquery (no FROM) and `pager` is `None` only in that case.
///
/// The M8.6 first slice supports a subquery with zero or one plain-table FROM entries. A
/// subquery with a join in its own FROM is rejected here; it lands with later M7/M8 work.
fn resolve_subquery_source(
    db: &mut Sqlite3,
    subquery: &SelectStmt,
) -> Result<(Option<Table>, Vec<IndexObject>, Option<Arc<Pager>>)> {
    if !subquery.values.is_empty() {
        // A VALUES subquery has no real FROM table.
        return Ok((None, Vec::new(), None));
    }
    if subquery.from.is_empty() {
        // A constant SELECT (no FROM) — no source table needed.
        return Ok((None, Vec::new(), None));
    }
    if subquery.from.len() > 1 {
        return Err(Error::msg("joins inside a FROM subquery are not supported yet"));
    }
    let Some(table_ref) = subquery.from[0].table() else {
        return Err(Error::msg("nested subqueries in FROM are not supported yet"));
    };
    // The implicit `sqlite_schema` table — synthesize it directly (no catalog row).
    if table_ref.name.eq_ignore_ascii_case("sqlite_schema")
        || table_ref.name.eq_ignore_ascii_case("sqlite_master")
    {
        let pager = db.pager_arc()?;
        let table = resolve_sqlite_schema(&pager)?;
        return Ok((Some(table), Vec::new(), Some(pager)));
    }
    let pager = db.pager_arc()?;
    let catalog = block_on(read_catalog(&pager))?;
    let obj = catalog
        .find_table(&table_ref.name)
        .ok_or_else(|| Error::msg(format!("no such table: {}", table_ref.name)))?;
    let table = Table::from_schema_object(obj)?;
    let mut indexes = Vec::new();
    for obj in catalog.indexes() {
        if obj.tbl_name.eq_ignore_ascii_case(&table_ref.name) {
            indexes.push(IndexObject::from_schema_object(obj)?);
        }
    }
    Ok((Some(table), indexes, Some(pager)))
}

impl Sqlite3Stmt {
    /// `sqlite3_step()` — advance the statement, returning `Row` (a result row is available),
    /// `Done`, or `Error`.
    pub fn step(&mut self) -> ResultCode {
        match &mut self.backing {
            Backing::Vdbe(vdbe) => match block_on(vdbe.step()) {
                Ok(StepResult::Row) => ResultCode::Row,
                Ok(StepResult::Done) => {
                    // A write program publishes its change counters to the connection when it
                    // finishes (mirrors `db->nChange`/`db->lastRowid` updated at statement end).
                    // Taken (not just read) so re-stepping a finished statement does not double the
                    // running `total_changes`.
                    if let Some(counts) = self.counts.take() {
                        let (changes, _total, last_rowid, did_insert) = vdbe.change_counts();
                        let mut c = counts.lock().unwrap();
                        c.changes = changes;
                        c.total_changes += changes;
                        if did_insert {
                            c.last_insert_rowid = last_rowid;
                        }
                    }
                    ResultCode::Done
                }
                Err(e) => {
                    // Surface the error's own result code (e.g. `SQLITE_BUSY` from a lock
                    // contention) rather than collapsing every error to `SQLITE_ERROR`.
                    // Mirrors `sqlite3_step`'s contract of returning the primary code.
                    let code = e.code;
                    self.last_error = Some(e);
                    code
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

/// Compile a `PRAGMA` statement. M5.3.7 implements the `auto_vacuum` family
/// (`auto_vacuum` and `incremental_vacuum`); other pragmas remain deferred to M20 and return
/// an "unsupported PRAGMA" error.
///
/// `PRAGMA auto_vacuum` (read) returns the current mode as a single-row, single-column result
/// (0 = NONE, 1 = FULL, 2 = INCREMENTAL). `PRAGMA auto_vacuum = N` sets the mode; this is only
/// allowed before the database has been written (matching upstream's `BTS_PAGESIZE_FIXED`
/// guard, which is set once page 1 is laid down).
///
/// `PRAGMA incremental_vacuum(N)` runs up to N incremental-vacuum steps in a write transaction,
/// returning one result row per step with the new page count. With no argument (or a large N),
/// it runs until the freelist is exhausted.
fn compile_pragma(db: &mut Sqlite3, sql: &str, pragma: &PragmaStmt) -> Result<Sqlite3Stmt> {
    let name = pragma.name.to_ascii_lowercase();
    match name.as_str() {
        "auto_vacuum" => compile_pragma_auto_vacuum(db, sql, pragma),
        "incremental_vacuum" => compile_pragma_incremental_vacuum(db, sql, pragma),
        "integrity_check" | "quick_check" => {
            compile_pragma_integrity_check(db, sql, pragma, name == "quick_check")
        }
        "wal_checkpoint" => compile_pragma_wal_checkpoint(db, sql, pragma),
        "journal_mode" => compile_pragma_journal_mode(db, sql, pragma),
        _ => Err(Error::msg(format!("PRAGMA {name} is not supported yet"))),
    }
}

/// `PRAGMA integrity_check` / `PRAGMA quick_check` — run the integrity check and return the
/// result rows (one row per error, or a single "ok" row when consistent). `quick` skips the
/// overflow-chain and page-reference checks (mirrors `PRAGMA quick_check`).
fn compile_pragma_integrity_check(
    db: &mut Sqlite3,
    sql: &str,
    _pragma: &PragmaStmt,
    quick: bool,
) -> Result<Sqlite3Stmt> {
    let pager = db.ensure_pager()?;
    let rows = block_on(crate::btree::integrity_check::integrity_check(&pager, quick))?;
    Ok(Sqlite3Stmt {
        sql: sql.to_string(),
        program: Arc::new(Program::empty()),
        column_names: vec!["integrity_check".to_string()],
        backing: Backing::Static {
            rows,
            cur: None,
            pos: 0,
        },
        explain: 0,
        counts: None,
        last_error: None,
    })
}

/// `PRAGMA wal_checkpoint [ = passive|full|restart|truncate ]` — checkpoint the WAL into the
/// database file, returning a single row of three columns: `busy` (0/1), `log` (frames in the
/// WAL), `checkpointed` (frames backfilled). Mirrors the `PragTyp_WAL_CHECKPOINT` handler in
/// `pragma.c` which emits `OP_Checkpoint iBt eMode 1` + `OP_ResultRow 1 3`.
///
/// The mode maps to `OP_Checkpoint`'s `p2`: PASSIVE=0, FULL=1, RESTART=2, TRUNCATE=3
/// (the `SQLITE_CHECKPOINT_*` constants). `PRAGMA wal_checkpoint` (no value) defaults to
/// PASSIVE. An unknown mode name is silently treated as PASSIVE (matching upstream's ladder
/// that falls through to the default `eMode = SQLITE_CHECKPOINT_PASSIVE`).
///
/// If the database is not in WAL mode (no `-wal` sidecar / the pager's `wal` field is `None`),
/// the checkpoint is a no-op returning `(0, 0, 0)`. Upstream returns the same for a non-WAL
/// database (it opens the WAL lazily; when there is no WAL, `mxFrame == 0` and `pnLog == 0`).
fn compile_pragma_wal_checkpoint(
    db: &mut Sqlite3,
    sql: &str,
    pragma: &PragmaStmt,
) -> Result<Sqlite3Stmt> {
    use crate::pager::wal::CheckpointMode;
    // Parse the mode from the value (if any). Upstream accepts `= <mode>` or `(<mode>)` and
    // defaults to PASSIVE. Unknown names fall through to PASSIVE.
    let mode = match &pragma.value {
        Some(PragmaValue::Equal(kind)) | Some(PragmaValue::Paren(kind)) => match kind {
            PragmaValueKind::Ident(s) => {
                CheckpointMode::from_name(s).unwrap_or(CheckpointMode::Passive)
            }
            _ => CheckpointMode::Passive,
        },
        _ => CheckpointMode::Passive,
    };
    // Run the checkpoint synchronously (the engine's async path is driven through `block_on`
    // from the C-API, and a PRAGMA is a one-shot statement — there is no benefit to compiling
    // it into the VDBE here; the result is captured as a Static row set).
    let pager = db.pager_arc()?;
    let (n_log, n_ckpt) = block_on(pager.checkpoint(mode))?;
    let rows = vec![vec![
        Value::Int(0), // busy flag (we never go busy in this iteration)
        Value::Int(n_log as i64),
        Value::Int(n_ckpt as i64),
    ]];
    Ok(Sqlite3Stmt {
        sql: sql.to_string(),
        program: Arc::new(Program::empty()),
        column_names: vec!["busy".to_string(), "log".to_string(), "checkpointed".to_string()],
        backing: Backing::Static {
            rows,
            cur: None,
            pos: 0,
        },
        explain: 0,
        counts: None,
        last_error: None,
    })
}

/// `PRAGMA journal_mode` — read returns the current mode name; set switches the mode.
///
/// Mirrors `PragTyp_JOURNAL_MODE` in `pragma.c`. The read form (no `= value`) returns the
/// current journal mode as a single-row, single-column result (the lowercase mode name:
/// `delete`/`persist`/`off`/`truncate`/`memory`/`wal`). The set form (`= wal`, `= delete`, …)
/// parses the value as one of the mode names (case-insensitive, unambiguous prefix match —
/// `= w` resolves to `wal`, matching upstream's `sqlite3StrNICmp` ladder) and switches the
/// pager's mode via `Pager::set_journal_mode`. An unknown name falls back to a query (the
/// current mode is returned, no change is made), matching upstream.
///
/// The switch is performed synchronously through `block_on` (like `wal_checkpoint`) rather
/// than by emitting an `OP_JournalMode` opcode, because the pragma is a one-shot statement
/// and the engine's existing wal_checkpoint pragma already establishes this synchronous
/// pattern. The result row is captured as a Static row set.
fn compile_pragma_journal_mode(
    db: &mut Sqlite3,
    sql: &str,
    pragma: &PragmaStmt,
) -> Result<Sqlite3Stmt> {
    use crate::pager::JournalMode;
    let pager = db.ensure_pager()?;
    // Parse the value (if any). The set form accepts an identifier or a number (upstream's
    // `nmnum`); numbers are not documented for journal_mode but accepted as a 0-based index
    // — only the identifier path is meaningful here. An unknown name falls back to a query.
    //
    // The parser tokenizes `ON`/`DELETE`/`DEFAULT` as keywords (matching upstream's `nmnum`
    // grammar), so `PRAGMA journal_mode = delete` arrives as `PragmaValueKind::Delete` rather
    // than `Ident("delete")`. Map each keyword to its corresponding mode.
    let requested = match &pragma.value {
        None => JournalMode::Query,
        Some(PragmaValue::Equal(kind)) | Some(PragmaValue::Paren(kind)) => match kind {
            PragmaValueKind::Ident(s) => JournalMode::from_name(s).unwrap_or(JournalMode::Query),
            PragmaValueKind::Delete => JournalMode::Delete,
            PragmaValueKind::On => JournalMode::Query, // `= ON` is not a journal_mode
            PragmaValueKind::Default => JournalMode::Query, // `= DEFAULT` likewise
            PragmaValueKind::Number(lit) => {
                // Upstream treats a numeric value as the index into `azModeName`
                // (0=delete, 1=persist, 2=off, 3=truncate, 4=memory, 5=wal).
                let n = match lit {
                    Literal::Integer(n) => *n as i32,
                    Literal::Real(f) => *f as i32,
                    _ => return Err(Error::msg("PRAGMA journal_mode: invalid numeric value")),
                };
                match n {
                    0 => JournalMode::Delete,
                    1 => JournalMode::Persist,
                    2 => JournalMode::Off,
                    3 => JournalMode::Truncate,
                    4 => JournalMode::Memory,
                    5 => JournalMode::Wal,
                    _ => JournalMode::Query,
                }
            }
        },
    };

    // Upstream refuses a journal_mode change inside a transaction (the `OP_JournalMode`
    // opcode raises "cannot change into/out of wal mode from within a transaction" unless
    // `db->autoCommit && db->nVdbeRead<=1`). We approximate this with the pager's
    // `in_write_txn` check inside `set_journal_mode`; a `BEGIN` outside an autocommit
    // statement would have set that flag. The autocommit-only guard is also enforced there.
    let pager_for_switch = pager.clone();
    let final_mode = block_on(pager_for_switch.set_journal_mode(requested))?;
    let rows = vec![vec![Value::Text(final_mode.name().to_string())]];
    Ok(Sqlite3Stmt {
        sql: sql.to_string(),
        program: Arc::new(Program::empty()),
        column_names: vec!["journal_mode".to_string()],
        backing: Backing::Static {
            rows,
            cur: None,
            pos: 0,
        },
        explain: 0,
        counts: None,
        last_error: None,
    })
}

/// `PRAGMA auto_vacuum` — read returns the current mode (0/1/2); set writes the header flag.
fn compile_pragma_auto_vacuum(
    db: &mut Sqlite3,
    sql: &str,
    pragma: &PragmaStmt,
) -> Result<Sqlite3Stmt> {
    let pager = db.ensure_pager()?;
    match &pragma.value {
        None => {
            // Read: return the current mode.
            let mode = if pager.incr_vacuum() {
                2
            } else if pager.auto_vacuum() {
                1
            } else {
                0
            };
            let rows = vec![vec![Value::Int(mode as i64)]];
            Ok(Sqlite3Stmt {
                sql: sql.to_string(),
                program: Arc::new(Program::empty()),
                column_names: vec!["auto_vacuum".to_string()],
                backing: Backing::Static {
                    rows,
                    cur: None,
                    pos: 0,
                },
                explain: 0,
                counts: None,
                last_error: None,
            })
        }
        Some(PragmaValue::Equal(kind)) | Some(PragmaValue::Paren(kind)) => {
            // Set: parse the value as one of NONE/FULL/INCREMENTAL or 0/1/2.
            let mode = pragma_auto_vacuum_mode(kind)?;
            // The set path must open a write transaction so the header change is committed
            // atomically. We use a tiny program: Transaction + Halt. The header mutation runs
            // before the program (synchronously) via `set_auto_vacuum`, which also refuses to
            // change the mode after the database has been written. `ensure_pager` creates a
            // fresh database file (page 1) on the first write, so setting auto_vacuum before
            // any CREATE TABLE works (mirroring how upstream sets the flag before
            // `BTS_PAGESIZE_FIXED` is set by the first real write).
            let pager = db.ensure_pager()?;
            pager.set_auto_vacuum(mode)?;
            // Mark page 1 dirty so the commit path serializes the updated header into the
            // file. Without this the Transaction+Halt program would commit with no dirty
            // pages and the in-memory header change would be lost.
            {
                let pager = pager.clone();
                block_on(async move {
                    let mut page1 = pager.read_page_for_write(1).await?;
                    let bytes = pager.header().serialize();
                    page1[0..100].copy_from_slice(&bytes);
                    pager.write_page(1, page1)?;
                    Ok::<(), Error>(())
                })?;
            }
            // Build a minimal write program: `Transaction 0 1` + `Halt` commits it.
            let mut p = Program::default();
            p.instructions
                .push(Instruction::new(Opcode::Transaction, 0, 1, 0));
            p.instructions.push(Instruction::new(Opcode::Halt, 0, 0, 0));
            let program = Arc::new(p);
            let vdbe = vdbe_for(Arc::clone(&program), Some(pager), db);
            Ok(Sqlite3Stmt {
                sql: sql.to_string(),
                program,
                column_names: Vec::new(),
                backing: Backing::Vdbe(vdbe),
                explain: 0,
                counts: Some(db.counts_handle()),
                last_error: None,
            })
        }
    }
}

/// Parse the right-hand side of `PRAGMA auto_vacuum = X`: 0/NONE, 1/FULL, 2/INCREMENTAL.
fn pragma_auto_vacuum_mode(kind: &PragmaValueKind) -> Result<u8> {
    match kind {
        PragmaValueKind::Number(lit) => {
            use crate::types::Value;
            let v = match lit {
                Literal::Integer(n) => Value::Int(*n),
                Literal::Real(f) => Value::Real(*f),
                _ => return Err(Error::msg("PRAGMA auto_vacuum: invalid numeric value")),
            };
            let n = match v {
                Value::Int(n) => n as i32,
                Value::Real(f) => f as i32,
                _ => 0,
            };
            if !(0..=2).contains(&n) {
                return Err(Error::msg(format!(
                    "PRAGMA auto_vacuum: {n} out of range (0..2)"
                )));
            }
            Ok(n as u8)
        }
        PragmaValueKind::Ident(s) => match s.to_ascii_lowercase().as_str() {
            "none" => Ok(0),
            "full" => Ok(1),
            "incremental" => Ok(2),
            _ => Err(Error::msg(format!("PRAGMA auto_vacuum: unknown mode '{s}'"))),
        },
        PragmaValueKind::On => Ok(1),
        PragmaValueKind::Delete => Err(Error::msg(
            "PRAGMA auto_vacuum: DELETE is not a valid mode (use NONE/FULL/INCREMENTAL or 0/1/2)",
        )),
        PragmaValueKind::Default => Ok(0),
    }
}

/// `PRAGMA incremental_vacuum(N)` — run up to N steps of incremental vacuum, returning one row
/// per step with the new page count. With no argument, runs until the freelist is exhausted.
fn compile_pragma_incremental_vacuum(
    db: &mut Sqlite3,
    sql: &str,
    pragma: &PragmaStmt,
) -> Result<Sqlite3Stmt> {
    let pager = db.pager_arc()?;
    // Only valid in INCREMENTAL mode.
    if !pager.auto_vacuum() || !pager.incr_vacuum() {
        // Upstream silently no-ops (returns no rows) when incremental vacuum is not enabled.
        return Ok(Sqlite3Stmt {
            sql: sql.to_string(),
            program: Arc::new(Program::empty()),
            column_names: Vec::new(),
            backing: Backing::Static {
                rows: Vec::new(),
                cur: None,
                pos: 0,
            },
            explain: 0,
            counts: None,
            last_error: None,
        });
    }
    // Determine the step limit. Default (no value) is "until done" — use u32::MAX.
    let limit = match &pragma.value {
        None => u32::MAX,
        Some(PragmaValue::Equal(kind)) | Some(PragmaValue::Paren(kind)) => match kind {
            PragmaValueKind::Number(lit) => {
                use crate::types::Value;
                let v = match lit {
                    Literal::Integer(n) => Value::Int(*n),
                    Literal::Real(f) => Value::Real(*f),
                    _ => return Err(Error::msg("PRAGMA incremental_vacuum: invalid value")),
                };
                match v {
                    Value::Int(n) if n > 0 => n as u32,
                    Value::Real(f) if f > 0.0 => f as u32,
                    _ => 0,
                }
            }
            _ => 0,
        },
    };
    // Run the incremental vacuum synchronously: open a write transaction, call
    // `incremental_vacuum` for up to `limit` steps, commit, and capture the resulting page
    // counts as a Static result set.
    let rows = block_on(incremental_vacuum_run(&pager, limit))?;
    Ok(Sqlite3Stmt {
        sql: sql.to_string(),
        program: Arc::new(Program::empty()),
        column_names: vec!["incremental_vacuum".to_string()],
        backing: Backing::Static {
            rows,
            cur: None,
            pos: 0,
        },
        explain: 0,
        counts: None,
        last_error: None,
    })
}

/// Drive an incremental vacuum: open a write transaction, run up to `limit` steps (each
/// relocating one tail page), commit, and return the page-count-after-each-step result rows.
async fn incremental_vacuum_run(pager: &Arc<Pager>, limit: u32) -> Result<Vec<Vec<Value>>> {
    use crate::btree::autovac::incr_vacuum_step_impl;
    pager.begin_write(false).await?;
    let mut rows = Vec::new();
    let usable = pager.usable_size();
    let mut steps = 0u32;
    loop {
        if steps >= limit {
            break;
        }
        let n_orig = pager.page_count();
        let n_free = pager.header().freelist_count;
        if n_free == 0 || n_free >= n_orig {
            break;
        }
        let n_fin = crate::btree::autovac::final_db_size_pub(usable, n_orig, n_free);
        if n_fin >= n_orig {
            break;
        }
        // Find the last non-reserved page.
        let mut i_last = n_orig;
        while i_last > n_fin
            && (crate::btree::ptrmap::is_ptrmap_page(usable, i_last)
                || crate::btree::ptrmap::is_pending_byte_page(usable, i_last))
        {
            i_last -= 1;
        }
        if i_last <= n_fin {
            break;
        }
        match incr_vacuum_step_impl(pager, n_fin, i_last).await {
            Ok(()) => {}
            Err(e) if e.message == "autovacuum done" => break,
            Err(other) => {
                let _ = pager.rollback().await;
                return Err(other);
            }
        }
        steps += 1;
        // After the step, the page count is `n_orig - 1` (one page was relocated away from the
        // end). Upstream's PRAGMA incremental_vacuum yields the new page count per step.
        let new_count = n_orig - 1;
        rows.push(vec![Value::Int(new_count as i64)]);
    }
    // If no steps ran, the freelist is unchanged; just commit. Otherwise the header was
    // updated by the steps; commit persists it.
    pager.commit().await?;
    Ok(rows)
}
