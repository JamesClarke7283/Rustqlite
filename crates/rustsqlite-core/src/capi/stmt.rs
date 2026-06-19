//! The prepared-statement handle — `sqlite3_stmt *` (mirrors `vdbeapi.c` / `prepare.c`).
//!
//! `sqlite3_prepare_v2` runs the parser, resolves the (single) table from the catalog, compiles
//! the `SELECT` to a VDBE [`Program`], and builds a [`Vdbe`] that owns a cloned `Arc<Pager>` —
//! so the statement borrows nothing from the connection (mirroring how a C `sqlite3_stmt` holds
//! its own `db` pointer). `sqlite3_step` drives the async executor via the process-global
//! runtime; the column accessors read the current result row out of the VDBE registers.

use std::sync::{Arc, Mutex};

use rustqlite_parser::{
    parse, DropIndexStmt, DropTableStmt, ExplainKind, InsertSource, SelectStmt, Stmt,
};

use crate::codegen;
use crate::codegen::returning::Returning;
use crate::error::{Error, Result, ResultCode};
use crate::pager::Pager;
use crate::schema::{read_catalog, schema_cookie, IndexObject, Table};
use crate::types::Value;
use crate::vdbe::{explain, Program, StepResult, Vdbe};

use super::connection::{ChangeCounts, Sqlite3};
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
            let vdbe = Vdbe::new(Arc::clone(&program), compiled.pager);
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
            let vdbe = Vdbe::new(Arc::clone(&program), Some(pager));
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
            let vdbe = Vdbe::new(Arc::clone(&program), Some(pager));
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
            let vdbe = Vdbe::new(Arc::clone(&program), Some(pager));
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
            let vdbe = Vdbe::new(Arc::clone(&program), Some(pager));
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
            let vdbe = Vdbe::new(Arc::clone(&program), Some(pager));
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
                    backing: Backing::Vdbe(Vdbe::new(Arc::new(Program::empty()), None)),
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
            let vdbe = Vdbe::new(Arc::clone(&program), Some(pager));
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
            let vdbe = Vdbe::new(Arc::clone(&program), Some(pager));
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
            },
            crate::schema::Column {
                name: "name".to_string(),
                affinity: Affinity::Text,
                collation: crate::types::Collation::Binary,
                notnull: false,
                pk: false,
                default: None,
            },
            crate::schema::Column {
                name: "tbl_name".to_string(),
                affinity: Affinity::Text,
                collation: crate::types::Collation::Binary,
                notnull: false,
                pk: false,
                default: None,
            },
            crate::schema::Column {
                name: "rootpage".to_string(),
                affinity: Affinity::Integer,
                collation: crate::types::Collation::Binary,
                notnull: false,
                pk: false,
                default: None,
            },
            crate::schema::Column {
                name: "sql".to_string(),
                affinity: Affinity::Text,
                collation: crate::types::Collation::Binary,
                notnull: false,
                pk: false,
                default: None,
            },
        ],
        rowid_alias: None,
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
        if obj.is_index() && obj.name.eq_ignore_ascii_case(&di.name) {
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
}

/// Resolve the single FROM table (if any) from the catalog and compile the SELECT. Shared by the
/// Resolve the single FROM table (if any) from the catalog and compile the SELECT. Shared by the
/// normal SELECT path and the EXPLAIN path.
fn compile_select(db: &mut Sqlite3, select: &SelectStmt) -> Result<CompiledSelect> {
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

    let (program, column_names) = codegen::compile_select(select, table.as_ref(), &indexes)?;
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
