//! In-memory ephemeral table for `RETURNING` and `SELECT DISTINCT` (mirrors the ephemeral
//! b-tree used upstream).
//!
//! Upstream `OP_OpenEphemeral` creates a transient b-tree that is automatically dropped when
//! the statement ends. When `P4` is a `KeyInfo` the ephemeral is an *index* keyed by the
//! record (used for `DISTINCT` dedup and `IN (SELECT ...)` sets); otherwise it's a rowid-keyed
//! *table* (used by `RETURNING` to buffer rows). We model both with a `Vec`-backed buffer:
//! the table variant stores records with an auto-incremented rowid and reads them back
//! sequentially via `Column`; the index variant stores record blobs as dedup keys, supports
//! `find` for `OP_Found`, and is never read back via `Column`.

use crate::error::Result;
use crate::format::{decode_record, encode_record, TextEncoding};
use crate::types::Value;

/// Whether the ephemeral behaves as a rowid-keyed table or a record-keyed index.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EphemeralKind {
    /// Rowid-keyed table: `Insert` with `NewRowid`, read back via `Rewind`/`Next`/`Column`.
    /// Used by `RETURNING`.
    Table,
    /// Record-keyed index: `IdxInsert` of a record blob; `Found` checks membership. Used by
    /// `SELECT DISTINCT` dedup (and future `IN (SELECT ...)` sets).
    Index,
}

/// A rowid-keyed in-memory table or a record-keyed in-memory index.
pub struct Ephemeral {
    records: Vec<Vec<u8>>,
    /// Rowid of the next insert (starts at 1, like upstream's `NewRowid`). Table variant only.
    next_rowid: i64,
    /// Iteration position after [`rewind`](Self::rewind).
    pos: usize,
    /// Decoded current record for `Column` reads.
    current: Option<Vec<Value>>,
    encoding: TextEncoding,
    kind: EphemeralKind,
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
            kind: EphemeralKind::Table,
        }
    }

    /// Open an ephemeral index (record-keyed, dedup) — mirrors upstream's `OP_OpenEphemeral`
    /// with a non-null `P4` KeyInfo. `_nfield` is the column count of the key record.
    pub fn new_index(_nfield: usize, encoding: TextEncoding) -> Ephemeral {
        Ephemeral {
            records: Vec::new(),
            next_rowid: 1,
            pos: 0,
            current: None,
            encoding,
            kind: EphemeralKind::Index,
        }
    }

    /// Which kind of ephemeral this is.
    pub fn kind(&self) -> EphemeralKind {
        self.kind
    }

    /// Allocate and return the next integer rowid, advancing the cursor. Table variant only.
    pub fn next_rowid(&mut self) -> i64 {
        let rowid = self.next_rowid;
        self.next_rowid += 1;
        rowid
    }

    /// Insert a record (the BLOB from `MakeRecord`) keyed by `rowid`. Table variant.
    pub fn insert(&mut self, rowid: i64, record: Vec<u8>) {
        // The ephemeral table is append-only; callers always pass the rowid that was just
        // returned by `next_rowid`.
        let _ = rowid;
        self.records.push(record);
    }

    /// Insert a record blob as a dedup key (mirrors `OP_IdxInsert` on an ephemeral index).
    /// Returns `true` if the key was new (inserted), `false` if it was already present.
    /// The comparison matches `OP_Found` semantics: equal under BINARY collation with
    /// NULL-equality (NULL == NULL), so each row of values is compared element-wise.
    pub fn idx_insert(&mut self, record: &[u8]) -> Result<bool> {
        if self.find_record(record)? {
            return Ok(false);
        }
        self.records.push(record.to_vec());
        Ok(true)
    }

    /// Search the index for a record equal to `values` under BINARY collation with NULL-equality.
    /// Used by `OP_Found` on an ephemeral index. Returns `true` if a matching record is present.
    pub fn find_values(&self, values: &[Value]) -> Result<bool> {
        let needle = encode_record(values);
        self.find_record(&needle)
    }

    /// Internal: linear-scan for a stored record byte-equal to `needle`. Encoding a record is
    /// canonical, so byte-equality of the encoded blob is equivalent to element-wise value
    /// equality with NULL-equality (NULL serializes as serial type 0, and any two NULLs encode
    /// identically). This matches `OP_Found` on an ephemeral index under BINARY collation.
    fn find_record(&self, needle: &[u8]) -> Result<bool> {
        for rec in &self.records {
            if rec.as_slice() == needle {
                return Ok(true);
            }
        }
        Ok(false)
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

    /// The current iteration position (the index into `records` of the row the cursor is on).
    /// Used by `OP_RowData` to fetch the raw record bytes.
    pub fn current_position(&self) -> usize {
        self.pos
    }

    /// The raw record bytes at position `pos`, or `None` if out of range.
    pub fn record_at(&self, pos: usize) -> Option<Vec<u8>> {
        self.records.get(pos).cloned()
    }

    /// Remove the record at the current position, shifting subsequent records down. Used by
    /// the recursive CTE loop to drain the Queue as rows are processed.
    pub fn delete_current(&mut self) {
        if self.pos < self.records.len() {
            self.records.remove(self.pos);
            self.current = None;
        }
    }

    /// Whether the ephemeral is empty (no records).
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}
