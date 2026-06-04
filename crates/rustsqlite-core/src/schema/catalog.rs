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

/// One row of `sqlite_schema`.
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaObject {
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
    fn from_row(values: &[Value]) -> SchemaObject {
        SchemaObject {
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
}

/// The in-memory catalog: all `sqlite_schema` rows, in storage (rowid) order.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    pub objects: Vec<SchemaObject>,
}

impl Catalog {
    /// Iterate the table objects (`type = 'table'`).
    pub fn tables(&self) -> impl Iterator<Item = &SchemaObject> {
        self.objects.iter().filter(|o| o.is_table())
    }

    /// Find a table by name (case-insensitive, as SQLite resolves identifiers).
    pub fn find_table(&self, name: &str) -> Option<&SchemaObject> {
        self.objects
            .iter()
            .find(|o| o.is_table() && o.name.eq_ignore_ascii_case(name))
    }
}

/// Read and decode the entire `sqlite_schema` table.
pub async fn read_catalog(pager: &Pager) -> Result<Catalog> {
    let encoding = pager.text_encoding();
    let rows = scan_table(pager, 1).await?;
    let mut objects = Vec::with_capacity(rows.len());
    for (_rowid, payload) in rows {
        let values = decode_record(&payload, encoding)?;
        objects.push(SchemaObject::from_row(&values));
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
