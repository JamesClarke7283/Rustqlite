//! In-memory sorter for `ORDER BY` (mirrors the in-memory path of `vdbesort.c`).
//!
//! M3a uses a simple `Vec`-backed sorter with no run-spilling: records are inserted (each a
//! `MakeRecord` blob whose leading `keys.len()` fields are the sort key), sorted in place by
//! [`mem_compare`](crate::vdbe::compare::mem_compare) honoring each key's DESC flag and
//! collation, then iterated to drive the output loop. The external merge sorter that spills to
//! temporary storage arrives with the larger-dataset work.

use std::cmp::Ordering;

use crate::error::Result;
use crate::format::{decode_record, TextEncoding};
use crate::types::Value;
use crate::vdbe::compare::mem_compare;
use crate::vdbe::KeyField;

/// A `Vec`-backed sorter. The leading `keys.len()` fields of every inserted record are its sort
/// key; the remaining fields are payload that the output loop reads back via `Column`.
pub struct Sorter {
    keys: Vec<KeyField>,
    encoding: TextEncoding,
    records: Vec<Vec<u8>>,
    /// Iteration cursor into `records` after [`sort`](Self::sort).
    pos: usize,
    /// The decoded current record, populated by [`data`](Self::data) for `Column` reads.
    current: Option<Vec<Value>>,
}

impl Sorter {
    pub fn new(keys: Vec<KeyField>, encoding: TextEncoding) -> Sorter {
        Sorter {
            keys,
            encoding,
            records: Vec::new(),
            pos: 0,
            current: None,
        }
    }

    /// Insert one `MakeRecord` blob.
    pub fn insert(&mut self, record: Vec<u8>) {
        self.records.push(record);
    }

    /// Sort all inserted records and position at the first. Returns `true` if there is at least
    /// one record. The sort is stable, so equal-key rows keep their insertion order — matching
    /// upstream.
    pub fn sort(&mut self) -> Result<bool> {
        let keys = self.keys.clone();
        let encoding = self.encoding;
        // Decode each record's fields once, pairing the key view with the raw record, then sort.
        let mut decoded: Vec<(Vec<Value>, Vec<u8>)> = Vec::with_capacity(self.records.len());
        for rec in std::mem::take(&mut self.records) {
            let values = decode_record(&rec, encoding)?;
            decoded.push((values, rec));
        }
        decoded.sort_by(|a, b| compare_keys(&a.0, &b.0, &keys));
        self.records = decoded.into_iter().map(|(_, rec)| rec).collect();
        self.pos = 0;
        self.current = None;
        Ok(!self.records.is_empty())
    }

    /// Whether the iteration cursor is on a valid record.
    pub fn is_valid(&self) -> bool {
        self.pos < self.records.len()
    }

    /// Decode the current record into `current` so `Column` can read its fields.
    pub fn data(&mut self) -> Result<()> {
        let rec = &self.records[self.pos];
        self.current = Some(decode_record(rec, self.encoding)?);
        Ok(())
    }

    /// Advance to the next record.
    pub fn next(&mut self) {
        self.pos += 1;
    }

    /// The `i`-th field of the current (decoded) record. NULL if out of range or not yet
    /// `data`-loaded.
    pub fn column(&self, i: usize) -> Value {
        self.current
            .as_ref()
            .and_then(|vals| vals.get(i).cloned())
            .unwrap_or(Value::Null)
    }
}

/// Compare two records by their leading sort-key fields, applying each key's DESC flag and
/// collation. NULL is the smallest value, so it sorts first under ASC and last under DESC —
/// matching SQLite (the plan's "NULLs first in both directions" note is incorrect; verified
/// against sqlite3 3.53.1).
fn compare_keys(a: &[Value], b: &[Value], keys: &[KeyField]) -> Ordering {
    for (i, key) in keys.iter().enumerate() {
        let va = a.get(i).unwrap_or(&Value::Null);
        let vb = b.get(i).unwrap_or(&Value::Null);
        let ord = mem_compare(va, vb, key.collation);
        if ord != Ordering::Equal {
            return if key.desc { ord.reverse() } else { ord };
        }
    }
    Ordering::Equal
}
