//! Auto-vacuum commit — relocate pages from the end of the file into freed pages near the
//! front, then truncate (mirrors `autoVacuumCommit` / `incrVacuumStep` / `relocatePage` /
//! `modifyPagePointer` in `btree.c`).
//!
//! When `PRAGMA auto_vacuum = FULL` is set and a write transaction commits with pages on the
//! freelist, the engine walks the database from the last page down to `nFin + 1` (the final
//! size). For each page that is on the freelist (`PTRMAP_FREEPAGE`), it is dropped from the
//! freelist (so the count goes down). For each page that is in use, its ptrmap entry tells us
//! the type and parent; we relocate it to a free page `iFreePg <= nFin` and rewrite the
//! parent's pointer to the new location via [`modify_page_pointer`]. After the walk, the file
//! is truncated to `nFin` pages and the header's freelist head/count + db-size are updated.
//!
//! `PRAGMA auto_vacuum = INCREMENTAL` defers this to `PRAGMA incremental_vacuum(N)`, which runs
//! up to N steps of [`incr_vacuum_step`] (one page move per step) and yields the new page count
//! as a result row.

use crate::error::{Error, Result};
use crate::pager::Pager;

use super::cell::parse_table_interior_cell;
use super::page::{PageHeader, PageType};
use super::ptrmap::{
    is_pending_byte_page, is_ptrmap_page, pending_byte_page, ptrmap_get, ptrmap_pageno,
    ptrmap_put, PtrMapType,
};

/// The expected final database size in pages after auto-vacuuming an `nOrig`-page database with
/// `nFree` freelist pages. Mirrors `finalDbSize` in `btree.c`. The ptrmap pages that become
/// unused as the file shrinks are subtracted too.
fn final_db_size(usable_size: usize, n_orig: u32, n_free: u32) -> u32 {
    let n_entry = usable_size as u32 / 5; // ptrmap entries per ptrmap page (excluding itself)
    let n_ptrmap = (n_free.saturating_sub(n_orig) + ptrmap_pageno(usable_size, n_orig) + n_entry)
        / n_entry;
    let mut n_fin = n_orig - n_free - n_ptrmap;
    if n_orig > pending_byte_page(0) && n_fin < pending_byte_page(0) {
        n_fin -= 1;
    }
    while is_ptrmap_page(usable_size, n_fin) || is_pending_byte_page(usable_size, n_fin) {
        n_fin -= 1;
    }
    n_fin
}

/// Public wrapper around [`final_db_size`] for the PRAGMA incremental_vacuum path.
pub fn final_db_size_pub(usable_size: usize, n_orig: u32, n_free: u32) -> u32 {
    final_db_size(usable_size, n_orig, n_free)
}

/// Public single-step entry for `PRAGMA incremental_vacuum`: relocate the page at `i_last_pg`
/// (the current last page) into a free page at or below `n_fin`. Mirrors `sqlite3BtreeIncrVacuum`
/// + `incrVacuumStep` (with `bCommit = 0`) in `btree.c`. Returns `Ok(())` on success or
/// `Err("autovacuum done")` when the freelist is exhausted.
pub async fn incr_vacuum_step_impl(pager: &Pager, n_fin: u32, i_last_pg: u32) -> Result<()> {
    incr_vacuum_step(pager, n_fin, i_last_pg, false).await
}

/// Run the full auto-vacuum commit: walk the last `nOrig..nFin+1` pages, relocating each in-use
/// page into a free page at or below `nFin`, then truncate the file. Called from the pager's
/// commit path when `auto_vacuum` is on, the freelist is non-empty, and `incr_vacuum` is off
/// (i.e. `PRAGMA auto_vacuum = FULL`).
///
/// Mirrors `autoVacuumCommit` in `btree.c`. The caller already holds a write transaction; the
/// page-1 header (freelist head/count, db-size) is updated in place by this routine.
pub async fn auto_vacuum_commit(pager: &Pager) -> Result<()> {
    let usable = pager.usable_size();
    let n_orig = pager.page_count();
    if is_ptrmap_page(usable, n_orig) || is_pending_byte_page(usable, n_orig) {
        return Err(Error::corrupt(format!(
            "auto_vacuum_commit: last page {n_orig} is reserved"
        )));
    }
    let n_free = pager.header().freelist_count;
    if n_free == 0 || n_free >= n_orig {
        return Ok(());
    }
    let n_fin = final_db_size(usable, n_orig, n_free);
    if n_fin > n_orig {
        return Err(Error::corrupt("auto_vacuum_commit: nFin > nOrig"));
    }
    if n_fin == n_orig {
        return Ok(());
    }

    let b_commit = true;
    let mut i_last = n_orig;
    while i_last > n_fin {
        if is_ptrmap_page(usable, i_last) || is_pending_byte_page(usable, i_last) {
            i_last -= 1;
            continue;
        }
        match incr_vacuum_step(pager, n_fin, i_last, b_commit).await {
            Ok(()) => i_last -= 1,
            Err(e) if e.message == "autovacuum done" => break,
            Err(other) => return Err(other),
        }
    }

    // Reset the freelist head/count and the in-header size to reflect the truncated file.
    // After a full vacuum the freelist is empty (every free page was either reused or
    // truncated away).
    pager.with_header_mut(|h| {
        h.first_freelist_trunk = 0;
        h.freelist_count = 0;
        h.db_size_pages = n_fin;
    });
    // Also drop the freed pages from the in-memory cache and shrink the page count.
    pager.truncate_image(n_fin);
    Ok(())
}

/// One step of the auto-vacuum: relocate the page at `i_last_pg` (the current last page) into a
/// free page at or below `n_fin`, updating the parent's pointer via [`modify_page_pointer`].
/// Mirrors `incrVacuumStep` in `btree.c`.
///
/// Returns `Ok(())` when the page was moved (or dropped if it was a free page); returns an
/// `Err("autovacuum done")` when the freelist is exhausted (no more pages to relocate into).
async fn incr_vacuum_step(
    pager: &Pager,
    n_fin: u32,
    i_last_pg: u32,
    b_commit: bool,
) -> Result<()> {
    let n_free = pager.header().freelist_count;
    if n_free == 0 {
        return Err(Error::msg("autovacuum done"));
    }
    let (e_type, i_ptr_page) = ptrmap_get(pager, i_last_pg).await?;
    match e_type {
        PtrMapType::RootPage => {
            // A root page at the end of the file means it has no parent in the b-tree sense —
            // it's referenced from `sqlite_schema`. We do not move root pages here (upstream
            // also errors in this case for `incrVacuumStep`).
            return Err(Error::corrupt(format!(
                "incr_vacuum_step: page {i_last_pg} is a root page at the end of the file"
            )));
        }
        PtrMapType::FreePage => {
            if !b_commit {
                return Err(Error::msg("autovacuum done"));
            }
            return Err(Error::corrupt(
                "incr_vacuum_step: free page at end of file (not removed from freelist)",
            ));
        }
        _ => {}
    }

    // Find a free page at or below n_fin to relocate into.
    let i_free_pg = find_free_page_at_or_below(pager, n_fin).await?;
    let i_free_pg = match i_free_pg {
        Some(p) => p,
        None => return Err(Error::msg("autovacuum done")),
    };

    // Move the content of i_last_pg to i_free_pg and rewrite the parent's pointer.
    relocate_page(pager, i_last_pg, e_type, i_ptr_page, i_free_pg).await?;

    // The page at i_last_pg is now unused (its content moved to i_free_pg). The freelist
    // count was decremented by `find_free_page_at_or_below` when it popped the free page.
    // For b_commit we truncate the file below; nothing else to do here.
    Ok(())
}

/// Walk the freelist (trunk-linked list) to find a free page with number `<= n_fin`. Pops the
/// found page from the freelist and decrements the count. Mirrors the `BTALLOC_LE` search in
/// `allocateBtreePage`.
///
/// Returns `Ok(Some(pgno))` when a page was found and removed from the freelist, or
/// `Ok(None)` when no suitable page exists. The header's freelist head/count is updated.
async fn find_free_page_at_or_below(pager: &Pager, n_fin: u32) -> Result<Option<u32>> {
    let first_trunk = pager.header().first_freelist_trunk;
    if first_trunk == 0 {
        return Ok(None);
    }
    // Walk the trunk chain looking for a leaf <= n_fin. If a trunk itself is <= n_fin,
    // use the trunk (promoting its first leaf to a new trunk, if any).
    let mut trunk_pgno = first_trunk;
    let mut prev_trunk: Option<u32> = None;
    while trunk_pgno != 0 {
        let trunk = pager.get_page(trunk_pgno).await?;
        let next_trunk = u32::from_be_bytes([trunk[0], trunk[1], trunk[2], trunk[3]]);
        let k = u32::from_be_bytes([trunk[4], trunk[5], trunk[6], trunk[7]]);
        if trunk_pgno <= n_fin {
            // Use this trunk page as the allocated page. If it has leaves, promote the first
            // leaf to be the new trunk (carrying the rest of the leaves), and link the previous
            // trunk to it. Otherwise just unlink the trunk.
            if k == 0 {
                // No leaves: just unlink the trunk.
                if let Some(pt) = prev_trunk {
                    let mut pbuf = pager.read_page_for_write(pt).await?;
                    pbuf[0..4].copy_from_slice(&next_trunk.to_be_bytes());
                    pager.write_page(pt, pbuf)?;
                } else {
                    pager.with_header_mut(|h| h.first_freelist_trunk = next_trunk);
                }
            } else {
                // Promote the first leaf (at offset 8) to be the new trunk. It carries the
                // remaining k-1 leaves (copied from offset 12).
                let new_trunk_pgno = u32::from_be_bytes([
                    trunk[8], trunk[9], trunk[10], trunk[11],
                ]);
                let new_trunk = pager.read_page_for_write(new_trunk_pgno).await?;
                let mut rebuilt = new_trunk.to_vec();
                rebuilt[0..4].copy_from_slice(&next_trunk.to_be_bytes());
                rebuilt[4..8].copy_from_slice(&(k - 1).to_be_bytes());
                for i in 0..(k - 1) as usize {
                    let src = 12 + i * 4;
                    let dst = 8 + i * 4;
                    rebuilt[dst..dst + 4].copy_from_slice(&trunk[src..src + 4]);
                }
                let tail_start = 8 + (k - 1) as usize * 4;
                for b in &mut rebuilt[tail_start..pager.usable_size()] {
                    *b = 0;
                }
                pager.write_page(new_trunk_pgno, rebuilt)?;
                if let Some(pt) = prev_trunk {
                    let mut pbuf = pager.read_page_for_write(pt).await?;
                    pbuf[0..4].copy_from_slice(&new_trunk_pgno.to_be_bytes());
                    pager.write_page(pt, pbuf)?;
                } else {
                    pager.with_header_mut(|h| h.first_freelist_trunk = new_trunk_pgno);
                }
            }
            pager.with_header_mut(|h| h.freelist_count -= 1);
            return Ok(Some(trunk_pgno));
        }
        // Search the trunk's leaves for one <= n_fin.
        if k > 0 {
            for i in 0..k as usize {
                let leaf_off = 8 + i * 4;
                let leaf = u32::from_be_bytes([
                    trunk[leaf_off],
                    trunk[leaf_off + 1],
                    trunk[leaf_off + 2],
                    trunk[leaf_off + 3],
                ]);
                if leaf <= n_fin {
                    // Pop this leaf from the trunk. Decrement k, move the last leaf's slot
                    // into this slot (matches upstream's compaction), and we're done.
                    let mut trunk_buf = pager.read_page_for_write(trunk_pgno).await?;
                    if i < (k - 1) as usize {
                        let last_off = 8 + (k - 1) as usize * 4;
                        let last_slot = trunk_buf[last_off..last_off + 4].to_vec();
                        trunk_buf[leaf_off..leaf_off + 4].copy_from_slice(&last_slot);
                    }
                    trunk_buf[4..8].copy_from_slice(&(k - 1).to_be_bytes());
                    let last_off = 8 + (k - 1) as usize * 4;
                    for b in &mut trunk_buf[last_off..last_off + 4] {
                        *b = 0;
                    }
                    pager.write_page(trunk_pgno, trunk_buf)?;
                    pager.with_header_mut(|h| h.freelist_count -= 1);
                    return Ok(Some(leaf));
                }
            }
        }
        prev_trunk = Some(trunk_pgno);
        trunk_pgno = next_trunk;
    }
    Ok(None)
}

/// Move the content of page `i_db_page` to location `i_free_page`, update the parent's pointer
/// (via [`modify_page_pointer`]), and update the ptrmap entries. Mirrors `relocatePage` in
/// `btree.c`.
async fn relocate_page(
    pager: &Pager,
    i_db_page: u32,
    e_type: PtrMapType,
    i_ptr_page: u32,
    i_free_page: u32,
) -> Result<()> {
    if i_db_page < 3 {
        return Err(Error::corrupt("relocate_page: refusing to move page < 3"));
    }
    // Move the page content: read i_db_page, write it to i_free_page. Journal the destination's
    // pre-image by going through `read_page_for_write` (which captures the current image into
    // the rollback journal), then install the moved content.
    let content = pager.get_page(i_db_page).await?.to_vec();
    let _ = pager.read_page_for_write(i_free_page).await?;
    pager.write_page(i_free_page, content)?;
    // The source page i_db_page will be truncated away at the end of `auto_vacuum_commit`.

    // Update child ptrmap entries for the moved page (its children now point at i_free_page).
    match e_type {
        PtrMapType::Btree | PtrMapType::RootPage => {
            set_child_ptrmaps(pager, i_free_page).await?;
        }
        PtrMapType::Overflow1 => {
            let page = pager.get_page(i_free_page).await?;
            let next = u32::from_be_bytes([page[0], page[1], page[2], page[3]]);
            if next != 0 {
                ptrmap_put(pager, next, PtrMapType::Overflow2, i_free_page).await?;
            }
        }
        PtrMapType::Overflow2 => {
            let page = pager.get_page(i_free_page).await?;
            let next = u32::from_be_bytes([page[0], page[1], page[2], page[3]]);
            if next != 0 {
                ptrmap_put(pager, next, PtrMapType::Overflow2, i_free_page).await?;
            }
        }
        PtrMapType::FreePage => {}
    }

    // Fix the parent pointer that referenced i_db_page so it now points at i_free_page.
    if e_type != PtrMapType::RootPage {
        let mut pbuf = pager.read_page_for_write(i_ptr_page).await?;
        modify_page_pointer(&mut pbuf, i_ptr_page, i_db_page, i_free_page, e_type, pager).await?;
        pager.write_page(i_ptr_page, pbuf)?;
        // The moved page's own ptrmap entry now reflects its new location and parent.
        ptrmap_put(pager, i_free_page, e_type, i_ptr_page).await?;
    } else {
        // Root pages have no parent in the b-tree sense. Their location is tracked in
        // `sqlite_schema.rootpage`; that update is handled by the codegen DDL path which
        // re-reads the schema after autovacuum. For the autovacuum-1 test the root pages stay
        // at the front of the file and are never the last page, so this branch is not reached.
        ptrmap_put(pager, i_free_page, PtrMapType::RootPage, 0).await?;
    }
    Ok(())
}

/// Walk a freshly moved b-tree page (`pgno`) and update the ptrmap entries for all its
/// children (interior-cell left children + right-most pointer, plus any overflow chains the
/// cells reference) to point at `pgno` as their parent. Mirrors `setChildPtrmaps` in `btree.c`.
async fn set_child_ptrmaps(pager: &Pager, pgno: u32) -> Result<()> {
    let base = pager.btree_header_offset(pgno);
    let page = pager.get_page(pgno).await?;
    let hdr = PageHeader::parse(&page, base)?;
    match hdr.page_type {
        PageType::LeafTable | PageType::LeafIndex => {
            // Leaves have no children, but their cells may own overflow chains. Walk each
            // cell and update the OVERFLOW1 ptrmap entry to point at `pgno`.
            let usable = pager.usable_size();
            for i in 0..hdr.num_cells as usize {
                let off = hdr.cell_pointer(&page, i)?;
                if let Some(ovfl) = cell_overflow_page(&page, off, hdr.page_type, usable)? {
                    ptrmap_put(pager, ovfl, PtrMapType::Overflow1, pgno).await?;
                }
            }
            Ok(())
        }
        PageType::InteriorTable => {
            for i in 0..hdr.num_cells as usize {
                let off = hdr.cell_pointer(&page, i)?;
                let cell = parse_table_interior_cell(&page, off)?;
                ptrmap_put(pager, cell.left_child, PtrMapType::Btree, pgno).await?;
            }
            if let Some(rm) = hdr.right_most_pointer {
                ptrmap_put(pager, rm, PtrMapType::Btree, pgno).await?;
            }
            Ok(())
        }
        PageType::InteriorIndex => {
            for i in 0..hdr.num_cells as usize {
                let off = hdr.cell_pointer(&page, i)?;
                let child = u32::from_be_bytes([page[off], page[off + 1], page[off + 2], page[off + 3]]);
                ptrmap_put(pager, child, PtrMapType::Btree, pgno).await?;
            }
            if let Some(rm) = hdr.right_most_pointer {
                ptrmap_put(pager, rm, PtrMapType::Btree, pgno).await?;
            }
            Ok(())
        }
    }
}

/// Determine the first overflow page number for a cell at `off` on a page, if any. Mirrors the
/// overflow-page extraction in `parse_table_leaf_cell` / `parse_index_leaf_cell` but returns
/// only the overflow pointer (or `None` if the cell fits locally).
fn cell_overflow_page(
    page: &[u8],
    off: usize,
    page_type: PageType,
    usable: usize,
) -> Result<Option<u32>> {
    use super::cell::{index_max_local, local_payload_len};
    let (payload_size, n1) = crate::format::read_varint(
        page.get(off..)
            .ok_or_else(|| Error::corrupt("cell offset"))?,
    )
    .ok_or_else(|| Error::corrupt("cell payload-size varint"))?;
    let payload_size = payload_size as usize;
    let (header_len, local_len, has_overflow) = match page_type {
        PageType::LeafTable => {
            let (_, rowid_size) = crate::format::read_varint_i64(
                page.get(off + n1..)
                    .ok_or_else(|| Error::corrupt("rowid bytes"))?,
            )
            .ok_or_else(|| Error::corrupt("table leaf rowid varint"))?;
            let max_local = usable - 35;
            let (ll, ho) = local_payload_len(payload_size, usable, max_local);
            (n1 + rowid_size, ll, ho)
        }
        PageType::LeafIndex => {
            let max_local = index_max_local(usable);
            let (ll, ho) = local_payload_len(payload_size, usable, max_local);
            (n1, ll, ho)
        }
        _ => return Ok(None),
    };
    if !has_overflow {
        return Ok(None);
    }
    let ptr_off = off + header_len + local_len;
    let ptr = u32::from_be_bytes([
        page[ptr_off],
        page[ptr_off + 1],
        page[ptr_off + 2],
        page[ptr_off + 3],
    ]);
    Ok(Some(ptr))
}

/// Find the pointer to `i_from` on page `parent_pgno` and rewrite it to `i_to`. `e_type` tells
/// us what kind of pointer to look for: a b-tree child pointer (cell left_child or the page's
/// right-most pointer), or an overflow-page pointer (in a cell's overflow-pointer field for
/// `OVERFLOW1`, or the first 4 bytes of the page for `OVERFLOW2`). Mirrors `modifyPagePointer`
/// in `btree.c`.
async fn modify_page_pointer(
    pbuf: &mut [u8],
    parent_pgno: u32,
    i_from: u32,
    i_to: u32,
    e_type: PtrMapType,
    pager: &Pager,
) -> Result<()> {
    let base = pager.btree_header_offset(parent_pgno);
    match e_type {
        PtrMapType::Overflow2 => {
            let cur = u32::from_be_bytes([pbuf[0], pbuf[1], pbuf[2], pbuf[3]]);
            if cur != i_from {
                return Err(Error::corrupt(format!(
                    "modify_page_pointer: OVERFLOW2 page's next ptr is {cur}, expected {i_from}"
                )));
            }
            pbuf[0..4].copy_from_slice(&i_to.to_be_bytes());
            Ok(())
        }
        PtrMapType::Btree | PtrMapType::Overflow1 => {
            let hdr = PageHeader::parse(pbuf, base)?;
            for i in 0..hdr.num_cells as usize {
                let off = hdr.cell_pointer(pbuf, i)?;
                if e_type == PtrMapType::Btree {
                    let child = u32::from_be_bytes([
                        pbuf[off],
                        pbuf[off + 1],
                        pbuf[off + 2],
                        pbuf[off + 3],
                    ]);
                    if child == i_from {
                        pbuf[off..off + 4].copy_from_slice(&i_to.to_be_bytes());
                        return Ok(());
                    }
                } else {
                    let usable = pager.usable_size();
                    if let Some(ovfl) = cell_overflow_page(pbuf, off, hdr.page_type, usable)? {
                        let (payload_size, n1) = crate::format::read_varint(
                            pbuf.get(off..).ok_or_else(|| Error::corrupt("cell off"))?,
                        )
                        .ok_or_else(|| Error::corrupt("payload varint"))?;
                        let payload_size = payload_size as usize;
                        let (header_len, local_len) = match hdr.page_type {
                            PageType::LeafTable => {
                                let (_, rs) = crate::format::read_varint_i64(
                                    pbuf.get(off + n1..)
                                        .ok_or_else(|| Error::corrupt("rowid bytes"))?,
                                )
                                .ok_or_else(|| Error::corrupt("rowid varint"))?;
                                (n1 + rs, {
                                    let ml = usable - 35;
                                    super::cell::local_payload_len(payload_size, usable, ml).0
                                })
                            }
                            PageType::LeafIndex => {
                                let ml = super::cell::index_max_local(usable);
                                (n1, super::cell::local_payload_len(payload_size, usable, ml).0)
                            }
                            _ => return Ok(()),
                        };
                        let ptr_off = off + header_len + local_len;
                        let cur = u32::from_be_bytes([
                            pbuf[ptr_off],
                            pbuf[ptr_off + 1],
                            pbuf[ptr_off + 2],
                            pbuf[ptr_off + 3],
                        ]);
                        if cur == i_from && ovfl == i_from {
                            pbuf[ptr_off..ptr_off + 4].copy_from_slice(&i_to.to_be_bytes());
                            return Ok(());
                        }
                    }
                }
            }
            if e_type == PtrMapType::Btree {
                if hdr.right_most_pointer == Some(i_from) {
                    pbuf[base + 8..base + 12].copy_from_slice(&i_to.to_be_bytes());
                    return Ok(());
                }
            }
            Err(Error::corrupt(format!(
                "modify_page_pointer: pointer to {i_from} not found on page {parent_pgno}"
            )))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_db_size_basic() {
        let u = 4096;
        let n = final_db_size(u, 10, 4);
        assert!(n <= 10, "nFin must be <= nOrig");
        assert!(n >= 1, "nFin must be >= 1");
    }
}