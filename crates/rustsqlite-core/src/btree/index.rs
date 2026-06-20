//! Index b-tree creation and full-tree scan (mirrors `sqlite3BtreeCreateTable` for an index
//! and the scan path in `btree.c`).
//!
//! Index b-trees are key-keyed (no rowid in the key storage; the rowid is appended to the key
//! record as a tiebreaker). Their on-disk format mirrors the table b-tree (cells with a
//! varint payload-size, the key record, and an optional overflow pointer), but the cells do
//! not store a rowid varint.

use std::future::Future;
use std::pin::Pin;

use crate::error::{Error, Result};
use crate::pager::Pager;

use super::cell::{
    assemble_index_interior_payload, assemble_index_payload, parse_index_interior_cell,
    parse_index_leaf_cell,
};
use super::page::{PageHeader, PageType};

/// Create a new index b-tree and return its root page number. Allocates a fresh page and
/// initializes it as an empty leaf-index — the analogue of `sqlite3BtreeCreateTable` for the
/// `idxType == SQLITE_IDXTYPE_APPDEF` case. The caller must hold a write transaction; the new
/// page is committed with the rest of the transaction.
pub async fn create_index_btree(pager: &Pager) -> Result<u32> {
    if pager.auto_vacuum() {
        super::create_index_btree_autovac(pager).await
    } else {
        create_index_btree_plain(pager).await
    }
}

/// Non-auto-vacuum index b-tree creation: allocate a fresh page at the end of the file.
async fn create_index_btree_plain(pager: &Pager) -> Result<u32> {
    let pgno = pager.allocate_page();
    let mut buf = pager.read_page_for_write(pgno).await?;
    let base = pager.btree_header_offset(pgno);
    super::page::init_empty_index_leaf(&mut buf, base);
    pager.write_page(pgno, buf)?;
    Ok(pgno)
}

/// Scan an entire index b-tree rooted at `root`, returning every `(key_record, rowid)` pair in
/// ascending key order. The key record is the raw index cell payload (the record header +
/// body); the rowid is the last value in the record. Mirrors [`super::scan_table`] but for
/// index pages.
pub async fn scan_index(pager: &Pager, root: u32) -> Result<Vec<(Vec<u8>, i64)>> {
    let mut out = Vec::new();
    visit(pager, root, &mut out).await?;
    Ok(out)
}

fn visit<'a>(
    pager: &'a Pager,
    pgno: u32,
    out: &'a mut Vec<(Vec<u8>, i64)>,
) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
    Box::pin(async move {
        let page = pager.get_page(pgno).await?;
        let base = pager.btree_header_offset(pgno);
        let hdr = PageHeader::parse(&page, base)?;

        match hdr.page_type {
            PageType::LeafIndex => {
                for i in 0..hdr.num_cells as usize {
                    let cell_off = hdr.cell_pointer(&page, i)?;
                    let cell = parse_index_leaf_cell(&page, cell_off, pager.usable_size())?;
                    let key = assemble_index_payload(pager, &cell).await?;
                    let rowid = key_record_rowid(pager, &key)?;
                    out.push((key, rowid));
                }
            }
            PageType::InteriorIndex => {
                let usable = pager.usable_size();
                for i in 0..hdr.num_cells as usize {
                    let cell_off = hdr.cell_pointer(&page, i)?;
                    let cell = parse_index_interior_cell(&page, cell_off, usable)?;
                    visit(pager, cell.left_child, out).await?;
                    let key = assemble_index_interior_payload(pager, &cell).await?;
                    let rowid = key_record_rowid(pager, &key)?;
                    out.push((key, rowid));
                }
                if let Some(right) = hdr.right_most_pointer {
                    visit(pager, right, out).await?;
                }
            }
            _ => return Err(Error::corrupt("expected an index b-tree page during scan")),
        }
        Ok(())
    })
}

/// Extract the rowid (the last value) from an index key record. The key record is the
/// serialized form `record_header ++ body`, where the body holds the indexed columns followed
/// by the table's rowid as the final value.
pub fn key_record_rowid(pager: &Pager, key_record: &[u8]) -> Result<i64> {
    let values = crate::format::decode_record(key_record, pager.text_encoding())?;
    values
        .last()
        .map(|v| v.as_i64())
        .ok_or_else(|| Error::corrupt("index key record has no rowid field"))
}
