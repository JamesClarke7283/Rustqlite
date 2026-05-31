//! B-tree cell decoding, including the overflow-threshold arithmetic
//! (<https://www.sqlite.org/fileformat2.html#b_tree_pages>).
//!
//! Cell layouts:
//! * **table leaf**   — varint(payload size), varint(rowid), payload, [overflow page no].
//! * **table interior** — u32(left child page), varint(rowid key). No payload, no overflow.
//! * **index leaf**   — varint(payload size), payload (key), [overflow page no].
//! * **index interior** — u32(left child page), varint(payload size), payload, [overflow].
//!
//! When the payload is too large to fit on the page, a prefix is stored locally and the rest
//! spills to an overflow-page chain; the local amount follows SQLite's exact formula (see
//! [`local_payload_len`]).

use crate::error::{Error, Result};
use crate::format::{read_varint, read_varint_i64};

use super::be_u32;

/// A decoded table-leaf cell. `local_payload` borrows the page bytes; the full record is
/// reassembled by following `overflow_page` (see the pager/cursor).
#[derive(Debug)]
pub struct TableLeafCell<'a> {
    pub payload_size: u64,
    pub rowid: i64,
    pub local_payload: &'a [u8],
    pub overflow_page: Option<u32>,
}

/// A decoded table-interior cell: a left-child pointer and the largest rowid in that subtree.
#[derive(Debug)]
pub struct TableInteriorCell {
    pub left_child: u32,
    pub rowid: i64,
}

/// A decoded index-leaf cell (the payload is the index key record).
#[derive(Debug)]
pub struct IndexLeafCell<'a> {
    pub payload_size: u64,
    pub local_payload: &'a [u8],
    pub overflow_page: Option<u32>,
}

/// A decoded index-interior cell.
#[derive(Debug)]
pub struct IndexInteriorCell<'a> {
    pub left_child: u32,
    pub payload_size: u64,
    pub local_payload: &'a [u8],
    pub overflow_page: Option<u32>,
}

/// Compute how many payload bytes are stored locally on the page, and whether an overflow
/// pointer follows. `max_local` is the page-type-specific threshold `X`; the minimum-local `M`
/// is shared across page types. This is SQLite's exact algorithm.
pub fn local_payload_len(payload: usize, usable: usize, max_local: usize) -> (usize, bool) {
    if payload <= max_local {
        return (payload, false);
    }
    let min_local = ((usable - 12) * 32 / 255) - 23;
    let surplus = min_local + (payload - min_local) % (usable - 4);
    let local = if surplus <= max_local {
        surplus
    } else {
        min_local
    };
    (local, true)
}

/// `X` for a table-leaf page: the largest payload kept entirely on the page.
fn table_leaf_max_local(usable: usize) -> usize {
    usable - 35
}

/// `X` for index pages (leaf and interior).
fn index_max_local(usable: usize) -> usize {
    ((usable - 12) * 64 / 255) - 23
}

fn slice(page: &[u8], start: usize, len: usize) -> Result<&[u8]> {
    page.get(start..start + len)
        .ok_or_else(|| Error::corrupt("cell extends past end of page"))
}

pub fn parse_table_leaf_cell(
    page: &[u8],
    offset: usize,
    usable: usize,
) -> Result<TableLeafCell<'_>> {
    let (payload_size, n1) = read_varint(
        page.get(offset..)
            .ok_or_else(|| Error::corrupt("cell offset"))?,
    )
    .ok_or_else(|| Error::corrupt("table leaf payload-size varint"))?;
    let (rowid, n2) = read_varint_i64(&page[offset + n1..])
        .ok_or_else(|| Error::corrupt("table leaf rowid varint"))?;
    let content = offset + n1 + n2;

    let (local_len, has_overflow) =
        local_payload_len(payload_size as usize, usable, table_leaf_max_local(usable));
    let local_payload = slice(page, content, local_len)?;
    let overflow_page = if has_overflow {
        Some(be_u32(slice(page, content + local_len, 4)?))
    } else {
        None
    };

    Ok(TableLeafCell {
        payload_size,
        rowid,
        local_payload,
        overflow_page,
    })
}

pub fn parse_table_interior_cell(page: &[u8], offset: usize) -> Result<TableInteriorCell> {
    let left_child = be_u32(slice(page, offset, 4)?);
    let (rowid, _) = read_varint_i64(&page[offset + 4..])
        .ok_or_else(|| Error::corrupt("table interior rowid varint"))?;
    Ok(TableInteriorCell { left_child, rowid })
}

pub fn parse_index_leaf_cell(
    page: &[u8],
    offset: usize,
    usable: usize,
) -> Result<IndexLeafCell<'_>> {
    let (payload_size, n1) = read_varint(
        page.get(offset..)
            .ok_or_else(|| Error::corrupt("cell offset"))?,
    )
    .ok_or_else(|| Error::corrupt("index leaf payload-size varint"))?;
    let content = offset + n1;
    let (local_len, has_overflow) =
        local_payload_len(payload_size as usize, usable, index_max_local(usable));
    let local_payload = slice(page, content, local_len)?;
    let overflow_page = if has_overflow {
        Some(be_u32(slice(page, content + local_len, 4)?))
    } else {
        None
    };
    Ok(IndexLeafCell {
        payload_size,
        local_payload,
        overflow_page,
    })
}

pub fn parse_index_interior_cell(
    page: &[u8],
    offset: usize,
    usable: usize,
) -> Result<IndexInteriorCell<'_>> {
    let left_child = be_u32(slice(page, offset, 4)?);
    let (payload_size, n1) = read_varint(&page[offset + 4..])
        .ok_or_else(|| Error::corrupt("index interior payload-size varint"))?;
    let content = offset + 4 + n1;
    let (local_len, has_overflow) =
        local_payload_len(payload_size as usize, usable, index_max_local(usable));
    let local_payload = slice(page, content, local_len)?;
    let overflow_page = if has_overflow {
        Some(be_u32(slice(page, content + local_len, 4)?))
    } else {
        None
    };
    Ok(IndexInteriorCell {
        left_child,
        payload_size,
        local_payload,
        overflow_page,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::write_varint;

    #[test]
    fn small_payloads_have_no_overflow() {
        let usable = 4096;
        let max = table_leaf_max_local(usable);
        assert_eq!(local_payload_len(10, usable, max), (10, false));
        assert_eq!(local_payload_len(max, usable, max), (max, false));
        let (local, overflow) = local_payload_len(max + 1, usable, max);
        assert!(overflow);
        let min_local = ((usable - 12) * 32 / 255) - 23;
        assert!((min_local..=max).contains(&local));
    }

    #[test]
    fn table_leaf_cell_no_overflow() {
        // Build a table-leaf cell: payload of "AB" record bytes, rowid 5.
        let mut page = vec![0u8; 4096];
        let cell_off = 100;
        let mut cell = Vec::new();
        let payload = [0x03u8, 0x01, 0x41]; // bogus mini-record bytes for layout test
        write_varint(payload.len() as u64, &mut cell);
        write_varint(5, &mut cell);
        cell.extend_from_slice(&payload);
        page[cell_off..cell_off + cell.len()].copy_from_slice(&cell);

        let parsed = parse_table_leaf_cell(&page, cell_off, 4096).unwrap();
        assert_eq!(parsed.rowid, 5);
        assert_eq!(parsed.payload_size, 3);
        assert_eq!(parsed.local_payload, &payload);
        assert!(parsed.overflow_page.is_none());
    }

    #[test]
    fn table_interior_cell() {
        let mut page = vec![0u8; 4096];
        let off = 12;
        page[off..off + 4].copy_from_slice(&77u32.to_be_bytes());
        let mut rid = Vec::new();
        write_varint(1234, &mut rid);
        page[off + 4..off + 4 + rid.len()].copy_from_slice(&rid);
        let parsed = parse_table_interior_cell(&page, off).unwrap();
        assert_eq!(parsed.left_child, 77);
        assert_eq!(parsed.rowid, 1234);
    }
}
