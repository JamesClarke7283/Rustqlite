//! VDBE cursor table (mirrors `VdbeCursor` in `vdbe.c`).
//!
//! The per-program table of open cursors. M3a's read query path needs two kinds: a [`TableCursor`]
//! opened by `OpenRead` over a table b-tree, and a [`Sorter`] opened by `SorterOpen` for
//! `ORDER BY`. Index and write cursors arrive with later milestones.

use crate::btree::TableCursor;

use super::sorter::Sorter;

/// One open cursor in a running program, addressed by its `p1` cursor number.
pub enum VdbeCursor {
    /// A read cursor over a table b-tree (`OpenRead`).
    Table(TableCursor),
    /// An `ORDER BY` sorter (`SorterOpen`).
    Sorter(Sorter),
}

impl VdbeCursor {
    /// Borrow this cursor as a table cursor, or `None` if it is a sorter.
    pub fn as_table(&self) -> Option<&TableCursor> {
        match self {
            VdbeCursor::Table(c) => Some(c),
            VdbeCursor::Sorter(_) => None,
        }
    }

    /// Mutably borrow this cursor as a table cursor.
    pub fn as_table_mut(&mut self) -> Option<&mut TableCursor> {
        match self {
            VdbeCursor::Table(c) => Some(c),
            VdbeCursor::Sorter(_) => None,
        }
    }

    /// Borrow this cursor as a sorter.
    pub fn as_sorter(&self) -> Option<&Sorter> {
        match self {
            VdbeCursor::Sorter(s) => Some(s),
            VdbeCursor::Table(_) => None,
        }
    }

    /// Mutably borrow this cursor as a sorter.
    pub fn as_sorter_mut(&mut self) -> Option<&mut Sorter> {
        match self {
            VdbeCursor::Sorter(s) => Some(s),
            VdbeCursor::Table(_) => None,
        }
    }

    /// Whether this is a sorter cursor.
    pub fn is_sorter(&self) -> bool {
        matches!(self, VdbeCursor::Sorter(_))
    }
}
