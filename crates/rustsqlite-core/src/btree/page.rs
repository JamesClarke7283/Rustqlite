//! B-tree page header and cell-pointer array
//! (<https://www.sqlite.org/fileformat2.html#b_tree_pages>).
//!
//! Every b-tree page begins with an 8- or 12-byte header. On page 1 the header starts at byte
//! offset 100 (after the database header); the `base_offset` parameter accounts for that. Cell
//! pointers and the cell content area are measured from the start of the page (offset 0),
//! regardless of `base_offset`.

use crate::error::{Error, Result};

use super::{be_u16, be_u32};

/// The four b-tree page types, identified by the first header byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageType {
    InteriorIndex,
    InteriorTable,
    LeafIndex,
    LeafTable,
}

impl PageType {
    fn from_byte(b: u8) -> Result<PageType> {
        Ok(match b {
            0x02 => PageType::InteriorIndex,
            0x05 => PageType::InteriorTable,
            0x0a => PageType::LeafIndex,
            0x0d => PageType::LeafTable,
            other => {
                return Err(Error::corrupt(format!(
                    "invalid b-tree page type {other:#04x}"
                )))
            }
        })
    }

    pub fn is_leaf(self) -> bool {
        matches!(self, PageType::LeafIndex | PageType::LeafTable)
    }

    pub fn is_table(self) -> bool {
        matches!(self, PageType::InteriorTable | PageType::LeafTable)
    }
}

/// A parsed b-tree page header.
#[derive(Clone, Copy, Debug)]
pub struct PageHeader {
    pub page_type: PageType,
    pub first_freeblock: u16,
    pub num_cells: u16,
    /// Start of the cell content area; a stored value of 0 means 65536.
    pub cell_content_start: u32,
    pub fragmented_free_bytes: u8,
    /// Right-most child pointer (interior pages only).
    pub right_most_pointer: Option<u32>,
    /// Header length: 8 for leaf pages, 12 for interior pages.
    pub header_size: usize,
    /// Offset of the b-tree header within the page (100 on page 1, else 0).
    pub base_offset: usize,
}

impl PageHeader {
    /// Parse the b-tree header located at `base_offset` within `page`.
    pub fn parse(page: &[u8], base_offset: usize) -> Result<PageHeader> {
        if page.len() < base_offset + 8 {
            return Err(Error::corrupt("page too small for a b-tree header"));
        }
        let h = base_offset;
        let page_type = PageType::from_byte(page[h])?;
        let raw_ccs = be_u16(&page[h + 5..h + 7]);
        let (right_most_pointer, header_size) = if page_type.is_leaf() {
            (None, 8)
        } else {
            if page.len() < h + 12 {
                return Err(Error::corrupt(
                    "interior page too small for a 12-byte header",
                ));
            }
            (Some(be_u32(&page[h + 8..h + 12])), 12)
        };

        Ok(PageHeader {
            page_type,
            first_freeblock: be_u16(&page[h + 1..h + 3]),
            num_cells: be_u16(&page[h + 3..h + 5]),
            cell_content_start: if raw_ccs == 0 { 65_536 } else { raw_ccs as u32 },
            fragmented_free_bytes: page[h + 7],
            right_most_pointer,
            header_size,
            base_offset,
        })
    }

    /// Absolute offset within the page of cell `i` (0-based), from the cell pointer array.
    pub fn cell_pointer(&self, page: &[u8], i: usize) -> Result<usize> {
        let ptr_off = self.base_offset + self.header_size + i * 2;
        if page.len() < ptr_off + 2 {
            return Err(Error::corrupt("cell pointer array out of range"));
        }
        Ok(be_u16(&page[ptr_off..ptr_off + 2]) as usize)
    }
}

// ---- Page mutation (write path, M4) ----
//
// These operate directly on an owned page buffer (`&mut [u8]`, exactly `page_size` long) — the
// copy the pager hands out via `read_page_for_write`. They mirror the no-freeblock subset of
// `zeroPage`/`allocateSpace`/`insertCell` in `btree.c`; freeblock reuse and page balancing (when a
// page fills) arrive in later phases. M5.1 adds the index-page equivalents for the
// `CREATE INDEX` / `DROP INDEX` / `IdxInsert` write path.

/// Initialize the b-tree page region of `page` as an **empty leaf-index** page (page type
/// `0x0a`), with the cell content area at the end of the page. Used by the M5.1 index layer
/// for the new root of a fresh index b-tree. Mirrors [`init_empty_leaf`]; same on-page layout,
/// just a different page-type byte.
pub fn init_empty_index_leaf(page: &mut [u8], base_offset: usize) {
    let page_size = page.len();
    for b in &mut page[base_offset..] {
        *b = 0;
    }
    let h = base_offset;
    page[h] = 0x0a; // leaf index page
    page[h + 1..h + 3].copy_from_slice(&0u16.to_be_bytes());
    page[h + 3..h + 5].copy_from_slice(&0u16.to_be_bytes());
    let ccs: u16 = if page_size == 65_536 {
        0
    } else {
        page_size as u16
    };
    page[h + 5..h + 7].copy_from_slice(&ccs.to_be_bytes());
    page[h + 7] = 0;
}

/// Initialize the b-tree page region of `page` as an **empty interior-index** page (page type
/// `0x02`), with `right_most` as the right-most child pointer. Mirrors [`init_empty_interior`].
pub fn init_empty_interior_index(page: &mut [u8], base_offset: usize, right_most: u32) {
    let page_size = page.len();
    for b in &mut page[base_offset..] {
        *b = 0;
    }
    let h = base_offset;
    page[h] = 0x02; // interior index page
    page[h + 1..h + 3].copy_from_slice(&0u16.to_be_bytes());
    page[h + 3..h + 5].copy_from_slice(&0u16.to_be_bytes());
    let ccs: u16 = if page_size == 65_536 {
        0
    } else {
        page_size as u16
    };
    page[h + 5..h + 7].copy_from_slice(&ccs.to_be_bytes());
    page[h + 7] = 0;
    page[h + 8..h + 12].copy_from_slice(&right_most.to_be_bytes());
}

/// Initialize the b-tree page region of `page` as an **empty leaf-table** page. `base_offset` is
/// 100 on page 1 (after the database header) and 0 otherwise. The page data area from
/// `base_offset` to the end is zeroed (`zeroPage`), then the 8-byte leaf header is written with
/// zero cells and the cell content area at the end of the page.
pub fn init_empty_leaf(page: &mut [u8], base_offset: usize) {
    let page_size = page.len();
    for b in &mut page[base_offset..] {
        *b = 0;
    }
    let h = base_offset;
    page[h] = 0x0d; // leaf table page
    page[h + 1..h + 3].copy_from_slice(&0u16.to_be_bytes()); // first freeblock = 0
    page[h + 3..h + 5].copy_from_slice(&0u16.to_be_bytes()); // num cells = 0
    // The cell content area starts at the end of the page; 65536 is stored as 0.
    let ccs: u16 = if page_size == 65_536 {
        0
    } else {
        page_size as u16
    };
    page[h + 5..h + 7].copy_from_slice(&ccs.to_be_bytes());
    page[h + 7] = 0; // fragmented free bytes
}

/// Initialize the b-tree page region of `page` as an **empty interior-table** page (page type
/// `0x05`), with `right_most` as the right-most child pointer. Used when the b-tree root is
/// promoted from a leaf (a leaf becomes a full root after the first split). `base_offset` is 100
/// on page 1, else 0. The page bytes from `base_offset` onward are zeroed (`zeroPage`).
pub fn init_empty_interior(page: &mut [u8], base_offset: usize, right_most: u32) {
    let page_size = page.len();
    for b in &mut page[base_offset..] {
        *b = 0;
    }
    let h = base_offset;
    page[h] = 0x05; // interior table page
    page[h + 1..h + 3].copy_from_slice(&0u16.to_be_bytes()); // first freeblock = 0
    page[h + 3..h + 5].copy_from_slice(&0u16.to_be_bytes()); // num cells = 0
    let ccs: u16 = if page_size == 65_536 {
        0
    } else {
        page_size as u16
    };
    page[h + 5..h + 7].copy_from_slice(&ccs.to_be_bytes());
    page[h + 7] = 0; // fragmented free bytes
    page[h + 8..h + 12].copy_from_slice(&right_most.to_be_bytes());
}

/// The number of contiguous free bytes between the end of the cell-pointer array and the start of
/// the cell content area on a **leaf** page — the space available for a new cell plus its 2-byte
/// pointer. (This is the simple unallocated gap; reclaimable freeblocks inside the content area are
/// not counted, since the first-slice insert path never creates them.)
pub fn leaf_free_space(page: &[u8], base_offset: usize) -> usize {
    let h = base_offset;
    let num_cells = be_u16(&page[h + 3..h + 5]) as usize;
    let raw_ccs = be_u16(&page[h + 5..h + 7]) as usize;
    let cell_content_start = if raw_ccs == 0 { 65_536 } else { raw_ccs };
    let ptr_array_end = h + LEAF_HEADER_SIZE + num_cells * 2;
    cell_content_start.saturating_sub(ptr_array_end)
}

/// The 8-byte header length of a leaf b-tree page (interior pages use 12).
const LEAF_HEADER_SIZE: usize = 8;

/// Insert `cell` into a **leaf-table or leaf-index** page at cell-pointer index `idx`
/// (the 0-based, key-sorted position). Allocates the cell's bytes from the content area
/// (growing it downward), writes the new 2-byte pointer (shifting the pointers at `idx..` up by
/// two), and bumps the cell count. Returns `Err` ([`page_full_error`]) when the cell plus its
/// pointer do not fit — the caller will split the page once balancing lands (M4.5; the M5.1
/// index path defers the split case to a follow-up slice and propagates the error verbatim).
/// `base_offset` is 100 on page 1, else 0.
///
/// Faithful to `insertCell`/`allocateSpace` in `btree.c` for the case with no reusable freeblocks.
pub fn insert_leaf_cell(
    page: &mut [u8],
    base_offset: usize,
    idx: usize,
    cell: &[u8],
) -> Result<()> {
    let h = base_offset;
    let page_type = page[h];
    if page_type != 0x0d && page_type != 0x0a {
        return Err(Error::corrupt("insert_leaf_cell: not a leaf-table or leaf-index page"));
    }
    let num_cells = be_u16(&page[h + 3..h + 5]) as usize;
    if idx > num_cells {
        return Err(Error::corrupt("insert_leaf_cell: index past cell count"));
    }
    let raw_ccs = be_u16(&page[h + 5..h + 7]) as usize;
    let cell_content_start = if raw_ccs == 0 { 65_536 } else { raw_ccs };
    let ptr_array_end = h + LEAF_HEADER_SIZE + num_cells * 2;

    // Need room for the cell bytes (in the content area) plus a 2-byte pointer (in the array).
    if cell_content_start < ptr_array_end + cell.len() + 2 {
        return Err(page_full_error());
    }

    // Allocate the cell from the top of the content area downward and copy it in.
    let new_content_start = cell_content_start - cell.len();
    page[new_content_start..new_content_start + cell.len()].copy_from_slice(cell);

    // Make room in the pointer array: shift entries [idx, num_cells) up by one slot (2 bytes).
    let ptr_at = |i: usize| h + LEAF_HEADER_SIZE + i * 2;
    if idx < num_cells {
        page.copy_within(ptr_at(idx)..ptr_at(num_cells), ptr_at(idx + 1));
    }
    page[ptr_at(idx)..ptr_at(idx) + 2].copy_from_slice(&(new_content_start as u16).to_be_bytes());

    // Update the header: one more cell, content area moved down.
    page[h + 3..h + 5].copy_from_slice(&((num_cells + 1) as u16).to_be_bytes());
    let stored_ccs: u16 = if new_content_start == 65_536 {
        0
    } else {
        new_content_start as u16
    };
    page[h + 5..h + 7].copy_from_slice(&stored_ccs.to_be_bytes());
    Ok(())
}

/// The error returned when a cell does not fit on its leaf page. The b-tree split that handles this
/// The message is distinct so the caller can recognize the "needs split" condition and invoke
/// balancing (split + promote).
pub fn page_full_error() -> Error {
    Error::msg("btree page is full")
}

/// The number of contiguous free bytes between the end of the cell-pointer array and the start of
/// the cell content area on an **interior** page — the same accounting as
/// [`leaf_free_space`], but interior pages carry a 4-byte larger header and a 4-byte larger cell
/// (a child pointer, vs no pointer in a table-leaf cell).
pub fn interior_free_space(page: &[u8], base_offset: usize) -> usize {
    let h = base_offset;
    let num_cells = be_u16(&page[h + 3..h + 5]) as usize;
    let raw_ccs = be_u16(&page[h + 5..h + 7]) as usize;
    let cell_content_start = if raw_ccs == 0 { 65_536 } else { raw_ccs };
    let ptr_array_end = h + INTERIOR_HEADER_SIZE + num_cells * 2;
    cell_content_start.saturating_sub(ptr_array_end)
}

/// The 12-byte header length of an interior b-tree page.
const INTERIOR_HEADER_SIZE: usize = 12;

/// Insert a **table-interior or index-interior** cell at cell-pointer index `idx` on an
/// interior page. The page's cell-pointer array grows downward the same way the leaf's does.
/// Returns [`page_full_error`] if the cell does not fit (the caller will route into the split
/// path). The `0x05` (interior-table) and `0x02` (interior-index) page types both use this
/// helper — they share the same 12-byte header + 2-byte-cell-pointer layout.
pub fn insert_interior_cell(
    page: &mut [u8],
    base_offset: usize,
    idx: usize,
    cell: &[u8],
) -> Result<()> {
    let h = base_offset;
    let page_type = page[h];
    if page_type != 0x05 && page_type != 0x02 {
        return Err(Error::corrupt("insert_interior_cell: not an interior-table or interior-index page"));
    }
    let num_cells = be_u16(&page[h + 3..h + 5]) as usize;
    if idx > num_cells {
        return Err(Error::corrupt("insert_interior_cell: index past cell count"));
    }
    let raw_ccs = be_u16(&page[h + 5..h + 7]) as usize;
    let cell_content_start = if raw_ccs == 0 { 65_536 } else { raw_ccs };
    let ptr_array_end = h + INTERIOR_HEADER_SIZE + num_cells * 2;

    if cell_content_start < ptr_array_end + cell.len() + 2 {
        return Err(page_full_error());
    }

    let new_content_start = cell_content_start - cell.len();
    page[new_content_start..new_content_start + cell.len()].copy_from_slice(cell);

    let ptr_at = |i: usize| h + INTERIOR_HEADER_SIZE + i * 2;
    if idx < num_cells {
        page.copy_within(ptr_at(idx)..ptr_at(num_cells), ptr_at(idx + 1));
    }
    page[ptr_at(idx)..ptr_at(idx) + 2].copy_from_slice(&(new_content_start as u16).to_be_bytes());

    page[h + 3..h + 5].copy_from_slice(&((num_cells + 1) as u16).to_be_bytes());
    let stored_ccs: u16 = if new_content_start == 65_536 {
        0
    } else {
        new_content_start as u16
    };
    page[h + 5..h + 7].copy_from_slice(&stored_ccs.to_be_bytes());
    Ok(())
}

/// Reset a page's cell-pointer array to a fresh, empty list of `cells` and a free space
/// starting at `cell_content_start` (a byte offset within the page). All four b-tree page
/// types (leaf-table, interior-table, leaf-index, interior-index) share this layout: an
/// 8-/12-byte header, then a `2 * num_cells` pointer array, then the cell content area.
/// Used by the split path to build a fresh sibling page out of a redistributed set of cells.
pub fn write_page_cells(
    page: &mut [u8],
    base_offset: usize,
    page_type: PageType,
    right_most: Option<u32>,
    cells: &[(u16, Vec<u8>)],
) -> Result<()> {
    let page_size = page.len();
    for b in &mut page[base_offset..] {
        *b = 0;
    }
    let h = base_offset;
    match page_type {
        PageType::LeafTable => page[h] = 0x0d,
        PageType::InteriorTable => page[h] = 0x05,
        PageType::LeafIndex => page[h] = 0x0a,
        PageType::InteriorIndex => page[h] = 0x02,
    }
    page[h + 1..h + 3].copy_from_slice(&0u16.to_be_bytes());
    page[h + 3..h + 5].copy_from_slice(&(cells.len() as u16).to_be_bytes());

    let header_size = if page_type.is_leaf() { 8 } else { 12 };
    let ptr_array_end = h + header_size + cells.len() * 2;
    // Lay out cells from the end of the page downward; for each cell index `i`, record the
    // physical offset of its cell bytes. `cells` is already in rowid-sorted order, and the
    // pointer array is written at index `i`, so the pointer at index `i` ends up pointing
    // at the i-th cell.
    let mut offsets: Vec<usize> = vec![0; cells.len()];
    let mut cell_content_start = page_size;
    for (i, cell) in cells.iter().enumerate().rev() {
        cell_content_start -= cell.1.len();
        offsets[i] = cell_content_start;
    }
    // Sanity: content area must start at or after the pointer array.
    if cell_content_start < ptr_array_end {
        return Err(page_full_error());
    }
    for (i, (_, cell)) in cells.iter().enumerate() {
        let off = offsets[i];
        page[off..off + cell.len()].copy_from_slice(cell);
        let ptr_at = h + header_size + i * 2;
        page[ptr_at..ptr_at + 2].copy_from_slice(&(off as u16).to_be_bytes());
    }
    let stored_ccs: u16 = if cell_content_start == 65_536 {
        0
    } else {
        cell_content_start as u16
    };
    page[h + 5..h + 7].copy_from_slice(&stored_ccs.to_be_bytes());
    page[h + 7] = 0; // fragmented free bytes
    if let Some(rm) = right_most {
        page[h + 8..h + 12].copy_from_slice(&rm.to_be_bytes());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_leaf_table_header() {
        // A leaf-table page (type 0x0d) with 2 cells, content area starting at 0x0f00,
        // cell pointers at offsets 0x0ff0 and 0x0fe0.
        let mut page = vec![0u8; 4096];
        page[0] = 0x0d;
        page[1..3].copy_from_slice(&0u16.to_be_bytes()); // first freeblock
        page[3..5].copy_from_slice(&2u16.to_be_bytes()); // num cells
        page[5..7].copy_from_slice(&0x0f00u16.to_be_bytes());
        page[7] = 0;
        page[8..10].copy_from_slice(&0x0ff0u16.to_be_bytes());
        page[10..12].copy_from_slice(&0x0fe0u16.to_be_bytes());

        let hdr = PageHeader::parse(&page, 0).unwrap();
        assert_eq!(hdr.page_type, PageType::LeafTable);
        assert_eq!(hdr.num_cells, 2);
        assert_eq!(hdr.cell_content_start, 0x0f00);
        assert_eq!(hdr.header_size, 8);
        assert!(hdr.right_most_pointer.is_none());
        assert_eq!(hdr.cell_pointer(&page, 0).unwrap(), 0x0ff0);
        assert_eq!(hdr.cell_pointer(&page, 1).unwrap(), 0x0fe0);
    }

    #[test]
    fn parse_interior_table_header_has_right_pointer() {
        let mut page = vec![0u8; 4096];
        page[0] = 0x05; // interior table
        page[3..5].copy_from_slice(&1u16.to_be_bytes());
        page[8..12].copy_from_slice(&42u32.to_be_bytes()); // right-most pointer
        let hdr = PageHeader::parse(&page, 0).unwrap();
        assert_eq!(hdr.page_type, PageType::InteriorTable);
        assert_eq!(hdr.header_size, 12);
        assert_eq!(hdr.right_most_pointer, Some(42));
    }

    #[test]
    fn content_start_zero_means_65536() {
        let mut page = vec![0u8; 65536];
        page[0] = 0x0d;
        page[5..7].copy_from_slice(&0u16.to_be_bytes());
        let hdr = PageHeader::parse(&page, 0).unwrap();
        assert_eq!(hdr.cell_content_start, 65_536);
    }

    #[test]
    fn rejects_bad_page_type() {
        let page = vec![0u8; 4096];
        assert!(PageHeader::parse(&page, 0).is_err());
    }
}
