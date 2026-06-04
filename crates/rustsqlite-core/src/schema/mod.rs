//! Schema / catalog (mirrors the `sqlite_schema` handling in `build.c` / `prepare.c`).

pub mod bootstrap;
pub mod catalog;
pub mod table;

pub use catalog::{
    read_catalog, resolve_index_for_column, resolve_index_object, schema_cookie, Catalog,
    SchemaObject,
};
pub use table::{is_rowid_name, Column, ColumnRef, IndexObject, IndexedColumn, Table};
