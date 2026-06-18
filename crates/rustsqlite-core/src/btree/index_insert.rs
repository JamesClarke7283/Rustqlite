//! Index b-tree insertion with page splitting (mirrors `sqlite3BtreeInsert` for index b-trees).
//!
//! M5.1 introduced single-leaf index insertion. M5.2 extends this with a full path-walking
//! insert that splits an index leaf when it fills, splits a parent when it fills, and promotes
//! a single-leaf root to an interior page (the "balance_deeper" path) when the root itself
//! outgrows one page. The split logic is in [`super::balance`] (`split_index_leaf` and
//! `promote_index_root_and_split`); this module provides the recursive insertion walk and the
//! public entry point [`index_insert`].
//!
//! The `key_record` is the index columns followed by the table's rowid, all encoded by
//! [`crate::format::encode_record`]. The function walks the b-tree from root to leaf, following
//! interior-page child pointers, and inserts at the correct position. On overflow it splits and
//! recurses — the same shape as the table-side [`super::insert::table_insert`].

use std::future::Future;
use std::pin::Pin;

use crate::error::{Error, Result};
use crate::format::{decode_record, TextEncoding};
use crate::pager::Pager;
use crate::types::{Collation, Value};
use crate::vdbe::compare::mem_compare;
use crate::vdbe::KeyField;

use super::balance;
use super::cell::{build_index_leaf_cell, parse_index_interior_cell, parse_index_leaf_cell};
use super::page::{self, PageHeader, PageType};

/// Insert a new index entry into the b-tree rooted at `root`. Walks interior pages if the tree
/// is multi-level, splits leaves that overflow, and recurses up the ancestor path when parents
/// overflow. `key_record` is the encoded record (`[indexed columns..., rowid]`). `key_info`
/// carries the per-column collation and DESC flag so comparisons during descent/split use the
/// same rules as the index cursor.
pub async fn index_insert(
    pager: &Pager,
    root: u32,
    key_record: &[u8],
    key_info: &[KeyField],
) -> Result<()> {
    loop {
        match index_insert_with_splitting(pager, root, key_record, key_info, &mut Vec::new()).await
        {
            Ok(()) => return Ok(()),
            Err(e) if needs_restart(&e) => continue,
            Err(other) => return Err(other),
        }
    }
}

fn needs_restart(e: &Error) -> bool {
    e.message == "index insert needs restart after ancestor split"
}

/// Recursive helper. `path` is the ancestor stack above the node we are descending into.
/// `key_info` is passed through every recursive call so comparisons use the same collation.
fn index_insert_with_splitting<'a>(
    pager: &'a Pager,
    pgno: u32,
    key_record: &'a [u8],
    key_info: &'a [KeyField],
    path: &'a mut Vec<(u32, usize)>,
) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
    Box::pin(async move {
        let base = pager.btree_header_offset(pgno);
        let buf = pager.get_page(pgno).await?;
        let hdr = PageHeader::parse(&buf, base)?;
        match hdr.page_type {
            PageType::LeafIndex => {
                insert_into_index_leaf(pager, pgno, key_record, key_info, path).await
            }
            PageType::InteriorIndex => {
                let child = pick_index_child(
                    key_info,
                    &buf,
                    &hdr,
                    key_record,
                    pager.text_encoding(),
                    pager.usable_size(),
                )?;
                path.push((pgno, base));
                index_insert_with_splitting(pager, child, key_record, key_info, path).await
            }
            _ => Err(Error::corrupt("index_insert: not an index b-tree page")),
        }
    })
}

/// Insert `key_record` into the leaf-index page `pgno`, splitting if the page is full.
async fn insert_into_index_leaf(
    pager: &Pager,
    leaf_pgno: u32,
    key_record: &[u8],
    key_info: &[KeyField],
    path: &mut Vec<(u32, usize)>,
) -> Result<()> {
    let usable = pager.usable_size();
    let base = pager.btree_header_offset(leaf_pgno);
    let page = pager.get_page(leaf_pgno).await?;
    let hdr = PageHeader::parse(&page, base)?;

    let idx = index_leaf_insert_position(
        key_info,
        &page,
        &hdr,
        key_record,
        usable,
        pager.text_encoding(),
    )?;

    let cell = build_index_leaf_cell(pager, key_record, usable);
    let mut leaf = pager.read_page_for_write(leaf_pgno).await?;
    match page::insert_leaf_cell(&mut leaf, base, idx, &cell) {
        Ok(()) => {
            pager.write_page(leaf_pgno, leaf)?;
            Ok(())
        }
        Err(e) if is_page_full(&e) => {
            drop(leaf);
            let parent = path.pop();
            balance_index_page(pager, leaf_pgno, parent, key_info, path, key_record).await
        }
        Err(other) => Err(other),
    }
}

/// Handle a full index page (leaf or interior) by splitting it and reinserting
/// `pending_key` into the correct child. Called when the original descent found
/// a full leaf.
fn balance_index_page<'a>(
    pager: &'a Pager,
    pgno: u32,
    parent: Option<(u32, usize)>,
    key_info: &'a [KeyField],
    ancestor_path: &'a mut Vec<(u32, usize)>,
    pending_key: &'a [u8],
) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
    Box::pin(async move {
        let parent_pgno = match parent {
            Some((p, _)) => p,
            None => {
                let base = pager.btree_header_offset(pgno);
                let buf = pager.get_page(pgno).await?;
                let hdr = PageHeader::parse(&buf, base)?;
                match hdr.page_type {
                    PageType::LeafIndex => {
                        balance::promote_index_root_and_split(pager, pgno).await?;
                    }
                    PageType::InteriorIndex => {
                        balance::promote_index_root_interior(pager, pgno).await?;
                    }
                    _ => {
                        return Err(Error::corrupt(
                            "balance_index_page: root is not an index page",
                        ))
                    }
                }
                return index_insert_with_splitting(
                    pager,
                    pgno,
                    pending_key,
                    key_info,
                    &mut Vec::new(),
                )
                .await;
            }
        };

        let split_result =
            balance::split_index_leaf(pager, pgno, Some(parent_pgno), key_info).await;
        let (new_pgno, divider_key) = match split_result {
            Ok(result) => result,
            Err(e) if is_page_full(&e) => {
                // The parent interior page is full. Split the parent to make room, then
                // restart the whole insertion from the root so the descent uses the new tree
                // shape. Do not insert pending_key here; it will be inserted once by the
                // restarted descent.
                split_ancestor_page(pager, parent_pgno, ancestor_path, key_info).await?;
                return Err(Error::msg(
                    "index insert needs restart after ancestor split",
                ));
            }
            Err(other) => return Err(other),
        };

        // Decide which child the pending key belongs to using the divider key directly.
        // The divider key was promoted from the split point; keys < divider go left (pgno),
        // keys >= divider go right (new_pgno). Since rowids in index keys are unique, equality
        // means we go right.
        let target = choose_index_child_after_split(
            key_info,
            pager,
            pgno,
            new_pgno,
            &divider_key,
            pending_key,
        )?;
        let mut fresh = Vec::new();
        index_insert_with_splitting(pager, target, pending_key, key_info, &mut fresh).await
    })
}

/// Split an interior-index page that is too full to accept a new divider. This is
/// used during the ancestor-split phase of `balance_index_page`; it does not
/// insert the original pending key, it only reshapes the tree.
fn split_ancestor_page<'a>(
    pager: &'a Pager,
    pgno: u32,
    ancestor_path: &'a mut Vec<(u32, usize)>,
    key_info: &'a [KeyField],
) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
    Box::pin(async move {
        let parent = match ancestor_path.pop() {
            Some(p) => Some(p),
            None => None,
        };
        let parent_pgno = match parent {
            Some((p, _)) => p,
            None => {
                balance::promote_index_root_interior(pager, pgno).await?;
                return Ok(());
            }
        };
        match balance::split_index_interior_page(pager, pgno, Some(parent_pgno), key_info).await {
            Ok(_) => Ok(()),
            Err(e) if is_page_full(&e) => {
                split_ancestor_page(pager, parent_pgno, ancestor_path, key_info).await?;
                Ok(())
            }
            Err(other) => Err(other),
        }
    })
}

fn choose_index_child_after_split(
    key_info: &[KeyField],
    pager: &Pager,
    left_pgno: u32,
    right_pgno: u32,
    divider_key: &[u8],
    pending_key: &[u8],
) -> Result<u32> {
    let encoding = pager.text_encoding();
    let div_values = decode_record(divider_key, encoding)?;
    let key_values = decode_record(pending_key, encoding)?;
    let cmp = compare_record_prefixes(
        key_info,
        &div_values[..div_values.len().saturating_sub(1)],
        &div_values[div_values.len().saturating_sub(1)],
        &key_values[..key_values.len().saturating_sub(1)],
        &key_values[key_values.len().saturating_sub(1)],
    );
    Ok(if cmp == std::cmp::Ordering::Greater {
        left_pgno
    } else {
        right_pgno
    })
}

/// Pick the child page of an interior-index page that should contain `key_record`.
///
/// Interior index cells have the form `(left_child, key)`. The in-order traversal is:
/// left_child of cell[0], cell[0].key, cell[1].left_child, cell[1].key, …, right_most.
/// So `cell[i].left_child` holds keys ≤ `cell[i].key`, and the region between
/// `cell[i].key` and `cell[i+1].key` (or right_most) holds keys > `cell[i].key`
/// and ≤ the next cell's key.
///
/// To find which child to descend into for `key_record`: walk the cells in order; the
/// first cell whose key is ≥ `key_record` means `key_record ≤ cell.key`, so descend
/// into `cell.left_child`. If no cell key is ≥ the search key, descend into `right_most`.
fn pick_index_child(
    key_info: &[KeyField],
    page: &[u8],
    hdr: &PageHeader,
    key_record: &[u8],
    encoding: TextEncoding,
    usable: usize,
) -> Result<u32> {
    let search = decode_record(key_record, encoding)?;
    let search_prefix_len = search.len().saturating_sub(1);
    let search_prefix = &search[..search_prefix_len];
    let search_rowid = &search[search_prefix_len];
    for i in 0..hdr.num_cells as usize {
        let off = hdr.cell_pointer(page, i)?;
        let cell = parse_index_interior_cell(page, off, usable)?;
        let existing = decode_record(cell.local_payload, encoding)?;
        let existing_prefix_len = existing.len().saturating_sub(1);
        let existing_prefix = &existing[..existing_prefix_len.min(search_prefix_len)];
        let cmp = compare_record_prefixes(
            key_info,
            existing_prefix,
            &existing[existing_prefix_len],
            search_prefix,
            search_rowid,
        );
        // If existing >= search, the search key belongs in this cell's left child.
        if cmp != std::cmp::Ordering::Less {
            return Ok(cell.left_child);
        }
    }
    hdr.right_most_pointer
        .ok_or_else(|| Error::corrupt("interior index page has no right pointer"))
}

/// Binary-search an index leaf page for the insertion position of `key_record`.
fn index_leaf_insert_position(
    key_info: &[KeyField],
    page: &[u8],
    hdr: &PageHeader,
    key_record: &[u8],
    usable: usize,
    encoding: TextEncoding,
) -> Result<usize> {
    let n = hdr.num_cells as usize;
    let search_values = decode_record(key_record, encoding)?;
    let search_prefix_len = search_values.len().saturating_sub(1);
    let search_prefix = &search_values[..search_prefix_len];
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = (lo + hi) / 2;
        let off = hdr.cell_pointer(page, mid)?;
        let cell = parse_index_leaf_cell(page, off, usable)?;
        let existing = decode_record(cell.local_payload, encoding)?;
        let existing_prefix_len = existing.len().saturating_sub(1);
        let existing_prefix = &existing[..existing_prefix_len];
        let cmp = compare_record_prefixes(
            key_info,
            existing_prefix,
            &existing[existing_prefix_len],
            search_prefix,
            &search_values[search_prefix_len],
        );
        if cmp == std::cmp::Ordering::Less {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

fn compare_record_prefixes(
    key_info: &[KeyField],
    a_prefix: &[Value],
    a_rowid: &Value,
    b_prefix: &[Value],
    b_rowid: &Value,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let n = a_prefix.len().min(b_prefix.len());
    for i in 0..n {
        let coll = key_info
            .get(i)
            .map(|f| f.collation)
            .unwrap_or(Collation::Binary);
        match mem_compare(&a_prefix[i], &b_prefix[i], coll) {
            Ordering::Equal => {}
            non_eq => return non_eq,
        }
    }
    match a_prefix.len().cmp(&b_prefix.len()) {
        Ordering::Equal => mem_compare(a_rowid, b_rowid, Collation::Binary),
        // A shorter prefix is considered "less" when compared to a longer key. This is used
        // only at interior-page boundaries where the divider may have fewer fields than the
        // search key; the remaining fields then determine the final ordering against the
        // next divider or right-most region.
        Ordering::Less => Ordering::Less,
        Ordering::Greater => Ordering::Greater,
    }
}

fn is_page_full(e: &Error) -> bool {
    e.message.contains("page is full")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::btree::{create_index_btree, scan_index};
    use crate::format::encode_record;
    use crate::pager::Pager;
    use crate::types::Value;
    use crate::vfs::{MemVfs, OpenFlags, Vfs};

    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
    }

    /// Insert 200 index entries on a 512-byte page, forcing several leaf splits and root
    /// promotions. Each key record is `[Int(rowid), Int(rowid)]` — a single-column index
    /// where the indexed column equals the rowid.
    #[test]
    fn index_split_grows_beyond_one_leaf() {
        rt().block_on(async {
            let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
            let file = vfs
                .open("isplit.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            let pager = Pager::create_fresh(vfs.clone(), "isplit.db".into(), file, 512)
                .await
                .unwrap();

            pager.begin_write().await.unwrap();
            let idx_root = create_index_btree(&pager).await.unwrap();

            for rowid in 1i64..=200 {
                let key = encode_record(&[Value::Int(rowid), Value::Int(rowid)]);
                index_insert(&pager, idx_root, &key, &[]).await.unwrap();
            }

            let scanned = scan_index(&pager, idx_root).await.unwrap();
            assert_eq!(scanned.len(), 200, "all 200 index entries must be present");
            for (i, (_, rowid)) in scanned.iter().enumerate() {
                assert_eq!(
                    *rowid,
                    (i + 1) as i64,
                    "entry at index {i} must be rowid {}",
                    i + 1
                );
            }

            pager.commit().await.unwrap();
        });
    }

    /// Insert entries with multi-column keys to test wider key records in split contexts.
    #[test]
    fn index_split_multi_column_keys() {
        rt().block_on(async {
            let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
            let file = vfs
                .open("imulti.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            let pager = Pager::create_fresh(vfs.clone(), "imulti.db".into(), file, 512)
                .await
                .unwrap();

            pager.begin_write().await.unwrap();
            let idx_root = create_index_btree(&pager).await.unwrap();

            for i in 1i64..=100 {
                let key = encode_record(&[Value::Int(i % 5), Value::Int(i), Value::Int(i)]);
                index_insert(&pager, idx_root, &key, &[]).await.unwrap();
            }

            let scanned = scan_index(&pager, idx_root).await.unwrap();
            assert_eq!(
                scanned.len(),
                100,
                "all 100 multi-column entries must be present"
            );

            pager.commit().await.unwrap();
        });
    }

    /// Insert in reverse order and verify the sorted order holds after splits.
    #[test]
    fn index_split_out_of_order_insertion() {
        rt().block_on(async {
            for n in [10usize, 50, 62, 63, 80, 100] {
                let db_name = format!("iooo{n}.db");
                let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
                let file = vfs
                    .open(&db_name, OpenFlags::READWRITE_CREATE)
                    .await
                    .unwrap();
                let pager = Pager::create_fresh(vfs.clone(), db_name.clone(), file, 512)
                    .await
                    .unwrap();

                pager.begin_write().await.unwrap();
                let idx_root = create_index_btree(&pager).await.unwrap();

                for i in (1i64..=n as i64).rev() {
                    let key = encode_record(&[Value::Int(i), Value::Int(i)]);
                    index_insert(&pager, idx_root, &key, &[]).await.unwrap();
                }

                let scanned = scan_index(&pager, idx_root).await.unwrap();
                assert_eq!(
                    scanned.len(),
                    n,
                    "n={n}: expected {n} entries, got {}",
                    scanned.len()
                );
                for (i, (_, rowid)) in scanned.iter().enumerate() {
                    assert_eq!(
                        *rowid,
                        (i + 1) as i64,
                        "n={n}: entry at index {i} must be rowid {}",
                        i + 1
                    );
                }
                pager.commit().await.unwrap();
            }
        });
    }

    /// Insert 10 entries with 4096-byte pages (no splits expected).
    #[test]
    fn index_no_split_basic() {
        rt().block_on(async {
            let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
            let file = vfs
                .open("nosplit.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            let pager = Pager::create_fresh(vfs.clone(), "nosplit.db".into(), file, 4096)
                .await
                .unwrap();

            pager.begin_write().await.unwrap();
            let idx_root = create_index_btree(&pager).await.unwrap();

            for i in 1i64..=10 {
                let key = encode_record(&[Value::Int(i), Value::Int(i)]);
                index_insert(&pager, idx_root, &key, &[]).await.unwrap();
            }
            let scanned = scan_index(&pager, idx_root).await.unwrap();
            assert_eq!(scanned.len(), 10);
            for (i, (_, rowid)) in scanned.iter().enumerate() {
                assert_eq!(*rowid, (i + 1) as i64);
            }
            pager.commit().await.unwrap();
        });
    }

    /// Insert enough entries on a 512-byte page to grow the index b-tree to three levels,
    /// forcing interior-page splits. All entries must remain findable and in sorted order.
    #[test]
    fn index_split_interior_pages() {
        rt().block_on(async {
            let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
            let file = vfs
                .open("iint.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            let pager = Pager::create_fresh(vfs.clone(), "iint.db".into(), file, 512)
                .await
                .unwrap();

            pager.begin_write().await.unwrap();
            let idx_root = create_index_btree(&pager).await.unwrap();

            let n = 5000i64;
            for rowid in 1i64..=n {
                let key = encode_record(&[Value::Int(rowid), Value::Int(rowid)]);
                index_insert(&pager, idx_root, &key, &[]).await.unwrap();
            }

            let scanned = scan_index(&pager, idx_root).await.unwrap();
            assert_eq!(
                scanned.len() as i64,
                n,
                "expected {n} entries, got {}",
                scanned.len()
            );
            for (i, (_, rowid)) in scanned.iter().enumerate() {
                assert_eq!(
                    *rowid,
                    (i + 1) as i64,
                    "entry at index {i} must be rowid {}",
                    i + 1
                );
            }

            pager.commit().await.unwrap();
        });
    }

    /// Insert just enough entries on a 512-byte page to trigger a single root promotion,
    /// then verify the exact key count and ordering.
    #[test]
    fn index_split_single_root_promotion() {
        rt().block_on(async {
            let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
            let file = vfs
                .open("isp.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            let pager = Pager::create_fresh(vfs.clone(), "isp.db".into(), file, 512)
                .await
                .unwrap();

            pager.begin_write().await.unwrap();
            let idx_root = create_index_btree(&pager).await.unwrap();

            for i in 1i64..=10 {
                let key = encode_record(&[Value::Int(i), Value::Int(i)]);
                index_insert(&pager, idx_root, &key, &[]).await.unwrap();
            }
            let scanned = scan_index(&pager, idx_root).await.unwrap();
            assert_eq!(scanned.len(), 10, "pre-split: expected 10 entries");

            for i in 11i64..=80 {
                let key = encode_record(&[Value::Int(i), Value::Int(i)]);
                index_insert(&pager, idx_root, &key, &[]).await.unwrap();
            }
            let scanned = scan_index(&pager, idx_root).await.unwrap();
            assert_eq!(
                scanned.len(),
                80,
                "post-split: expected 80 entries, got {}",
                scanned.len()
            );
            for (i, (_, rowid)) in scanned.iter().enumerate() {
                assert_eq!(
                    *rowid,
                    (i + 1) as i64,
                    "entry at index {i} must be rowid {}",
                    i + 1
                );
            }

            pager.commit().await.unwrap();
        });
    }
}
