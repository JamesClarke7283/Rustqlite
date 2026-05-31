//! B-tree layer (mirrors `btree.c`).
//!
//! SQLite stores everything in b-trees: table b-trees are rowid-keyed (data in the leaves),
//! index b-trees are key-keyed. This module decodes the on-disk page and cell layout and
//! provides read cursors over it. For M1 the read cursor walks **table** b-trees (enough to
//! read `sqlite_schema` and table-scan rows, following overflow chains). Index cursors and
//! the write-path balancing ([`balance`]) arrive in later milestones.

pub mod balance;
pub mod cell;
pub mod cursor;
pub mod page;

pub use cell::{
    parse_index_interior_cell, parse_index_leaf_cell, parse_table_interior_cell,
    parse_table_leaf_cell,
};
pub use cursor::scan_table;
pub use page::{PageHeader, PageType};

/// Read a big-endian `u16` from the start of `b`.
pub(crate) fn be_u16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

/// Read a big-endian `u32` from the start of `b`.
pub(crate) fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
