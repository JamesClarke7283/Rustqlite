//! Rollback journal (mirrors the rollback-journal half of `pager.c`).
//!
//! Placeholder for the write-path milestone (M4): the `-journal` sidecar file, the journal
//! header + page records, `synchronous` handling, and crash-recovery replay on open. The
//! read-only pager does not need it.
