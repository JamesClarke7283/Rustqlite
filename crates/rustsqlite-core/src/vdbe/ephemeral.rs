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
//!
//! `OP_OpenDup` (M7.12) opens a second cursor sharing the same underlying record storage as
//! an existing ephemeral cursor — used by the window-function sliding-frame algorithm to
//! keep multiple cursors (start/current/end) on the same partition cache. The shared data is
//! held in an `Rc<RefCell<EphemeralData>>`; the per-cursor iteration state (`pos`, `current`)
//! is owned by each `Ephemeral` wrapper.

use std::cell::RefCell;
use std::rc::Rc;

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

/// The shared, cursor-independent state of an ephemeral table: the record buffer, the next
/// rowid counter, and the encoding/kind metadata. Held in an `Rc<RefCell<>>` so that
/// `OP_OpenDup` can share it across multiple cursors.
struct EphemeralData {
    records: Vec<Vec<u8>>,
    /// Rowid of the next insert (starts at 1, like upstream's `NewRowid`). Table variant only.
    next_rowid: i64,
    encoding: TextEncoding,
    kind: EphemeralKind,
}

/// A rowid-keyed in-memory table or a record-keyed in-memory index. The shared data lives in
/// an `Rc<RefCell<EphemeralData>>` so that `OP_OpenDup` can clone the cursor and share storage
/// with the original.
pub struct Ephemeral {
    data: Rc<RefCell<EphemeralData>>,
    /// Iteration position after [`rewind`](Self::rewind).
    pos: usize,
    /// Decoded current record for `Column` reads.
    current: Option<Vec<Value>>,
}

impl Ephemeral {
    /// Open an ephemeral table that will hold `nfield`-column records.
    pub fn new(_nfield: usize, encoding: TextEncoding) -> Ephemeral {
        Ephemeral {
            data: Rc::new(RefCell::new(EphemeralData {
                records: Vec::new(),
                next_rowid: 1,
                encoding,
                kind: EphemeralKind::Table,
            })),
            pos: 0,
            current: None,
        }
    }

    /// Open an ephemeral index (record-keyed, dedup) — mirrors upstream's `OP_OpenEphemeral`
    /// with a non-null `P4` KeyInfo. `_nfield` is the column count of the key record.
    pub fn new_index(_nfield: usize, encoding: TextEncoding) -> Ephemeral {
        Ephemeral {
            data: Rc::new(RefCell::new(EphemeralData {
                records: Vec::new(),
                next_rowid: 1,
                encoding,
                kind: EphemeralKind::Index,
            })),
            pos: 0,
            current: None,
        }
    }

    /// Clone the shared storage into a new cursor with a fresh iteration position. Used by
    /// `OP_OpenDup` to open a second cursor on the same ephemeral table (e.g. the
    /// window-function start/current/end cursors over the partition cache).
    pub fn dup(&self) -> Ephemeral {
        Ephemeral {
            data: Rc::clone(&self.data),
            pos: 0,
            current: None,
        }
    }

    /// Which kind of ephemeral this is.
    pub fn kind(&self) -> EphemeralKind {
        self.data.borrow().kind
    }

    /// Allocate and return the next integer rowid, advancing the cursor. Table variant only.
    pub fn next_rowid(&mut self) -> i64 {
        let rowid = self.data.borrow().next_rowid;
        self.data.borrow_mut().next_rowid += 1;
        rowid
    }

    /// Insert a record (the BLOB from `MakeRecord`) keyed by `rowid`. Table variant.
    pub fn insert(&mut self, rowid: i64, record: Vec<u8>) {
        // The ephemeral table is append-only; callers always pass the rowid that was just
        // returned by `next_rowid`.
        let _ = rowid;
        self.data.borrow_mut().records.push(record);
    }

    /// Insert a record blob as a dedup key (mirrors `OP_IdxInsert` on an ephemeral index).
    /// Returns `true` if the key was new (inserted), `false` if it was already present.
    /// The comparison matches `OP_Found` semantics: equal under BINARY collation with
    /// NULL-equality (NULL == NULL), so each row of values is compared element-wise.
    pub fn idx_insert(&mut self, record: &[u8]) -> Result<bool> {
        if self.find_record(record)? {
            return Ok(false);
        }
        self.data.borrow_mut().records.push(record.to_vec());
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
        for rec in &self.data.borrow().records {
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
        !self.data.borrow().records.is_empty()
    }

    /// Whether the cursor is on a valid row.
    pub fn is_valid(&self) -> bool {
        self.pos < self.data.borrow().records.len()
    }

    /// Advance to the next record.
    pub fn next(&mut self) {
        self.pos += 1;
        self.current = None;
    }

    /// Decode the current record so `Column` can read it.
    pub fn data(&mut self) -> Result<()> {
        let pos = self.pos;
        let rec = self.data.borrow().records.get(pos).cloned();
        if let Some(rec) = rec {
            self.current = Some(decode_record(&rec, self.data.borrow().encoding)?);
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
        self.data.borrow().records.get(pos).cloned()
    }

    /// Remove the record at the current position, shifting subsequent records down. Used by
    /// the recursive CTE loop to drain the Queue as rows are processed.
    pub fn delete_current(&mut self) {
        let pos = self.pos;
        if pos < self.data.borrow().records.len() {
            self.data.borrow_mut().records.remove(pos);
            self.current = None;
        }
    }

    /// Whether the ephemeral is empty (no records).
    pub fn is_empty(&self) -> bool {
        self.data.borrow().records.is_empty()
    }

    /// Remove all records and reset the iteration position. Used by `OP_Clear` on an
    /// ephemeral cursor (window-function peer-buf reset between peer groups).
    pub fn clear(&mut self) {
        self.data.borrow_mut().records.clear();
        self.data.borrow_mut().next_rowid = 1;
        self.pos = 0;
        self.current = None;
    }

    // ---- window-function sliding-frame support (M11.8) ----

    /// The number of records currently stored. Used by the window codegen to size the frame.
    pub fn len(&self) -> usize {
        self.data.borrow().records.len()
    }

    /// Position the cursor at row `pos` (0-indexed). Returns `true` if valid.
    /// Used by the window-function sliding-frame codegen to seek the start/current/end cursors
    /// to specific rowids in the partition cache (mirrors upstream's `OP_SeekRowid` on the
    /// ephemeral table, but our ephemeral is rowid-numbered 1..=n so we map rowid → index).
    pub fn seek_rowid(&mut self, rowid: i64) -> bool {
        // Our ephemeral rowids are 1..=n (sequential), so rowid R is index R-1.
        let idx = (rowid - 1) as usize;
        if idx < self.data.borrow().records.len() {
            self.pos = idx;
            self.current = None;
            true
        } else {
            false
        }
    }

    /// The rowid of the current row (1-indexed). Used by the window codegen to read the
    /// current row's position in the partition cache.
    pub fn rowid(&self) -> i64 {
        (self.pos + 1) as i64
    }

    /// Reset the cursor to before the first row (so the next `next` advances to row 0).
    /// Mirrors upstream's `OP_Rewind` followed by no `data()` call. Used by the window
    /// codegen to reposition cursors before the sliding-frame loop.
    pub fn reset(&mut self) {
        self.pos = 0;
        self.current = None;
    }
}