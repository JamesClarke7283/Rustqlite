//! Rollback journal (mirrors the rollback-journal half of `pager.c`).
//!
//! The rollback journal is the `-journal` sidecar file that makes a write transaction atomic and
//! crash-safe under the default `journal_mode=DELETE`. Before a page in the database is modified
//! for the first time in a transaction, its **pre-image** (the bytes as they were when the
//! transaction began) is appended to the journal. To commit, SQLite syncs the journal, writes the
//! new page contents into the database, syncs the database, then deletes the journal — the delete
//! is the atomic commit point. To roll back (explicitly, or on the next open if a crash left a
//! *hot* journal), it copies the pre-images back over the database and discards the journal.
//!
//! On-disk layout (big-endian, all offsets within the first sector):
//!
//! ```text
//! journal header (the first `sector_size` bytes, zero-padded):
//!   [0..8]    magic   = d9 d5 05 f9 20 a1 63 d7
//!   [8..12]   nRec    = number of page records that follow (patched in at commit)
//!   [12..16]  cksumInit = quasi-random seed added into every page checksum
//!   [16..20]  nDatabase = database size in pages before the transaction (dbOrigSize)
//!   [20..24]  sectorSize = 512
//!   [24..28]  pageSize
//!   [28..sector_size] unused (zero)
//!
//! then `nRec` page records, each:
//!   [0..4]              pgno  (1-based page number)
//!   [4..4+pageSize]     the page's pre-image bytes
//!   [4+pageSize..+4]    checksum = pager_cksum(pre-image)
//! ```
//!
//! This is a faithful port of `writeJournalHdr`, `pager_cksum`, and the record format read by
//! `pager_playback_one_page` in `pager.c`.

/// The 8-byte journal magic (`aJournalMagic` in `pager.c`).
pub const JOURNAL_MAGIC: [u8; 8] = [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];

/// The sector size assumed for rollback. SQLite rounds the journal header up to a sector; 512 is
/// the conventional value it writes (`pPager->sectorSize`).
pub const SECTOR_SIZE: u32 = 512;

/// The journal header occupies the whole first sector (`JOURNAL_HDR_SZ`).
pub const JOURNAL_HDR_SZ: usize = SECTOR_SIZE as usize;

/// Number of meaningful bytes in the journal header (the rest of the sector is zero padding).
const HEADER_FIELDS_LEN: usize = 28;

/// `pager_cksum`: the page checksum used in journal records. Starts from `cksum_init` and adds
/// every 200th byte of the page, walking *down* from `pageSize - 200` while the index stays
/// positive. The sparse sampling is exactly upstream's (it is a corruption sanity check, not a
/// cryptographic digest), and the additions wrap on overflow.
pub fn pager_cksum(cksum_init: u32, page: &[u8]) -> u32 {
    let mut cksum = cksum_init;
    let mut i = page.len() as isize - 200;
    while i > 0 {
        cksum = cksum.wrapping_add(u32::from(page[i as usize]));
        i -= 200;
    }
    cksum
}

/// Build the journal header sector (`writeJournalHdr`). `nrec` is the record count (we write the
/// final value rather than the `0xffffffff` streaming sentinel and re-patch it at commit). The
/// returned buffer is exactly [`JOURNAL_HDR_SZ`] bytes, zero-padded past the 28 header fields.
pub fn build_header(nrec: u32, cksum_init: u32, db_orig_size: u32, page_size: u32) -> Vec<u8> {
    let mut h = vec![0u8; JOURNAL_HDR_SZ];
    h[0..8].copy_from_slice(&JOURNAL_MAGIC);
    h[8..12].copy_from_slice(&nrec.to_be_bytes());
    h[12..16].copy_from_slice(&cksum_init.to_be_bytes());
    h[16..20].copy_from_slice(&db_orig_size.to_be_bytes());
    h[20..24].copy_from_slice(&SECTOR_SIZE.to_be_bytes());
    h[24..28].copy_from_slice(&page_size.to_be_bytes());
    h
}

/// Build one page record (`pgno`, the page pre-image, and its checksum), ready to append to the
/// journal. `page` must be exactly `page_size` bytes.
pub fn build_record(pgno: u32, page: &[u8], cksum_init: u32) -> Vec<u8> {
    let mut rec = Vec::with_capacity(8 + page.len());
    rec.extend_from_slice(&pgno.to_be_bytes());
    rec.extend_from_slice(page);
    rec.extend_from_slice(&pager_cksum(cksum_init, page).to_be_bytes());
    rec
}

/// The size in bytes of one journal page record for a given page size (`pgno` + page + checksum).
pub fn record_len(page_size: usize) -> usize {
    4 + page_size + 4
}

/// A parsed journal header.
#[derive(Clone, Copy, Debug)]
pub struct JournalHeader {
    pub nrec: u32,
    pub cksum_init: u32,
    pub db_orig_size: u32,
    pub sector_size: u32,
    pub page_size: u32,
}

/// Parse a journal header from the start of `bytes`. Returns `None` if the magic does not match or
/// the buffer is too short — i.e. there is no valid journal here.
pub fn parse_header(bytes: &[u8]) -> Option<JournalHeader> {
    if bytes.len() < HEADER_FIELDS_LEN || bytes[0..8] != JOURNAL_MAGIC {
        return None;
    }
    Some(JournalHeader {
        nrec: be32(&bytes[8..12]),
        cksum_init: be32(&bytes[12..16]),
        db_orig_size: be32(&bytes[16..20]),
        sector_size: be32(&bytes[20..24]),
        page_size: be32(&bytes[24..28]),
    })
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cksum_is_seed_plus_sparse_sample() {
        // For a 4096-byte page, the sampled indices are 3896, 3696, ..., 96 (every 200th, down
        // from 4096-200, while > 0). An all-zero page checksums to just the seed.
        let zero = vec![0u8; 4096];
        assert_eq!(pager_cksum(0x1234_5678, &zero), 0x1234_5678);

        // Set one sampled byte and one un-sampled byte; only the sampled one moves the checksum.
        let mut page = vec![0u8; 4096];
        page[96] = 5; // 96 = 4096 - 200*20, a sampled index
        page[97] = 9; // not sampled
        assert_eq!(pager_cksum(0, &page), 5);

        // Wrapping add on overflow (faithful to the C u32 arithmetic).
        let mut page = vec![0u8; 4096];
        page[96] = 1;
        assert_eq!(pager_cksum(u32::MAX, &page), 0);
    }

    #[test]
    fn header_roundtrip() {
        let h = build_header(7, 0xdead_beef, 3, 4096);
        assert_eq!(h.len(), JOURNAL_HDR_SZ);
        assert_eq!(&h[0..8], &JOURNAL_MAGIC);
        let parsed = parse_header(&h).expect("valid header");
        assert_eq!(parsed.nrec, 7);
        assert_eq!(parsed.cksum_init, 0xdead_beef);
        assert_eq!(parsed.db_orig_size, 3);
        assert_eq!(parsed.sector_size, 512);
        assert_eq!(parsed.page_size, 4096);
        // The padding past the fields is zero.
        assert!(h[28..].iter().all(|&b| b == 0));
    }

    #[test]
    fn parse_rejects_bad_magic() {
        let mut h = build_header(1, 0, 1, 4096);
        h[0] = 0;
        assert!(parse_header(&h).is_none());
        assert!(parse_header(&[]).is_none());
    }

    #[test]
    fn record_layout() {
        let mut page = vec![0u8; 4096];
        page[96] = 3; // a sampled byte, so the checksum is the seed + 3
        let rec = build_record(5, &page, 10);
        assert_eq!(rec.len(), record_len(4096));
        assert_eq!(&rec[0..4], &5u32.to_be_bytes()); // pgno
        assert_eq!(&rec[4..4 + 4096], &page[..]); // pre-image
        let cksum = be32(&rec[4 + 4096..]);
        assert_eq!(cksum, 13); // 10 + 3
    }
}
