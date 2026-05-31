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
