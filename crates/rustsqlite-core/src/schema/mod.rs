//! Schema / catalog (mirrors the `sqlite_schema` handling in `build.c` / `prepare.c`).

pub mod bootstrap;
pub mod catalog;
pub mod table;

pub use catalog::{read_catalog, schema_cookie, Catalog, SchemaObject};
pub use table::{is_rowid_name, Column, ColumnRef, Table};
