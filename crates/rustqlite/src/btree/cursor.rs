//! Read cursor over a table b-tree (mirrors the `BtCursor` read paths in `btree.c`).
//!
//! For M1 this provides a full in-order scan of a table b-tree, returning each row's rowid and
//! reassembled record payload (following overflow-page chains). A streaming `BtCursor` with
//! `seek`/`next`/`prev` and index support arrives with the query planner; a full scan is all
//! the read path needs today (it is what `sqlite_schema` reads and what a table-scan plan does).

use std::future::Future;
use std::pin::Pin;

use crate::error::{Error, Result};
use crate::pager::Pager;

use super::be_u32;
use super::cell::{parse_table_interior_cell, parse_table_leaf_cell, TableLeafCell};
use super::page::{PageHeader, PageType};

/// Scan an entire table b-tree rooted at `root`, returning `(rowid, payload)` for every row in
/// ascending rowid order. `payload` is the full record (overflow chains are followed).
pub async fn scan_table(pager: &Pager, root: u32) -> Result<Vec<(i64, Vec<u8>)>> {
    let mut out = Vec::new();
    visit(pager, root, &mut out).await?;
    Ok(out)
}

/// In-order DFS of a table b-tree. Async recursion is boxed (a recursive `async fn` cannot
/// name its own future type).
fn visit<'a>(
    pager: &'a Pager,
    pgno: u32,
    out: &'a mut Vec<(i64, Vec<u8>)>,
) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
    Box::pin(async move {
        let page = pager.get_page(pgno).await?;
        let base = pager.btree_header_offset(pgno);
        let hdr = PageHeader::parse(&page, base)?;

        match hdr.page_type {
            PageType::LeafTable => {
                for i in 0..hdr.num_cells as usize {
                    let cell_off = hdr.cell_pointer(&page, i)?;
                    let cell = parse_table_leaf_cell(&page, cell_off, pager.usable_size())?;
                    let payload = assemble_payload(pager, &cell).await?;
                    out.push((cell.rowid, payload));
                }
            }
            PageType::InteriorTable => {
                for i in 0..hdr.num_cells as usize {
                    let cell_off = hdr.cell_pointer(&page, i)?;
                    let cell = parse_table_interior_cell(&page, cell_off)?;
                    visit(pager, cell.left_child, out).await?;
                }
                if let Some(right) = hdr.right_most_pointer {
                    visit(pager, right, out).await?;
                }
            }
            _ => return Err(Error::corrupt("expected a table b-tree page during scan")),
        }
        Ok(())
    })
}

/// Reassemble a cell's full payload, following the overflow-page chain if present. Overflow
/// pages are `[u32 next-page][content...]`, with `usable - 4` content bytes each.
async fn assemble_payload(pager: &Pager, cell: &TableLeafCell<'_>) -> Result<Vec<u8>> {
    let total = cell.payload_size as usize;
    let mut payload = Vec::with_capacity(total);
    payload.extend_from_slice(cell.local_payload);

    let usable = pager.usable_size();
    let mut next = cell.overflow_page;
    while payload.len() < total {
        let Some(pgno) = next.filter(|&p| p != 0) else {
            break;
        };
        let page = pager.get_page(pgno).await?;
        let next_pgno = be_u32(&page[0..4]);
        let want = (total - payload.len()).min(usable - 4);
        if 4 + want > page.len() {
            return Err(Error::corrupt("overflow page shorter than expected"));
        }
        payload.extend_from_slice(&page[4..4 + want]);
        next = if next_pgno == 0 {
            None
        } else {
            Some(next_pgno)
        };
    }

    if payload.len() < total {
        return Err(Error::corrupt("payload shorter than declared size"));
    }
    payload.truncate(total);
    Ok(payload)
}
