//! `rustqlite` — a faithful, file-format-compatible reimplementation of the SQLite3 engine.
//!
//! The public face mirrors the SQLite **C API** (`sqlite3_open`, `sqlite3_prepare_v2`,
//! `sqlite3_step`, `sqlite3_column_*`, the `SQLITE_*` result codes, …) translated to Rust
//! types; see [`capi`]. Internally the module layout mirrors upstream SQLite's source layout
//! (see the README mapping table) so the implementation can be checked file-by-file.
//!
//! I/O is async on tokio (see [`vfs`]); the `sqlite3_*` functions keep synchronous signatures
//! and drive the async engine via a process-global runtime.
//!
//! Compatibility target: SQLite [`SQLITE_VERSION`].

// TODO(M3+): tighten this once every subsystem is wired into the prepare/step path. While the
// engine is being built bottom-up, lower layers (format/btree/pager) are exercised by tests
// and the CLI's read path before the VDBE consumes them, so some items read as "unused".
#![allow(dead_code)]

pub mod btree;
pub mod capi;
pub mod codegen;
pub mod error;
pub mod format;
pub mod func;
pub mod pager;
pub mod pragma;
pub mod schema;
pub mod types;
pub mod util;
pub mod vdbe;
pub mod vfs;

pub use error::{Error, Result, ResultCode};
pub use types::Value;

// The C-API surface is the canonical public interface; re-export the key items at the crate
// root so callers can write `rustqlite::sqlite3_open(...)` directly.
pub use capi::{sqlite3_open, sqlite3_open_v2, sqlite3_prepare_v2, Sqlite3, Sqlite3Stmt};

/// The SQLite version string this build targets, e.g. `"3.53.1"` (kept in sync with the
/// repo-root `VERSION` file; see the `version_matches_version_file` test).
pub const SQLITE_VERSION: &str = "3.53.1";

/// The numeric encoding of [`SQLITE_VERSION`]: `major*1_000_000 + minor*1_000 + patch`.
pub const SQLITE_VERSION_NUMBER: i32 = 3_053_001;

/// The source-id string (`<date> <time> <hash>`) of the targeted build.
pub const SQLITE_SOURCE_ID: &str =
    "2026-05-05 10:34:17 c88b22011a54b4f6fbd149e9f8e4de77658ce58143a1af0e3785e4e64751alt1";

/// `sqlite3_libversion()` — the library version string.
pub fn sqlite3_libversion() -> &'static str {
    SQLITE_VERSION
}

/// `sqlite3_libversion_number()` — the numeric library version.
pub fn sqlite3_libversion_number() -> i32 {
    SQLITE_VERSION_NUMBER
}

/// `sqlite3_sourceid()` — the source-id of the targeted build.
pub fn sqlite3_sourceid() -> &'static str {
    SQLITE_SOURCE_ID
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_matches_version_file() {
        // Guards against VERSION and the compiled-in string drifting apart.
        let file = include_str!("../../../VERSION").trim();
        assert_eq!(file, SQLITE_VERSION);
    }

    #[test]
    fn version_number_encoding() {
        assert_eq!(sqlite3_libversion(), "3.53.1");
        assert_eq!(sqlite3_libversion_number(), 3_053_001);
        assert!(sqlite3_sourceid().starts_with("2026-05-05"));
    }
}
