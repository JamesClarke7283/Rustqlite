//! Table b-tree insertion and rowid allocation (the write-path counterpart of [`super::cursor`],
//! mirroring `sqlite3BtreeInsert` / `OP_NewRowid` in `btree.c`/`vdbe.c`).
//!
//! First slice (M4.2): insertion is **single-leaf only** — the root must be (or still be) a leaf
//! page and the new cell must fit, otherwise [`page::page_full_error`] is returned. Page splitting
//! (`balance_nonroot`) and overflow-page payloads land in later phases; until then the writer only
//! stores small rows on a fresh tree, which is enough for the first end-to-end CREATE/INSERT slice.

use crate::error::{Error, Result};
use crate::pager::Pager;

use super::cell::{build_table_leaf_cell, table_leaf_cell_rowid};
use super::page::{self, PageHeader, PageType};

/// Insert `(rowid, payload)` into the table b-tree rooted at `root`, keeping the leaf's cells in
/// ascending rowid order. The caller must already hold a write transaction (so the modified page is
/// journaled). First-slice constraint: `root` must be a leaf page; a non-leaf root means the tree
/// has grown past one page, which needs the balancing not yet implemented.
pub async fn table_insert(pager: &Pager, root: u32, rowid: i64, payload: &[u8]) -> Result<()> {
    let cell = build_table_leaf_cell(rowid, payload);

    let mut buf = pager.read_page_for_write(root).await?;
    let base = pager.btree_header_offset(root);
    let hdr = PageHeader::parse(&buf, base)?;
    if hdr.page_type != PageType::LeafTable {
        return Err(Error::msg(
            "b-tree insert into a multi-page table is not supported yet (balancing pending)",
        ));
    }

    let idx = leaf_insert_index(&buf, &hdr, rowid)?;
    page::insert_leaf_cell(&mut buf, base, idx, &cell)?;
    pager.write_page(root, buf)?;
    Ok(())
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

            // Create a table b-tree and insert rows OUT of rowid order to exercise the sorted
            // insert position, all inside one write transaction.
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
            // max_rowid reflects in-transaction inserts (reads through the dirty overlay).
            assert_eq!(max_rowid(&pager, root).await.unwrap(), 5);
            pager.commit().await.unwrap();

            // Reopen and scan: rows come back in ascending rowid order with their values intact.
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
    fn full_leaf_reports_page_full() {
        rt().block_on(async {
            let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
            let file = vfs
                .open("full.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            // A tiny page makes the single leaf fill quickly, so we can observe the
            // not-yet-implemented split surfacing as an error (rather than corrupting the page).
            let pager = Pager::create_fresh(vfs.clone(), "full.db".into(), file, 512)
                .await
                .unwrap();
            pager.begin_write().await.unwrap();
            let root = create_table_btree(&pager).await.unwrap();

            let big = encode_record(&[Value::Blob(vec![0u8; 200])]);
            let mut inserted = 0;
            let mut hit_full = false;
            for rowid in 1..100 {
                match table_insert(&pager, root, rowid, &big).await {
                    Ok(()) => inserted += 1,
                    Err(e) => {
                        assert!(e.message.contains("page is full"), "unexpected error: {e}");
                        hit_full = true;
                        break;
                    }
                }
            }
            assert!(hit_full, "expected the leaf to fill on a 512-byte page");
            assert!(inserted >= 1, "should fit at least one 200-byte row");
            // Roll back so we leave no half-built tree behind.
            pager.rollback().await.unwrap();
        });
    }
}
