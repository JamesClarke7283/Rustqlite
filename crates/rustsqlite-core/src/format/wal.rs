//! The write-ahead log (WAL) file format — the `-wal` sidecar header and frame codec.
//!
//! <https://www.sqlite.org/fileformat2.html#the_write_ahead_log>
//!
//! A WAL file begins with a 32-byte header followed by zero or more *frames*. Each frame is a
//! 24-byte frame header followed by `<page-size>` bytes of page data. The frame header carries
//! the page number, the commit size (non-zero on the last frame of a transaction), the salts
//! (copied from the WAL header), and a running checksum.
//!
//! The WAL header and the frame headers store multi-byte integers in **big-endian** byte order.
//! The checksum is computed over 32-bit words; the endianness of those words is big-endian when
//! the magic number is `0x377f0683` and little-endian when it is `0x377f0682` (the two magics
//! differ only in the least-significant bit, which selects the checksum byte order). The
//! checksum values themselves are always stored in big-endian in the header.
//!
//! This module is the byte-faithful codec for the header and frame headers. The pager-side
//! read/write paths (M13.4/M13.5) and the shared-memory WAL index (M13.3) live elsewhere.

/// The WAL header magic numbers. The low bit selects the checksum byte order: `0x377f0683`
/// (big-endian checksum) is the default written by upstream; `0x377f0682` (little-endian
/// checksum) is the alternative. Both are recognized on read.
pub const WAL_MAGIC_BE: u32 = 0x377f0683;
pub const WAL_MAGIC_LE: u32 = 0x377f0682;

/// The WAL file format version. Upstream writes `3007000`.
pub const WAL_FORMAT_VERSION: u32 = 3007000;

/// The size of the WAL header in bytes.
pub const WAL_HEADER_SIZE: usize = 32;

/// The size of a WAL frame header in bytes.
pub const WAL_FRAME_HEADER_SIZE: usize = 24;

/// A parsed WAL header (the first 32 bytes of the `-wal` file).
///
/// All eight fields are big-endian `u32` values on disk. The checksum (bytes 24–31) covers the
/// first 24 bytes of the header and is computed with the byte order selected by the magic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalHeader {
    /// Magic number. `0x377f0683` (big-endian checksum) or `0x377f0682` (little-endian).
    pub magic: u32,
    /// File format version. Currently `3007000`.
    pub format_version: u32,
    /// Database page size in bytes (a power of two, 512..=65536; the on-disk u16 `1` means
    /// 65536, but the WAL header stores the full `u32`).
    pub page_size: u32,
    /// Checkpoint sequence number, incremented with each checkpoint.
    pub checkpoint_seq: u32,
    /// Salt-1: a random integer incremented with each checkpoint.
    pub salt1: u32,
    /// Salt-2: a different random integer changing with each checkpoint.
    pub salt2: u32,
    /// Checksum-1: first half of the running checksum over the first 24 bytes of the header.
    pub checksum1: u32,
    /// Checksum-2: second half of the running checksum.
    pub checksum2: u32,
}

impl WalHeader {
    /// Decode a 32-byte WAL header. Returns the parsed header; the caller validates the
    /// checksum separately (see [`wal_checksum`]) so a header with a mismatched checksum can
    /// still be reported with a precise error rather than a generic decode failure.
    pub fn decode(buf: &[u8]) -> crate::error::Result<WalHeader> {
        if buf.len() < WAL_HEADER_SIZE {
            return Err(crate::error::Error::msg("WAL header is too short"));
        }
        let magic = u32::from_be_bytes(buf[0..4].try_into().unwrap());
        let format_version = u32::from_be_bytes(buf[4..8].try_into().unwrap());
        let page_size = u32::from_be_bytes(buf[8..12].try_into().unwrap());
        let checkpoint_seq = u32::from_be_bytes(buf[12..16].try_into().unwrap());
        let salt1 = u32::from_be_bytes(buf[16..20].try_into().unwrap());
        let salt2 = u32::from_be_bytes(buf[20..24].try_into().unwrap());
        let checksum1 = u32::from_be_bytes(buf[24..28].try_into().unwrap());
        let checksum2 = u32::from_be_bytes(buf[28..32].try_into().unwrap());
        Ok(WalHeader {
            magic,
            format_version,
            page_size,
            checkpoint_seq,
            salt1,
            salt2,
            checksum1,
            checksum2,
        })
    }

    /// Encode the header into a 32-byte buffer (big-endian). The caller must compute the
    /// checksum (`checksum1`/`checksum2`) before calling this — the encoded bytes are exactly
    /// the eight `u32` fields in order.
    pub fn encode(&self, out: &mut [u8]) {
        assert!(out.len() >= WAL_HEADER_SIZE);
        out[0..4].copy_from_slice(&self.magic.to_be_bytes());
        out[4..8].copy_from_slice(&self.format_version.to_be_bytes());
        out[8..12].copy_from_slice(&self.page_size.to_be_bytes());
        out[12..16].copy_from_slice(&self.checkpoint_seq.to_be_bytes());
        out[16..20].copy_from_slice(&self.salt1.to_be_bytes());
        out[20..24].copy_from_slice(&self.salt2.to_be_bytes());
        out[24..28].copy_from_slice(&self.checksum1.to_be_bytes());
        out[28..32].copy_from_slice(&self.checksum2.to_be_bytes());
    }

    /// Whether the checksum is computed over big-endian (`true`, magic `0x377f0683`) or
    /// little-endian (`false`, magic `0x377f0682`) 32-bit words.
    pub fn checksum_big_endian(&self) -> bool {
        self.magic == WAL_MAGIC_BE
    }
}

/// A parsed WAL frame header (the 24 bytes preceding a page's data).
///
/// A frame is *valid* when (1) its salts match the WAL header's salts and (2) the running
/// checksum computed over the WAL header + the first 8 bytes of every frame header + the
/// page data, up to and including this frame, matches the frame's `checksum1`/`checksum2`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalFrameHeader {
    /// The page number this frame's data is for.
    pub page_number: u32,
    /// For the last frame of a transaction (a "commit frame"), the size of the database image
    /// in pages after the commit. Zero for all non-commit frames.
    pub commit_size: u32,
    /// Salt-1 (copied from the WAL header).
    pub salt1: u32,
    /// Salt-2 (copied from the WAL header).
    pub salt2: u32,
    /// Checksum-1 (the running checksum's first half after this frame).
    pub checksum1: u32,
    /// Checksum-2 (the running checksum's second half after this frame).
    pub checksum2: u32,
}

impl WalFrameHeader {
    /// Decode a 24-byte frame header.
    pub fn decode(buf: &[u8]) -> crate::error::Result<WalFrameHeader> {
        if buf.len() < WAL_FRAME_HEADER_SIZE {
            return Err(crate::error::Error::msg("WAL frame header is too short"));
        }
        let page_number = u32::from_be_bytes(buf[0..4].try_into().unwrap());
        let commit_size = u32::from_be_bytes(buf[4..8].try_into().unwrap());
        let salt1 = u32::from_be_bytes(buf[8..12].try_into().unwrap());
        let salt2 = u32::from_be_bytes(buf[12..16].try_into().unwrap());
        let checksum1 = u32::from_be_bytes(buf[16..20].try_into().unwrap());
        let checksum2 = u32::from_be_bytes(buf[20..24].try_into().unwrap());
        Ok(WalFrameHeader {
            page_number,
            commit_size,
            salt1,
            salt2,
            checksum1,
            checksum2,
        })
    }

    /// Encode the frame header into a 24-byte buffer (big-endian).
    pub fn encode(&self, out: &mut [u8]) {
        assert!(out.len() >= WAL_FRAME_HEADER_SIZE);
        out[0..4].copy_from_slice(&self.page_number.to_be_bytes());
        out[4..8].copy_from_slice(&self.commit_size.to_be_bytes());
        out[8..12].copy_from_slice(&self.salt1.to_be_bytes());
        out[12..16].copy_from_slice(&self.salt2.to_be_bytes());
        out[16..20].copy_from_slice(&self.checksum1.to_be_bytes());
        out[20..24].copy_from_slice(&self.checksum2.to_be_bytes());
    }
}

/// The running WAL checksum algorithm (mirrors `walChecksumBytes` in `wal.c`).
///
/// The input is interpreted as an even number of `u32` words. `big_endian` selects the word
/// byte order (`true` for magic `0x377f0683`, `false` for `0x377f0682`). The algorithm is:
///
/// ```text
/// for i from 0 to n-1 step 2:
///   s0 += x[i] + s1;
///   s1 += x[i+1] + s0;
/// ```
///
/// The caller passes the running `(s0, s1)` from the previous chunk (the WAL header's first 24
/// bytes seed the checksum; each frame extends it over the first 8 bytes of its header + the
/// page data). The result is the updated `(s0, s1)`.
pub fn wal_checksum(
    buf: &[u8],
    big_endian: bool,
    s0_in: u32,
    s1_in: u32,
) -> (u32, u32) {
    // The input length must be a multiple of 8 bytes (an even number of u32 words). Upstream
    // silently drops trailing bytes that don't fit a full word pair; we assert so a bad caller
    // is caught early.
    assert!(buf.len() % 8 == 0, "WAL checksum input must be a multiple of 8 bytes");
    let mut s0 = s0_in;
    let mut s1 = s1_in;
    let n = buf.len() / 4;
    for i in (0..n).step_by(2) {
        let x0 = read_u32(&buf[i * 4..i * 4 + 4], big_endian);
        let x1 = read_u32(&buf[i * 4 + 4..i * 4 + 8], big_endian);
        s0 = s0.wr_add(x0).wr_add(s1);
        s1 = s1.wr_add(x1).wr_add(s0);
    }
    (s0, s1)
}

/// Read a `u32` in the given endianness (the WAL checksum's word byte order).
fn read_u32(b: &[u8], big_endian: bool) -> u32 {
    if big_endian {
        u32::from_be_bytes(b.try_into().unwrap())
    } else {
        u32::from_le_bytes(b.try_into().unwrap())
    }
}

trait WrAdd {
    fn wr_add(self, rhs: u32) -> u32;
}

impl WrAdd for u32 {
    fn wr_add(self, rhs: u32) -> u32 {
        self.wrapping_add(rhs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 32-byte WAL header buffer from the field values, computing the checksum over
    /// the first 24 bytes.
    fn build_header(magic: u32, page_size: u32, checkpoint_seq: u32, salt1: u32, salt2: u32) -> [u8; 32] {
        let mut buf = [0u8; 32];
        buf[0..4].copy_from_slice(&magic.to_be_bytes());
        buf[4..8].copy_from_slice(&WAL_FORMAT_VERSION.to_be_bytes());
        buf[8..12].copy_from_slice(&page_size.to_be_bytes());
        buf[12..16].copy_from_slice(&checkpoint_seq.to_be_bytes());
        buf[16..20].copy_from_slice(&salt1.to_be_bytes());
        buf[20..24].copy_from_slice(&salt2.to_be_bytes());
        let big = magic == WAL_MAGIC_BE;
        let (c0, c1) = wal_checksum(&buf[0..24], big, 0, 0);
        buf[24..28].copy_from_slice(&c0.to_be_bytes());
        buf[28..32].copy_from_slice(&c1.to_be_bytes());
        buf
    }

    #[test]
    fn wal_header_round_trips() {
        let buf = build_header(WAL_MAGIC_BE, 4096, 7, 0x11223344, 0x55667788);
        let h = WalHeader::decode(&buf).unwrap();
        assert_eq!(h.magic, WAL_MAGIC_BE);
        assert_eq!(h.format_version, WAL_FORMAT_VERSION);
        assert_eq!(h.page_size, 4096);
        assert_eq!(h.checkpoint_seq, 7);
        assert_eq!(h.salt1, 0x11223344);
        assert_eq!(h.salt2, 0x55667788);
        // The checksum must verify: recompute over the first 24 bytes and compare.
        let (c0, c1) = wal_checksum(&buf[0..24], true, 0, 0);
        assert_eq!(c0, h.checksum1);
        assert_eq!(c1, h.checksum2);
        // Re-encoding the parsed header must produce the same bytes.
        let mut reencoded = [0u8; 32];
        h.encode(&mut reencoded);
        assert_eq!(&reencoded[..], &buf[..]);
    }

    #[test]
    fn wal_header_le_magic_selects_le_checksum() {
        let buf = build_header(WAL_MAGIC_LE, 1024, 0, 1, 2);
        let h = WalHeader::decode(&buf).unwrap();
        assert!(!h.checksum_big_endian());
        // The LE checksum must verify under little-endian word order.
        let (c0, c1) = wal_checksum(&buf[0..24], false, 0, 0);
        assert_eq!(c0, h.checksum1);
        assert_eq!(c1, h.checksum2);
    }

    #[test]
    fn wal_frame_header_round_trips() {
        let fh = WalFrameHeader {
            page_number: 42,
            commit_size: 100,
            salt1: 0x01020304,
            salt2: 0x05060708,
            checksum1: 0x11111111,
            checksum2: 0x22222222,
        };
        let mut buf = [0u8; WAL_FRAME_HEADER_SIZE];
        fh.encode(&mut buf);
        let back = WalFrameHeader::decode(&buf).unwrap();
        assert_eq!(back, fh);
    }

    #[test]
    fn wal_checksum_known_vector() {
        // A zero-filled 24-byte input: the checksum is a function of (0, 0) over all-zero
        // words. With s0 = s1 = 0, the first iteration gives s0 = 0, s1 = 0 (nothing changes),
        // and so on — the checksum stays (0, 0).
        let zero = [0u8; 24];
        let (c0, c1) = wal_checksum(&zero, true, 0, 0);
        assert_eq!(c0, 0);
        assert_eq!(c1, 0);
        // A single non-zero word pair: s0 = a + s1 (= a), s1 = b + s0 (= a + b).
        let mut buf = [0u8; 8];
        buf[0..4].copy_from_slice(&1u32.to_be_bytes());
        buf[4..8].copy_from_slice(&2u32.to_be_bytes());
        let (c0, c1) = wal_checksum(&buf, true, 0, 0);
        assert_eq!(c0, 1);
        assert_eq!(c1, 3);
    }

    #[test]
    fn wal_checksum_running_state_carries_across_chunks() {
        // Two chunks: the running (s0, s1) from the first feeds the second.
        let mut buf1 = [0u8; 8];
        buf1[0..4].copy_from_slice(&1u32.to_be_bytes());
        buf1[4..8].copy_from_slice(&2u32.to_be_bytes());
        let (s0, s1) = wal_checksum(&buf1, true, 0, 0);
        let mut buf2 = [0u8; 8];
        buf2[0..4].copy_from_slice(&3u32.to_be_bytes());
        buf2[4..8].copy_from_slice(&4u32.to_be_bytes());
        let (s0, s1) = wal_checksum(&buf2, true, s0, s1);
        // Concatenating and checksumming in one pass must give the same result.
        let mut concat = [0u8; 16];
        concat[0..8].copy_from_slice(&buf1);
        concat[8..16].copy_from_slice(&buf2);
        let (c0, c1) = wal_checksum(&concat, true, 0, 0);
        assert_eq!((s0, s1), (c0, c1));
    }
}