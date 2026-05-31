//! Schema bootstrap (mirrors the hard-coded `sqlite_schema` definition in `build.c`).
//!
//! Placeholder: when the write path lands, this module will hold the built-in definition of
//! the `sqlite_schema` table (column names/affinities, rooted at page 1), the schema cookie
//! handling, and `WITHOUT ROWID` / `AUTOINCREMENT` (`sqlite_sequence`) bookkeeping. The
//! read path reads `sqlite_schema` directly via [`super::catalog`] and does not need it yet.
