//! The WAL index (the `-shm` shared-memory file) format.
//!
//! <https://www.sqlite.org/fileformat2.html#wal_index_format>
//! <https://www.sqlite.org/walformat.html#shm>
//!
//! The wal-index is a shared-memory structure that lets a reader quickly find, given a page
//! number `P` and a maximum frame index `M`, the index of the last frame in the WAL at or
//! before `M` for page `P` (or `NULL` if none).  It is transient — rebuilt on recovery — and
//! uses *native* byte order (unlike the WAL and database file formats which are big-endian).
//!
//! Layout (the 136-byte header + per-block index/hash tables):
//!
//! ```text
//!   0:  WalIndexHdr (first copy)        48 bytes
//!  48:  WalIndexHdr (second copy)       48 bytes
//!  96:  WalCkptInfo                     40 bytes
//! 136:  first index block:  page-mapping[HASHTABLE_NPAGE_ONE] u32 + hash[HASHTABLE_NSLOT] u16
//!       subsequent blocks: page-mapping[HASHTABLE_NPAGE]     u32 + hash[HASHTABLE_NSLOT] u16
//! ```
//!
//! A block of `SQLITE_SHM_NLOCK = 8` lock bytes begins at byte 120 (inside `WalCkptInfo.aLock`).
//! Upstream's layout comment in `wal.c` is the authoritative reference.

/// The number of lock bytes reserved in the wal-index header (mirrors `SQLITE_SHM_NLOCK`).
pub const SQLITE_SHM_NLOCK: usize = 8;

/// The number of reader read-marks in `WalCkptInfo.aReadMark` (mirrors `WAL_NREADER =
/// SQLITE_SHM_NLOCK - 3`).
pub const WAL_NREADER: usize = SQLITE_SHM_NLOCK - 3; // 5

/// The "read mark not used" sentinel (mirrors `READMARK_NOT_USED`).
pub const READMARK_NOT_USED: u32 = 0xffff_ffff;

/// Lock byte indices (mirrors `wal.c`).
pub const WAL_WRITE_LOCK: usize = 0;
pub const WAL_CKPT_LOCK: usize = 1;
pub const WAL_RECOVER_LOCK: usize = 2;
/// `WAL_READ_LOCK(I) = 3 + I`.
pub const fn wal_read_lock(i: usize) -> usize {
    3 + i
}

/// The size of one `WalIndexHdr` copy (48 bytes — the sum of the struct fields below).
pub const WAL_INDEX_HDR_SIZE: usize = 48;

/// The size of the `WalCkptInfo` struct (40 bytes).
pub const WAL_CKPT_INFO_SIZE: usize = 4 + 4 * WAL_NREADER + SQLITE_SHM_NLOCK + 4 + 4;

/// The total size of the wal-index header (two `WalIndexHdr` copies + `WalCkptInfo`):
/// `48 + 48 + 40 = 136` bytes (mirrors `WALINDEX_HDR_SIZE`).
pub const WAL_INDEX_HEADER_SIZE: usize = 2 * WAL_INDEX_HDR_SIZE + WAL_CKPT_INFO_SIZE;

/// The byte offset of the lock region (120 bytes in — the second `WalIndexHdr`'s end + the
/// `aLock` offset within `WalCkptInfo`).  Mirrors `WALINDEX_LOCK_OFFSET`.  Computed as
/// `2*WAL_INDEX_HDR_SIZE + offsetof(WalCkptInfo, aLock)` where `aLock` follows `nBackfill` (4
/// bytes) and `aReadMark` (4*WAL_NREADER bytes), so its offset within `WalCkptInfo` is
/// `4 + 4*WAL_NREADER = 24`.
pub const WALINDEX_LOCK_OFFSET: usize = 2 * WAL_INDEX_HDR_SIZE + 4 + 4 * WAL_NREADER;

/// Frames per index block (mirrors `HASHTABLE_NPAGE = 4096`).
pub const HASHTABLE_NPAGE: usize = 4096;

/// Frames in the *first* index block (mirrors `HASHTABLE_NPAGE_ONE = 4062`).
pub const HASHTABLE_NPAGE_ONE: usize = 4062;

/// Hash table slots per index block (mirrors `HASHTABLE_NSLOT = 2 * HASHTABLE_NPAGE`).
pub const HASHTABLE_NSLOT: usize = 2 * HASHTABLE_NPAGE;

/// The wal-index header (one copy; the on-disk layout has two copies followed by
/// `WalCkptInfo`).  All fields are native byte order (the wal-index is host-specific, not
/// cross-platform).  The struct mirrors `struct WalIndexHdr` in `wal.c`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalIndexHdr {
    /// Wal-index version.
    pub i_version: u32,
    /// Unused padding field.
    pub unused: u32,
    /// Counter incremented each transaction.
    pub i_change: u32,
    /// 1 when initialized.
    pub is_init: u8,
    /// True if checksums in the WAL are big-endian (matches the WAL header magic's low bit).
    pub big_end_cksum: u8,
    /// Database page size in bytes.  `1` represents a 65536-byte page (mirrors the DB header
    /// convention).  Stored as a `u16` on disk.
    pub sz_page: u16,
    /// Index of the last valid frame in the WAL.
    pub mx_frame: u32,
    /// Size of the database in pages.
    pub n_page: u32,
    /// Checksum of the last frame in the log (the running checksum carried from the WAL).
    pub a_frame_cksum: [u32; 2],
    /// The two salt values copied from the WAL header.
    pub a_salt: [u32; 2],
    /// Checksum over all prior fields (the wal-index's own integrity check, separate from the
    /// WAL's frame checksum).
    pub a_cksum: [u32; 2],
}

impl Default for WalIndexHdr {
    fn default() -> Self {
        WalIndexHdr {
            i_version: 0,
            unused: 0,
            i_change: 0,
            is_init: 0,
            big_end_cksum: 0,
            sz_page: 0,
            mx_frame: 0,
            n_page: 0,
            a_frame_cksum: [0, 0],
            a_salt: [0, 0],
            a_cksum: [0, 0],
        }
    }
}

impl WalIndexHdr {
    /// The on-disk size (48 bytes — the sum of the fields' sizes).
    pub const SIZE: usize = 4 + 4 + 4 + 1 + 1 + 2 + 4 + 4 + 8 + 8 + 8;

    /// Encode the header into a buffer of at least [`Self::SIZE`] bytes using native byte order.
    pub fn encode(&self, out: &mut [u8]) {
        assert!(out.len() >= Self::SIZE);
        let mut o = 0;
        out[o..o + 4].copy_from_slice(&self.i_version.to_ne_bytes()); o += 4;
        out[o..o + 4].copy_from_slice(&self.unused.to_ne_bytes()); o += 4;
        out[o..o + 4].copy_from_slice(&self.i_change.to_ne_bytes()); o += 4;
        out[o] = self.is_init; o += 1;
        out[o] = self.big_end_cksum; o += 1;
        out[o..o + 2].copy_from_slice(&self.sz_page.to_ne_bytes()); o += 2;
        out[o..o + 4].copy_from_slice(&self.mx_frame.to_ne_bytes()); o += 4;
        out[o..o + 4].copy_from_slice(&self.n_page.to_ne_bytes()); o += 4;
        out[o..o + 4].copy_from_slice(&self.a_frame_cksum[0].to_ne_bytes()); o += 4;
        out[o..o + 4].copy_from_slice(&self.a_frame_cksum[1].to_ne_bytes()); o += 4;
        out[o..o + 4].copy_from_slice(&self.a_salt[0].to_ne_bytes()); o += 4;
        out[o..o + 4].copy_from_slice(&self.a_salt[1].to_ne_bytes()); o += 4;
        out[o..o + 4].copy_from_slice(&self.a_cksum[0].to_ne_bytes()); o += 4;
        out[o..o + 4].copy_from_slice(&self.a_cksum[1].to_ne_bytes());
    }

    /// Decode a header from a buffer of at least [`Self::SIZE`] bytes (native byte order).
    pub fn decode(buf: &[u8]) -> crate::error::Result<WalIndexHdr> {
        if buf.len() < Self::SIZE {
            return Err(crate::error::Error::msg("wal-index header too short"));
        }
        let mut o = 0;
        let i_version = u32::from_ne_bytes(buf[o..o + 4].try_into().unwrap()); o += 4;
        let unused = u32::from_ne_bytes(buf[o..o + 4].try_into().unwrap()); o += 4;
        let i_change = u32::from_ne_bytes(buf[o..o + 4].try_into().unwrap()); o += 4;
        let is_init = buf[o]; o += 1;
        let big_end_cksum = buf[o]; o += 1;
        let sz_page = u16::from_ne_bytes(buf[o..o + 2].try_into().unwrap()); o += 2;
        let mx_frame = u32::from_ne_bytes(buf[o..o + 4].try_into().unwrap()); o += 4;
        let n_page = u32::from_ne_bytes(buf[o..o + 4].try_into().unwrap()); o += 4;
        let a_frame_cksum = [
            u32::from_ne_bytes(buf[o..o + 4].try_into().unwrap()),
            u32::from_ne_bytes(buf[o + 4..o + 8].try_into().unwrap()),
        ]; o += 8;
        let a_salt = [
            u32::from_ne_bytes(buf[o..o + 4].try_into().unwrap()),
            u32::from_ne_bytes(buf[o + 4..o + 8].try_into().unwrap()),
        ]; o += 8;
        let a_cksum = [
            u32::from_ne_bytes(buf[o..o + 4].try_into().unwrap()),
            u32::from_ne_bytes(buf[o + 4..o + 8].try_into().unwrap()),
        ];
        Ok(WalIndexHdr {
            i_version,
            unused,
            i_change,
            is_init,
            big_end_cksum,
            sz_page,
            mx_frame,
            n_page,
            a_frame_cksum,
            a_salt,
            a_cksum,
        })
    }
}

/// The checkpoint-info record that follows the two `WalIndexHdr` copies (40 bytes).  Mirrors
/// `struct WalCkptInfo` in `wal.c`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalCkptInfo {
    /// Number of WAL frames backfilled into the database.
    pub n_backfill: u32,
    /// Reader read-marks (one entry per reader lock; `aReadMark[0]` is a placeholder).
    pub a_read_mark: [u32; WAL_NREADER],
    /// Reserved lock bytes (never read or written bytewise; the locking primitives touch
    /// these offsets directly).
    pub a_lock: [u8; SQLITE_SHM_NLOCK],
    /// Largest value of `n_backfill` a checkpoint has attempted (>= `n_backfill`).
    pub n_backfill_attempted: u32,
    /// Available for future enhancements.
    pub not_used_0: u32,
}

impl Default for WalCkptInfo {
    fn default() -> Self {
        WalCkptInfo {
            n_backfill: 0,
            a_read_mark: [READMARK_NOT_USED; WAL_NREADER],
            a_lock: [0; SQLITE_SHM_NLOCK],
            n_backfill_attempted: 0,
            not_used_0: 0,
        }
    }
}

impl WalCkptInfo {
    /// The on-disk size (40 bytes).
    pub const SIZE: usize = 4 + 4 * WAL_NREADER + SQLITE_SHM_NLOCK + 4 + 4;

    /// Encode the record into a buffer of at least [`Self::SIZE`] bytes (native byte order).
    pub fn encode(&self, out: &mut [u8]) {
        assert!(out.len() >= Self::SIZE);
        let mut o = 0;
        out[o..o + 4].copy_from_slice(&self.n_backfill.to_ne_bytes()); o += 4;
        for i in 0..WAL_NREADER {
            out[o..o + 4].copy_from_slice(&self.a_read_mark[i].to_ne_bytes());
            o += 4;
        }
        out[o..o + SQLITE_SHM_NLOCK].copy_from_slice(&self.a_lock); o += SQLITE_SHM_NLOCK;
        out[o..o + 4].copy_from_slice(&self.n_backfill_attempted.to_ne_bytes()); o += 4;
        out[o..o + 4].copy_from_slice(&self.not_used_0.to_ne_bytes());
    }

    /// Decode a record from a buffer of at least [`Self::SIZE`] bytes (native byte order).
    pub fn decode(buf: &[u8]) -> crate::error::Result<WalCkptInfo> {
        if buf.len() < Self::SIZE {
            return Err(crate::error::Error::msg("wal-index ckpt-info too short"));
        }
        let mut o = 0;
        let n_backfill = u32::from_ne_bytes(buf[o..o + 4].try_into().unwrap()); o += 4;
        let mut a_read_mark = [0u32; WAL_NREADER];
        for i in 0..WAL_NREADER {
            a_read_mark[i] = u32::from_ne_bytes(buf[o..o + 4].try_into().unwrap());
            o += 4;
        }
        let mut a_lock = [0u8; SQLITE_SHM_NLOCK];
        a_lock.copy_from_slice(&buf[o..o + SQLITE_SHM_NLOCK]);
        o += SQLITE_SHM_NLOCK;
        let n_backfill_attempted = u32::from_ne_bytes(buf[o..o + 4].try_into().unwrap()); o += 4;
        let not_used_0 = u32::from_ne_bytes(buf[o..o + 4].try_into().unwrap());
        Ok(WalCkptInfo {
            n_backfill,
            a_read_mark,
            a_lock,
            n_backfill_attempted,
            not_used_0,
        })
    }
}

/// Compute the hash-table slot index for page `P` in an index block (mirrors
/// `iKey = (P * 383) % HASHTABLE_NSLOT`).
pub fn wal_hash_key(page: u32) -> usize {
    (page.wrapping_mul(383) as usize) % HASHTABLE_NSLOT
}

/// The byte offset of frame `i_frame` (1-based) in the WAL file for a database with the given
/// page size (mirrors `walFrameOffset`).
pub fn wal_frame_offset(i_frame: u32, page_size: u32) -> i64 {
    // WAL_HDRSIZE + (iFrame-1) * (szPage + WAL_FRAME_HDRSIZE)
    crate::format::wal::WAL_HEADER_SIZE as i64
        + ((i_frame as i64) - 1) * (page_size as i64 + crate::format::wal::WAL_FRAME_HEADER_SIZE as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wal_index_header_round_trips() {
        let h = WalIndexHdr {
            i_version: 3007000,
            unused: 0,
            i_change: 42,
            is_init: 1,
            big_end_cksum: 1,
            sz_page: 4096,
            mx_frame: 17,
            n_page: 8,
            a_frame_cksum: [0x1234_5678, 0x9abc_def0],
            a_salt: [0x0102_0304, 0x0506_0708],
            a_cksum: [0x1111_1111, 0x2222_2222],
        };
        let mut buf = vec![0u8; WalIndexHdr::SIZE];
        h.encode(&mut buf);
        let back = WalIndexHdr::decode(&buf).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn wal_ckpt_info_round_trips() {
        let info = WalCkptInfo {
            n_backfill: 7,
            a_read_mark: [10, 20, 30, READMARK_NOT_USED, READMARK_NOT_USED],
            a_lock: [0; SQLITE_SHM_NLOCK],
            n_backfill_attempted: 12,
            not_used_0: 0,
        };
        let mut buf = vec![0u8; WalCkptInfo::SIZE];
        info.encode(&mut buf);
        let back = WalCkptInfo::decode(&buf).unwrap();
        assert_eq!(back, info);
    }

    #[test]
    fn wal_index_layout_constants_match_upstream() {
        // The headline layout from wal.c's schematic comment:
        //   0..48:   first  WalIndexHdr
        //  48..96:   second WalIndexHdr
        //  96..136:  WalCkptInfo
        assert_eq!(WAL_INDEX_HDR_SIZE, 48);
        assert_eq!(WAL_CKPT_INFO_SIZE, 40);
        assert_eq!(WAL_INDEX_HEADER_SIZE, 136);
        // Lock bytes start at 120 (the `aLock` field of `WalCkptInfo`).
        assert_eq!(WALINDEX_LOCK_OFFSET, 120);
        // 8 lock bytes, 5 readers.
        assert_eq!(SQLITE_SHM_NLOCK, 8);
        assert_eq!(WAL_NREADER, 5);
        // HASHTABLE constants.
        assert_eq!(HASHTABLE_NPAGE, 4096);
        assert_eq!(HASHTABLE_NPAGE_ONE, 4062);
        assert_eq!(HASHTABLE_NSLOT, 8192);
    }

    #[test]
    fn wal_hash_key_is_page_times_383_mod_nslot() {
        // (P * 383) % HASHTABLE_NSLOT
        assert_eq!(wal_hash_key(1), 383);
        assert_eq!(wal_hash_key(2), 766);
        assert_eq!(wal_hash_key(4096), (4096 * 383) % HASHTABLE_NSLOT);
        // Page 0 hashes to 0 (the slot is unused in practice — page numbers are 1-based).
        assert_eq!(wal_hash_key(0), 0);
    }

    #[test]
    fn wal_frame_offset_first_frame_is_after_header() {
        // Frame 1 starts at offset 32 (WAL_HDRSIZE), regardless of page size.
        assert_eq!(wal_frame_offset(1, 4096), 32);
        // Frame 2 starts at 32 + 4096 + 24 = 4152.
        assert_eq!(wal_frame_offset(2, 4096), 32 + 4096 + 24);
        // Frame N+1 = 32 + N * (page + frame_hdr).
        assert_eq!(wal_frame_offset(10, 1024), 32 + 9 * (1024 + 24));
    }
}