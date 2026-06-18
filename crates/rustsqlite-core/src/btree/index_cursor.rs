//! Read/seek cursor over an index b-tree (mirrors the index-cursor paths in `btree.c`).
//!
//! An `IndexCursor` is the streaming analogue of `scan_index`: it holds the current leaf and
//! advances cell-by-cell with `next()`, and supports a `seek` that descends the b-tree to
//! the first entry in a given direction relative to an unpacked key. The `rowid()` accessor
//! returns the trailing rowid of the current key record; `payload()` returns the full key
//! record (with overflow reassembled).
//!
//! Faithful to the table-cursor shape ([`super::TableCursor`]) — same page stack, same
//! "pop-and-try-next-child" advance-to-next-leaf logic — but operates on `LeafIndex` /
//! `InteriorIndex` page types and uses the `mem_compare` ordering for the seek comparison.
//!
//! The first M5.1 slice supports `seek(Ge)` (and `seek(Gt)`/`seek(Le)`/`seek(Lt)` via the
//! `IdxGE`/`IdxGT`/`IdxLT`/`IdxLE` post-seek pair that establishes the inclusive/exclusive
//! boundary). The single-leaf `insert` / `delete_current` paths live in
//! [`super::index_insert`] and [`super::index_delete`].

use std::cmp::Ordering;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::format::{decode_record, TextEncoding};
use crate::pager::{PageRef, Pager};
use crate::types::{Collation, Value};
use crate::vdbe::compare::mem_compare;
use crate::vdbe::KeyField;

use super::cell::{parse_index_interior_cell, parse_index_leaf_cell};
use super::page::{PageHeader, PageType};

/// The seek-side compare class for a `seek` call. `Ge` / `Gt` mean "find the first entry
/// `>=` / `>` the search key"; `Le` / `Lt` mean "find the last entry `<=` / `<`". M5.1 uses
/// `Ge` for the index-aware `WHERE`/`ORDER BY` path; the others are exposed for the
/// `IdxGE`/`IdxGT`/`IdxLT`/`IdxLE` post-seek pair that establishes the inclusive/exclusive
/// boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeekOp {
    Ge,
    Gt,
    Le,
    Lt,
}

/// A streaming read cursor over an index b-tree. Analogous to [`super::TableCursor`].
pub struct IndexCursor {
    pager: Arc<Pager>,
    root: u32,
    usable: usize,
    /// Per-key comparison rules. For a single-column index this is one field; for a
    /// multi-column index it is one field per indexed column. The final rowid tiebreaker
    /// always uses `BINARY`.
    key_info: Vec<KeyField>,
    /// Stack of interior pages currently being descended: `(page, header, index of the next
    /// child to visit)`. The leaf itself is not on the stack.
    stack: Vec<(PageRef, PageHeader, usize)>,
    /// The current leaf page and its parsed header (`None` before the first `rewind`).
    leaf: Option<PageRef>,
    leaf_hdr: Option<PageHeader>,
    /// 1-based page number of the current leaf (0 if not positioned on a leaf).
    leaf_pgno: u32,
    /// Index of the current cell within the current leaf.
    cell_idx: usize,
    /// `true` once the scan has run off the end (or before `rewind`).
    at_end: bool,
    /// When `true`, the next `next()` call should NOT advance — the cursor is "on" the cell
    /// that slid into the deleted slot. Set by `delete_current`; cleared by the next `next()`.
    pending_advance: bool,
    /// Cached payload of the current key record (overflow reassembled), set on every
    /// `position`/`seek`/`next`. Used by `rowid()`/`payload()` so the column accessors don't
    /// re-read the cell after every opcode.
    current_payload: Option<Vec<u8>>,
}

impl IndexCursor {
    /// Create a cursor over the index b-tree rooted at `root`. Does no I/O until
    /// [`rewind`](Self::rewind) or [`seek`](Self::seek).
    ///
    /// `key_info` carries the per-column collation and DESC flag; comparisons inside the
    /// cursor use this instead of hard-coded `BINARY`.
    pub fn new(pager: Arc<Pager>, root: u32, key_info: Vec<KeyField>) -> IndexCursor {
        let usable = pager.usable_size();
        IndexCursor {
            pager,
            root,
            usable,
            key_info,
            stack: Vec::new(),
            leaf: None,
            leaf_hdr: None,
            leaf_pgno: 0,
            cell_idx: 0,
            at_end: true,
            pending_advance: false,
            current_payload: None,
        }
    }

    fn leaf_cells(&self) -> usize {
        self.leaf_hdr.map_or(0, |h| h.num_cells as usize)
    }

    /// Whether the cursor is positioned on a valid entry.
    pub fn is_valid(&self) -> bool {
        !self.at_end && self.leaf.is_some() && self.cell_idx < self.leaf_cells()
    }

    /// Position the cursor on the first entry (smallest key). If the b-tree is empty the
    /// cursor becomes invalid.
    pub async fn rewind(&mut self) -> Result<()> {
        self.stack.clear();
        self.leaf = None;
        self.leaf_hdr = None;
        self.leaf_pgno = 0;
        self.cell_idx = 0;
        self.at_end = false;
        self.pending_advance = false;
        self.current_payload = None;
        self.descend_left(self.root).await?;
        if self.leaf_cells() == 0 {
            self.advance_to_next_leaf().await?;
        }
        self.refresh_payload().await?;
        Ok(())
    }

    /// Advance to the next entry in ascending key order. After the last entry the cursor
    /// becomes invalid.
    pub async fn next(&mut self) -> Result<()> {
        if self.at_end {
            return Ok(());
        }
        if self.pending_advance {
            self.pending_advance = false;
        } else {
            self.cell_idx += 1;
        }
        if self.cell_idx < self.leaf_cells() {
            self.refresh_payload().await?;
            return Ok(());
        }
        self.advance_to_next_leaf().await?;
        self.refresh_payload().await?;
        Ok(())
    }

    /// The rowid of the current entry (the last value in the key record).
    pub fn rowid(&self) -> Result<i64> {
        let payload = self
            .current_payload
            .as_ref()
            .ok_or_else(|| Error::msg("index cursor is not positioned on an entry"))?;
        let values = decode_record(payload, self.pager.text_encoding())?;
        values
            .last()
            .map(|v| v.as_i64())
            .ok_or_else(|| Error::corrupt("index key record has no rowid field"))
    }

    /// The full key record of the current entry (overflow chains followed).
    pub fn payload(&self) -> &[u8] {
        self.current_payload.as_deref().unwrap_or(&[])
    }

    /// The 1-based page number of the leaf the cursor is currently on. Used by the write path
    /// to address the leaf directly.
    pub fn leaf_pgno(&self) -> u32 {
        self.leaf_pgno
    }

    /// The per-column key-info this cursor was opened with. The executor passes a copy of this
    /// to the insert/delete helpers so they compare with the same collation sequence.
    pub fn key_info(&self) -> &[KeyField] {
        &self.key_info
    }

    /// Seek the cursor to the first entry whose key is in the direction implied by `op`
    /// relative to `key`. Returns `true` when such an entry exists.
    pub async fn seek(&mut self, op: SeekOp, key: &[Value]) -> Result<bool> {
        self.stack.clear();
        self.leaf = None;
        self.leaf_hdr = None;
        self.leaf_pgno = 0;
        self.cell_idx = 0;
        self.at_end = false;
        self.pending_advance = false;
        self.current_payload = None;
        self.descend_to_target(self.root, op, key).await?;
        self.refresh_payload().await?;
        Ok(self.is_valid())
    }

    /// Mark the current entry as deleted (called by the `IdxDelete` opcode path). The cursor
    /// stays on the cell that slid in; the next `next()` advances past it.
    pub fn mark_deleted(&mut self) {
        self.pending_advance = true;
    }

    /// Refresh `current_payload` from the leaf cell at `cell_idx`. Called after every move.
    async fn refresh_payload(&mut self) -> Result<()> {
        if !self.is_valid() {
            self.current_payload = None;
            return Ok(());
        }
        let leaf = self.leaf.as_ref().expect("leaf present when is_valid");
        let hdr = self.leaf_hdr.expect("leaf_hdr present when is_valid");
        let off = hdr.cell_pointer(leaf, self.cell_idx)?;
        let cell = parse_index_leaf_cell(leaf, off, self.usable)?;
        let payload = super::cell::assemble_index_payload(&self.pager, &cell).await?;
        self.current_payload = Some(payload);
        Ok(())
    }

    /// The k-th child of an interior-index page (k is 0-based, in in-order traversal order).
    fn interior_child(
        hdr: &PageHeader,
        page: &[u8],
        k: usize,
        usable: usize,
    ) -> Result<Option<u32>> {
        let n = hdr.num_cells as usize;
        if k < n {
            let off = hdr.cell_pointer(page, k)?;
            Ok(Some(
                parse_index_interior_cell(page, off, usable)?.left_child,
            ))
        } else if k == n {
            Ok(hdr.right_most_pointer)
        } else {
            Ok(None)
        }
    }

    /// Descend from `pgno`, following the left-most child of every interior page, until a leaf
    /// is reached; positions on its first cell.
    async fn descend_left(&mut self, pgno: u32) -> Result<()> {
        let mut pgno = pgno;
        loop {
            let page = self.pager.get_page(pgno).await?;
            let base = self.pager.btree_header_offset(pgno);
            let hdr = PageHeader::parse(&page, base)?;
            match hdr.page_type {
                PageType::LeafIndex => {
                    self.leaf_pgno = pgno;
                    self.leaf = Some(page);
                    self.leaf_hdr = Some(hdr);
                    self.cell_idx = 0;
                    self.pending_advance = false;
                    return Ok(());
                }
                PageType::InteriorIndex => {
                    let first = Self::interior_child(&hdr, &page, 0, self.usable)?
                        .ok_or_else(|| Error::corrupt("interior index page with no children"))?;
                    self.stack.push((page, hdr, 1));
                    pgno = first;
                }
                _ => return Err(Error::corrupt("expected an index b-tree page during scan")),
            }
        }
    }

    /// Descend to the leaf that should contain `key` per the `op` direction, then position
    /// on the matching cell.
    async fn descend_to_target(&mut self, pgno: u32, op: SeekOp, key: &[Value]) -> Result<()> {
        let mut pgno = pgno;
        loop {
            let page = self.pager.get_page(pgno).await?;
            let base = self.pager.btree_header_offset(pgno);
            let hdr = PageHeader::parse(&page, base)?;
            match hdr.page_type {
                PageType::LeafIndex => {
                    self.leaf_pgno = pgno;
                    self.leaf = Some(page);
                    self.leaf_hdr = Some(hdr);
                    self.cell_idx = self.leaf_index_for_key(key, op).await?;
                    self.pending_advance = false;
                    return Ok(());
                }
                PageType::InteriorIndex => {
                    let child =
                        pick_child_for_key(&self.key_info, &page, &hdr, key, op, self.usable)
                            .await?;
                    self.stack.push((page, hdr, 0));
                    pgno = child;
                }
                _ => return Err(Error::corrupt("expected an index b-tree page during seek")),
            }
        }
    }

    /// On a positioned leaf, the cell index where the seek comparison's "first `op`" condition
    /// holds. Mirrors the binary-search pattern in `sqlite3BtreeIndexMoveto` for the leaf phase.
    async fn leaf_index_for_key(&self, key: &[Value], op: SeekOp) -> Result<usize> {
        let leaf = self
            .leaf
            .as_ref()
            .expect("descend_to_target positions a leaf before this");
        let hdr = self.leaf_hdr.expect("leaf_hdr present when leaf present");
        let n = hdr.num_cells as usize;
        let mut lo = 0usize;
        let mut hi = n;
        let encoding = self.pager.text_encoding();
        while lo < hi {
            let mid = (lo + hi) / 2;
            let off = hdr.cell_pointer(leaf, mid)?;
            let cell = parse_index_leaf_cell(leaf, off, self.usable)?;
            let payload = super::cell::assemble_index_payload(&self.pager, &cell).await?;
            let values = decode_record(&payload, encoding)?;
            let prefix_len = values.len().saturating_sub(1).min(key.len());
            let prefix = &values[..prefix_len];
            let cmp = compare_prefix(prefix, key, &self.key_info);
            let pos = match op {
                SeekOp::Ge => matches!(cmp, Ordering::Less),
                SeekOp::Gt => matches!(cmp, Ordering::Less | Ordering::Equal),
                SeekOp::Le => !matches!(cmp, Ordering::Greater),
                SeekOp::Lt => matches!(cmp, Ordering::Greater),
            };
            if pos {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let idx = match op {
            SeekOp::Ge | SeekOp::Gt => lo,
            SeekOp::Le | SeekOp::Lt => lo.saturating_sub(1),
        };
        Ok(idx)
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
            match Self::interior_child(&hdr, &page, next_k, self.usable)? {
                Some(child) => {
                    self.stack.last_mut().expect("stack non-empty").2 += 1;
                    self.descend_left(child).await?;
                    if self.leaf_cells() > 0 {
                        return Ok(());
                    }
                }
                None => {
                    self.stack.pop();
                }
            }
        }
    }
}

/// The interior-page child that should hold the entry in the direction of `op` relative to
/// `key`. Mirrors `pick_child_for_rowid` in the table-cursor but on key comparison.
async fn pick_child_for_key(
    key_info: &[KeyField],
    page: &[u8],
    hdr: &PageHeader,
    key: &[Value],
    op: SeekOp,
    usable: usize,
) -> Result<u32> {
    let encoding = TextEncoding::Utf8;
    let n = hdr.num_cells as usize;
    for i in 0..n {
        let off = hdr.cell_pointer(page, i)?;
        let cell = parse_index_interior_cell(page, off, usable)?;
        // The divider payload is a key record. The first `n-1` values are the indexed
        // columns; the last is the rowid. We compare only the indexed-column prefix against
        // the search key.
        let values = decode_record(cell.local_payload, encoding)?;
        let prefix_len = values.len().saturating_sub(1).min(key.len());
        let prefix = &values[..prefix_len];
        let cmp = compare_prefix(prefix, key, key_info);
        // The divider key in an interior-index cell is the LARGEST key in the left-child
        // subtree. For `Ge`/`Gt`, we go left when our search key `<=` the divider (i.e. the
        // target is in or to the left of the left subtree); otherwise we go right.
        // For `Le`/`Lt`, we go right when our search key `>=` the divider.
        let go_left = match op {
            SeekOp::Ge | SeekOp::Gt => !matches!(cmp, Ordering::Greater),
            SeekOp::Le | SeekOp::Lt => matches!(cmp, Ordering::Less),
        };
        if go_left {
            return Ok(cell.left_child);
        }
    }
    hdr.right_most_pointer
        .ok_or_else(|| Error::corrupt("interior index page has no right pointer"))
}

/// Compare two key prefixes field-by-field using the per-column collation in `key_info`.
/// `prefix` is a slice of `Value` taken from the on-disk key record; `key` is the unpacked
/// register vector. If one vector is shorter than the other, the shorter one is considered less
/// — matching the prefix-vs-full comparison used by `index_insert`.
fn compare_prefix(prefix: &[Value], key: &[Value], key_info: &[KeyField]) -> Ordering {
    let n = prefix.len().min(key.len());
    for i in 0..n {
        let coll = key_info
            .get(i)
            .map(|f| f.collation)
            .unwrap_or(Collation::Binary);
        match mem_compare(&prefix[i], &key[i], coll) {
            Ordering::Equal => {}
            non_eq => return non_eq,
        }
    }
    prefix.len().cmp(&key.len())
}
