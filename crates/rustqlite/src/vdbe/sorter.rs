//! External merge sorter (mirrors `vdbesort.c`).
//!
//! Placeholder for M3+: the sorter used by `ORDER BY`/`GROUP BY`/`CREATE INDEX` when the data
//! does not fit a simple in-memory sort, spilling runs to temporary storage and merging them.
