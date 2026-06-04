//! Single-leaf index insertion (mirrors `sqlite3BtreeInsert` for index b-trees).
//!
//! The first M5.1 slice accepts inserts only when the destination leaf has room. If the leaf
//! fills, the function returns the same `page_full_error()` that the table-insert path
//! produces — the index page-split path (`balance_shallow` for indexes, analogous to
//! `balance::split_leaf` for tables) is a follow-up slice. The differential tests in
//! `tests/diff.rs` and the `slt_lang_dropindex` evidence file are sized so a single-leaf
//! index comfortably holds the fixture rows.
//!
//! The `key_record` is the index columns followed by the table's rowid, all encoded by
//! [`crate::format::encode_record`]. The function finds the insertion point via a binary
//! search of the leaf's cell prefixes (the indexed columns, ignoring the trailing rowid for
//! ordering), then delegates to [`super::page::insert_leaf_cell`] for the byte-level
//! insertion.

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::format::decode_record;
use crate::pager::Pager;
use crate::types::{Collation, Value};
use crate::vdbe::compare::mem_compare;

use super::cell::{build_index_leaf_cell, parse_index_leaf_cell};
use super::page::{insert_leaf_cell, PageHeader, PageType};

/// Insert a new index entry into the b-tree rooted at `root`. `key_record` is the encoded
/// record (`[indexed columns..., rowid]`). Returns `Ok(())` on success or `Err(page_full_error())`
/// when the leaf has no room.
pub async fn index_insert(
    pager: &Arc<Pager>,
    root: u32,
    key_record: &[u8],
) -> Result<()> {
    let usable = pager.usable_size();
    let base = pager.btree_header_offset(root);
    let page = pager.get_page(root).await?;
    let hdr = PageHeader::parse(&page, base)?;
    if hdr.page_type != PageType::LeafIndex {
        return Err(Error::corrupt("index_insert: not a leaf-index page"));
    }
    let n = hdr.num_cells as usize;

    // Find the insertion point. The order is by the prefix of `key_record`'s values (all but
    // the trailing rowid). Binary search for the lower bound; ties on the prefix are broken
    // by the rowid (the rowid is also a Value in the decoded record).
    let encoding = pager.text_encoding();
    let search_values = decode_record(key_record, encoding)?;
    let search_prefix_len = search_values.len().saturating_sub(1);
    let search_prefix = &search_values[..search_prefix_len];
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = (lo + hi) / 2;
        let off = hdr.cell_pointer(&page, mid)?;
        let cell = parse_index_leaf_cell(&page, off, usable)?;
        let existing = decode_record(cell.local_payload, encoding)?;
        let existing_prefix_len = existing.len().saturating_sub(1);
        let existing_prefix = &existing[..existing_prefix_len];
        let cmp = compare_record_prefixes(
            existing_prefix,
            &existing[existing_prefix_len],
            search_prefix,
            &search_values[search_prefix_len],
            Collation::Binary,
        );
        if cmp == std::cmp::Ordering::Less {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let idx = lo;

    let cell = build_index_leaf_cell(pager, key_record, usable);
    let mut leaf = pager.read_page_for_write(root).await?;
    if let Err(e) = insert_leaf_cell(&mut leaf, base, idx, &cell) {
        // Page full: drop the partial copy and surface the same error the table path does.
        drop(leaf);
        return Err(e);
    }
    pager.write_page(root, leaf)?;
    Ok(())
}

/// A variant used by the (post-root-promotion) re-insert path: it skips the descendent search
/// (the cell goes onto a known leaf) and just inserts at `idx`. Not used by the M5.1 first
/// slice (we don't promote index roots) but kept here for parity with the table-side
/// `insert_after_root_promotion` helper.
#[allow(dead_code)]
pub async fn index_insert_after_root_promotion(
    pager: &Arc<Pager>,
    leaf_pgno: u32,
    idx: usize,
    key_record: &[u8],
) -> Result<()> {
    let usable = pager.usable_size();
    let base = pager.btree_header_offset(leaf_pgno);
    let cell = build_index_leaf_cell(pager, key_record, usable);
    let mut leaf = pager.read_page_for_write(leaf_pgno).await?;
    if let Err(e) = insert_leaf_cell(&mut leaf, base, idx, &cell) {
        drop(leaf);
        return Err(e);
    }
    pager.write_page(leaf_pgno, leaf)?;
    Ok(())
}

fn compare_record_prefixes(
    a_prefix: &[Value],
    a_rowid: &Value,
    b_prefix: &[Value],
    b_rowid: &Value,
    coll: Collation,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let n = a_prefix.len().min(b_prefix.len());
    for i in 0..n {
        match mem_compare(&a_prefix[i], &b_prefix[i], coll) {
            Ordering::Equal => {}
            non_eq => return non_eq,
        }
    }
    match a_prefix.len().cmp(&b_prefix.len()) {
        Ordering::Equal => mem_compare(a_rowid, b_rowid, coll),
        other => other,
    }
}
