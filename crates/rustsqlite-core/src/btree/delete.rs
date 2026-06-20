//! Table b-tree deletion (the write-path counterpart of [`super::insert`], mirroring
//! `OP_Delete` in `vdbe.c` and `sqlite3BtreeDelete` in `btree.c`).
//!
//! M4.6 first slice: removing a single cell from a leaf. The cursor's current position
//! is used to identify the row; we walk the page, find the cell at the cursor's
//! `cell_idx`, free any overflow chain it points at, and rewrite the page without that
//! cell. No underflow merging yet — the C `balance_nonroot` redistribution paths land in
//! a later slice; the first slice is sufficient to drop individual rows and to collapse
//! a table back to a single empty leaf when the last row is removed.

use crate::error::{Error, Result};
use crate::pager::Pager;

use super::cell::parse_table_leaf_cell;
use super::page::{self, PageHeader, PageType};

/// Remove the cell at `cell_idx` on the leaf-table page `leaf_pgno`. The page is rewritten
/// with one fewer cell. The cursor's `cell_idx` is left unchanged — the caller's loop
/// (`Rewind`/`Next`/`Delete`) will see the row that slid into the slot, which is the
/// next-larger rowid (the same behavior as upstream `OP_Delete`).
pub async fn leaf_delete_current(pager: &Pager, leaf_pgno: u32, cell_idx: usize) -> Result<()> {
    let base = pager.btree_header_offset(leaf_pgno);
    let mut leaf = pager.read_page_for_write(leaf_pgno).await?;
    let hdr = PageHeader::parse(&leaf, base)?;
    if hdr.page_type != PageType::LeafTable {
        return Err(Error::corrupt(
            "leaf_delete_current called on a non-leaf page",
        ));
    }
    if cell_idx >= hdr.num_cells as usize {
        return Err(Error::corrupt("leaf_delete_current: cell_idx out of range"));
    }

    // Reap the cell's overflow chain (if any) before we drop the cell bytes themselves.
    let cell_off = hdr.cell_pointer(&leaf, cell_idx)?;
    let usable = pager.usable_size();
    let cell = parse_table_leaf_cell(&leaf, cell_off, usable)?;
    if let Some(first_overflow) = cell.overflow_page {
        free_overflow_chain(pager, first_overflow).await?;
    }

    // First-slice approach: rebuild the page from the surviving cells. This sidesteps the
    // freeblock-chain machinery that lands with the underflow-merging slice.
    let n = hdr.num_cells as usize;
    let mut cells: Vec<(u16, Vec<u8>)> = Vec::with_capacity(n.saturating_sub(1));
    for i in 0..n {
        if i == cell_idx {
            continue;
        }
        let off = hdr.cell_pointer(&leaf, i)?;
        let size = cell_on_page_size(&leaf, off, usable)?;
        let mut cell_bytes = vec![0u8; size];
        cell_bytes.copy_from_slice(&leaf[off..off + size]);
        cells.push((cells.len() as u16, cell_bytes));
    }
    page::write_page_cells(&mut leaf, base, PageType::LeafTable, None, &cells)?;
    pager.write_page(leaf_pgno, leaf)?;
    Ok(())
}

/// Walk a chain of overflow pages and free each. The chain is
/// `[u32 next][u32 next]…` terminated by a `0`. The page's payload follows the `next`
/// pointer. Each page is added to the freelist via [`Pager::free_page`] (so a subsequent
/// allocation can reuse it, and the auto-vacuum pointer map records the free state). Rollback
/// reclaims the page numbers via the size-truncate path.
async fn free_overflow_chain(pager: &Pager, first_pgno: u32) -> Result<()> {
    let mut pgno = first_pgno;
    while pgno != 0 {
        let page = pager.get_page(pgno).await?;
        let next = u32::from_be_bytes([page[0], page[1], page[2], page[3]]);
        pager.free_page(pgno).await?;
        pgno = next;
    }
    Ok(())
}

/// Total on-page size of a table-leaf cell at `offset`, including the local payload and
/// the 4-byte overflow pointer (when present).
fn cell_on_page_size(page: &[u8], offset: usize, usable: usize) -> Result<usize> {
    let (payload_size, n1) = crate::format::read_varint(
        page.get(offset..)
            .ok_or_else(|| Error::corrupt("cell offset"))?,
    )
    .ok_or_else(|| Error::corrupt("table leaf payload-size varint"))?;
    let (_, rowid_size) = crate::format::read_varint_i64(
        page.get(offset + n1..)
            .ok_or_else(|| Error::corrupt("rowid bytes"))?,
    )
    .ok_or_else(|| Error::corrupt("table leaf rowid varint"))?;
    let max_local = usable - 35;
    let (local_len, has_overflow) =
        super::cell::local_payload_len(payload_size as usize, usable, max_local);
    let overflow = if has_overflow { 4 } else { 0 };
    Ok(n1 + rowid_size + local_len + overflow)
}
