//! Read cursor over a table b-tree (mirrors the `BtCursor` read paths in `btree.c`).
//!
//! For M1 this provides a full in-order scan of a table b-tree, returning each row's rowid and
//! reassembled record payload (following overflow-page chains). A streaming `BtCursor` with
//! `seek`/`next`/`prev` and index support arrives with the query planner; a full scan is all
//! the read path needs today (it is what `sqlite_schema` reads and what a table-scan plan does).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::pager::{PageRef, Pager};

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

/// The `k`-th child page of a table-interior page, in in-order traversal order: the left-child
/// pointers of cells `0..num_cells` followed by the right-most pointer. Returns `None` once `k`
/// is past the last child.
fn interior_child(hdr: &PageHeader, page: &[u8], k: usize) -> Result<Option<u32>> {
    let n = hdr.num_cells as usize;
    if k < n {
        let off = hdr.cell_pointer(page, k)?;
        Ok(Some(parse_table_interior_cell(page, off)?.left_child))
    } else if k == n {
        Ok(hdr.right_most_pointer)
    } else {
        Ok(None)
    }
}

/// A streaming read cursor over a table b-tree — the VDBE-facing analogue of `BtCursor` that
/// `OpenRead`/`Rewind`/`Next` drive one row at a time (mirrors the table-scan paths in
/// `btree.c`).
///
/// Unlike [`scan_table`], which materializes every `(rowid, payload)` up front, this descends
/// the tree lazily and exposes the current row through [`rowid`](Self::rowid) /
/// [`payload`](Self::payload). It holds only cheap `Arc` page clones (no borrow of the pager
/// across opcodes), so the executor can keep it live across the async/sync boundary.
pub struct TableCursor {
    pager: Arc<Pager>,
    root: u32,
    usable: usize,
    /// Stack of interior pages currently being descended: `(page, header, index of the next
    /// child to visit)`. The leaf itself is not on the stack.
    stack: Vec<(PageRef, PageHeader, usize)>,
    /// The current leaf page and its parsed header (`None` before the first `rewind`).
    leaf: Option<PageRef>,
    leaf_hdr: Option<PageHeader>,
    /// 1-based page number of the current leaf (0 if not positioned on a leaf). Tracked so
    /// the delete-by-rowid path can address the leaf directly.
    leaf_pgno: u32,
    /// Index of the current cell within the current leaf.
    cell_idx: usize,
    /// `true` once the scan has run off the end (or before `rewind`).
    at_end: bool,
    /// When `true`, the next `next()` call should NOT advance — the cursor is "on" the cell
    /// that slid into the deleted slot, and we want a subsequent `next()` to move past it.
    /// Set by `delete_current`; cleared by the next `next()`.
    pending_advance: bool,
}

impl TableCursor {
    /// Create a cursor over the table b-tree rooted at `root`. Does no I/O until
    /// [`rewind`](Self::rewind).
    pub fn new(pager: Arc<Pager>, root: u32) -> TableCursor {
        let usable = pager.usable_size();
        TableCursor {
            pager,
            root,
            usable,
            stack: Vec::new(),
            leaf: None,
            leaf_hdr: None,
            leaf_pgno: 0,
            cell_idx: 0,
            at_end: true,
            pending_advance: false,
        }
    }

    /// Number of cells on the current leaf (0 if not positioned on a leaf).
    fn leaf_cells(&self) -> usize {
        self.leaf_hdr.map_or(0, |h| h.num_cells as usize)
    }

    /// Whether the cursor is positioned on a valid row.
    pub fn is_valid(&self) -> bool {
        !self.at_end && self.leaf.is_some() && self.cell_idx < self.leaf_cells()
    }

    /// Position the cursor on the first row (smallest rowid). If the table is empty the cursor
    /// becomes invalid (`is_valid()` returns false).
    pub async fn rewind(&mut self) -> Result<()> {
        self.stack.clear();
        self.leaf = None;
        self.leaf_hdr = None;
        self.leaf_pgno = 0;
        self.cell_idx = 0;
        self.at_end = false;
        self.pending_advance = false;
        self.descend_left(self.root).await?;
        // An empty leaf (e.g. an empty single-page table) means we must advance — which, with
        // an empty stack, simply marks the cursor at-end.
        if self.leaf_cells() == 0 {
            self.advance_to_next_leaf().await?;
        }
        Ok(())
    }

    /// Advance to the next row in ascending rowid order. After the last row the cursor becomes
    /// invalid.
    pub async fn next(&mut self) -> Result<()> {
        if self.at_end {
            return Ok(());
        }
        // If the previous op was a delete, the cell at the current `cell_idx` is the row
        // that just slid in; we want the next `next()` to land on the cell AFTER it.
        if self.pending_advance {
            self.pending_advance = false;
        } else {
            self.cell_idx += 1;
        }
        if self.cell_idx < self.leaf_cells() {
            return Ok(());
        }
        self.advance_to_next_leaf().await
    }

    /// The rowid of the current row.
    pub fn rowid(&self) -> Result<i64> {
        Ok(self.current_cell()?.rowid)
    }

    /// The full record payload of the current row (overflow chains followed).
    pub async fn payload(&self) -> Result<Vec<u8>> {
        let cell = self.current_cell()?;
        assemble_payload(&self.pager, &cell).await
    }

    /// The 1-based page number of the leaf the cursor is currently on (0 if not on a
    /// leaf). Used by the write path to address the leaf directly.
    pub fn leaf_pgno(&self) -> u32 {
        self.leaf_pgno
    }

    /// Delete the row the cursor is currently positioned on. Removes the cell at
    /// `cell_idx` from the leaf, frees any overflow chain, and refreshes the cursor's
    /// cached leaf so subsequent reads see the post-delete state. The cursor's `cell_idx`
    /// is left unchanged (the cell that just slid into the slot is now the current
    /// cell); the next `next()` call advances past it (via the `pending_advance` flag).
    pub async fn delete_current(&mut self) -> Result<()> {
        if !self.is_valid() {
            return Err(Error::msg("table cursor is not positioned on a row"));
        }
        let pgno = self.leaf_pgno;
        let cell_idx = self.cell_idx;
        super::delete::leaf_delete_current(&self.pager, pgno, cell_idx).await?;
        // The leaf's contents are now stale in our cache. Re-read and refresh.
        let page = self.pager.get_page(pgno).await?;
        let base = self.pager.btree_header_offset(pgno);
        let hdr = PageHeader::parse(&page, base)?;
        self.leaf = Some(page);
        self.leaf_hdr = Some(hdr);
        // Mark the cursor so the next `next()` will advance past the cell that slid in,
        // rather than skipping a row.
        self.pending_advance = true;
        Ok(())
    }

    /// Parse the current leaf cell.
    fn current_cell(&self) -> Result<TableLeafCell<'_>> {
        let leaf = self
            .leaf
            .as_ref()
            .filter(|_| self.is_valid())
            .ok_or_else(|| Error::msg("table cursor is not positioned on a row"))?;
        let hdr = self.leaf_hdr.expect("leaf_hdr present when leaf present");
        let off = hdr.cell_pointer(leaf, self.cell_idx)?;
        parse_table_leaf_cell(leaf, off, self.usable)
    }

    /// Descend from `pgno`, following the left-most child of every interior page, until a leaf
    /// is reached; positions on its first cell. Pushes a frame for each interior page passed.
    async fn descend_left(&mut self, pgno: u32) -> Result<()> {
        let mut pgno = pgno;
        loop {
            let page = self.pager.get_page(pgno).await?;
            let base = self.pager.btree_header_offset(pgno);
            let hdr = PageHeader::parse(&page, base)?;
            match hdr.page_type {
                PageType::LeafTable => {
                    self.leaf_pgno = pgno;
                    self.leaf = Some(page);
                    self.leaf_hdr = Some(hdr);
                    self.cell_idx = 0;
                    self.pending_advance = false;
                    return Ok(());
                }
                PageType::InteriorTable => {
                    let first = interior_child(&hdr, &page, 0)?
                        .ok_or_else(|| Error::corrupt("interior table page with no children"))?;
                    self.stack.push((page, hdr, 1));
                    pgno = first;
                }
                _ => return Err(Error::corrupt("expected a table b-tree page during scan")),
            }
        }
    }

    /// Pop back up the stack to the next unvisited child and descend into it. Sets `at_end`
    /// when the whole tree is exhausted.
    async fn advance_to_next_leaf(&mut self) -> Result<()> {
        loop {
            let Some((page, hdr, next_k)) = self.stack.last().map(|(p, h, k)| (p.clone(), *h, *k))
            else {
                self.at_end = true;
                self.leaf = None;
                self.leaf_hdr = None;
                return Ok(());
            };
            match interior_child(&hdr, &page, next_k)? {
                Some(child) => {
                    self.stack.last_mut().expect("stack non-empty").2 += 1;
                    self.descend_left(child).await?;
                    if self.leaf_cells() > 0 {
                        return Ok(());
                    }
                    // Empty leaf (not expected in a valid b-tree): keep advancing.
                }
                None => {
                    self.stack.pop();
                }
            }
        }
    }
}
