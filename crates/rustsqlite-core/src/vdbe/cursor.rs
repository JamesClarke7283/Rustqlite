//! VDBE cursor table (mirrors `VdbeCursor` in `vdbe.c`).
//!
//! The per-program table of open cursors. M3a's read query path needs two kinds: a [`TableCursor`]
//! opened by `OpenRead` over a table b-tree, and a [`Sorter`] opened by `SorterOpen` for
//! `ORDER BY`. M5.1 adds the [`IndexCursor`](crate::btree::IndexCursor) variant for index
//! b-trees.

use crate::btree::{IndexCursor, TableCursor};

use super::sorter::Sorter;

/// One open cursor in a running program, addressed by its `p1` cursor number.
pub enum VdbeCursor {
    /// A read cursor over a table b-tree (`OpenRead` / `OpenWrite`).
    Table(TableCursor),
    /// A read cursor over an index b-tree (`OpenRead` / `OpenWrite` with a `P4::KeyInfo`).
    Index(IndexCursor),
    /// An `ORDER BY` sorter (`SorterOpen`).
    Sorter(Sorter),
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

    /// Whether this is a sorter cursor.
    pub fn is_sorter(&self) -> bool {
        matches!(self, VdbeCursor::Sorter(_))
    }

    /// Whether this is an index cursor.
    pub fn is_index(&self) -> bool {
        matches!(self, VdbeCursor::Index(_))
    }
}
