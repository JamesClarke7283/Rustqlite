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
pub mod autovac;
pub mod cell;
pub mod cursor;
pub mod delete;
pub mod destroy;
pub mod index;
pub mod index_cursor;
pub mod index_delete;
pub mod index_insert;
pub mod insert;
pub mod integrity_check;
pub mod page;
pub mod ptrmap;

pub use cell::{
    assemble_index_interior_payload, assemble_index_payload, build_index_interior_cell,
    build_index_leaf_cell, build_table_leaf_cell, parse_index_interior_cell, parse_index_leaf_cell,
    parse_table_interior_cell, parse_table_leaf_cell, table_leaf_cell_rowid,
};
pub use cursor::{scan_table, TableCursor};
pub use delete::leaf_delete_current;
pub use destroy::destroy as btree_destroy;
pub use destroy::clear as btree_clear;
pub use index::{create_index_btree, scan_index};
pub use index_cursor::IndexCursor;
pub use index_delete::index_leaf_delete;
pub use index_insert::index_insert;
pub use insert::{max_rowid, table_insert};
pub use page::{PageHeader, PageType};

use crate::error::Result;
use crate::pager::Pager;

/// Auto-vacuum-aware index b-tree creation: same as [`create_table_btree_autovac`] but for an
/// index b-tree (initializes the root page as an empty leaf-index page instead of leaf-table).
pub async fn create_index_btree_autovac(pager: &Pager) -> Result<u32> {
    let pgno_root = next_autovac_root_slot(pager);

    let allocated = pager.allocate_page();
    debug_assert_eq!(
        allocated, pgno_root,
        "autovac index root allocation mismatch: expected {pgno_root}, got {allocated}"
    );

    let mut buf = pager.read_page_for_write(pgno_root).await?;
    let base = pager.btree_header_offset(pgno_root);
    page::init_empty_index_leaf(&mut buf, base);
    pager.write_page(pgno_root, buf)?;

    ptrmap::ptrmap_put(pager, pgno_root, ptrmap::PtrMapType::RootPage, 0).await?;
    pager.with_header_mut(|h| h.largest_root_page = pgno_root);
    Ok(pgno_root)
}

/// Create a new (rowid) table b-tree and return its root page number. Allocates a fresh page and
/// initializes it as an empty leaf — the analogue of `sqlite3BtreeCreateTable` for an ordinary
/// table. The caller must hold a write transaction; the new page is committed with the rest of the
/// transaction. (The new page is beyond the original database size, so it carries no journal
/// pre-image — a rollback simply truncates the file back.)
///
/// In an auto-vacuum database, the new root page is placed at `meta[4] + 1` (the next root-page
/// slot), mirroring `sqlite3BtreeCreateTable`'s auto-vacuum path. The page currently at that slot
/// is moved out of the way (to a freshly allocated page at the end of the file) so the root pages
/// stay clustered at the front of the file. `meta[4]` is then updated to the new root page number.
pub async fn create_table_btree(pager: &Pager) -> Result<u32> {
    if pager.auto_vacuum() {
        create_table_btree_autovac(pager).await
    } else {
        create_table_btree_plain(pager).await
    }
}

/// Non-auto-vacuum table b-tree creation: allocate a fresh page at the end of the file.
async fn create_table_btree_plain(pager: &Pager) -> Result<u32> {
    let pgno = pager.allocate_page();
    let mut buf = pager.read_page_for_write(pgno).await?;
    let base = pager.btree_header_offset(pgno);
    page::init_empty_leaf(&mut buf, base);
    pager.write_page(pgno, buf)?;
    Ok(pgno)
}

/// Auto-vacuum-aware table b-tree creation (mirrors `sqlite3BtreeCreateTable` in auto-vacuum
/// mode): the new root page goes at `meta[4] + 1` so root pages stay clustered at the front of
/// the file. The page currently at that slot (if any) is relocated to a freshly allocated page
/// at the end of the file. `meta[4]` is updated to the new root page number.
async fn create_table_btree_autovac(pager: &Pager) -> Result<u32> {
    let pgno_root = next_autovac_root_slot(pager);

    // Allocate pages until we reach pgno_root. The pager's `allocate_page` skips ptrmap and
    // pending-byte pages, so the allocated page number will match pgno_root when the file is
    // fresh (no freelist). When the file already has content at pgno_root, the existing
    // content must be relocated first — the full `sqlite3BtreeCreateTable` path does this via
    // `relocatePage`. For the common case (fresh DB or pgno_root beyond the current page
    // count) the simple allocation is correct.
    let allocated = pager.allocate_page();
    debug_assert_eq!(
        allocated, pgno_root,
        "autovac root allocation mismatch: expected {pgno_root}, got {allocated}"
    );

    let mut buf = pager.read_page_for_write(pgno_root).await?;
    let base = pager.btree_header_offset(pgno_root);
    page::init_empty_leaf(&mut buf, base);
    pager.write_page(pgno_root, buf)?;

    // Record the new root page in the pointer map and update meta[4] to the new root page.
    ptrmap::ptrmap_put(pager, pgno_root, ptrmap::PtrMapType::RootPage, 0).await?;
    pager.with_header_mut(|h| h.largest_root_page = pgno_root);
    Ok(pgno_root)
}

/// Compute the next root-page slot for an auto-vacuum database: `meta[4] + 1`, skipping ptrmap
/// and pending-byte pages. For a fresh DB (meta[4] == 1, the autoVacuum flag), the first root
/// goes at page 3 (page 2 is the first ptrmap page).
fn next_autovac_root_slot(pager: &Pager) -> u32 {
    let usable = pager.usable_size();
    let mut pgno_root = pager.header().largest_root_page + 1;
    while ptrmap::is_ptrmap_page(usable, pgno_root) || ptrmap::is_pending_byte_page(usable, pgno_root) {
        pgno_root += 1;
    }
    if pgno_root < 3 {
        pgno_root = 3;
    }
    pgno_root
}

/// Read a big-endian `u16` from the start of `b`.
pub(crate) fn be_u16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

/// Read a big-endian `u32` from the start of `b`.
pub(crate) fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
