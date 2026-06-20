//! B-tree destruction (the analogue of `OP_Destroy` / `sqlite3BtreeDropTable`).
//!
//! Walk a b-tree rooted at `root` and free every page in it, adding each to the
//! pager's freelist. Interior pages are descended recursively; leaves are freed once
//! their cells' overflow chains have been reaped. The freed pages can be re-used by
//! future `allocate_page` calls (or reclaimed by `VACUUM`).
//!
//! This is the write-path half of `DROP TABLE` / `DROP INDEX`: the `sqlite_schema`
//! row is removed separately by the codegen path. Handles table b-trees, index
//! b-trees, and `WITHOUT ROWID` (index-organized) tables uniformly — the on-disk
//! shape of any interior page is identical (a 4-byte left-child pointer per cell
//! plus a right-most pointer in the header), so the same DFS works for all three.

use crate::error::{Error, Result};
use crate::pager::Pager;

use super::cell::{parse_index_interior_cell, parse_index_leaf_cell, parse_table_interior_cell};
use super::page::{PageHeader, PageType};

/// Recursively free every page in the b-tree rooted at `root`. The page is added to
/// the pager's freelist (so a subsequent `allocate_page` will reuse it). Returns the
/// number of pages freed. Works for table, index, and `WITHOUT ROWID` b-trees.
pub async fn destroy(pager: &Pager, root: u32) -> Result<u32> {
    if root == 0 {
        return Ok(0);
    }
    let mut freed = 0;
    // Iterative DFS: use a stack of pages still to visit. We pop the top, decide whether
    // it's a leaf or an interior page, and either free it (leaf) or push its children
    // (interior). A small fixed stack keeps the work bounded for any reasonable b-tree.
    let mut stack: Vec<u32> = vec![root];
    while let Some(pgno) = stack.pop() {
        let base = pager.btree_header_offset(pgno);
        let page = pager.get_page(pgno).await?;
        let hdr = PageHeader::parse(&page, base)?;
        match hdr.page_type {
            PageType::LeafTable | PageType::LeafIndex => {
                // Reap each leaf cell's overflow chain before freeing the page itself.
                // Both table-leaf and index-leaf cells can carry overflow when the
                // payload exceeds the local-only window.
                let usable = pager.usable_size();
                for i in 0..hdr.num_cells as usize {
                    let off = hdr.cell_pointer(&page, i)?;
                    let overflow = match hdr.page_type {
                        PageType::LeafTable => {
                            super::cell::parse_table_leaf_cell(&page, off, usable)
                                .ok()
                                .and_then(|c| c.overflow_page)
                        }
                        PageType::LeafIndex => parse_index_leaf_cell(&page, off, usable)
                            .ok()
                            .and_then(|c| c.overflow_page),
                        _ => None,
                    };
                    if let Some(first) = overflow {
                        free_overflow_chain(pager, first).await?;
                    }
                }
                pager.free_page(pgno).await?;
                freed += 1;
            }
            PageType::InteriorTable | PageType::InteriorIndex => {
                // Both interior-table and interior-index cells start with a 4-byte
                // left-child pointer; the right-most child sits in the page header at
                // hdr+8. Push every child before freeing the interior page itself.
                let n = hdr.num_cells as usize;
                let usable = pager.usable_size();
                for i in 0..n {
                    let off = hdr.cell_pointer(&page, i)?;
                    let left_child = match hdr.page_type {
                        PageType::InteriorTable => {
                            parse_table_interior_cell(&page, off)?.left_child
                        }
                        PageType::InteriorIndex => {
                            parse_index_interior_cell(&page, off, usable)?.left_child
                        }
                        _ => unreachable!(),
                    };
                    stack.push(left_child);
                }
                if let Some(rm) = hdr.right_most_pointer {
                    stack.push(rm);
                }
                pager.free_page(pgno).await?;
                freed += 1;
            }
        }
    }
    Ok(freed)
}

/// Delete every row from the table b-tree rooted at `root`, leaving it as a single
/// empty leaf page at `root`. All data pages (including overflow chains) are added to
/// the pager's freelist, and the root page itself is reset as an empty leaf-table page.
/// This is the analogue of `OP_Clear` for ordinary rowid tables.
pub async fn clear(pager: &Pager, root: u32) -> Result<u32> {
    if root == 0 {
        return Ok(0);
    }
    let mut freed = 0u32;
    // Gather every page owned by the tree except the root itself.
    let mut stack: Vec<u32> = Vec::new();
    {
        let base = pager.btree_header_offset(root);
        let page = pager.get_page(root).await?;
        let hdr = PageHeader::parse(&page, base)?;
        match hdr.page_type {
            PageType::LeafTable => {
                // Root is already a leaf: just clear its cells below.
            }
            PageType::InteriorTable => {
                let n = hdr.num_cells as usize;
                for i in 0..n {
                    let off = hdr.cell_pointer(&page, i)?;
                    let cell = parse_table_interior_cell(&page, off)?;
                    stack.push(cell.left_child);
                }
                if let Some(rm) = hdr.right_most_pointer {
                    stack.push(rm);
                }
            }
            _ => {
                return Err(Error::corrupt(format!(
                    "clear: unexpected page type on root page {root}"
                )))
            }
        }
    }

    // Free all non-root pages (and their overflow chains for leaf pages).
    while let Some(pgno) = stack.pop() {
        let base = pager.btree_header_offset(pgno);
        let page = pager.get_page(pgno).await?;
        let hdr = PageHeader::parse(&page, base)?;
        match hdr.page_type {
            PageType::LeafTable => {
                let usable = pager.usable_size();
                for i in 0..hdr.num_cells as usize {
                    let off = hdr.cell_pointer(&page, i)?;
                    if let Ok(cell) = super::cell::parse_table_leaf_cell(&page, off, usable) {
                        if let Some(first) = cell.overflow_page {
                            free_overflow_chain(pager, first).await?;
                        }
                    }
                }
                pager.free_page(pgno).await?;
                freed += 1;
            }
            PageType::InteriorTable => {
                let n = hdr.num_cells as usize;
                for i in 0..n {
                    let off = hdr.cell_pointer(&page, i)?;
                    let cell = parse_table_interior_cell(&page, off)?;
                    stack.push(cell.left_child);
                }
                if let Some(rm) = hdr.right_most_pointer {
                    stack.push(rm);
                }
                pager.free_page(pgno).await?;
                freed += 1;
            }
            _ => {
                return Err(Error::corrupt(format!(
                    "clear: unexpected page type on page {pgno}"
                )))
            }
        }
    }

    // Reset the root page to an empty leaf-table page.
    let base = pager.btree_header_offset(root);
    let mut root_buf = pager.read_page_for_write(root).await?;
    super::page::init_empty_leaf(&mut root_buf, base);
    pager.write_page(root, root_buf)?;
    Ok(freed)
}

/// Walk an overflow chain (set of pages with `[u32 next][chunk]…` layout) and free each.
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
