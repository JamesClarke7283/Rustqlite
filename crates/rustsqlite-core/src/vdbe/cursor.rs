//! VDBE cursor table (mirrors `VdbeCursor` in `vdbe.c`).
//!
//! The per-program table of open cursors. M3a's read query path needs two kinds: a [`TableCursor`]
//! opened by `OpenRead` over a table b-tree, and a [`Sorter`] opened by `SorterOpen` for
//! `ORDER BY`. M5.1 adds the [`IndexCursor`](crate::btree::IndexCursor) variant for index
//! b-trees. M2.24 adds an in-memory ephemeral table cursor for `RETURNING`.

use crate::btree::{IndexCursor, TableCursor};

use super::ephemeral::Ephemeral;
use super::sorter::Sorter;

/// A pseudo-cursor that reads a single record stored in a register. Used by recursive CTEs
/// to expose the "Current" row (the row just pulled from the Queue) to the recursive query's
/// scan via `OP_Column`. The register holds the record blob (set by `OP_RowData`); `Column`
/// decodes a field from it.
pub struct PseudoCursor {
    /// The register holding the current record blob (a `Value::Blob`).
    pub reg: i32,
    /// The decoded column values, cached after the first `Column` read.
    pub current: Option<Vec<crate::types::Value>>,
    /// The text encoding to use when decoding the record.
    pub encoding: crate::format::TextEncoding,
}

impl PseudoCursor {
    pub fn new(reg: i32, encoding: crate::format::TextEncoding) -> PseudoCursor {
        PseudoCursor {
            reg,
            current: None,
            encoding,
        }
    }

    /// Decode the record blob from the register into the cached column values.
    pub fn data(&mut self, regs: &[crate::types::Value]) -> Result<(), crate::error::Error> {
        let blob = match regs.get(self.reg as usize) {
            Some(crate::types::Value::Blob(b)) => b.clone(),
            _ => return Err(crate::error::Error::msg("pseudo-cursor register is not a record blob")),
        };
        self.current = Some(crate::format::decode_record(&blob, self.encoding)?);
        Ok(())
    }

    /// Read column `i` from the cached decoded record.
    pub fn column(&self, i: usize) -> crate::types::Value {
        self.current
            .as_ref()
            .and_then(|vals| vals.get(i).cloned())
            .unwrap_or(crate::types::Value::Null)
    }
}

/// One open cursor in a running program, addressed by its `p1` cursor number.
pub enum VdbeCursor {
    /// A read cursor over a table b-tree (`OpenRead` / `OpenWrite`).
    Table(TableCursor),
    /// A read cursor over an index b-tree (`OpenRead` / `OpenWrite` with a `P4::KeyInfo`).
    Index(IndexCursor),
    /// An `ORDER BY` sorter (`SorterOpen`).
    Sorter(Sorter),
    /// An in-memory ephemeral table used by `OpenEphemeral` for `RETURNING`.
    Ephemeral(Ephemeral),
    /// A pseudo-cursor that reads a single record from a register (`OpenPseudo`).
    Pseudo(PseudoCursor),
}

impl VdbeCursor {
    /// Borrow this cursor as a table cursor, or `None` if it is an index/sorter.
    pub fn as_table(&self) -> Option<&TableCursor> {
        match self {
            VdbeCursor::Table(c) => Some(c),
            _ => None,
        }
    }

    /// Mutably borrow this cursor as a table cursor.
    pub fn as_table_mut(&mut self) -> Option<&mut TableCursor> {
        match self {
            VdbeCursor::Table(c) => Some(c),
            _ => None,
        }
    }

    /// Borrow this cursor as an index cursor.
    pub fn as_index(&self) -> Option<&IndexCursor> {
        match self {
            VdbeCursor::Index(c) => Some(c),
            _ => None,
        }
    }

    /// Mutably borrow this cursor as an index cursor.
    pub fn as_index_mut(&mut self) -> Option<&mut IndexCursor> {
        match self {
            VdbeCursor::Index(c) => Some(c),
            _ => None,
        }
    }

    /// Borrow this cursor as a sorter.
    pub fn as_sorter(&self) -> Option<&Sorter> {
        match self {
            VdbeCursor::Sorter(s) => Some(s),
            _ => None,
        }
    }

    /// Mutably borrow this cursor as a sorter.
    pub fn as_sorter_mut(&mut self) -> Option<&mut Sorter> {
        match self {
            VdbeCursor::Sorter(s) => Some(s),
            _ => None,
        }
    }

    /// Borrow this cursor as an ephemeral table.
    pub fn as_ephemeral(&self) -> Option<&Ephemeral> {
        match self {
            VdbeCursor::Ephemeral(e) => Some(e),
            _ => None,
        }
    }

    /// Mutably borrow this cursor as an ephemeral table.
    pub fn as_ephemeral_mut(&mut self) -> Option<&mut Ephemeral> {
        match self {
            VdbeCursor::Ephemeral(e) => Some(e),
            _ => None,
        }
    }

    /// Whether this is a sorter cursor.
    pub fn is_sorter(&self) -> bool {
        matches!(self, VdbeCursor::Sorter(_))
    }

    /// Whether this is an index cursor.
    pub fn is_index(&self) -> bool {
        matches!(self, VdbeCursor::Index(_))
    }

    /// Whether this is an ephemeral table cursor.
    pub fn is_ephemeral(&self) -> bool {
        matches!(self, VdbeCursor::Ephemeral(_))
    }

    /// Whether this is a pseudo-cursor.
    pub fn is_pseudo(&self) -> bool {
        matches!(self, VdbeCursor::Pseudo(_))
    }

    /// Borrow this cursor as a pseudo-cursor.
    pub fn as_pseudo(&self) -> Option<&PseudoCursor> {
        match self {
            VdbeCursor::Pseudo(p) => Some(p),
            _ => None,
        }
    }

    /// Mutably borrow this cursor as a pseudo-cursor.
    pub fn as_pseudo_mut(&mut self) -> Option<&mut PseudoCursor> {
        match self {
            VdbeCursor::Pseudo(p) => Some(p),
            _ => None,
        }
    }
}
