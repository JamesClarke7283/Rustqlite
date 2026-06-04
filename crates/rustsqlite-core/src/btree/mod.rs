//! B-tree layer (mirrors `btree.c`).
//!
//! SQLite stores everything in b-trees: table b-trees are rowid-keyed (data in the leaves),
//! index b-trees are key-keyed. This module decodes the on-disk page and cell layout and
//! provides read cursors over it. For M1 the read cursor walks **table** b-trees (enough to
//! read `sqlite_schema` and table-scan rows, following overflow chains). The write path
//! ([`insert`]) adds single-leaf table insertion + rowid allocation + b-tree creation. M5.1
//! adds the index b-tree layer ([`index`], [`index_cursor`], [`index_insert`],
//! [`index_delete`]) that backs `CREATE INDEX` / `DROP INDEX` and the index-aware `WHERE` /
//! `ORDER BY` paths.

pub mod balance;
pub mod cell;
pub mod cursor;
pub mod delete;
pub mod destroy;
pub mod index;
pub mod index_cursor;
pub mod index_delete;
pub mod index_insert;
pub mod insert;
pub mod page;

pub use cell::{
    assemble_index_payload, build_index_interior_cell, build_index_leaf_cell,
    build_table_leaf_cell, parse_index_interior_cell, parse_index_leaf_cell,
    parse_table_interior_cell, parse_table_leaf_cell, table_leaf_cell_rowid,
};
pub use cursor::{scan_table, TableCursor};
pub use delete::leaf_delete_current;
pub use destroy::destroy as btree_destroy;
pub use index::{create_index_btree, scan_index};
pub use index_cursor::IndexCursor;
pub use index_delete::index_leaf_delete;
pub use index_insert::{index_insert, index_insert_after_root_promotion};
pub use insert::{max_rowid, table_insert};
pub use page::{PageHeader, PageType};

use crate::error::Result;
use crate::pager::Pager;

/// Create a new (rowid) table b-tree and return its root page number. Allocates a fresh page and
/// initializes it as an empty leaf — the analogue of `sqlite3BtreeCreateTable` for an ordinary
/// table. The caller must hold a write transaction; the new page is committed with the rest of the
/// transaction. (The new page is beyond the original database size, so it carries no journal
/// pre-image — a rollback simply truncates the file back.)
pub async fn create_table_btree(pager: &Pager) -> Result<u32> {
    let pgno = pager.allocate_page();
    let mut buf = pager.read_page_for_write(pgno).await?;
    let base = pager.btree_header_offset(pgno);
    page::init_empty_leaf(&mut buf, base);
    pager.write_page(pgno, buf)?;
    Ok(pgno)
}

/// Read a big-endian `u16` from the start of `b`.
pub(crate) fn be_u16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

/// Read a big-endian `u32` from the start of `b`.
pub(crate) fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
