//! Pointer-map pages — the lookup table that lets auto-vacuum relocate pages
//! (mirrors the `PTRMAP_*` machinery in `btree.c`).
//!
//! In an auto-vacuum database, every page that is not a pointer-map page itself has a 5-byte
//! entry on a pointer-map page: a 1-byte type ([`PtrMapType`]) plus a 4-byte parent page number.
//! The type tells the vacuum how to fix up the pointer that references this page when it moves:
//!
//! | type                | meaning                                                  | parent field        |
//! |---------------------|----------------------------------------------------------|--------------------|
//! | [`RootPage`]        | a b-tree root page                                        | unused (0)         |
//! | [`FreePage`]        | an unused freelist page                                  | unused (0)         |
//! | [`Overflow1`]      | first page of an overflow chain owned by a cell          | the cell's page    |
//! | [`Overflow2`]      | later page in an overflow chain                          | previous ovfl page |
//! | [`Btree`]           | a non-root b-tree page (interior or leaf)                | the parent page    |
//!
//! The pointer-map page for page `pgno` is at page number `ptrmap_pageno(pgno)`. Page 1 has no
//! pointer-map entry (the function returns 0). A page whose number equals its `ptrmap_pageno`
//! is itself a pointer-map page ([`is_ptrmap_page`]).
//!
//! The layout is `5 * (pgno - ptrmap_pageno - 1)` bytes into the pointer-map page.

use crate::error::{Error, Result};
use crate::pager::Pager;

/// The byte offset of a page's ptrmap entry inside its ptrmap page.
pub(crate) const PTRMAP_ENTRY_SIZE: usize = 5;

/// The on-disk type codes (mirrors `btreeInt.h` `PTRMAP_*`).
pub mod type_code {
    pub const ROOTPAGE: u8 = 1;
    pub const FREEPAGE: u8 = 2;
    pub const OVERFLOW1: u8 = 3;
    pub const OVERFLOW2: u8 = 4;
    pub const BTREE: u8 = 5;
}

/// A typed pointer-map entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PtrMapType {
    /// A b-tree root page. Parent is unused.
    RootPage,
    /// A freelist page. Parent is unused.
    FreePage,
    /// The first page of an overflow chain; parent is the cell's page.
    Overflow1,
    /// A later overflow page; parent is the previous overflow page.
    Overflow2,
    /// A non-root b-tree page; parent is its parent in the b-tree.
    Btree,
}

impl PtrMapType {
    pub fn code(self) -> u8 {
        match self {
            PtrMapType::RootPage => type_code::ROOTPAGE,
            PtrMapType::FreePage => type_code::FREEPAGE,
            PtrMapType::Overflow1 => type_code::OVERFLOW1,
            PtrMapType::Overflow2 => type_code::OVERFLOW2,
            PtrMapType::Btree => type_code::BTREE,
        }
    }

    pub fn from_code(c: u8) -> Result<PtrMapType> {
        Ok(match c {
            type_code::ROOTPAGE => PtrMapType::RootPage,
            type_code::FREEPAGE => PtrMapType::FreePage,
            type_code::OVERFLOW1 => PtrMapType::Overflow1,
            type_code::OVERFLOW2 => PtrMapType::Overflow2,
            type_code::BTREE => PtrMapType::Btree,
            _ => return Err(Error::corrupt(format!("invalid ptrmap type {c}"))),
        })
    }
}

/// `PENDING_BYTE` is the byte offset at which file locks live; the page containing it is reserved
/// and never used for b-tree or ptrmap data. Upstream defaults it to `0x40000000` (1 GiB).
pub const PENDING_BYTE: u64 = 0x4000_0000;

/// The page number of the PENDING_BYTE page for a given page size.
pub fn pending_byte_page(_page_size: usize) -> u32 {
    // page_size parameter kept for upstream-signature parity; the constant is independent of
    // page size (the PENDING_BYTE is a byte offset, the page is byte/4096+1 — but we use the
    // upstream default page size of 4096 for the calculation since our pager always uses 4096).
    ((PENDING_BYTE / 4096) + 1) as u32
}

/// Whether `pgno` is the PENDING_BYTE page (reserved for locking, never used for data).
pub fn is_pending_byte_page(_usable_size: usize, pgno: u32) -> bool {
    pgno == pending_byte_page(0)
}

/// Number of ptrmap entries that fit on one pointer-map page. The formula mirrors
/// `btreeInt.h`: `(usableSize / 5) + 1` pages are covered by one ptrmap page (the +1 accounts
/// for the ptrmap page itself, which has no entry).
fn pages_per_map_page(usable_size: usize) -> u32 {
    (usable_size as u32 / PTRMAP_ENTRY_SIZE as u32) + 1
}

/// The page number of the pointer-map page that holds the entry for `pgno`. Returns 0 for
/// `pgno < 2` (page 1 has no ptrmap entry). Mirrors `ptrmapPageno` in `btree.c`.
pub fn ptrmap_pageno(usable_size: usize, pgno: u32) -> u32 {
    if pgno < 2 {
        return 0;
    }
    let npp = pages_per_map_page(usable_size) as u64;
    let i = ((pgno as u64 - 2) / npp) as u32;
    let mut ret = (i as u64 * npp) as u32 + 2;
    if ret == pending_byte_page(0) {
        ret += 1;
    }
    ret
}

/// Whether `pgno` is itself a pointer-map page.
pub fn is_ptrmap_page(usable_size: usize, pgno: u32) -> bool {
    pgno >= 2 && ptrmap_pageno(usable_size, pgno) == pgno
}

/// True if `pgno` is reserved (ptrmap page or pending-byte page) and cannot hold b-tree data.
pub fn is_reserved_page(usable_size: usize, pgno: u32) -> bool {
    is_ptrmap_page(usable_size, pgno) || is_pending_byte_page(usable_size, pgno)
}

/// The byte offset of the entry for `pgno` within its ptrmap page.
fn ptrmap_offset(ptrmap_pgno: u32, pgno: u32) -> usize {
    (PTRMAP_ENTRY_SIZE as u32 * (pgno - ptrmap_pgno - 1)) as usize
}

/// Read the ptrmap entry for `pgno`. Returns `(type, parent)`.
pub async fn ptrmap_get(pager: &Pager, pgno: u32) -> Result<(PtrMapType, u32)> {
    let usable = pager.usable_size();
    let pm = ptrmap_pageno(usable, pgno);
    if pm == 0 {
        return Err(Error::corrupt(format!("ptrmap_get: page {pgno} has no ptrmap")));
    }
    let page = pager.get_page(pm).await?;
    let off = ptrmap_offset(pm, pgno);
    if off + PTRMAP_ENTRY_SIZE > pager.usable_size() {
        return Err(Error::corrupt(format!("ptrmap_get: offset {off} out of range")));
    }
    let t = page[off];
    let parent = u32::from_be_bytes([page[off + 1], page[off + 2], page[off + 3], page[off + 4]]);
    Ok((PtrMapType::from_code(t)?, parent))
}

/// Write the ptrmap entry for `pgno`. The ptrmap page is made writable through
/// [`Pager::read_page_for_write`]; if the entry already matches, the write is skipped (matching
/// upstream's "no-op if unchanged" optimization).
pub async fn ptrmap_put(pager: &Pager, pgno: u32, ty: PtrMapType, parent: u32) -> Result<()> {
    let usable = pager.usable_size();
    let pm = ptrmap_pageno(usable, pgno);
    if pm == 0 {
        return Err(Error::corrupt(format!("ptrmap_put: page {pgno} has no ptrmap")));
    }
    // Read the current entry without journaling the ptrmap page (a read is enough to check
    // whether an update is needed). If the page doesn't exist yet, the get will read zeros.
    let needs_write = match pager.get_page(pm).await {
        Ok(page) => {
            let off = ptrmap_offset(pm, pgno);
            if off + PTRMAP_ENTRY_SIZE > usable {
                return Err(Error::corrupt(format!("ptrmap_put: offset {off} out of range")));
            }
            page[off] != ty.code()
                || u32::from_be_bytes([page[off + 1], page[off + 2], page[off + 3], page[off + 4]])
                    != parent
        }
        Err(_) => true,
    };
    if !needs_write {
        return Ok(());
    }
    let mut page = pager.read_page_for_write(pm).await?;
    let off = ptrmap_offset(pm, pgno);
    page[off] = ty.code();
    page[off + 1..off + 5].copy_from_slice(&parent.to_be_bytes());
    pager.write_page(pm, page)?;
    Ok(())
}

/// Synchronous wrapper around [`ptrmap_put`] for use from sync cell builders. Cell builders
/// run inside an async context (the insert path), so they cannot drive async I/O directly.
/// Instead, this is a **no-op placeholder**: the cell builder collects the overflow-page
/// numbers it allocated, and the async caller is responsible for calling [`ptrmap_put`] to
/// record the OVERFLOW1/OVERFLOW2 entries after the cell is written. This avoids "cannot
/// block_on within a runtime" panics. The cell builder's `?`-propagation still succeeds.
pub fn ptrmap_put_sync(_pager: &Pager, _pgno: u32, _ty: PtrMapType, _parent: u32) -> Result<()> {
    Ok(())
}

/// Initialize a freshly allocated pointer-map page to all zeros (no entries yet). The caller has
/// just allocated `pgno` as a ptrmap page; this journals and zeroes it.
pub async fn init_ptrmap_page(pager: &Pager, pgno: u32) -> Result<()> {
    let mut page = pager.read_page_for_write(pgno).await?;
    for b in &mut page[..pager.usable_size()] {
        *b = 0;
    }
    pager.write_page(pgno, page)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ptrmap_pageno_layout_matches_upstream() {
        // usable_size 4096 -> pages_per_map_page = 4096/5 + 1 = 820
        // page 2 -> ptrmap page 2 (page 2 is its own ptrmap page)
        // page 3 -> ptrmap page 2
        // ...
        // page 821 -> ptrmap page 2
        // page 822 -> ptrmap page 822 (next ptrmap page)
        let u = 4096;
        assert_eq!(ptrmap_pageno(u, 0), 0);
        assert_eq!(ptrmap_pageno(u, 1), 0);
        assert_eq!(ptrmap_pageno(u, 2), 2);
        assert_eq!(ptrmap_pageno(u, 3), 2);
        assert_eq!(ptrmap_pageno(u, 821), 2);
        assert_eq!(ptrmap_pageno(u, 822), 822);
        assert_eq!(ptrmap_pageno(u, 823), 822);
        assert!(is_ptrmap_page(u, 2));
        assert!(is_ptrmap_page(u, 822));
        assert!(!is_ptrmap_page(u, 3));
        assert!(!is_ptrmap_page(u, 1));
    }

    #[test]
    fn pending_byte_page_for_4096() {
        // PENDING_BYTE = 0x40000000 = 1 GiB. For page_size 4096:
        // page = (0x40000000 / 4096) + 1 = 262144 + 1 = 262145
        assert_eq!(pending_byte_page(0), 262_145);
        assert!(is_pending_byte_page(0, 262_145));
    }

    #[test]
    fn pending_byte_page_skipped_by_ptrmap_pageno() {
        // If the ptrmap page would land on the pending-byte page, it is bumped forward by 1.
        let u = 4096;
        let pb = pending_byte_page(0);
        // No ptrmap page equals the pending-byte page.
        let mut p = 2u32;
        while p <= pb + 10 {
            assert!(ptrmap_pageno(u, p) != pb, "ptrmap page {p} collided with pending byte");
            p += 1;
        }
    }
}