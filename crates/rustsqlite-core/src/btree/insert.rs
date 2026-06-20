//! Table b-tree insertion and rowid allocation (the write-path counterpart of [`super::cursor`],
//! mirroring `sqlite3BtreeInsert` / `OP_NewRowid` in `btree.c`/`vdbe.c`).
//!
//! M4.2 introduced single-leaf-only insertion. M4.5 extends this with a full path-walking
//! insert that splits a leaf when it fills, splits a parent when it fills, and promotes a
//! single-leaf root to an interior page (the "balance_deeper" path) when the root itself
//! outgrows one page. The split boundary is a 50/50 redistribution implemented in
//! [`super::balance`].

use std::future::Future;
use std::pin::Pin;

use crate::error::{Error, Result};
use crate::pager::Pager;

use super::balance;
use super::cell::{parse_table_interior_cell, table_leaf_cell_rowid};
use super::page::{self, PageHeader, PageType};

/// Recursive helper. `path` is the stack of ancestor `(pgno, base)` frames above the node we
/// are about to descend into (empty at the root). When a leaf fills, we split it and recurse
/// up the path to possibly split the parent in turn.
fn insert_with_splitting<'a>(
    pager: &'a Pager,
    pgno: u32,
    rowid: i64,
    cell: &'a [u8],
    path: &'a mut Vec<(u32, usize)>,
) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
    Box::pin(async move {
        let base = pager.btree_header_offset(pgno);
        let buf = pager.get_page(pgno).await?;
        let hdr = PageHeader::parse(&buf, base)?;
        match hdr.page_type {
            PageType::LeafTable => {
                let idx = leaf_insert_index(&buf, &hdr, rowid)?;
                let mut leaf = pager.read_page_for_write(pgno).await?;
                match page::insert_leaf_cell(&mut leaf, base, idx, cell) {
                    Ok(()) => {
                        pager.write_page(pgno, leaf)?;
                        Ok(())
                    }
                    Err(e) if is_page_full(&e) => {
                        // The leaf is full. Drop the partial copy (never written back) and split.
                        drop(leaf);
                        let parent = path.pop();
                        balance_leaf(pager, pgno, parent, path, rowid, cell).await
                    }
                    Err(other) => Err(other),
                }
            }
            PageType::InteriorTable => {
                let child = pick_child(&buf, &hdr, rowid)?;
                path.push((pgno, base));
                insert_with_splitting(pager, child, rowid, cell, path).await
            }
            _ => Err(Error::corrupt("table_insert: not a table b-tree page")),
        }
    })
}

/// Handle a leaf overflow by splitting it. If the leaf is the b-tree's root, promote the root
/// to an interior page; otherwise, split and install a divider on the parent, then place the
/// pending cell on the correct side of the divider. If the parent itself fills during the
/// divider install, recurse up the ancestor path; if the parent is a non-root interior page
/// that fills, split it in place (mirrors `balance_nonroot` for table b-trees).
fn balance_leaf<'a>(
    pager: &'a Pager,
    leaf_pgno: u32,
    parent: Option<(u32, usize)>,
    ancestor_path: &'a mut Vec<(u32, usize)>,
    pending_rowid: i64,
    pending_cell: &'a [u8],
) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
    Box::pin(async move {
        let parent_pgno = match parent {
            Some((p, _)) => p,
            None => {
                balance::promote_root_and_split(pager, leaf_pgno).await?;
                return insert_after_root_promotion(pager, leaf_pgno, pending_rowid, pending_cell)
                    .await;
            }
        };

        let new_pgno = match balance::split_leaf(pager, leaf_pgno, Some(parent_pgno)).await {
            Ok(p) => p,
            Err(e) if is_page_full(&e) => {
                // The parent filled during the divider install. Recurse up the path.
                if let Some(grand) = ancestor_path.pop() {
                    // Parent is a non-root interior page. Split it to make room, then
                    // restart the pending insert from the root so it descends the new tree.
                    split_table_parent(pager, parent_pgno, Some(grand), ancestor_path).await?;
                    return insert_with_splitting(
                        pager,
                        leaf_pgno,
                        pending_rowid,
                        pending_cell,
                        &mut Vec::new(),
                    )
                    .await;
                }
                // No grandparent: the parent IS the root. Promote and split the parent.
                balance::promote_root_and_split(pager, parent_pgno).await?;
                return insert_after_root_promotion(
                    pager,
                    parent_pgno,
                    pending_rowid,
                    pending_cell,
                )
                .await;
            }
            Err(other) => return Err(other),
        };

        // Place the pending cell on the correct side of the divider.
        let target = target_after_split(pager, parent_pgno, new_pgno, pending_rowid).await?;
        let mut fresh = Vec::new();
        insert_with_splitting(pager, target, pending_rowid, pending_cell, &mut fresh).await
    })
}

/// Split a table-interior page `parent_pgno` to make room for a divider above it.
/// `grand` is the immediate ancestor above `parent_pgno` (its parent), if any; otherwise
/// `parent_pgno` is the root and is promoted.  After splitting, the pending insertion must
/// be restarted from the leaf so it descends the new tree shape.
fn split_table_parent<'a>(
    pager: &'a Pager,
    parent_pgno: u32,
    grand: Option<(u32, usize)>,
    ancestor_path: &'a mut Vec<(u32, usize)>,
) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
    Box::pin(async move {
        let grand_pgno = match grand {
            Some((g, _)) => Some(g),
            None => None,
        };
        match balance::split_table_interior_page(pager, parent_pgno, grand_pgno).await {
            Ok(_) => Ok(()),
            Err(e) if is_page_full(&e) => {
                // The grandparent is also full: recurse upward.
                let next_ancestor = ancestor_path.pop();
                split_table_parent(pager, grand_pgno.unwrap(), next_ancestor, ancestor_path).await
            }
            Err(other) => Err(other),
        }
    })
}

/// After splitting a leaf, decide which of the two children the pending rowid belongs to.
/// Reads the divider rowid on the parent (the cell whose left_child points at `new_pgno`).
async fn target_after_split(
    pager: &Pager,
    parent_pgno: u32,
    new_pgno: u32,
    pending_rowid: i64,
) -> Result<u32> {
    let base = pager.btree_header_offset(parent_pgno);
    let buf = pager.get_page(parent_pgno).await?;
    let hdr = PageHeader::parse(&buf, base)?;
    if hdr.page_type != PageType::InteriorTable {
        return Ok(new_pgno);
    }
    for i in 0..hdr.num_cells as usize {
        let off = hdr.cell_pointer(&buf, i)?;
        let cell = parse_table_interior_cell(&buf, off)?;
        if cell.left_child == new_pgno {
            return Ok(if pending_rowid > cell.rowid {
                new_pgno
            } else {
                cell.left_child
            });
        }
    }
    if hdr.right_most_pointer == Some(new_pgno) {
        if hdr.num_cells == 0 {
            return Ok(new_pgno);
        }
        let last_off = hdr.cell_pointer(&buf, hdr.num_cells as usize - 1)?;
        let last = parse_table_interior_cell(&buf, last_off)?;
        return Ok(if pending_rowid > last.rowid {
            new_pgno
        } else {
            last.left_child
        });
    }
    Ok(new_pgno)
}

/// After a single-leaf root has been promoted to an interior page with two leaf children,
/// place the pending cell on the correct side of the new divider.
fn insert_after_root_promotion<'a>(
    pager: &'a Pager,
    new_root_pgno: u32,
    pending_rowid: i64,
    pending_cell: &'a [u8],
) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
    Box::pin(async move {
        let base = pager.btree_header_offset(new_root_pgno);
        let buf = pager.get_page(new_root_pgno).await?;
        let hdr = PageHeader::parse(&buf, base)?;
        if hdr.num_cells == 0 {
            return Err(Error::corrupt(
                "insert_after_root_promotion: promoted root has no cells",
            ));
        }
        let cell_off = hdr.cell_pointer(&buf, 0)?;
        let interior = parse_table_interior_cell(&buf, cell_off)?;
        let target = if pending_rowid <= interior.rowid {
            interior.left_child
        } else {
            hdr.right_most_pointer
                .ok_or_else(|| Error::corrupt("promoted root missing right-most child"))?
        };
        let mut fresh = Vec::new();
        insert_with_splitting(pager, target, pending_rowid, pending_cell, &mut fresh).await
    })
}

/// Pick the child page that owns `rowid`: the left child of the first interior cell whose
/// rowid is `> rowid`, falling back to the right-most child. Mirrors the descent rule in
/// `moveToChild` / `sqlite3BtreeNext`.
fn pick_child(page: &[u8], hdr: &PageHeader, rowid: i64) -> Result<u32> {
    let n = hdr.num_cells as usize;
    for i in 0..n {
        let off = hdr.cell_pointer(page, i)?;
        let cell = parse_table_interior_cell(page, off)?;
        if rowid <= cell.rowid {
            return Ok(cell.left_child);
        }
    }
    hdr.right_most_pointer
        .ok_or_else(|| Error::corrupt("interior table page has no right pointer"))
}

/// The cell-pointer index at which a cell with key `rowid` belongs on a leaf page, keeping the
/// array sorted ascending. Returns the position of the first existing cell whose rowid is `>=`
/// the new one (so an equal rowid would insert before it — callers allocate fresh rowids via
/// [`max_rowid`], so duplicates do not normally occur).
fn leaf_insert_index(page: &[u8], hdr: &PageHeader, rowid: i64) -> Result<usize> {
    let n = hdr.num_cells as usize;
    for i in 0..n {
        let off = hdr.cell_pointer(page, i)?;
        if rowid <= table_leaf_cell_rowid(page, off)? {
            return Ok(i);
        }
    }
    Ok(n)
}

/// Match the "page is full" error from [`page::insert_leaf_cell`] / [`page::insert_interior_cell`].
fn is_page_full(e: &Error) -> bool {
    e.message.contains("page is full")
}

/// Public entry point for table insertion. Wraps the recursive [`insert_with_splitting`] helper
/// (which needs to be boxed, since Rust's async recursion cannot be statically sized).
pub async fn table_insert(pager: &Pager, root: u32, rowid: i64, payload: &[u8]) -> Result<()> {
    let usable = pager.usable_size();
    // When auto-vacuum is on, the cell's overflow chain (if any) needs the host leaf page as
    // its OVERFLOW1 ptrmap parent. We don't know the host leaf until we descend, so for the
    // autovac case we build the cell *inside* the descent (in `insert_with_splitting`) once
    // the leaf is known. For the common non-overflow/non-autovac path this is equivalent.
    if pager.auto_vacuum() {
        return insert_with_splitting_autovac(pager, root, rowid, payload, &mut Vec::new())
            .await;
    }
    let cell = super::cell::build_table_leaf_cell(pager, rowid, payload, usable);
    insert_with_splitting(pager, root, rowid, &cell, &mut Vec::new()).await
}

/// Auto-vacuum-aware insertion: descends the b-tree without first building the cell, then
/// builds the cell at the leaf (passing the leaf pgno as the overflow chain's host). This is
/// only used when auto-vacuum is on, since the overflow ptrmap entry needs the host leaf.
fn insert_with_splitting_autovac<'a>(
    pager: &'a Pager,
    pgno: u32,
    rowid: i64,
    payload: &'a [u8],
    path: &'a mut Vec<(u32, usize)>,
) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
    Box::pin(async move {
        let base = pager.btree_header_offset(pgno);
        let buf = pager.get_page(pgno).await?;
        let hdr = PageHeader::parse(&buf, base)?;
        match hdr.page_type {
            PageType::LeafTable => {
                let usable = pager.usable_size();
                let cell = super::cell::build_table_leaf_cell_with_host(
                    pager,
                    rowid,
                    payload,
                    usable,
                    Some(pgno),
                );
                let idx = leaf_insert_index(&buf, &hdr, rowid)?;
                let mut leaf = pager.read_page_for_write(pgno).await?;
                match page::insert_leaf_cell(&mut leaf, base, idx, &cell) {
                    Ok(()) => {
                        pager.write_page(pgno, leaf)?;
                        Ok(())
                    }
                    Err(e) if is_page_full(&e) => {
                        drop(leaf);
                        let parent = path.pop();
                        balance_leaf(pager, pgno, parent, path, rowid, &cell).await
                    }
                    Err(other) => Err(other),
                }
            }
            PageType::InteriorTable => {
                let child = pick_child(&buf, &hdr, rowid)?;
                path.push((pgno, base));
                insert_with_splitting_autovac(pager, child, rowid, payload, path).await
            }
            _ => Err(Error::corrupt("table_insert: not a table b-tree page")),
        }
    })
}

/// The largest rowid currently stored in the table b-tree rooted at `root` (0 if the table is
/// empty). `OP_NewRowid` uses this to pick the next rowid (`max + 1`). Walks the rightmost child
/// pointers down to the rightmost leaf, then reads that leaf's last cell — reading through the
/// pager's dirty overlay, so it reflects rows inserted earlier in the same transaction.
pub async fn max_rowid(pager: &Pager, root: u32) -> Result<i64> {
    let mut pgno = root;
    loop {
        let page = pager.get_page(pgno).await?;
        let base = pager.btree_header_offset(pgno);
        let hdr = PageHeader::parse(&page, base)?;
        match hdr.page_type {
            PageType::LeafTable => {
                let n = hdr.num_cells as usize;
                if n == 0 {
                    return Ok(0);
                }
                let off = hdr.cell_pointer(&page, n - 1)?;
                return table_leaf_cell_rowid(&page, off);
            }
            PageType::InteriorTable => {
                pgno = hdr
                    .right_most_pointer
                    .ok_or_else(|| Error::corrupt("interior table page has no right pointer"))?;
            }
            _ => return Err(Error::corrupt("max_rowid: not a table b-tree page")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::btree::{create_table_btree, scan_table};
    use crate::format::{decode_record, encode_record, TextEncoding};
    use crate::pager::Pager;
    use crate::types::Value;
    use crate::vfs::{MemVfs, OpenFlags, Vfs};

    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
    }

    #[test]
    fn create_insert_commit_reopen_roundtrip() {
        rt().block_on(async {
            let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
            let file = vfs
                .open("bt.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            let pager = Pager::create_fresh(vfs.clone(), "bt.db".into(), file, 4096)
                .await
                .unwrap();

            pager.begin_write().await.unwrap();
            let root = create_table_btree(&pager).await.unwrap();
            assert_eq!(max_rowid(&pager, root).await.unwrap(), 0); // empty

            let rows: &[(i64, Value)] = &[
                (2, Value::Text("two".into())),
                (1, Value::Int(11)),
                (5, Value::Real(2.5)),
                (3, Value::Blob(vec![0xde, 0xad])),
            ];
            for (rowid, v) in rows {
                let payload = encode_record(std::slice::from_ref(v));
                table_insert(&pager, root, *rowid, &payload).await.unwrap();
            }
            assert_eq!(max_rowid(&pager, root).await.unwrap(), 5);
            pager.commit().await.unwrap();

            let file2 = vfs.open("bt.db", OpenFlags::READONLY).await.unwrap();
            let reopened = Pager::open(vfs.clone(), "bt.db".into(), file2)
                .await
                .unwrap();
            let scanned = scan_table(&reopened, root).await.unwrap();
            let got: Vec<(i64, Value)> = scanned
                .into_iter()
                .map(|(rowid, payload)| {
                    let vals = decode_record(&payload, TextEncoding::Utf8).unwrap();
                    (rowid, vals.into_iter().next().unwrap())
                })
                .collect();
            assert_eq!(
                got,
                vec![
                    (1, Value::Int(11)),
                    (2, Value::Text("two".into())),
                    (3, Value::Blob(vec![0xde, 0xad])),
                    (5, Value::Real(2.5)),
                ]
            );
        });
    }

    #[test]
    fn page_split_grows_beyond_one_leaf() {
        // Insert enough rows on a 512-byte page to force the single leaf to split and a
        // root promotion (interior page + two leaves).
        rt().block_on(async {
            let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
            let file = vfs
                .open("split.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            let pager = Pager::create_fresh(vfs.clone(), "split.db".into(), file, 512)
                .await
                .unwrap();
            pager.begin_write().await.unwrap();
            let root = create_table_btree(&pager).await.unwrap();
            let big = encode_record(&[Value::Blob(vec![0u8; 60])]);
            // 200 rows forces several leaf splits; on a 512-byte page each leaf holds ~3 rows.
            for rowid in 1..=200 {
                table_insert(&pager, root, rowid, &big).await.unwrap();
            }
            pager.commit().await.unwrap();

            // Reopen and scan; every rowid must be present and in ascending order.
            let file2 = vfs.open("split.db", OpenFlags::READONLY).await.unwrap();
            let reopened = Pager::open(vfs.clone(), "split.db".into(), file2)
                .await
                .unwrap();
            let scanned = scan_table(&reopened, root).await.unwrap();
            assert_eq!(scanned.len(), 200);
            for (i, (rid, _)) in scanned.iter().enumerate() {
                assert_eq!(*rid, (i + 1) as i64, "rowid at index {i} must be {}", i + 1);
            }
        });
    }

    #[test]
    fn overflow_page_chain_round_trip() {
        // A payload larger than `usable - 35` forces the write path to allocate an overflow
        // page; the cell stores a 4-byte overflow pointer and the tail lives on the chained
        // page. After commit + reopen, the read cursor must reassemble the full payload.
        rt().block_on(async {
            let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
            let file = vfs
                .open("overflow.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            let pager = Pager::create_fresh(vfs.clone(), "overflow.db".into(), file, 4096)
                .await
                .unwrap();
            pager.begin_write().await.unwrap();
            let root = create_table_btree(&pager).await.unwrap();
            // 8 KiB payload, far larger than the inline 4061-byte local window.
            let big_payload = vec![0xABu8; 8 * 1024];
            let record = encode_record(&[Value::Blob(big_payload.clone())]);
            for rowid in 1..=5 {
                table_insert(&pager, root, rowid, &record).await.unwrap();
            }
            pager.commit().await.unwrap();

            let file2 = vfs.open("overflow.db", OpenFlags::READONLY).await.unwrap();
            let reopened = Pager::open(vfs.clone(), "overflow.db".into(), file2)
                .await
                .unwrap();
            let scanned = scan_table(&reopened, root).await.unwrap();
            assert_eq!(scanned.len(), 5);
            for (i, (rid, payload)) in scanned.iter().enumerate() {
                assert_eq!(*rid, (i + 1) as i64);
                let vals = decode_record(payload, TextEncoding::Utf8).unwrap();
                assert_eq!(vals.len(), 1);
                assert_eq!(vals[0], Value::Blob(big_payload.clone()));
            }
        });
    }
}
