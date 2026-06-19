//! B-tree balancing (mirrors the `balance*` routines in `btree.c`).
//!
//! M4.5 introduced table-leaf splitting: when a leaf-table page fills, redistribute its
//! cells to a new sibling leaf (a 50/50 split at the payload boundary), and either insert a
//! divider cell into the parent (the non-root case) or promote the root to an interior page
//! that points at two children (the root case). M5.2 adds the analogous logic for index
//! b-trees: [`split_index_leaf`] and [`promote_index_root_and_split`] mirror the table-side
//! helpers, producing interior-index divider cells whose key is a full index record rather
//! than a rowid. Overflow-page cells are handled when present; sibling merging on delete
//! arrives in a later milestone.

use crate::error::{Error, Result};
use crate::format::read_varint;
use crate::pager::Pager;

use super::cell::{
    assemble_index_interior_payload, build_index_interior_cell, build_table_interior_cell,
    index_max_local, local_payload_len, parse_index_interior_cell, parse_table_interior_cell,
    table_leaf_cell_rowid,
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
pub async fn split_leaf(pager: &Pager, leaf_pgno: u32, parent_root: Option<u32>) -> Result<u32> {
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
    page::write_page_cells(
        &mut new_leaf,
        pager.btree_header_offset(new_pgno),
        PageType::LeafTable,
        None,
        &right_cells,
    )?;
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
            find_child_cell_index(&pbuf, &phdr, leaf_pgno)?
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
        return Err(Error::corrupt("promote_root_and_split: root is not a leaf"));
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
    let local_payload_size =
        super::cell::local_payload_len(payload_size as usize, usable, usable - 35).0;
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
    table_leaf_cell_rowid(cell, 0).or_else(|_| {
        // Fallback: decode the full cell (the slice passed here is just the cell bytes,
        // not a page; `table_leaf_cell_rowid` walks by absolute page-relative offset, so
        // fall back to a manual read).
        let (_, n1) =
            crate::format::read_varint(cell).ok_or_else(|| Error::corrupt("payload varint"))?;
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
pub fn split_cells_for_test(
    usable: usize,
    cells: &[(i64, Vec<u8>)],
) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
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

// ---- Index b-tree splitting (M5.2) ----
//
// Mirrors the table-side helpers but for leaf-index and interior-index pages. The key
// difference is that index cells don't have a rowid varint — the divider on an interior page
// is a full index key record plus a left-child pointer, not `(left_child, rowid)`.

/// Compute the on-page size of an index-leaf cell at `offset`: `varint(payload_size) ++
/// local_payload ++ [4-byte overflow pointer]`. Mirrors the table-leaf version in
/// `cell_total_size` but without the rowid varint.
fn index_leaf_cell_on_page_size(page: &[u8], offset: usize, usable: usize) -> Result<usize> {
    let (payload_size, n1) = read_varint(
        page.get(offset..)
            .ok_or_else(|| Error::corrupt("cell offset"))?,
    )
    .ok_or_else(|| Error::corrupt("index leaf payload-size varint"))?;
    let max_local = index_max_local(usable);
    let (local_len, has_overflow) = local_payload_len(payload_size as usize, usable, max_local);
    let overflow = if has_overflow { 4 } else { 0 };
    Ok(n1 + local_len + overflow)
}

/// Split a **full index-leaf** page into two halves. For index b-trees, the divider key is
/// the **first key on the right side** (the cell at `split_at`), which is **promoted** into the
/// parent and removed from both child pages. This mirrors SQLite's `balance_nonroot` where index
/// b-trees use `leafData==0` and the divider cell is extracted from the cell array rather than
/// being a copy of an existing row (as it is for table b-trees).
///
/// After the split:
/// - Left child (leaf_pgno) holds `cells[0..split_at]`
/// - Right child (new_pgno) holds `cells[split_at+1..]`
/// - The parent receives an interior cell `(left=leaf_pgno, key=cells[split_at].key)`
/// - The parent's right-most pointer (or the next cell's left_child) covers new_pgno
///
/// If `parent_root` is `None`, the leaf is the b-tree's root; the caller must promote the root
/// (see [`promote_index_root_and_split`]).
///
/// Returns the page number of the newly allocated right sibling **and** the divider key
/// (the promoted cell's full index record, extracted from the leaf cell).
/// Split a **full index-leaf** page into two halves. For index b-trees, the divider key is
/// the **first key on the right side** (the cell at `split_at`), which is **promoted** into the
/// parent and removed from both child pages. This mirrors SQLite's `balance_nonroot` where index
/// b-trees use `leafData==0` and the divider cell is extracted from the cell array rather than
/// being a copy of an existing row (as it is for table b-trees).
pub async fn split_index_leaf(
    pager: &Pager,
    leaf_pgno: u32,
    parent_root: Option<u32>,
    _key_info: &[crate::vdbe::KeyField],
) -> Result<(u32, Vec<u8>)> {
    let usable = pager.usable_size();
    let base = pager.btree_header_offset(leaf_pgno);
    let leaf_buf = pager.read_page_for_write(leaf_pgno).await?;
    let hdr = PageHeader::parse(&leaf_buf, base)?;
    if hdr.page_type != PageType::LeafIndex {
        return Err(Error::corrupt(
            "split_index_leaf called on a non-index-leaf page",
        ));
    }

    let mut cells: Vec<Vec<u8>> = Vec::with_capacity(hdr.num_cells as usize);
    for i in 0..hdr.num_cells as usize {
        let off = hdr.cell_pointer(&leaf_buf, i)?;
        let size = index_leaf_cell_on_page_size(&leaf_buf, off, usable)?;
        let mut cell = vec![0u8; size];
        cell.copy_from_slice(&leaf_buf[off..off + size]);
        cells.push(cell);
    }

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
            "split_index_leaf: cannot find a split point (all cells are larger than half the page)",
        ));
    }
    // The divider is the cell at split_at (first cell of the right side). It is promoted from
    // the child into the parent, so it does NOT appear on either child page.
    let divider_key = index_key_from_leaf_cell(&cells[split_at], usable)?;

    let left_cells: Vec<(u16, Vec<u8>)> = cells[..split_at]
        .iter()
        .enumerate()
        .map(|(i, c)| (i as u16, c.clone()))
        .collect();
    let right_cells: Vec<(u16, Vec<u8>)> = cells[split_at + 1..]
        .iter()
        .enumerate()
        .map(|(i, c)| (i as u16, c.clone()))
        .collect();

    let new_pgno = pager.allocate_page();

    // Defer writing the child pages until the parent divider is successfully
    // installed. If the parent is full, install_index_divider will fail before
    // we have mutated the child pages, so a restart/retry can safely recompute
    // the split instead of leaving a half-written sibling page unreachable.
    if let Some(parent_pgno) = parent_root {
        install_index_divider(pager, parent_pgno, leaf_pgno, new_pgno, &divider_key).await?;
    }

    let mut new_right = vec![0u8; pager.page_size()];
    page::write_page_cells(
        &mut new_right,
        pager.btree_header_offset(new_pgno),
        PageType::LeafIndex,
        None,
        &right_cells,
    )?;
    pager.write_page(new_pgno, new_right)?;

    let mut new_left = vec![0u8; pager.page_size()];
    page::write_page_cells(&mut new_left, base, PageType::LeafIndex, None, &left_cells)?;
    pager.write_page(leaf_pgno, new_left)?;

    Ok((new_pgno, divider_key))
}

/// Install a divider into an interior-index parent after a child split. The divider cell is
/// `(left=old_child, key=divider_key)` and the parent's right-most pointer (or the next cell's
/// left_child) is updated to point at `new_child`.
///
/// For a non-rightmost child, the old parent cell `cell[idx] = (left=old_child, key=old_key)`
/// is replaced by two cells:
///   * `cell[idx]   = (left=old_child, key=divider_key)` — covers keys ≤ divider_key
///   * `cell[idx+1] = (left=new_child,  key=old_key)`      — covers keys > divider_key and ≤ old_key
/// This preserves the in-order traversal: visit old_child, yield divider_key, visit new_child,
/// yield old_key, continue.
///
async fn install_index_divider(
    pager: &Pager,
    parent_pgno: u32,
    old_child: u32,
    new_child: u32,
    divider_key: &[u8],
) -> Result<()> {
    let usable = pager.usable_size();
    let pbase = pager.btree_header_offset(parent_pgno);
    let mut pbuf = pager.read_page_for_write(parent_pgno).await?;
    let phdr = PageHeader::parse(&pbuf, pbase)?;
    if phdr.page_type != PageType::InteriorIndex {
        return Err(Error::corrupt(
            "install_index_divider: declared parent is not an interior-index page",
        ));
    }
    let is_rightmost = phdr.right_most_pointer == Some(old_child);

    if is_rightmost {
        let insert_idx = phdr.num_cells as usize;
        let cell = build_index_interior_cell(old_child, divider_key, pager, usable);
        if let Err(e) = page::insert_interior_cell(&mut pbuf, pbase, insert_idx, &cell) {
            return Err(e);
        }
        pbuf[pbase + 8..pbase + 12].copy_from_slice(&new_child.to_be_bytes());
    } else {
        let child_idx = find_child_cell_index(&pbuf, &phdr, old_child)?;
        let cell_off = phdr.cell_pointer(&pbuf, child_idx)?;
        let old_cell = parse_index_interior_cell(&pbuf, cell_off, usable)?;
        let old_key = assemble_index_interior_payload(pager, &old_cell).await?;
        debug_assert_eq!(old_cell.left_child, old_child);

        let cell_old = build_index_interior_cell(old_child, divider_key, pager, usable);
        let cell_new = build_index_interior_cell(new_child, &old_key, pager, usable);

        let mut new_cells: Vec<(usize, Vec<u8>)> = Vec::with_capacity(phdr.num_cells as usize + 1);
        for i in 0..phdr.num_cells as usize {
            if i == child_idx {
                new_cells.push((i, cell_old.clone()));
                new_cells.push((i + 1, cell_new.clone()));
            } else {
                let off = phdr.cell_pointer(&pbuf, i)?;
                let (ps, vn) = read_varint(&pbuf[off + 4..])
                    .ok_or_else(|| Error::corrupt("payload varint in parent cell"))?;
                let (ll, ho) = local_payload_len(ps as usize, usable, index_max_local(usable));
                let op = if ho { 4 } else { 0 };
                let sz = 4 + vn + ll + op;
                let mut c = vec![0u8; sz];
                c.copy_from_slice(&pbuf[off..off + sz]);
                new_cells.push((i, c));
            }
        }

        let right_most = phdr.right_most_pointer;
        let cells_with_idx: Vec<(u16, Vec<u8>)> = new_cells
            .iter()
            .enumerate()
            .map(|(i, (_, c))| (i as u16, c.clone()))
            .collect();
        page::write_page_cells(
            &mut pbuf,
            pbase,
            PageType::InteriorIndex,
            right_most,
            &cells_with_idx,
        )?;
    }
    pager.write_page(parent_pgno, pbuf)?;
    Ok(())
}

/// Split a **full index-interior** page into two halves. The divider key is the **first key on
/// the right side** (promoted from the cell at `split_at`), removed from both children, and
/// installed in the parent. Interior cells keep their `left_child` prefix on the children; only
/// the divider that goes to the parent has the prefix stripped.
pub async fn split_index_interior_page(
    pager: &Pager,
    interior_pgno: u32,
    parent_root: Option<u32>,
    _key_info: &[crate::vdbe::KeyField],
) -> Result<(u32, Vec<u8>)> {
    let usable = pager.usable_size();
    let base = pager.btree_header_offset(interior_pgno);
    let buf = pager.read_page_for_write(interior_pgno).await?;
    let hdr = PageHeader::parse(&buf, base)?;
    if hdr.page_type != PageType::InteriorIndex {
        return Err(Error::corrupt(
            "split_index_interior_page called on a non-index-interior page",
        ));
    }
    let right_most = hdr.right_most_pointer;

    let mut cells: Vec<Vec<u8>> = Vec::with_capacity(hdr.num_cells as usize);
    for i in 0..hdr.num_cells as usize {
        let off = hdr.cell_pointer(&buf, i)?;
        let size = index_interior_cell_on_page_size(&buf, off, usable)?;
        let mut cell = vec![0u8; size];
        cell.copy_from_slice(&buf[off..off + size]);
        cells.push(cell);
    }

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
            "split_index_interior_page: cannot find a split point",
        ));
    }
    let divider_key = index_key_from_interior_cell(&cells[split_at], usable)?;
    // The cell at split_at is promoted to the parent. Its left-child pointer
    // becomes the new right-most pointer of the left half, because every key
    // between the previous right-most boundary and the divider key now lives in
    // that subtree.
    let left_right_most = u32::from_be_bytes([
        cells[split_at][0],
        cells[split_at][1],
        cells[split_at][2],
        cells[split_at][3],
    ]);

    let left_cells: Vec<(u16, Vec<u8>)> = cells[..split_at]
        .iter()
        .enumerate()
        .map(|(i, c)| (i as u16, c.clone()))
        .collect();
    let right_cells: Vec<(u16, Vec<u8>)> = cells[split_at + 1..]
        .iter()
        .enumerate()
        .map(|(i, c)| (i as u16, c.clone()))
        .collect();

    let new_pgno = pager.allocate_page();

    // Defer writing the child pages until the parent divider is successfully
    // installed, matching the index-leaf split.
    if let Some(parent_pgno) = parent_root {
        install_index_divider(pager, parent_pgno, interior_pgno, new_pgno, &divider_key).await?;
    }

    let mut new_right = vec![0u8; pager.page_size()];
    page::write_page_cells(
        &mut new_right,
        pager.btree_header_offset(new_pgno),
        PageType::InteriorIndex,
        right_most,
        &right_cells,
    )?;
    pager.write_page(new_pgno, new_right)?;

    let mut new_left = vec![0u8; pager.page_size()];
    page::write_page_cells(
        &mut new_left,
        base,
        PageType::InteriorIndex,
        Some(left_right_most),
        &left_cells,
    )?;
    pager.write_page(interior_pgno, new_left)?;

    Ok((new_pgno, divider_key))
}

/// The on-page size of an index-interior cell: `u32(left_child) ++ varint(payload_size) ++
/// local_payload ++ [4-byte overflow pointer]`.
fn index_interior_cell_on_page_size(page: &[u8], offset: usize, usable: usize) -> Result<usize> {
    let (payload_size, n1) = read_varint(&page[offset + 4..])
        .ok_or_else(|| Error::corrupt("index interior payload-size varint"))?;
    let n1 = n1 + 4;
    let max_local = index_max_local(usable);
    let (local_len, has_overflow) = local_payload_len(payload_size as usize, usable, max_local);
    let overflow = if has_overflow { 4 } else { 0 };
    Ok(n1 + local_len + overflow)
}

/// Extract the key record from an index-interior cell's on-page bytes. Interior cells start
/// with a 4-byte left-child pointer before the payload-size varint.
fn index_key_from_interior_cell(cell: &[u8], usable: usize) -> Result<Vec<u8>> {
    let (payload_size, n1) = read_varint(&cell[4..])
        .ok_or_else(|| Error::corrupt("index interior payload-size varint in divider key"))?;
    let n1 = n1 + 4;
    let max_local = index_max_local(usable);
    let (local_len, has_overflow) = local_payload_len(payload_size as usize, usable, max_local);
    let key_len = if has_overflow {
        local_len
    } else {
        payload_size as usize
    };
    Ok(cell[n1..n1 + key_len].to_vec())
}

/// Promote a single-leaf index root to a two-level tree. For index b-trees the divider key is
/// the **first key on the right side** (promoted from the child, not a copy of the last left
/// key as in table b-trees).
///
/// After the promotion:
/// - Left child (left_pgno) holds `cells[0..split_at]`
/// - Right child (right_pgno) holds `cells[split_at+1..]`
/// - The root (root_pgno) becomes an interior page with one cell
///   `(left=left_pgno, key=cells[split_at].key)` and `right_most = right_pgno`
pub async fn promote_index_root_and_split(pager: &Pager, root_pgno: u32) -> Result<()> {
    promote_index_root(pager, root_pgno, PageType::LeafIndex).await
}

/// Promote a single-interior index root to a two-level tree when the interior root page
/// overflows. The root's cells are split across two new interior pages, and the old root is
/// rewritten as an interior-index page pointing at them.
pub async fn promote_index_root_interior(pager: &Pager, root_pgno: u32) -> Result<()> {
    promote_index_root(pager, root_pgno, PageType::InteriorIndex).await
}

async fn promote_index_root(pager: &Pager, root_pgno: u32, child_type: PageType) -> Result<()> {
    let usable = pager.usable_size();
    let base = pager.btree_header_offset(root_pgno);
    let root_buf = pager.read_page_for_write(root_pgno).await?;
    let hdr = PageHeader::parse(&root_buf, base)?;
    if hdr.page_type != child_type {
        return Err(Error::corrupt(format!(
            "promote_index_root: root is not a {child_type:?} page"
        )));
    }
    let is_interior = child_type == PageType::InteriorIndex;
    let n = hdr.num_cells as usize;
    if n == 0 {
        return Err(Error::corrupt("promote_index_root called on an empty page"));
    }

    let mut cells: Vec<Vec<u8>> = Vec::with_capacity(n);
    for i in 0..n {
        let off = hdr.cell_pointer(&root_buf, i)?;
        let size = if is_interior {
            index_interior_cell_on_page_size(&root_buf, off, usable)?
        } else {
            index_leaf_cell_on_page_size(&root_buf, off, usable)?
        };
        let mut cell = vec![0u8; size];
        cell.copy_from_slice(&root_buf[off..off + size]);
        cells.push(cell);
    }

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
            "promote_index_root: cannot split (single cell bigger than half the page)",
        ));
    }

    let right_most = if is_interior {
        hdr.right_most_pointer
    } else {
        None
    };
    let (divider_key, left_right_most) = if is_interior {
        let key = index_key_from_interior_cell(&cells[split_at], usable)?;
        let left_ptr = u32::from_be_bytes([
            cells[split_at][0],
            cells[split_at][1],
            cells[split_at][2],
            cells[split_at][3],
        ]);
        (key, Some(left_ptr))
    } else {
        (
            index_key_from_leaf_cell(&cells[split_at], usable)?,
            right_most,
        )
    };

    let left_cells: Vec<(u16, Vec<u8>)> = cells[..split_at]
        .iter()
        .enumerate()
        .map(|(i, c)| (i as u16, c.clone()))
        .collect();
    let right_cells: Vec<(u16, Vec<u8>)> = cells[split_at + 1..]
        .iter()
        .enumerate()
        .map(|(i, c)| (i as u16, c.clone()))
        .collect();

    let left_pgno = pager.allocate_page();
    let mut left_child = vec![0u8; pager.page_size()];
    page::write_page_cells(
        &mut left_child,
        pager.btree_header_offset(left_pgno),
        child_type,
        if is_interior {
            left_right_most
        } else {
            right_most
        },
        &left_cells,
    )?;
    pager.write_page(left_pgno, left_child)?;

    let right_pgno = pager.allocate_page();
    let mut right_child = vec![0u8; pager.page_size()];
    page::write_page_cells(
        &mut right_child,
        pager.btree_header_offset(right_pgno),
        child_type,
        right_most,
        &right_cells,
    )?;
    pager.write_page(right_pgno, right_child)?;

    // The old root becomes an interior-index page. Its single cell points at the left child
    // with the divider key, and its right-most pointer is the right child. The divider key
    // itself is promoted from the split point and does not live on either child page.
    let mut root_buf = vec![0u8; pager.page_size()];
    page::write_page_cells(
        &mut root_buf,
        base,
        PageType::InteriorIndex,
        Some(right_pgno),
        &[(
            0,
            build_index_interior_cell(left_pgno, &divider_key, pager, usable),
        )],
    )?;
    pager.write_page(root_pgno, root_buf)?;
    Ok(())
}

/// Extract the key record from an index-leaf cell's on-page bytes (decode the payload-size
/// varint, then slice out the payload including any overflow pointer). For divider purposes
/// this is the complete key and is identical regardless of whether overflow is present (the
/// on-page bytes already account for the local payload + overflow pointer if any).
fn index_key_from_leaf_cell(cell: &[u8], usable: usize) -> Result<Vec<u8>> {
    let (payload_size, n1) = read_varint(cell)
        .ok_or_else(|| Error::corrupt("index leaf payload-size varint in divider key"))?;
    let max_local = index_max_local(usable);
    let (local_len, has_overflow) = local_payload_len(payload_size as usize, usable, max_local);
    let overflow_size = if has_overflow { 4 } else { 0 };
    let total_size = n1 + local_len + overflow_size;
    if cell.len() < total_size {
        // The cell bytes were sliced from the page; use what's available.
        // For cells without overflow, the full payload is inline.
    }
    // For non-overflow cells (the common case in split contexts since overflow cells
    // are the minority), the key is `cell[n1..n1+local_len]`.
    // For cells that had overflow, the key record was already fully reassembled
    // before the write path builds the cell. In the balance path we work with
    // on-page bytes — but any overflow pointer is just 4 trailing bytes we skip.
    let key_len = if has_overflow {
        local_len
    } else {
        payload_size as usize
    };
    Ok(cell[n1..n1 + key_len].to_vec())
}

/// Find the cell index on an interior page whose `left_child` is `target`. Works for both
/// `InteriorTable` and `InteriorIndex` pages (both use a 4-byte left_child prefix per cell).
fn find_child_cell_index(page: &[u8], hdr: &PageHeader, target: u32) -> Result<usize> {
    match hdr.page_type {
        PageType::InteriorTable => {
            for i in 0..hdr.num_cells as usize {
                let off = hdr.cell_pointer(page, i)?;
                let cell = parse_table_interior_cell(page, off)?;
                if cell.left_child == target {
                    return Ok(i);
                }
            }
        }
        PageType::InteriorIndex => {
            for i in 0..hdr.num_cells as usize {
                let off = hdr.cell_pointer(page, i)?;
                // Interior-index cells start with a 4-byte left_child pointer.
                let left_child =
                    u32::from_be_bytes([page[off], page[off + 1], page[off + 2], page[off + 3]]);
                if left_child == target {
                    return Ok(i);
                }
            }
        }
        _ => {
            return Err(Error::corrupt(
                "find_child_cell_index: page is not an interior page",
            ))
        }
    }
    Err(Error::corrupt(format!(
        "child page {target} not found among {} interior cells",
        hdr.num_cells
    )))
}

/// Outcome of rebalancing a table leaf after a delete. When the leaf was merged
/// into its left sibling, the caller must move the cursor to the surviving page
/// and offset `cell_idx` by `cell_idx_offset` so the scan continues from the same
/// logical row.
#[derive(Debug)]
pub struct RebalanceOutcome {
    pub leaf_pgno: u32,
    pub cell_idx_offset: usize,
}

/// Split a **full table-interior** page into two halves. The divider is the
/// rightmost rowid on the left side (a copy of an existing row, as table b-trees
/// use `leafData==1`). This mirrors the table-side leaf split for interior pages
/// and lets table b-trees grow past two levels.
///
/// Returns the page number of the newly allocated right sibling.
pub async fn split_table_interior_page(
    pager: &Pager,
    interior_pgno: u32,
    parent_root: Option<u32>,
) -> Result<u32> {
    let usable = pager.usable_size();
    let base = pager.btree_header_offset(interior_pgno);
    let buf = pager.read_page_for_write(interior_pgno).await?;
    let hdr = PageHeader::parse(&buf, base)?;
    if hdr.page_type != PageType::InteriorTable {
        return Err(Error::corrupt(
            "split_table_interior_page called on a non-table-interior page",
        ));
    }
    let right_most = hdr.right_most_pointer;

    let mut cells: Vec<Vec<u8>> = Vec::with_capacity(hdr.num_cells as usize);
    for i in 0..hdr.num_cells as usize {
        let off = hdr.cell_pointer(&buf, i)?;
        let size = table_interior_cell_on_page_size(&buf, off)?;
        let mut cell = vec![0u8; size];
        cell.copy_from_slice(&buf[off..off + size]);
        cells.push(cell);
    }

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
            "split_table_interior_page: cannot find a split point",
        ));
    }
    let divider_rowid = max_rowid_of_cell(&cells[split_at - 1])?;
    let left_right_most = super::be_u32(&cells[split_at - 1][..4]);

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

    let new_pgno = pager.allocate_page();

    if let Some(parent_pgno) = parent_root {
        install_table_divider(pager, parent_pgno, interior_pgno, new_pgno, divider_rowid).await?;
    }

    let mut new_right = vec![0u8; pager.page_size()];
    page::write_page_cells(
        &mut new_right,
        pager.btree_header_offset(new_pgno),
        PageType::InteriorTable,
        right_most,
        &right_cells,
    )?;
    pager.write_page(new_pgno, new_right)?;

    let mut new_left = vec![0u8; pager.page_size()];
    page::write_page_cells(
        &mut new_left,
        base,
        PageType::InteriorTable,
        Some(left_right_most),
        &left_cells,
    )?;
    pager.write_page(interior_pgno, new_left)?;

    Ok(new_pgno)
}

/// Install a divider into an interior-table parent after a child split. The divider
/// cell is `(left=old_child, rowid=divider_rowid)` and the parent's right-most pointer
/// (or the next cell's left_child) is updated to point at `new_child`.
async fn install_table_divider(
    pager: &Pager,
    parent_pgno: u32,
    old_child: u32,
    new_child: u32,
    divider_rowid: i64,
) -> Result<()> {
    let pbase = pager.btree_header_offset(parent_pgno);
    let mut pbuf = pager.read_page_for_write(parent_pgno).await?;
    let phdr = PageHeader::parse(&pbuf, pbase)?;
    if phdr.page_type != PageType::InteriorTable {
        return Err(Error::corrupt(
            "install_table_divider: declared parent is not an interior-table page",
        ));
    }
    let is_rightmost = phdr.right_most_pointer == Some(old_child);

    if is_rightmost {
        let insert_idx = phdr.num_cells as usize;
        let cell = build_table_interior_cell(old_child, divider_rowid);
        page::insert_interior_cell(&mut pbuf, pbase, insert_idx, &cell)?;
        pbuf[pbase + 8..pbase + 12].copy_from_slice(&new_child.to_be_bytes());
    } else {
        let child_idx = find_child_cell_index(&pbuf, &phdr, old_child)?;
        let cells_with_idx: Vec<(usize, Vec<u8>)> = (0..phdr.num_cells as usize)
            .map(|i| {
                let off = phdr.cell_pointer(&pbuf, i).ok()?;
                let left = super::be_u32(&pbuf[off..off + 4]);
                let (rowid, rowid_size) = crate::format::read_varint_i64(&pbuf[off + 4..])?;
                let mut cell = Vec::with_capacity(4 + rowid_size);
                cell.extend_from_slice(&left.to_be_bytes());
                crate::format::write_varint(rowid as u64, &mut cell);
                Some((i, cell))
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| Error::corrupt("install_table_divider: bad parent cell"))?;

        let mut new_cells: Vec<(u16, Vec<u8>)> = Vec::with_capacity(cells_with_idx.len() + 1);
        for (i, c) in cells_with_idx.iter().enumerate() {
            if i == child_idx {
                new_cells.push((i as u16, build_table_interior_cell(old_child, divider_rowid)));
                // The old cell's rowid becomes the boundary for the new child pointer.
                let old_rowid = max_rowid_of_cell(&c.1)?;
                new_cells.push((i as u16 + 1, build_table_interior_cell(new_child, old_rowid)));
            } else {
                new_cells.push((i as u16, c.1.clone()));
            }
        }
        page::write_page_cells(
            &mut pbuf,
            pbase,
            PageType::InteriorTable,
            phdr.right_most_pointer,
            &new_cells,
        )?;
    }
    pager.write_page(parent_pgno, pbuf)?;
    Ok(())
}

/// The on-page size of a table-interior cell: `u32(left_child) ++ varint(rowid)`.
fn table_interior_cell_on_page_size(page: &[u8], offset: usize) -> Result<usize> {
    let (_, rowid_size) = crate::format::read_varint_i64(&page[offset + 4..])
        .ok_or_else(|| Error::corrupt("table interior rowid varint"))?;
    Ok(4 + rowid_size)
}

/// Rebalance a table-leaf page after a delete has left it underfull.
///
/// SQLite rebalances when a leaf's free space exceeds 2/3 of the usable area.
/// The leaf may be merged with a sibling (if the combined cells fit on one page),
/// or cells may be redistributed so both pages are adequately full. When the
/// parent is the root and the merge leaves it with a single right-most child, the
/// tree height is collapsed by copying the child into the root.
///
/// `parent_pgno` is the immediate interior-table parent and `child_idx` is the
/// index of `leaf_pgno` among the parent's children (0..=num_cells, where
/// num_cells means the right-most child). `is_root_parent` is true when the
/// parent is the root of this b-tree.
///
/// Returns `None` when no rebalance was needed or when the leaf stayed in place.
/// Returns `Some(RebalanceOutcome)` when the leaf was merged into its left
/// sibling and the cursor must be repositioned.
pub async fn rebalance_table_leaf_after_delete(
    pager: &Pager,
    leaf_pgno: u32,
    parent_pgno: u32,
    child_idx: usize,
    is_root_parent: bool,
) -> Result<Option<RebalanceOutcome>> {
    let usable = pager.usable_size();
    let base = pager.btree_header_offset(leaf_pgno);
    let leaf_buf = pager.get_page(leaf_pgno).await?;
    let hdr = PageHeader::parse(&leaf_buf, base)?;
    if hdr.page_type != PageType::LeafTable {
        return Err(Error::corrupt(
            "rebalance_table_leaf_after_delete: not a leaf-table page",
        ));
    }

    // Free space on a freshly rebuilt leaf is just the unallocated gap.
    let free_space = page::leaf_free_space(&leaf_buf, base);
    // Rebalance if free space is greater than 2/3 of the usable area. Also skip
    // rebalancing if the leaf is now empty: an empty table b-tree is valid as a
    // single empty leaf page (the M4 full-table-delete path already collapses to
    // this case).
    if free_space * 3 <= usable * 2 || hdr.num_cells == 0 {
        return Ok(None);
    }

    let pbase = pager.btree_header_offset(parent_pgno);
    let pbuf = pager.get_page(parent_pgno).await?;
    let phdr = PageHeader::parse(&pbuf, pbase)?;
    // The parent may have already been collapsed into a leaf by a prior rebalance
    // in the same DELETE scan. In that case there is nothing left to do.
    if phdr.page_type == PageType::LeafTable {
        return Ok(None);
    }
    if phdr.page_type != PageType::InteriorTable {
        return Err(Error::corrupt(
            "rebalance_table_leaf_after_delete: parent is not an interior table",
        ));
    }
    let n = phdr.num_cells as usize;

    // Locate immediate siblings in the parent's child order.
    let right_sibling = if child_idx < n {
        Some(
            parse_table_interior_cell(&pbuf, phdr.cell_pointer(&pbuf, child_idx)?)?
                .left_child,
        )
    } else {
        None
    };
    let left_sibling = if child_idx > 0 {
        Some(
            parse_table_interior_cell(&pbuf, phdr.cell_pointer(&pbuf, child_idx - 1)?)?
                .left_child,
        )
    } else {
        None
    };

    let leaf_cells = read_table_leaf_cells(&leaf_buf, base, usable)?;

    // Prefer merging with the right sibling; this keeps the cursor's leaf in place.
    if let Some(right_pgno) = right_sibling {
        let right_base = pager.btree_header_offset(right_pgno);
        let right_buf = pager.get_page(right_pgno).await?;
        let right_cells = read_table_leaf_cells(&right_buf, right_base, usable)?;
        let combined_layout = table_cells_layout_size(&leaf_cells) + right_cells.iter().map(|c| c.len()).sum::<usize>() + right_cells.len() * 2;
        if combined_layout + 8 <= usable {
            // Merge right sibling into the current leaf.
            let mut combined = leaf_cells;
            combined.extend(right_cells);
            write_table_leaf(pager, leaf_pgno, base, &combined).await?;
            pager.free_page(right_pgno).await?;
            rebuild_parent_without_divider(pager, parent_pgno, child_idx, is_root_parent)
                .await?;
            return Ok(None);
        }

        // Redistribute cells between the leaf and the right sibling.
        let (left_cells, right_cells) = redistribute_table_cells(usable, leaf_cells, right_cells)?;
        let divider_rowid = max_rowid_of_cells(&left_cells)?;
        write_table_leaf(pager, leaf_pgno, base, &left_cells).await?;
        write_table_leaf(pager, right_pgno, right_base, &right_cells).await?;
        rebuild_parent_with_divider(pager, parent_pgno, child_idx, divider_rowid).await?;
        return Ok(None);
    }

    // No right sibling: try the left sibling.
    if let Some(left_pgno) = left_sibling {
        let left_base = pager.btree_header_offset(left_pgno);
        let left_buf = pager.get_page(left_pgno).await?;
        let left_cells = read_table_leaf_cells(&left_buf, left_base, usable)?;
        let combined_layout = table_cells_layout_size(&left_cells) + leaf_cells.iter().map(|c| c.len()).sum::<usize>() + leaf_cells.len() * 2;
        if combined_layout + 8 <= usable {
            // Merge the current leaf into the left sibling. The cursor must move.
            let offset = left_cells.len();
            let mut combined = left_cells.clone();
            combined.extend(leaf_cells);
            write_table_leaf(pager, left_pgno, left_base, &combined).await?;
            pager.free_page(leaf_pgno).await?;
            rebuild_parent_without_divider(pager, parent_pgno, child_idx - 1, is_root_parent)
                .await?;
            return Ok(Some(RebalanceOutcome {
                leaf_pgno: left_pgno,
                cell_idx_offset: offset,
            }));
        }

        // Redistribute cells between the left sibling and the leaf.
        let (left_cells, right_cells) = redistribute_table_cells(usable, left_cells, leaf_cells)?;
        let divider_rowid = max_rowid_of_cells(&left_cells)?;
        write_table_leaf(pager, left_pgno, left_base, &left_cells).await?;
        write_table_leaf(pager, leaf_pgno, base, &right_cells).await?;
        rebuild_parent_with_divider(pager, parent_pgno, child_idx - 1, divider_rowid).await?;
        return Ok(None);
    }

    Ok(None)
}

/// Read every table-leaf cell from `page` as a freshly allocated byte vector.
fn read_table_leaf_cells(page: &[u8], base: usize, usable: usize) -> Result<Vec<Vec<u8>>> {
    let hdr = PageHeader::parse(page, base)?;
    if hdr.page_type != PageType::LeafTable {
        return Err(Error::corrupt("read_table_leaf_cells: not a table leaf"));
    }
    let mut cells = Vec::with_capacity(hdr.num_cells as usize);
    for i in 0..hdr.num_cells as usize {
        let off = hdr.cell_pointer(page, i)?;
        let (_payload_size, n1) = read_payload_size(page, off)?;
        let size = cell_total_size(page, off, n1, usable)?;
        cells.push(page[off..off + size].to_vec());
    }
    Ok(cells)
}

/// The on-page layout size of a list of table-leaf cells if they were placed on
/// a single page: 8-byte header + 2 bytes per pointer + sum of cell sizes.
fn table_cells_layout_size(cells: &[Vec<u8>]) -> usize {
    8 + cells.len() * 2 + cells.iter().map(|c| c.len()).sum::<usize>()
}

/// The largest rowid among a nonempty list of table-leaf cells.
fn max_rowid_of_cells(cells: &[Vec<u8>]) -> Result<i64> {
    cells
        .last()
        .map(|c| max_rowid_of_cell(c))
        .unwrap_or_else(|| Ok(0))
}

/// Rewrite a table-leaf page with the given cells.
async fn write_table_leaf(
    pager: &Pager,
    pgno: u32,
    base: usize,
    cells: &[Vec<u8>],
) -> Result<()> {
    let cells_with_idx: Vec<(u16, Vec<u8>)> = cells
        .iter()
        .enumerate()
        .map(|(i, c)| (i as u16, c.clone()))
        .collect();
    let mut buf = pager.read_page_for_write(pgno).await?;
    page::write_page_cells(&mut buf, base, PageType::LeafTable, None, &cells_with_idx)?;
    pager.write_page(pgno, buf)?;
    Ok(())
}

/// Redistribute two lists of table-leaf cells across two pages so each page fits.
/// The combined cells are kept in ascending order. The first returned list is the
/// left (smaller-rowid) half, the second is the right half.
fn redistribute_table_cells(
    usable: usize,
    left_cells: Vec<Vec<u8>>,
    right_cells: Vec<Vec<u8>>,
) -> Result<(Vec<Vec<u8>>, Vec<Vec<u8>>)> {
    let mut combined = left_cells;
    combined.extend(right_cells);
    let n = combined.len();
    if n < 2 {
        return Err(Error::corrupt(
            "redistribute_table_cells: need at least two cells",
        ));
    }

    // Fill the left page as full as possible without overflowing, then assign
    // the remainder to the right page. Because each original page fit on its
    // own, the right remainder also fits in the common case. Guard against
    // degenerate huge cells by ensuring the right page gets at least one cell.
    let mut k = 0usize;
    let mut left_layout = 8usize;
    while k < n - 1 {
        let next_layout = left_layout + combined[k].len() + 2;
        if next_layout > usable {
            break;
        }
        left_layout = next_layout;
        k += 1;
    }
    if k == 0 {
        // Even a single cell overflows; keep at least one cell on each side.
        k = 1;
    }
    let right_cells = combined.split_off(k);
    Ok((combined, right_cells))
}

/// Rebuild a parent interior-table page with the divider at `divider_idx` removed.
/// If `is_root_parent` is true and the removal leaves the root with no cells and a
/// single right-most child, the tree is collapsed by copying the child into the root.
async fn rebuild_parent_without_divider(
    pager: &Pager,
    parent_pgno: u32,
    divider_idx: usize,
    is_root_parent: bool,
) -> Result<()> {
    let pbase = pager.btree_header_offset(parent_pgno);
    let pbuf = pager.get_page(parent_pgno).await?;
    let phdr = PageHeader::parse(&pbuf, pbase)?;
    if phdr.page_type != PageType::InteriorTable {
        return Err(Error::corrupt("rebuild_parent_without_divider: not interior"));
    }
    let n = phdr.num_cells as usize;
    if divider_idx >= n {
        return Err(Error::corrupt("rebuild_parent_without_divider: bad idx"));
    }

    let mut cells: Vec<Vec<u8>> = Vec::with_capacity(n.saturating_sub(1));
    for i in 0..n {
        if i == divider_idx {
            continue;
        }
        let off = phdr.cell_pointer(&pbuf, i)?;
        let left_child = super::be_u32(&pbuf[off..off + 4]);
        let (rowid, rowid_size) = crate::format::read_varint_i64(&pbuf[off + 4..])
            .ok_or_else(|| Error::corrupt("parent rowid varint"))?;
        let mut cell = Vec::with_capacity(4 + rowid_size);
        cell.extend_from_slice(&left_child.to_be_bytes());
        crate::format::write_varint(rowid as u64, &mut cell);
        cells.push(cell);
    }

    let right_most = if divider_idx == n - 1 {
        // The removed divider sat just before the old right-most child. The
        // combined child (formerly the left child of that divider) becomes the
        // new right-most child.
        let off = phdr.cell_pointer(&pbuf, n - 1)?;
        Some(super::be_u32(&pbuf[off..off + 4]))
    } else {
        phdr.right_most_pointer
    };

    let cells_with_idx: Vec<(u16, Vec<u8>)> = cells
        .iter()
        .enumerate()
        .map(|(i, c)| (i as u16, c.clone()))
        .collect();
    let mut new_parent = pager.read_page_for_write(parent_pgno).await?;
    page::write_page_cells(
        &mut new_parent,
        pbase,
        PageType::InteriorTable,
        right_most,
        &cells_with_idx,
    )?;
    pager.write_page(parent_pgno, new_parent)?;

    if is_root_parent && cells_with_idx.is_empty() {
        if let Some(child) = right_most {
            collapse_root_into_child(pager, parent_pgno, child).await?;
        }
    }
    Ok(())
}

/// Rebuild a parent interior-table page, updating the rowid of the divider at
/// `divider_idx` to `new_rowid`. The left_child pointer of every cell is preserved.
async fn rebuild_parent_with_divider(
    pager: &Pager,
    parent_pgno: u32,
    divider_idx: usize,
    new_rowid: i64,
) -> Result<()> {
    let pbase = pager.btree_header_offset(parent_pgno);
    let pbuf = pager.get_page(parent_pgno).await?;
    let phdr = PageHeader::parse(&pbuf, pbase)?;
    if phdr.page_type != PageType::InteriorTable {
        return Err(Error::corrupt("rebuild_parent_with_divider: not interior"));
    }
    let n = phdr.num_cells as usize;
    if divider_idx >= n {
        return Err(Error::corrupt("rebuild_parent_with_divider: bad idx"));
    }

    let mut cells: Vec<Vec<u8>> = Vec::with_capacity(n);
    for i in 0..n {
        let off = phdr.cell_pointer(&pbuf, i)?;
        let left_child = super::be_u32(&pbuf[off..off + 4]);
        let rowid = if i == divider_idx {
            new_rowid
        } else {
            crate::format::read_varint_i64(&pbuf[off + 4..])
                .map(|(r, _)| r)
                .ok_or_else(|| Error::corrupt("parent rowid varint"))?
        };
        let mut cell = Vec::with_capacity(4 + 9);
        cell.extend_from_slice(&left_child.to_be_bytes());
        crate::format::write_varint(rowid as u64, &mut cell);
        cells.push(cell);
    }

    let cells_with_idx: Vec<(u16, Vec<u8>)> = cells
        .iter()
        .enumerate()
        .map(|(i, c)| (i as u16, c.clone()))
        .collect();
    let mut new_parent = pager.read_page_for_write(parent_pgno).await?;
    page::write_page_cells(
        &mut new_parent,
        pbase,
        PageType::InteriorTable,
        phdr.right_most_pointer,
        &cells_with_idx,
    )?;
    pager.write_page(parent_pgno, new_parent)?;
    Ok(())
}

/// Collapse a single-interior root page into its only child. The root page is
/// rewritten as a leaf-table page holding the child's cells, and the child is
/// added to the freelist.
async fn collapse_root_into_child(pager: &Pager, root_pgno: u32, child_pgno: u32) -> Result<()> {
    let usable = pager.usable_size();
    let base = pager.btree_header_offset(root_pgno);
    let cbase = pager.btree_header_offset(child_pgno);
    let child_buf = pager.get_page(child_pgno).await?;
    let cells = read_table_leaf_cells(&child_buf, cbase, usable)?;
    write_table_leaf(pager, root_pgno, base, &cells).await?;
    pager.free_page(child_pgno).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_keeps_each_half_under_target() {
        // 10 small cells on a 4096-byte page must split roughly in half.
        let usable = 4096;
        let cells: Vec<(i64, Vec<u8>)> = (1..=10).map(|i| (i as i64, vec![0u8; 100])).collect();
        let (left, right) = split_cells_for_test(usable, &cells);
        assert!(
            !left.is_empty() && !right.is_empty(),
            "split must give both sides cells"
        );
        let left_size: usize = left.iter().map(|c| c.len() + 2).sum();
        let right_size: usize = right.iter().map(|c| c.len() + 2).sum();
        // Both halves should be under `usable / 2 + one_cell` (the split aims for half).
        assert!(left_size <= usable / 2 + 200);
        assert!(right_size <= usable / 2 + 200);
    }

    #[test]
    fn redistribute_keeps_both_pages_within_budget() {
        let usable = 1024;
        let left: Vec<Vec<u8>> = (0..10).map(|_| vec![0u8; 40]).collect();
        let right: Vec<Vec<u8>> = (0..10).map(|_| vec![0u8; 40]).collect();
        let (l, r) = redistribute_table_cells(usable, left, right).unwrap();
        assert!(!l.is_empty() && !r.is_empty());
        assert!(table_cells_layout_size(&l) <= usable);
        assert!(table_cells_layout_size(&r) <= usable);
    }
}
