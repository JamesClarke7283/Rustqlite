//! Schema / catalog (mirrors the `sqlite_schema` handling in `build.c` / `prepare.c`).

pub mod bootstrap;
pub mod catalog;

pub use catalog::{read_catalog, Catalog, SchemaObject};
