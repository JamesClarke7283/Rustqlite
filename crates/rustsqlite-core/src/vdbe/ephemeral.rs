//! In-memory ephemeral table for `RETURNING` (mirrors the ephemeral b-tree used upstream).
//!
//! Upstream `OP_OpenEphemeral` creates a transient, rowid-keyed b-tree that is automatically
//! dropped when the statement ends. We model the same semantics with a simple `Vec`-backed
//! buffer: records are inserted with an auto-incremented integer key, then rewound and read
//! back sequentially via `Column`. No journaling or sorting is needed for RETURNING.

use crate::error::Result;
use crate::format::{decode_record, TextEncoding};
use crate::types::Value;

/// A rowid-keyed in-memory table.
pub struct Ephemeral {
    records: Vec<Vec<u8>>,
    /// Rowid of the next insert (starts at 1, like upstream's `NewRowid`).
    next_rowid: i64,
    /// Iteration position after [`rewind`](Self::rewind).
    pos: usize,
    /// Decoded current record for `Column` reads.
    current: Option<Vec<Value>>,
    encoding: TextEncoding,
}

impl Ephemeral {
    /// Open an ephemeral table that will hold `nfield`-column records.
    pub fn new(_nfield: usize, encoding: TextEncoding) -> Ephemeral {
        Ephemeral {
            records: Vec::new(),
            next_rowid: 1,
            pos: 0,
            current: None,
            encoding,
        }
    }

    /// Allocate and return the next integer rowid, advancing the cursor.
    pub fn next_rowid(&mut self) -> i64 {
        let rowid = self.next_rowid;
        self.next_rowid += 1;
        rowid
    }

    /// Insert a record (the BLOB from `MakeRecord`) keyed by `rowid`.
    pub fn insert(&mut self, rowid: i64, record: Vec<u8>) {
        // The ephemeral table is append-only; callers always pass the rowid that was just
        // returned by `next_rowid`.
        let _ = rowid;
        self.records.push(record);
    }

    /// Position at the first record. Return `true` if there is at least one row.
    pub fn rewind(&mut self) -> bool {
        self.pos = 0;
        self.current = None;
        !self.records.is_empty()
    }

    /// Whether the cursor is on a valid row.
    pub fn is_valid(&self) -> bool {
        self.pos < self.records.len()
    }

    /// Advance to the next record.
    pub fn next(&mut self) {
        self.pos += 1;
        self.current = None;
    }

    /// Decode the current record so `Column` can read it.
    pub fn data(&mut self) -> Result<()> {
        if let Some(rec) = self.records.get(self.pos) {
            self.current = Some(decode_record(rec, self.encoding)?);
        }
        Ok(())
    }

    /// The `i`-th field of the current decoded record.
    pub fn column(&self, i: usize) -> Value {
        self.current
            .as_ref()
            .and_then(|vals| vals.get(i).cloned())
            .unwrap_or(Value::Null)
    }
}
