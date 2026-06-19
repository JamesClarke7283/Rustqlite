//! Reading the `sqlite_schema` table (mirrors the schema-load paths in `prepare.c`/`build.c`).
//!
//! `sqlite_schema` (historically `sqlite_master`) is an ordinary table b-tree rooted at page 1
//! with five columns: `type, name, tbl_name, rootpage, sql`. Every table, index, view, and
//! trigger in the database has a row here. Reading it is the first thing any connection does.

use crate::btree::scan_table;
use crate::error::Result;
use crate::format::decode_record;
use crate::pager::Pager;
use crate::types::Value;

use super::table::IndexObject;

/// One row of `sqlite_schema`.
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaObject {
    /// The b-tree rowid of this row (preserved by the catalog reader for DDL that needs to
    /// target a specific row with `Delete` / `Update`).
    pub rowid: i64,
    /// `"table"`, `"index"`, `"view"`, or `"trigger"`.
    pub obj_type: String,
    /// The object's name.
    pub name: String,
    /// The table this object is associated with (the table itself for `"table"` rows).
    pub tbl_name: String,
    /// Root b-tree page (0 for views and triggers).
    pub rootpage: i64,
    /// The `CREATE` statement text (NULL for internal objects like `sqlite_sequence`'s rows).
    pub sql: Option<String>,
}

impl SchemaObject {
    fn from_row(rowid: i64, values: &[Value]) -> SchemaObject {
        SchemaObject {
            rowid,
            obj_type: text_at(values, 0).unwrap_or_default(),
            name: text_at(values, 1).unwrap_or_default(),
            tbl_name: text_at(values, 2).unwrap_or_default(),
            rootpage: int_at(values, 3),
            sql: text_at(values, 4),
        }
    }

    pub fn is_table(&self) -> bool {
        self.obj_type == "table"
    }

    pub fn is_index(&self) -> bool {
        self.obj_type == "index"
    }
}

/// The in-memory catalog: all `sqlite_schema` rows, in storage (rowid) order.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    /// One entry per `sqlite_schema` row; the position in the vec is the b-tree order, and
    /// `SchemaObject` carries the root page that was stored in the row. The actual rowid of
    /// the schema row is NOT preserved here (the catalog reader doesn't need it) — DDL code
    /// that needs to delete a specific row finds it by `name` and uses the position-relative
    /// rowid, which only works when the position-relative rowid matches the b-tree rowid. The
    /// first-slice DDL keeps the schema b-tree's insert order aligned with the catalog
    /// enumeration (new rows go to the end, deleted rows are not reused mid-transaction), so
    /// `enumerate_index + 1` is a valid rowid approximation. A faithful port would track the
    /// rowid alongside the parsed row.
    pub objects: Vec<SchemaObject>,
}

impl Catalog {
    /// Iterate the table objects (`type = 'table'`).
    pub fn tables(&self) -> impl Iterator<Item = &SchemaObject> {
        self.objects.iter().filter(|o| o.is_table())
    }

    /// Iterate the index objects (`type = 'index'`).
    pub fn indexes(&self) -> impl Iterator<Item = &SchemaObject> {
        self.objects.iter().filter(|o| o.is_index())
    }

    /// Find a table by name (case-insensitive, as SQLite resolves identifiers).
    pub fn find_table(&self, name: &str) -> Option<&SchemaObject> {
        self.objects
            .iter()
            .find(|o| o.is_table() && o.name.eq_ignore_ascii_case(name))
    }

    /// Find an index by name (case-insensitive).
    pub fn find_index(&self, name: &str) -> Option<&SchemaObject> {
        self.objects
            .iter()
            .find(|o| o.is_index() && o.name.eq_ignore_ascii_case(name))
    }

    /// Find an index on `table_name` that covers `column_name` (a single-column, equality-
    /// usable index). The first match (in catalog order) is returned; multi-column indexes
    /// are skipped in the M5.1 first slice.
    pub fn find_index_for_column(
        &self,
        table_name: &str,
        column_name: &str,
    ) -> Option<&SchemaObject> {
        self.objects.iter().find(|o| {
            o.is_index()
                && o.tbl_name.eq_ignore_ascii_case(table_name)
                && o.sql
                    .as_deref()
                    .is_some_and(|sql| index_covers_column(sql, column_name))
        })
    }
}

/// True when the `CREATE INDEX` SQL covers `column_name` as its first (and only, in M5.1)
/// indexed column. A faithful parse is what the table loader uses for the structural metadata;
/// the column-name match is loose (case-insensitive, accepts un-quoted identifiers) and
/// does not validate the column actually exists on the table — that check lives in the
/// codegen.
pub fn index_covers_column(sql: &str, column_name: &str) -> bool {
    use rustqlite_parser::parse;
    let ast = match parse(sql) {
        Ok(a) => a,
        Err(_) => return false,
    };
    let stmt = match ast.into_iter().next() {
        Some(rustqlite_parser::Stmt::CreateIndex(ci)) => ci,
        _ => return false,
    };
    if stmt.columns.len() != 1 {
        return false;
    }
    // Expression indexes are never matched by a bare column name.
    if stmt.columns[0].expr.is_some() {
        return false;
    }
    stmt.columns[0].name.eq_ignore_ascii_case(column_name)
}

/// Resolve an `IndexObject` (the codegen-facing view of a catalog index row) by name.
pub fn resolve_index_object(catalog: &Catalog, name: &str) -> Result<Option<IndexObject>> {
    let Some(obj) = catalog.find_index(name) else {
        return Ok(None);
    };
    let io = IndexObject::from_schema_object(obj)?;
    Ok(Some(io))
}

/// Resolve an `IndexObject` for `column` on `table` (the first single-column, equality-usable
/// match). Used by the M5.1 planner.
pub fn resolve_index_for_column(
    catalog: &Catalog,
    table_name: &str,
    column_name: &str,
) -> Result<Option<IndexObject>> {
    let Some(obj) = catalog.find_index_for_column(table_name, column_name) else {
        return Ok(None);
    };
    let io = IndexObject::from_schema_object(obj)?;
    Ok(Some(io))
}

/// Read and decode the entire `sqlite_schema` table.
pub async fn read_catalog(pager: &Pager) -> Result<Catalog> {
    let encoding = pager.text_encoding();
    let rows = scan_table(pager, 1).await?;
    let mut objects = Vec::with_capacity(rows.len());
    for (rowid, payload) in rows {
        let values = decode_record(&payload, encoding)?;
        objects.push(SchemaObject::from_row(rowid, &values));
    }
    Ok(Catalog { objects })
}

/// The current schema cookie from the pager's cached header (header bytes 40-43, the value
/// the on-disk `sqlite_schema` was last written with). Used by the DDL codegen to compute
/// the new value the next DDL statement should install via `SetCookie`.
pub fn schema_cookie(pager: &Pager) -> u32 {
    pager.header().schema_cookie
}

fn text_at(values: &[Value], i: usize) -> Option<String> {
    match values.get(i) {
        Some(Value::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

fn int_at(values: &[Value], i: usize) -> i64 {
    match values.get(i) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    }
}
