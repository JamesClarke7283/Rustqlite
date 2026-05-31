//! The public C-API surface, translated to Rust types (mirrors `main.c`, `vdbeapi.c`,
//! `prepare.c`, `legacy.c`).
//!
//! This is Rustqlite's canonical public interface: the `sqlite3_*`-named functions and the
//! `SQLITE_*` result codes, using Rust types (`Result`, `&str`, `Vec<u8>`) instead of raw C
//! pointers. The key items are re-exported at the crate root.

pub mod connection;
pub mod result_code;
pub mod runtime;
pub mod stmt;
pub mod value;

pub use connection::{sqlite3_open, sqlite3_open_v2, Sqlite3};
pub use result_code::ResultCode;
pub use stmt::{sqlite3_prepare_v2, Sqlite3Stmt};
pub use value::{
    value_blob, value_double, value_int64, value_text, value_type, SQLITE_BLOB, SQLITE_FLOAT,
    SQLITE_INTEGER, SQLITE_NULL, SQLITE_TEXT,
};
