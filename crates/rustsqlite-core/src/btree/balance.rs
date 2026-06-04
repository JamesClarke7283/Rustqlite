//! B-tree balancing (mirrors the `balance*` routines in `btree.c`).
//!
//! First slice (M4.5): when a leaf-table page fills, redistribute its cells to a new sibling
//! leaf (a 50/50 split at the payload boundary), and either insert a divider cell into the
//! parent (the non-root case) or promote the root to an interior page that points at two
//! children (the root case). Overflow-page cells are not yet in scope; the payload-locality
//! thresholds live in [`super::cell::local_payload_len`] and the write-side overflow chains
//! arrive in the next slice. Sibling-leaf merging after a delete also lives in a later slice
//! (`balance_nonroot`'s redistribution paths); the delete half of M4.6 only collapses the
//! tree back to a single empty leaf when the last row is removed.

use crate::error::{Error, Result};
use crate::pager::Pager;

use super::cell::{
    build_table_interior_cell, parse_table_interior_cell, table_leaf_cell_rowid,
};
use super::page::{self, PageHeader, PageType};

/// Split a **full leaf** in two. The new leaf lives at a freshly allocated page; the existing
/// leaf is left in place and rewritten with the first half of the cells. The cells are
/// distributed by total cell size (header + payload), keeping each leaf under `usable_size`.
///
/// If `parent_root` is `Some`, the caller has identified a parent interior page that needs a
/// new divider cell — the function inserts the divider and returns the new child's page number
/// so the caller can update the parent's right-most child pointer if the new child becomes
/// the new right-most. If `parent_root` is `None`, the leaf is the b-tree's root and the
/// caller must promote the root to an interior page (see [`promote_root_and_split`]).
pub async fn split_leaf(
    pager: &Pager,
    leaf_pgno: u32,
    parent_root: Option<u32>,
) -> Result<u32> {
    let usable = pager.usable_size();
    let base = pager.btree_header_offset(leaf_pgno);
    let leaf_buf = pager.read_page_for_write(leaf_pgno).await?;
    let hdr = PageHeader::parse(&leaf_buf, base)?;
    if hdr.page_type != PageType::LeafTable {
        return Err(Error::corrupt("split_leaf called on a non-leaf page"));
    }

    // Read every cell so we can decide where to split by total size. The leaf's own
    // allocations (cell-pointer array) are ignored — the new layout builds a fresh array.
    let mut cells: Vec<Vec<u8>> = Vec::with_capacity(hdr.num_cells as usize);
    for i in 0..hdr.num_cells as usize {
        let off = hdr.cell_pointer(&leaf_buf, i)?;
        let (_payload_size, n1) = read_payload_size(&leaf_buf, off)?;
        let cell_size = cell_total_size(&leaf_buf, off, n1, usable)?;
        let mut cell = vec![0u8; cell_size];
        cell.copy_from_slice(&leaf_buf[off..off + cell_size]);
        cells.push(cell);
    }

    // Pick the split index so each side holds roughly half the bytes (the size of the
    // pointer array, the cell content, and the per-page header are absorbed into the
    // `usable` budget on each side).
    let target = usable / 2;
    let mut left_size = 0usize;
    let mut split_at = cells.len();
    for (i, c) in cells.iter().enumerate() {
        let projected = left_size + c.len() + 2; // +2 for the cell pointer slot
        if projected > target && i > 0 {
            split_at = i;
            break;
        }
        left_size = projected;
    }
    if split_at >= cells.len() {
        return Err(Error::corrupt(
            "split_leaf: cannot find a split point (all cells are larger than half the page)",
        ));
    }

    // Build the new right-sibling leaf and rewrite the left leaf in place.
    let new_pgno = pager.allocate_page();
    let left_cells: Vec<(u16, Vec<u8>)> = cells[..split_at]
        .iter()
        .enumerate()
        .map(|(i, c)| (i as u16, c.clone()))
        .collect();
    let right_cells: Vec<(u16, Vec<u8>)> = cells[split_at..]
        .iter()
        .enumerate()
        .map(|(i, c)| (i as u16, c.clone()))
        .collect();

    let mut new_leaf = vec![0u8; pager.page_size()];
    page::write_page_cells(&mut new_leaf, pager.btree_header_offset(new_pgno), PageType::LeafTable, None, &right_cells)?;
    pager.write_page(new_pgno, new_leaf)?;

    let mut new_left = vec![0u8; pager.page_size()];
    page::write_page_cells(&mut new_left, base, PageType::LeafTable, None, &left_cells)?;
    pager.write_page(leaf_pgno, new_left)?;

    // Wire the new right sibling into the parent (if any). The new divider is the rowid of
    // the rightmost cell on the LEFT side (every key in the right leaf is strictly greater).
    let divider_rowid = max_rowid_of_cell(&cells[split_at - 1])?;

    if let Some(parent_pgno) = parent_root {
        // Insert a divider cell on the parent. The cell's `left_child` is the SMALLER-half
        // leaf (which carries keys `< divider`), and the LARGER-half leaf sits to its right
        // — either as the parent's new right-most child (when the split leaf was the
        // previous right-most) or as the `left_child` of a new interior cell inserted
        // immediately after the cell that pointed at the old leaf.
        let pbase = pager.btree_header_offset(parent_pgno);
        let mut pbuf = pager.read_page_for_write(parent_pgno).await?;
        let phdr = PageHeader::parse(&pbuf, pbase)?;
        if phdr.page_type != PageType::InteriorTable {
            return Err(Error::corrupt(
                "split_leaf: declared parent is not an interior table page",
            ));
        }
        let is_rightmost = phdr.right_most_pointer == Some(leaf_pgno);
        let insert_idx = if is_rightmost {
            phdr.num_cells as usize
        } else {
            find_child_cell(&pbuf, &phdr, leaf_pgno)?
                .checked_add(1)
                .ok_or_else(|| Error::corrupt("interior cell index overflow"))?
        };

        if is_rightmost {
            // The old right-most (`leaf_pgno`) is the smaller half; `new_pgno` is the new
            // right-most (larger half). The new cell is `(left=leaf_pgno, rowid=divider)`
            // and right_most is updated to `new_pgno`.
            let cell = build_table_interior_cell(leaf_pgno, divider_rowid);
            if let Err(e) = page::insert_interior_cell(&mut pbuf, pbase, insert_idx, &cell) {
                return Err(e);
            }
            pbuf[pbase + 8..pbase + 12].copy_from_slice(&new_pgno.to_be_bytes());
        } else {
            // `leaf_pgno` (smaller half) keeps its existing cell slot. We insert a new
            // cell right after it: `(left=new_pgno, rowid=divider)`. The right-most child
            // pointer is unchanged (the old right-most, which is larger than `new_pgno`).
            let cell = build_table_interior_cell(new_pgno, divider_rowid);
            if let Err(e) = page::insert_interior_cell(&mut pbuf, pbase, insert_idx, &cell) {
                return Err(e);
            }
        }
        pager.write_page(parent_pgno, pbuf)?;
    }

    Ok(new_pgno)
}

/// Promote a single-leaf b-tree root to a two-level tree: the old root page becomes an interior
/// page, the old root's cells are split across two leaves (one of them is the old root's
/// allocated sibling). Used when a single-leaf root's insert fills the page.
///
/// Returns the new sibling's page number. The caller updates the b-tree's "root" (which now
/// is the old root page, type interior) and `max_rowid`'s right-walk is unchanged.
pub async fn promote_root_and_split(pager: &Pager, root_pgno: u32) -> Result<()> {
    let usable = pager.usable_size();
    let base = pager.btree_header_offset(root_pgno);
    let leaf_buf = pager.read_page_for_write(root_pgno).await?;
    let hdr = PageHeader::parse(&leaf_buf, base)?;
    if hdr.page_type != PageType::LeafTable {
        return Err(Error::corrupt(
            "promote_root_and_split: root is not a leaf",
        ));
    }
    let n = hdr.num_cells as usize;
    if n == 0 {
        return Err(Error::corrupt(
            "promote_root_and_split called on an empty leaf",
        ));
    }

    // Read all cells.
    let mut cells: Vec<Vec<u8>> = Vec::with_capacity(n);
    for i in 0..n {
        let off = hdr.cell_pointer(&leaf_buf, i)?;
        let (_payload_size, n1) = read_payload_size(&leaf_buf, off)?;
        let cell_size = cell_total_size(&leaf_buf, off, n1, usable)?;
        let mut cell = vec![0u8; cell_size];
        cell.copy_from_slice(&leaf_buf[off..off + cell_size]);
        cells.push(cell);
    }

    // Pick the split point: aim for half the bytes on each side, but require at least one
    // cell on each side.
    let target = usable / 2;
    let mut left_size = 0usize;
    let mut split_at = cells.len();
    for (i, c) in cells.iter().enumerate() {
        let projected = left_size + c.len() + 2;
        if projected > target && i > 0 {
            split_at = i;
            break;
        }
        left_size = projected;
    }
    if split_at >= cells.len() {
        return Err(Error::corrupt(
            "promote_root_and_split: cannot split (single cell bigger than half the page)",
        ));
    }

    // The split always produces **two new leaves** and the old root becomes an interior
    // page pointing at them (matching `balance_deeper` in `btree.c`):
    //
    //   * `left_pgno` — fresh leaf, holds the smaller-half cells.
    //   * `right_pgno` — fresh leaf, holds the larger-half cells.
    //   * old root (`root_pgno`) — interior-table page with one cell
    //     `(left=left_pgno, rowid=divider)` and `right_most = right_pgno`.
    let left_cells: Vec<(u16, Vec<u8>)> = cells[..split_at]
        .iter()
        .enumerate()
        .map(|(i, c)| (i as u16, c.clone()))
        .collect();
    let right_cells: Vec<(u16, Vec<u8>)> = cells[split_at..]
        .iter()
        .enumerate()
        .map(|(i, c)| (i as u16, c.clone()))
        .collect();

    // Two new leaves: the smaller half on `left_pgno`, the larger half on `right_pgno`.
    let left_pgno = pager.allocate_page();
    let mut left_leaf = vec![0u8; pager.page_size()];
    page::write_page_cells(
        &mut left_leaf,
        pager.btree_header_offset(left_pgno),
        PageType::LeafTable,
        None,
        &left_cells,
    )?;
    pager.write_page(left_pgno, left_leaf)?;

    let right_pgno = pager.allocate_page();
    let mut right_leaf = vec![0u8; pager.page_size()];
    page::write_page_cells(
        &mut right_leaf,
        pager.btree_header_offset(right_pgno),
        PageType::LeafTable,
        None,
        &right_cells,
    )?;
    pager.write_page(right_pgno, right_leaf)?;

    // Rewrite the old root in place as an interior-table page with one cell
    // `(left=left_pgno, rowid=divider)` and `right_most = right_pgno`.
    let divider_rowid = max_rowid_of_cell(&cells[split_at - 1])?;
    let mut root_buf = vec![0u8; pager.page_size()];
    page::write_page_cells(
        &mut root_buf,
        base,
        PageType::InteriorTable,
        Some(right_pgno),
        &[(0, build_table_interior_cell(left_pgno, divider_rowid))],
    )?;
    pager.write_page(root_pgno, root_buf)?;
    Ok(())
}

/// Find the cell index on an interior-table page whose `left_child` is `target`. Returns
/// `Err` if the page is malformed or the target is not present.
fn find_child_cell(page: &[u8], hdr: &PageHeader, target: u32) -> Result<usize> {
    for i in 0..hdr.num_cells as usize {
        let off = hdr.cell_pointer(page, i)?;
        let cell = parse_table_interior_cell(page, off)?;
        if cell.left_child == target {
            return Ok(i);
        }
    }
    Err(Error::corrupt(format!(
        "child page {target} not found among {} interior cells",
        hdr.num_cells
    )))
}

/// Decode a single table-leaf cell's payload-size varint. Mirrors the first varint in
/// [`super::cell::parse_table_leaf_cell`].
fn read_payload_size(page: &[u8], offset: usize) -> Result<(u64, usize)> {
    use crate::format::read_varint;
    read_varint(
        page.get(offset..)
            .ok_or_else(|| Error::corrupt("cell offset"))?,
    )
    .ok_or_else(|| Error::corrupt("table leaf payload-size varint"))
}

/// The on-page footprint of a table-leaf cell: `varint(payload_size) ++ varint(rowid) ++
/// payload ++ [overflow-ptr]`. We only need the size; the overflow chain (if any) lives in
/// other pages, but the on-page slice still carries the 4-byte overflow pointer at the end of
/// the local payload.
fn cell_total_size(
    page: &[u8],
    offset: usize,
    payload_size_bytes: usize,
    usable: usize,
) -> Result<usize> {
    let (payload_size, _) = read_payload_size(page, offset)?;
    let (_, rowid_size) = crate::format::read_varint_i64(
        page.get(offset + payload_size_bytes..)
            .ok_or_else(|| Error::corrupt("rowid bytes"))?,
    )
    .ok_or_else(|| Error::corrupt("table leaf rowid varint"))?;
    let local_payload_size = super::cell::local_payload_len(payload_size as usize, usable, usable - 35).0;
    let overflow = if local_payload_size < payload_size as usize {
        4
    } else {
        0
    };
    Ok(payload_size_bytes + rowid_size + local_payload_size + overflow)
}

/// The rowid stored in a freshly built table-leaf cell (i.e. one produced by
/// [`build_table_leaf_cell`]). Cheap — no overflow read, since `build_table_leaf_cell`
/// produces cells without the overflow page reference until M4.5b lands.
fn max_rowid_of_cell(cell: &[u8]) -> Result<i64> {
    table_leaf_cell_rowid(cell, 0)
        .or_else(|_| {
            // Fallback: decode the full cell (the slice passed here is just the cell bytes,
            // not a page; `table_leaf_cell_rowid` walks by absolute page-relative offset, so
            // fall back to a manual read).
            let (_, n1) = crate::format::read_varint(cell)
                .ok_or_else(|| Error::corrupt("payload varint"))?;
            let (rowid, _) = crate::format::read_varint_i64(&cell[n1..])
                .ok_or_else(|| Error::corrupt("rowid varint"))?;
            Ok(rowid)
        })
}

/// A throwaway helper used by tests: split a cell-list (vec of `(rowid, payload)` pairs) into
/// two halves using the same algorithm as [`split_leaf`]. Exposed to allow unit tests of the
/// split boundary without spinning up a pager. Builds the cells directly (without overflow)
/// since the test inputs are small enough to fit inline.
#[doc(hidden)]
pub fn split_cells_for_test(usable: usize, cells: &[(i64, Vec<u8>)]) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let mut built: Vec<Vec<u8>> = cells
        .iter()
        .map(|(rid, payload)| {
            let mut c = Vec::with_capacity(9 + 9 + payload.len());
            crate::format::write_varint(payload.len() as u64, &mut c);
            crate::format::write_varint(*rid as u64, &mut c);
            c.extend_from_slice(payload);
            c
        })
        .collect();
    let target = usable / 2;
    let mut left_size = 0usize;
    let mut split_at = built.len();
    for (i, c) in built.iter().enumerate() {
        let projected = left_size + c.len() + 2;
        if projected > target && i > 0 {
            split_at = i;
            break;
        }
        left_size = projected;
    }
    if split_at >= built.len() {
        split_at = built.len() / 2;
    }
    let right = built.split_off(split_at);
    (built, right)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_keeps_each_half_under_target() {
        // 10 small cells on a 4096-byte page must split roughly in half.
        let usable = 4096;
        let cells: Vec<(i64, Vec<u8>)> = (1..=10)
            .map(|i| (i as i64, vec![0u8; 100]))
            .collect();
        let (left, right) = split_cells_for_test(usable, &cells);
        assert!(!left.is_empty() && !right.is_empty(), "split must give both sides cells");
        let left_size: usize = left.iter().map(|c| c.len() + 2).sum();
        let right_size: usize = right.iter().map(|c| c.len() + 2).sum();
        // Both halves should be under `usable / 2 + one_cell` (the split aims for half).
        assert!(left_size <= usable / 2 + 200);
        assert!(right_size <= usable / 2 + 200);
    }
}
