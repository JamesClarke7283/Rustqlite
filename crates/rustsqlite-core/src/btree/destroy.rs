//! B-tree destruction (the analogue of `OP_Destroy` / `sqlite3BtreeDropTable`).
//!
//! Walk a table b-tree rooted at `root` and free every page in it, adding each to the
//! pager's freelist. Interior pages are descended recursively; leaves are freed once
//! their cells have been cleared. The freed pages can be re-used by future
//! `allocate_page` calls (or reclaimed by `VACUUM`).
//!
//! This is the write-path half of `DROP TABLE`: the `sqlite_schema` row is removed
//! separately by the codegen path. Index b-trees, `WITHOUT ROWID` tables, and the
//! `auto-vacuum` ptrmap are out of scope for the first slice.

use crate::error::{Error, Result};
use crate::pager::Pager;

use super::cell::parse_table_interior_cell;
use super::page::{PageHeader, PageType};

/// Recursively free every page in the table b-tree rooted at `root`. The page is
/// added to the pager's freelist (so a subsequent `allocate_page` will reuse it).
/// Returns the number of pages freed.
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
                // Reap the cells' overflow chains before freeing the page. (For
                // table-leaf cells, an overflow chain is possible; index leaves will be
                // exercised when index destroy lands.)
                if hdr.page_type == PageType::LeafTable {
                    let usable = pager.usable_size();
                    for i in 0..hdr.num_cells as usize {
                        let off = hdr.cell_pointer(&page, i)?;
                        if let Ok(cell) =
                            super::cell::parse_table_leaf_cell(&page, off, usable)
                        {
                            if let Some(first) = cell.overflow_page {
                                free_overflow_chain(pager, first).await?;
                            }
                        }
                    }
                }
                pager.free_page(pgno).await?;
                freed += 1;
            }
            PageType::InteriorTable => {
                // Push children first; we cannot free the interior page until the
                // children are queued.
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
                    "destroy: unexpected page type on page {pgno}"
                )));
            }
        }
    }
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
