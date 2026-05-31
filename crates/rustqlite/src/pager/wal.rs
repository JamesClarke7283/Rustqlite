//! Write-Ahead Log (mirrors `wal.c`).
//!
//! Placeholder for the durability milestone (M8): the `-wal` file, the WAL header + frame
//! format, the shared-memory (`-shm`) index, readers' end-marks, and checkpointing. Deferred
//! until after the rollback-journal write path is solid.
