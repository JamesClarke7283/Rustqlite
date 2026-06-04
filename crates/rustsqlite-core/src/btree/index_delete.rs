//! Single-leaf index deletion (mirrors the table-side `leaf_delete_current` for index pages).
//!
//! The first M5.1 slice uses this only from the `IdxDelete` opcode path. The cell lookup uses
//! a linear scan over the leaf's cells, matching the same comparison rule the insert path uses;
//! once the cell is found, we rebuild the page from the surviving cells (the table-side
//! `leaf_delete_current` does the same) — the cursor that triggered the delete is told to
//! advance past the slot that just slid in (`pending_advance = true`).

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::format::decode_record;
use crate::pager::Pager;
use crate::types::{Collation, Value};
use crate::vdbe::compare::mem_compare;

use super::cell::parse_index_leaf_cell;
use super::page::{self, PageType};

/// Remove the index entry whose key record matches `key_record` from the leaf-index b-tree
/// rooted at `root`. Returns `Ok(true)` when the entry was found and removed, `Ok(false)` when
/// the leaf has no such entry (a no-op for `IdxDelete`, matching upstream's
/// `sqlite3BtreeIndexMoveto + sqlite3BtreeDelete` path which is silent on a miss).
pub async fn index_leaf_delete(
    pager: &Arc<Pager>,
    root: u32,
    key_record: &[u8],
) -> Result<bool> {
    let usable = pager.usable_size();
    let base = pager.btree_header_offset(root);
    let page = pager.get_page(root).await?;
    let hdr = super::page::PageHeader::parse(&page, base)?;
    if hdr.page_type != PageType::LeafIndex {
        return Err(Error::corrupt("index_leaf_delete: not a leaf-index page"));
    }
    let n = hdr.num_cells as usize;
    let encoding = pager.text_encoding();
    let search_values = decode_record(key_record, encoding)?;
    let search_prefix_len = search_values.len().saturating_sub(1);
    let search_prefix = &search_values[..search_prefix_len];
    let search_rowid = &search_values[search_prefix_len];

    // Linear scan to find the matching cell. (The M5.1 first slice uses a single-leaf index;
    // a multi-leaf index would binary-search the leaf phase, but the populate/insert path
    // is also single-leaf, so a linear scan is consistent.)
    let mut found: Option<usize> = None;
    for i in 0..n {
        let off = hdr.cell_pointer(&page, i)?;
        let cell = parse_index_leaf_cell(&page, off, usable)?;
        let existing = decode_record(cell.local_payload, encoding)?;
        let existing_prefix_len = existing.len().saturating_sub(1);
        let existing_prefix = &existing[..existing_prefix_len];
        let existing_rowid = &existing[existing_prefix_len];
        if prefixes_equal(existing_prefix, search_prefix, Collation::Binary)
            && mem_compare(existing_rowid, search_rowid, Collation::Binary) == std::cmp::Ordering::Equal
        {
            found = Some(i);
            break;
        }
    }
    let Some(idx_to_remove) = found else {
        return Ok(false);
    };

    // Rebuild the page from the surviving cells. The cell sizes are read directly from the
    // on-page bytes (the varint payload-size + the local payload + the optional 4-byte
    // overflow pointer), the cells are sliced, and `write_page_cells` lays them out from
    // the end of the page. The cell pointer array is also rebuilt (same order as before
    // minus the deleted slot). This sidesteps the in-place compaction bug-prone math and
    // matches the table-side `leaf_delete_current` approach.
    let mut cells: Vec<(u16, Vec<u8>)> = Vec::with_capacity(n - 1);
    for i in 0..n {
        if i == idx_to_remove {
            continue;
        }
        let off = hdr.cell_pointer(&page, i)?;
        let size = cell_on_page_size(&page, off, usable);
        let cell_bytes = page[off..off + size].to_vec();
        cells.push((cells.len() as u16, cell_bytes));
    }
    let mut leaf = pager.read_page_for_write(root).await?;
    page::write_page_cells(&mut leaf, base, PageType::LeafIndex, None, &cells)?;
    pager.write_page(root, leaf)?;
    Ok(true)
}

/// Total on-page size of an index-leaf cell at `offset`, including the local payload and
/// the 4-byte overflow pointer (when present). Mirrors `cell_on_page_size` in
/// `delete.rs`; the cell layout for index leaves differs from table leaves in that there
/// is no rowid varint between the payload-size varint and the payload.
fn cell_on_page_size(page: &[u8], offset: usize, usable: usize) -> usize {
    let (payload_size, n1) = crate::format::read_varint(
        page.get(offset..).unwrap_or(&[]),
    )
    .unwrap_or((0, 0));
    let max_local = super::cell::index_max_local(usable);
    let (local_len, has_overflow) =
        super::cell::local_payload_len(payload_size as usize, usable, max_local);
    let overflow = if has_overflow { 4 } else { 0 };
    n1 + local_len + overflow
}

fn prefixes_equal(a: &[Value], b: &[Value], coll: Collation) -> bool {
    if a.len() != b.len() {
        return false;
    }
    for (x, y) in a.iter().zip(b.iter()) {
        if mem_compare(x, y, coll) != std::cmp::Ordering::Equal {
            return false;
        }
    }
    true
}
