//! Write-Ahead Log (mirrors `wal.c`) — the read path.
//!
//! This module implements the WAL read path for M13.4: opening the `-wal` sidecar, rebuilding
//! the in-memory wal-index from the WAL frames (recovery), and answering "what is the latest
//! frame for page `P`" so the pager can read a page from the WAL instead of the database file.
//!
//! The on-disk format codecs for the WAL header, frame headers, and the wal-index header live
//! in [`crate::format::wal`] and [`crate::format::wal_index`]. This module owns the *runtime*
//! state — the in-memory hash tables, the salts, the page-size, and the `mxFrame`/`nPage`
//! carried by the last commit frame.
//!
//! The write path (M13.5 — appending frames to the WAL) and checkpointing (M13.6) are not
//! here yet; this is a read-only WAL reader. A reader is opened with [`Wal::open`], which runs
//! recovery, and answers page lookups with [`Wal::find_frame`] + [`Wal::read_frame`]. The
//! pager integrates this in [`Pager::get_page`](super::Pager::get_page): when the database
//! header says WAL mode (`write_version == 2`), the pager consults the WAL before reading the
//! database file, and only falls back to the file when the page is not in the WAL.
//!
//! In-memory index representation
//! ------------------------------
//!
//! Upstream maps the `-shm` file into memory and walks it as `u32`/`u16` arrays. We keep the
//! same logical structure in plain Rust vectors, one per *index block*:
//!
//! ```text
//! block 0:  page_mapping: Vec<u32> of len HASHTABLE_NPAGE_ONE  (frames 1..=4062)
//!          hash_table:    Vec<u16> of len HASHTABLE_NSLOT
//! block k: page_mapping: Vec<u32> of len HASHTABLE_NPAGE        (frames N..=N+4095)
//!          hash_table:    Vec<u16> of len HASHTABLE_NSLOT
//! ```
//!
//! `page_mapping[i]` is the database page number stored in frame `(block's iZero) + 1 + i`.
//! `hash_table[wal_hash_key(P)]` is `1 + (iFrame - iZero)` (so `0` means "empty slot"); a
//! lookup walks the hash chain comparing `page_mapping[(h - 1) & (NPG-1)] == P`, exactly
//! mirroring `walFindFrame`'s loop.
//!
//! Recovery reads each frame in WAL order, verifies the running checksum and salt match, and
//! appends the page number to the current block's mapping + hashes it. A frame whose checksum
//! does not verify ends recovery (the WAL was truncated by a crash); only frames up to and
//! including the last *commit frame* (non-zero `commit_size`) are made visible to readers —
//! `mxFrame` is the index of that last commit frame, matching upstream's "the WAL is the
//! durable prefix ending at the last commit frame" rule.

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::format::wal::{
    wal_checksum, WalFrameHeader, WalHeader, WAL_FRAME_HEADER_SIZE, WAL_HEADER_SIZE,
};
use crate::format::wal_index::{
    wal_hash_key, HASHTABLE_NPAGE, HASHTABLE_NPAGE_ONE, HASHTABLE_NSLOT,
};
use crate::vfs::{Vfs, VfsFile};

/// One index block in the in-memory wal-index.
struct IndexBlock {
    /// The "iZero" of this block: one less than the frame number of the first frame indexed.
    /// Block 0 has `iZero = 0`; block `k >= 1` has `iZero = HASHTABLE_NPAGE_ONE + (k-1)*HASHTABLE_NPAGE`.
    i_zero: u32,
    /// `page_mapping[i]` is the page number of frame `i_zero + 1 + i`. Length is
    /// `HASHTABLE_NPAGE_ONE` for block 0 and `HASHTABLE_NPAGE` for later blocks (mirrors the
    /// asymmetric first block from `walHashGet`).
    page_mapping: Vec<u32>,
    /// The hash table: `hash_table[i]` is `1 + (iFrame - i_zero)` (so `0` is the empty slot).
    hash_table: Vec<u16>,
}

impl IndexBlock {
    fn new(i_zero: u32, capacity: usize) -> IndexBlock {
        IndexBlock {
            i_zero,
            page_mapping: vec![0u32; capacity],
            hash_table: vec![0u16; HASHTABLE_NSLOT],
        }
    }

    /// Append a frame's page number to this block (mirrors `walIndexAppend` for one block).
    /// `i_frame` is the 1-based frame index; the slot index within the block is
    /// `(i_frame - i_zero - 1)`.
    fn append(&mut self, i_frame: u32, pgno: u32) {
        let idx = (i_frame - self.i_zero - 1) as usize;
        self.page_mapping[idx] = pgno;
        // Hash slot holds `1 + idx` so that `0` remains the empty sentinel.
        let h = wal_hash_key(pgno);
        // Linear-probe for an empty slot (mirrors `walIndexAppend`'s collision walk).
        let mut i = h;
        for _ in 0..HASHTABLE_NSLOT {
            if self.hash_table[i] == 0 {
                self.hash_table[i] = (idx + 1) as u16;
                return;
            }
            i = (i + 1) & (HASHTABLE_NSLOT - 1);
        }
        // The hash table is sized at 2x the page-mapping, so it can never fill before the
        // page-mapping does; reaching here means corruption.
        debug_assert!(false, "wal-index hash table full");
    }
}

/// The runtime state of an open WAL: the parsed header (carrying salts + page size + checksum
/// endianness), the in-memory index blocks, and the `mxFrame`/`nPage` carried by the last
/// commit frame (the durable prefix visible to readers).
pub struct Wal {
    /// The open `-wal` sidecar file. `None` when there is no WAL file yet (the database is in
    /// WAL mode but nothing is logged yet — every page lookup falls back to the database file,
    /// and the file is created lazily on the first write). Shared via `Arc` so a reader can
    /// clone the handle and read a frame without holding the `Wal` across an `await`.
    file: Option<Arc<dyn VfsFile>>,
    /// The VFS used to open the WAL file lazily on the first write. `None` only in the
    /// unit-test `empty` constructor (no VFS available).
    vfs: Option<Arc<dyn Vfs>>,
    /// The WAL file path (`<db>-wal`), used for the lazy open.
    path: String,
    /// The parsed WAL header. When `file` is `None`, this is a placeholder with zeroes.
    header: WalHeader,
    /// The database page size (a copy of `header.page_size` for convenience).
    page_size: u32,
    /// The in-memory wal-index blocks (block 0 is `blocks[0]`, etc.).
    blocks: Vec<IndexBlock>,
    /// The index of the last valid commit frame in the WAL (the durable prefix). Frames after
    /// `mx_frame` may be present in the file but are not visible to readers (they belong to an
    /// uncommitted transaction). `0` means the WAL has no committed data.
    mx_frame: u32,
    /// The database size in pages carried by the last commit frame (the `nTruncate` field).
    /// Readers see this as the database size, not the file size of the DB file.
    n_page: u32,
    /// The running checksum seed carried from the last commit frame (the `aFrameCksum` in
    /// upstream's `WalIndexHdr`). The next transaction's frames extend it. `None` when no
    /// commit frame has been written yet (the seed is the WAL header checksum).
    frame_cksum: Option<[u32; 2]>,
}

impl Wal {
    /// Open the WAL sidecar at `<path>-wal`, recover the in-memory wal-index, and return the
    /// runtime handle. A missing `-wal` file is not an error — the database is in WAL mode but
    /// has no WAL data yet (it was just opened or just checkpointed), so an empty WAL is
    /// returned (mirrors upstream: `sqlite3WalOpen` succeeds on a missing/empty WAL, and the
    /// first read transaction finds `mxFrame == 0`).
    ///
    /// `page_size` is the database page size (read from the DB header by the caller) — it is
    /// used to validate the WAL header's page size and to size frame reads. We trust the DB
    /// header over the WAL header on mismatch (a stale WAL from before a `VACUUM`/page-size
    /// change would have a different page size; upstream's `walIndexRecover` rejects a WAL
    /// whose page size differs from the pager's).
    ///
    /// The WAL file is opened read-write (mirrors upstream's
    /// `SQLITE_OPEN_READWRITE|SQLITE_OPEN_CREATE|SQLITE_OPEN_WAL`), so the same handle serves
    /// both the read path (`read_frame`) and the write path (`write_frames`). A read-only
    /// database would fail to open the WAL read-write; that case is not yet supported (a
    /// read-only WAL-mode database is M13.12 concurrency work).
    pub async fn open(vfs: Arc<dyn Vfs>, path: &str, page_size: u32) -> Result<Wal> {
        let wal_path = format!("{path}-wal");
        if !vfs.exists(&wal_path).await? {
            // No WAL file — return an empty WAL (the database is in WAL mode but nothing is
            // logged yet). The handle carries no index blocks and `mx_frame = 0`, so every
            // page lookup falls back to the database file. The file is opened lazily on the
            // first write.
            return Ok(Wal::placeholder(None, vfs, wal_path, page_size));
        }
        let file = vfs.open(&wal_path, crate::vfs::OpenFlags::READWRITE_CREATE).await?;
        let file_size = file.file_size().await?;
        if file_size < WAL_HEADER_SIZE as u64 {
            // The WAL file exists but is too short to hold a header — treat it as empty, but
            // keep the file handle (a writer will overwrite it).
            return Ok(Wal::placeholder(Some(file), vfs, wal_path, page_size));
        }

        // Read and parse the WAL header.
        let mut hdr_buf = [0u8; WAL_HEADER_SIZE];
        let n = file.read_at(0, &mut hdr_buf).await?;
        if n < WAL_HEADER_SIZE {
            return Ok(Wal::placeholder(Some(file), vfs, wal_path, page_size));
        }
        let header = WalHeader::decode(&hdr_buf)?;

        // Validate the page size against the database header's. A mismatch means the WAL is
        // stale (from a different database file that happened to share the path); ignore it.
        if header.page_size != page_size {
            return Ok(Wal::placeholder(Some(file), vfs, wal_path, page_size));
        }

        // Verify the WAL header checksum (over the first 24 bytes). A bad checksum means the
        // WAL header is corrupt or partially written; treat the WAL as empty.
        let big = header.checksum_big_endian();
        let (c0, c1) = wal_checksum(&hdr_buf[0..24], big, 0, 0);
        if c0 != header.checksum1 || c1 != header.checksum2 {
            return Ok(Wal::placeholder(Some(file), vfs, wal_path, page_size));
        }

        let mut wal = Wal {
            file: Some(Arc::from(file)),
            vfs: Some(vfs),
            path: wal_path,
            header: header.clone(),
            page_size,
            blocks: Vec::new(),
            mx_frame: 0,
            n_page: 0,
            frame_cksum: Some([header.checksum1, header.checksum2]),
        };
        wal.recover(file_size).await?;
        Ok(wal)
    }

    /// Build a placeholder WAL (no recovered frames). Used when the `-wal` file is missing,
    /// empty, or fails validation. The `file` is `Some` when the file exists (so a writer can
    /// overwrite it) and `None` when it doesn't (the writer creates it lazily).
    fn placeholder(
        file: Option<Box<dyn VfsFile>>,
        vfs: Arc<dyn Vfs>,
        path: String,
        page_size: u32,
    ) -> Wal {
        Wal {
            file: file.map(|f| Arc::from(f)),
            vfs: Some(vfs),
            path,
            header: WalHeader {
                magic: 0,
                format_version: 0,
                page_size,
                checkpoint_seq: 0,
                salt1: 0,
                salt2: 0,
                checksum1: 0,
                checksum2: 0,
            },
            page_size,
            blocks: Vec::new(),
            mx_frame: 0,
            n_page: 0,
            frame_cksum: None,
        }
    }

    /// The page size used by this WAL (matches the database header).
    pub fn page_size(&self) -> u32 {
        self.page_size
    }

    /// The database size in pages carried by the last commit frame. Readers see this instead
    /// of the DB file's page count when the WAL is non-empty (mirrors `sqlite3WalDbsize`).
    pub fn n_page(&self) -> u32 {
        self.n_page
    }

    /// The index of the last committed frame (`0` means the WAL is empty/uncommitted).
    pub fn mx_frame(&self) -> u32 {
        self.mx_frame
    }

    /// Clone the WAL file handle as an `Arc` so the caller can read a frame without holding
    /// the `Wal` lock across an `await`. Returns `None` when the WAL file is not open (the
    /// empty-WAL case — `find_frame` returns `0` before this is ever called for a missing
    /// page, so the caller never reaches the read when `file_clone` would be `None`).
    pub fn file_clone(&self) -> Option<Arc<dyn VfsFile>> {
        self.file.clone()
    }

    /// The byte offset within the WAL file of the page data for frame `i_frame` (the frame
    /// header is `WAL_FRAME_HEADER_SIZE` bytes before this). Exposed so the pager can read
    /// a frame's data directly via the cloned file handle without going through the `Wal`
    /// (which would require holding the `Wal` lock across an `await`).
    pub fn frame_data_offset(&self, i_frame: u32) -> u64 {
        WAL_HEADER_SIZE as u64
            + ((i_frame as u64 - 1) * (self.page_size as u64 + WAL_FRAME_HEADER_SIZE as u64))
            + WAL_FRAME_HEADER_SIZE as u64
    }

    /// Rebuild the in-memory wal-index by scanning the WAL frames (mirrors `walIndexRecover`).
    /// Reads every frame in the file, verifies the running checksum, and records the page
    /// number in the appropriate index block. Only frames up to and including the last *commit
    /// frame* (non-zero `commit_size`) are made visible to readers; `mx_frame` is the index of
    /// that frame, and `n_page` is its `commit_size`. Frames after the last commit frame belong
    /// to an uncommitted transaction and are dropped from the index.
    async fn recover(&mut self, file_size: u64) -> Result<()> {
        let frame_size = self.page_size as u64 + WAL_FRAME_HEADER_SIZE as u64;
        let n_frames = (file_size - WAL_HEADER_SIZE as u64) / frame_size;
        if n_frames == 0 {
            return Ok(());
        }

        let big = self.header.checksum_big_endian();
        let salts = [self.header.salt1, self.header.salt2];

        // The running checksum seeds from the WAL header (its first 24 bytes are checksummed
        // into `header.checksum1/2`); each frame extends it over its first 8 bytes + page data.
        let mut running = [self.header.checksum1, self.header.checksum2];

        let mut last_commit_frame: u32 = 0;
        let mut last_commit_npage: u32 = 0;
        let mut last_commit_cksum = [0u32; 2];

        // Read and validate every frame, collecting the page numbers in frame order. We avoid
        // holding an immutable borrow of `self.file` across the `self.append_frame` mutation
        // by reading all frames first into a `Vec` and then building the index blocks.
        let mut frame_pgnos: Vec<u32> = Vec::with_capacity(n_frames as usize);
        let mut frame_buf = vec![0u8; frame_size as usize];
        for i_frame in 1..=n_frames as u32 {
            let offset = WAL_HEADER_SIZE as u64 + (i_frame as u64 - 1) * frame_size;
            let n = {
                let file = match self.file.as_ref() {
                    Some(f) => f,
                    None => return Ok(()),
                };
                file.read_at(offset, &mut frame_buf).await?
            };
            if n < frame_size as usize {
                break; // truncated tail — stop recovery
            }
            let fh = WalFrameHeader::decode(&frame_buf[..WAL_FRAME_HEADER_SIZE])?;

            // The frame's salts must match the WAL header's salts. A mismatch means the frame
            // was left over from a previous WAL (before a checkpoint reset the salts); stop.
            if fh.salt1 != salts[0] || fh.salt2 != salts[1] {
                break;
            }
            // The page number must be non-zero.
            if fh.page_number == 0 {
                break;
            }

            // Verify the running checksum: extend over the first 8 bytes of the frame header
            // (page_number + commit_size) + the page data. The checksum in the frame header
            // (bytes 16..24) is the expected result.
            let (s0, s1) =
                wal_checksum(&frame_buf[0..8], big, running[0], running[1]);
            let (s0, s1) = wal_checksum(
                &frame_buf[WAL_FRAME_HEADER_SIZE..],
                big,
                s0,
                s1,
            );
            if s0 != fh.checksum1 || s1 != fh.checksum2 {
                break; // checksum failed — stop at this frame
            }
            running = [s0, s1];

            // Record the page number; we build the index blocks after the loop to avoid
            // borrowing `self.file` across `self.append_frame` mutation.
            frame_pgnos.push(fh.page_number);

            // A non-zero commit_size marks the last frame of a transaction. The WAL is durable
            // only up to the last commit frame; record it and its carried db size.
            if fh.commit_size != 0 {
                last_commit_frame = i_frame;
                last_commit_npage = fh.commit_size;
                last_commit_cksum = running;
            }
        }

        // Build the in-memory index blocks from the recovered frame sequence. Only frames up
        // to and including the last commit frame are visible to readers; any uncommitted tail
        // is dropped here (mirrors upstream's rule that `mxFrame` is the last commit frame).
        let visible = last_commit_frame as usize;
        for (idx, &pgno) in frame_pgnos.iter().enumerate() {
            if idx >= visible {
                break;
            }
            self.append_frame((idx + 1) as u32, pgno);
        }

        self.mx_frame = last_commit_frame;
        self.n_page = last_commit_npage;
        // The running checksum at the last commit frame becomes the seed for the next
        // transaction (this is the `aFrameCksum` carried in the wal-index header). When no
        // commit frame was recovered, the seed is the WAL header checksum (so the next
        // transaction's first frame extends the header's checksum).
        self.frame_cksum = Some(if last_commit_frame == 0 {
            [self.header.checksum1, self.header.checksum2]
        } else {
            last_commit_cksum
        });
        Ok(())
    }

    /// Append a frame's page number to the appropriate index block (mirrors
    /// `walIndexAppend`'s block selection). Grows the block vector when the frame lands in a
    /// new block.
    fn append_frame(&mut self, i_frame: u32, pgno: u32) {
        let block_idx = wal_frame_page(i_frame);
        while self.blocks.len() <= block_idx {
            let i_zero = if self.blocks.is_empty() {
                0
            } else {
                let prev = &self.blocks[self.blocks.len() - 1];
                prev.i_zero + prev.page_mapping.len() as u32
            };
            let cap = if self.blocks.is_empty() {
                HASHTABLE_NPAGE_ONE
            } else {
                HASHTABLE_NPAGE
            };
            self.blocks.push(IndexBlock::new(i_zero, cap));
        }
        self.blocks[block_idx].append(i_frame, pgno);
    }

    /// Find the latest frame in the WAL that holds page `pgno`, or `0` if the page is not in
    /// the WAL (mirrors `walFindFrame`). The caller then calls [`read_frame`] to read the
    /// page data. `0` means "read from the database file instead".
    ///
    /// Walks the hash tables from the last block backwards (so the *latest* matching frame
    /// wins — a page may appear in the WAL multiple times as it is modified across
    /// transactions, and the reader wants the most recent committed version). Within a block,
    /// the hash chain is walked from the hash slot for `pgno`, comparing the page-mapping
    /// entry at each slot against `pgno`; the first match (highest frame, since we walk blocks
    /// newest-first and the page-mapping is in frame order) is the answer.
    pub fn find_frame(&self, pgno: u32) -> u32 {
        if self.mx_frame == 0 {
            return 0;
        }
        let mut i_read: u32 = 0;
        let last_block = wal_frame_page(self.mx_frame);
        // Walk blocks newest-first so the first block that contains a match holds the latest
        // frame for the page (the page-mapping is in frame order). Within a block, walk the
        // entire hash chain — a page may appear in multiple slots (one per write), and we want
        // the highest frame (mirrors `walFindFrame`'s `iRead = iFrame` assignment).
        for block_idx in (0..=last_block).rev() {
            let block = match self.blocks.get(block_idx) {
                Some(b) => b,
                None => continue,
            };
            let i_zero = block.i_zero;
            let mut i_key = wal_hash_key(pgno);
            let mut n_collide = HASHTABLE_NSLOT;
            loop {
                let h = block.hash_table[i_key];
                if h == 0 {
                    break;
                }
                let i_frame = i_zero + h as u32;
                if i_frame <= self.mx_frame
                    && block.page_mapping[(h as usize - 1) & (HASHTABLE_NPAGE - 1)] == pgno
                    && i_frame > i_read
                {
                    i_read = i_frame;
                }
                if n_collide == 0 {
                    // Too many collisions — corrupt wal-index.
                    return 0;
                }
                n_collide -= 1;
                i_key = (i_key + 1) & (HASHTABLE_NSLOT - 1);
            }
            if i_read != 0 {
                return i_read;
            }
        }
        0
    }

    /// Read the page data stored in frame `i_frame` into `out` (mirrors `sqlite3WalReadFrame`).
    /// `out` must be exactly `page_size` bytes. Returns an error if the frame is out of range
    /// or the read is short.
    pub async fn read_frame(&self, i_frame: u32, out: &mut [u8]) -> Result<()> {
        if i_frame == 0 {
            return Err(Error::corrupt("wal::read_frame: frame 0 is invalid"));
        }
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| Error::corrupt("wal::read_frame: no WAL file"))?;
        if out.len() != self.page_size as usize {
            return Err(Error::corrupt(format!(
                "wal::read_frame: buffer is {} bytes, expected {}",
                out.len(),
                self.page_size
            )));
        }
        let offset = self.frame_data_offset(i_frame);
        let n = file.read_at(offset, out).await?;
        if n != self.page_size as usize {
            return Err(Error::corrupt(format!(
                "wal::read_frame: short read for frame {i_frame}: got {n} of {} bytes",
                self.page_size
            )));
        }
        Ok(())
    }

    /// Append frames for a set of dirty pages to the WAL, then sync, then update the in-memory
    /// wal-index so the new frames are visible to readers (mirrors `walFrames` in `wal.c`).
    ///
    /// `frames` is a list of `(page_number, page_data)` pairs in the order they should be
    /// written (the page data must be exactly `page_size` bytes). `n_truncate` is the database
    /// size in pages after this commit (the `nTruncate` field carried by the last frame's
    /// header — non-zero on a commit, zero mid-transaction). This is a **commit** write: the
    /// last frame carries `n_truncate` as its `commit_size`, making the transaction durable.
    ///
    /// On the first write to a fresh WAL (when `mx_frame == 0` and the file is empty/missing),
    /// the WAL header is written first: magic `0x377f0683` (big-endian checksum), format
    /// `3007000`, the page size, fresh random salts, and the header checksum. The running
    /// checksum seeds from the header checksum.
    ///
    /// After the frames are written and synced, the in-memory index blocks are extended with
    /// the new page numbers (via `append_frame`), and `mx_frame`/`n_page`/`frame_cksum` are
    /// advanced to the new last commit frame. Readers opening a fresh `Wal` on the same file
    /// will recover and see these frames.
    pub async fn write_frames(
        &mut self,
        frames: &[(u32, Vec<u8>)],
        n_truncate: u32,
    ) -> Result<()> {
        if frames.is_empty() {
            return Ok(());
        }
        // Ensure the WAL file is open. If `file` is `None` (the WAL didn't exist on open),
        // create it now (mirrors `walFrames` writing the WAL header on the first frame).
        let need_header = self.mx_frame == 0;
        if self.file.is_none() {
            let vfs = self.vfs.as_ref().expect("vfs missing").clone();
            let file: Box<dyn VfsFile> = vfs.open(&self.path, crate::vfs::OpenFlags::READWRITE_CREATE).await?;
            self.file = Some(Arc::from(file));
        }
        let file = self.file.as_ref().expect("wal file open");

        // On the first write to a fresh WAL, write the WAL header (magic, format, page size,
        // fresh salts, checksum). Mirrors `walFrames`'s `if (iFrame == 0)` branch.
        let mut running = if need_header {
            // Fresh random salts (mirrors `sqlite3_randomness(8, pWal->hdr.aSalt)`). We use a
            // simple splitmix64 over the process id + a counter; the exact values don't
            // matter as long as they're non-zero and change across checkpoints (the salts
            // invalidate stale frames after a restart).
            let salt1 = next_wal_salt();
            let salt2 = next_wal_salt();
            self.header = WalHeader {
                magic: crate::format::wal::WAL_MAGIC_BE,
                format_version: crate::format::wal::WAL_FORMAT_VERSION,
                page_size: self.page_size,
                checkpoint_seq: 0,
                salt1,
                salt2,
                checksum1: 0,
                checksum2: 0,
            };
            let mut hdr_buf = [0u8; WAL_HEADER_SIZE];
            self.header.encode(&mut hdr_buf);
            let (c0, c1) = wal_checksum(&hdr_buf[0..24], true, 0, 0);
            self.header.checksum1 = c0;
            self.header.checksum2 = c1;
            hdr_buf[24..28].copy_from_slice(&c0.to_be_bytes());
            hdr_buf[28..32].copy_from_slice(&c1.to_be_bytes());
            file.write_at(0, &hdr_buf).await?;
            file.sync().await?;
            [c0, c1]
        } else {
            // The running checksum seed is the last commit frame's checksum (or the WAL
            // header checksum when no commit has happened yet — set by `recover`/`open`).
            self.frame_cksum.unwrap_or([self.header.checksum1, self.header.checksum2])
        };

        // Append the frames. The last frame carries `n_truncate` as its `commit_size` (the
        // commit marker); earlier frames carry `0`.
        let mut i_frame = self.mx_frame;
        let big = self.header.checksum_big_endian();
        let salts = [self.header.salt1, self.header.salt2];
        let frame_size = self.page_size as usize + WAL_FRAME_HEADER_SIZE;
        let mut frame_buf = vec![0u8; frame_size];
        for (idx, (pgno, data)) in frames.iter().enumerate() {
            if data.len() != self.page_size as usize {
                return Err(Error::corrupt(format!(
                    "wal::write_frames: page {pgno} is {} bytes, expected {}",
                    data.len(),
                    self.page_size
                )));
            }
            i_frame += 1;
            let commit_size = if idx == frames.len() - 1 { n_truncate } else { 0 };
            // Build the frame header: page_number, commit_size, salts.
            frame_buf[0..4].copy_from_slice(&pgno.to_be_bytes());
            frame_buf[4..8].copy_from_slice(&commit_size.to_be_bytes());
            frame_buf[8..12].copy_from_slice(&salts[0].to_be_bytes());
            frame_buf[12..16].copy_from_slice(&salts[1].to_be_bytes());
            frame_buf[WAL_FRAME_HEADER_SIZE..].copy_from_slice(data);
            // Extend the running checksum over the first 8 bytes + page data.
            let (s0, s1) = wal_checksum(&frame_buf[0..8], big, running[0], running[1]);
            let (s0, s1) = wal_checksum(&frame_buf[WAL_FRAME_HEADER_SIZE..], big, s0, s1);
            frame_buf[16..20].copy_from_slice(&s0.to_be_bytes());
            frame_buf[20..24].copy_from_slice(&s1.to_be_bytes());
            running = [s0, s1];
            // Write the frame at the next WAL offset.
            let offset = WAL_HEADER_SIZE as u64
                + (i_frame as u64 - 1) * frame_size as u64;
            file.write_at(offset, &frame_buf).await?;
        }

        // Sync the WAL (the durable commit point — frames before the sync are not durable).
        file.sync().await?;

        // Update the in-memory wal-index with the new frames (mirrors `walIndexAppend`).
        let first_new_frame = self.mx_frame;
        for (idx, (pgno, _)) in frames.iter().enumerate() {
            self.append_frame(first_new_frame + 1 + idx as u32, *pgno);
        }

        // Advance the WAL state to the new last commit frame.
        self.mx_frame = i_frame;
        self.n_page = n_truncate;
        self.frame_cksum = Some(running);
        Ok(())
    }

    /// Run a checkpoint: copy the committed frames from the WAL back into the database file,
    /// then optionally reset the WAL (mirrors `walCheckpoint` + `sqlite3WalCheckpoint` in
    /// `wal.c`). `db_file` is the open database file (the pager's `self.file`); the WAL frames
    /// are written into it at their page's offset, and the database is truncated to the
    /// committed `n_page` and synced (the durable commit point for the checkpoint).
    ///
    /// `mode` selects the checkpoint flavor:
    ///   * `Passive` — copy whatever frames are committed at the moment and return. Does not
    ///     wait for readers (we have no concurrent readers in this iteration, so this is the
    ///     same as `Full` for now).
    ///   * `Full` — copy all committed frames; in this iteration (no concurrent readers), it
    ///     is identical to `Passive`.
    ///   * `Restart` — copy all committed frames AND reset the WAL so the next writer starts
    ///     a fresh log (new salts, `mx_frame = 0`). The WAL file is left in place but with new
    ///     salts in the header so any pre-restart frames are invalidated.
    ///   * `Truncate` — like `Restart`, but also truncate the WAL file to zero bytes.
    ///
    /// Returns `(n_log, n_ckpt)` — the number of frames in the WAL before the checkpoint and
    /// the number of frames actually copied (which equals `n_log` when the whole WAL was
    /// backfilled, the only case we support without concurrent readers). On `Busy` the
    /// caller surfaces `SQLITE_BUSY`; we never return it here (no concurrent readers yet).
    pub async fn checkpoint(
        &mut self,
        db_file: &dyn VfsFile,
        mode: CheckpointMode,
    ) -> Result<(u32, u32)> {
        let n_log = self.mx_frame;
        if n_log == 0 {
            // Nothing to checkpoint.
            return Ok((0, 0));
        }
        let page_size = self.page_size as usize;
        let frame_size = page_size + WAL_FRAME_HEADER_SIZE;
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| Error::corrupt("wal::checkpoint: no WAL file open"))?;

        // Walk frames 1..=mx_frame in order, read each frame's page data, and write it into
        // the database file at the page's offset. Upstream uses a WalIterator over the
        // wal-index; since we have the in-memory blocks in frame order and no concurrent
        // readers can be mid-frame, we re-read each frame from the WAL file directly (matches
        // `walCheckpoint`'s `sqlite3OsRead(pWal->pWalFd, zBuf, szPage, iOffset)` loop).
        let mut buf = vec![0u8; frame_size];
        let salts = [self.header.salt1, self.header.salt2];
        let mut n_ckpt: u32 = 0;
        for i_frame in 1..=n_log {
            let offset = WAL_HEADER_SIZE as u64 + (i_frame as u64 - 1) * frame_size as u64;
            let n = file.read_at(offset, &mut buf).await?;
            if n < frame_size {
                // Truncated tail — stop. This shouldn't happen for committed frames (recovery
                // already validated them), but a concurrent writer could have extended the
                // file; the loop walks only the committed prefix so this is unreachable.
                return Err(Error::corrupt(format!(
                    "wal::checkpoint: short read for frame {i_frame}"
                )));
            }
            let fh = WalFrameHeader::decode(&buf[..WAL_FRAME_HEADER_SIZE])?;
            // Validate the salts (a frame from a prior WAL would mismatch and must be
            // ignored — this is the same check `recover` makes).
            if fh.salt1 != salts[0] || fh.salt2 != salts[1] {
                break;
            }
            // The page data follows the 24-byte frame header.
            let data = &buf[WAL_FRAME_HEADER_SIZE..];
            let db_offset = (fh.page_number as u64 - 1) * page_size as u64;
            db_file.write_at(db_offset, data).await?;
            n_ckpt += 1;
        }

        // Truncate the database to the committed size and sync (the durable commit point).
        let db_size = self.n_page as u64 * page_size as u64;
        db_file.truncate(db_size).await?;
        db_file.sync().await?;

        // For RESTART/TRUNCATE, reset the WAL so the next writer starts a fresh log. New
        // salts invalidate any stale frames (matches `walRestartHdr`); `mx_frame = 0` makes
        // the WAL appear empty to a fresh reader. The WAL file is left in place (RESTART) or
        // truncated to zero (TRUNCATE).
        match mode {
            CheckpointMode::Passive | CheckpointMode::Full => {
                // Leave the WAL state as-is. Upstream keeps the frames in place after a
                // PASSIVE/FULL checkpoint (a subsequent writer appends at `mx_frame + 1`).
                // We don't reset `mx_frame` — readers would see the WAL as empty and miss
                // the un-reset frames. The next commit extends the WAL; this is fine for
                // single-writer usage (no concurrent readers to confuse).
                //
                // NOTE: This is a simplification — upstream's `nBackfill` tracks how much
                // has been copied so a reader that opens after the checkpoint still finds
                // the frames via the WAL until a subsequent writer resets the log. We have
                // no `nBackfill` field; the in-memory index is rebuilt from scratch on
                // every `Wal::open`, so a fresh reader post-checkpoint re-reads the WAL
                // (still containing the frames) and re-sees them — that is correct for
                // PASSIVE/FULL. The frames will be re-copied by the next checkpoint, which
                // is a wasted effort but not a correctness issue.
            }
            CheckpointMode::Restart | CheckpointMode::Truncate => {
                // Reset the WAL: new salts, mx_frame = 0, drop the in-memory index. The
                // next writer starts a fresh log at frame 1.
                let salt1 = next_wal_salt();
                let salt2 = next_wal_salt();
                self.header.salt1 = salt1;
                self.header.salt2 = salt2;
                self.header.checkpoint_seq = self.header.checkpoint_seq.wrapping_add(1);
                // Recompute the header checksum over the first 24 bytes.
                let mut hdr_buf = [0u8; WAL_HEADER_SIZE];
                self.header.encode(&mut hdr_buf);
                let big = self.header.checksum_big_endian();
                let (c0, c1) = wal_checksum(&hdr_buf[0..24], big, 0, 0);
                self.header.checksum1 = c0;
                self.header.checksum2 = c1;
                hdr_buf[24..28].copy_from_slice(&c0.to_be_bytes());
                hdr_buf[28..32].copy_from_slice(&c1.to_be_bytes());
                file.write_at(0, &hdr_buf).await?;
                file.sync().await?;
                if mode == CheckpointMode::Truncate {
                    file.truncate(0).await?;
                }
                // Reset the in-memory index.
                self.blocks.clear();
                self.mx_frame = 0;
                self.n_page = 0;
                self.frame_cksum = Some([c0, c1]);
            }
        }

        Ok((n_log, n_ckpt))
    }
}

/// The checkpoint mode (mirrors the `SQLITE_CHECKPOINT_*` constants in `sqlite3.h`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckpointMode {
    /// `SQLITE_CHECKPOINT_PASSIVE` — checkpoint as much as possible without waiting.
    Passive,
    /// `SQLITE_CHECKPOINT_FULL` — wait for readers (no-op here; we have no concurrent readers).
    Full,
    /// `SQLITE_CHECKPOINT_RESTART` — like FULL, then reset the WAL so the next writer starts
    /// a fresh log (new salts).
    Restart,
    /// `SQLITE_CHECKPOINT_TRUNCATE` — like RESTART, then truncate the WAL file to zero bytes.
    Truncate,
}

impl CheckpointMode {
    /// Parse the `PRAGMA wal_checkpoint(<mode>)` argument (case-insensitive). Matches
    /// upstream's `sqlite3StrICmp` ladder in `pragma.c`.
    pub fn from_name(name: &str) -> Option<CheckpointMode> {
        match name.to_ascii_lowercase().as_str() {
            "passive" => Some(CheckpointMode::Passive),
            "full" => Some(CheckpointMode::Full),
            "restart" => Some(CheckpointMode::Restart),
            "truncate" => Some(CheckpointMode::Truncate),
            _ => None,
        }
    }
}

/// Generate a fresh non-zero salt value for the WAL header (mirrors
/// `sqlite3_randomness(8, ...)` but with a simple splitmix64 over pid + counter). The exact
/// values don't matter for file-format faithfulness — the salts only need to be non-zero and
/// change across WAL resets so stale frames are invalidated.
fn next_wal_salt() -> u32 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let bump = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut seed = (u64::from(std::process::id()) << 32) ^ bump.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    seed = (seed ^ (seed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    seed = (seed ^ (seed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    let v = (seed ^ (seed >> 31)) as u32;
    if v == 0 { 1 } else { v }
}

/// The wal-index block index that contains frame `i_frame` (mirrors `walFramePage`). Block 0
/// holds frames `1..=HASHTABLE_NPAGE_ONE`; block `k >= 1` holds frames `HASHTABLE_NPAGE_ONE +
/// (k-1)*HASHTABLE_NPAGE + 1 ..= HASHTABLE_NPAGE_ONE + k*HASHTABLE_NPAGE`.
fn wal_frame_page(i_frame: u32) -> usize {
    ((i_frame + HASHTABLE_NPAGE as u32 - HASHTABLE_NPAGE_ONE as u32 - 1) / HASHTABLE_NPAGE as u32)
        as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::wal::{WAL_FORMAT_VERSION, WAL_MAGIC_BE};
    use crate::vfs::{MemVfs, OpenFlags, Vfs};

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
    }

    /// Build a 32-byte WAL header buffer with valid checksum.
    fn build_wal_header(page_size: u32, salt1: u32, salt2: u32) -> [u8; 32] {
        let mut buf = [0u8; 32];
        buf[0..4].copy_from_slice(&WAL_MAGIC_BE.to_be_bytes());
        buf[4..8].copy_from_slice(&WAL_FORMAT_VERSION.to_be_bytes());
        buf[8..12].copy_from_slice(&page_size.to_be_bytes());
        buf[12..16].copy_from_slice(&0u32.to_be_bytes()); // checkpoint_seq
        buf[16..20].copy_from_slice(&salt1.to_be_bytes());
        buf[20..24].copy_from_slice(&salt2.to_be_bytes());
        let (c0, c1) = wal_checksum(&buf[0..24], true, 0, 0);
        buf[24..28].copy_from_slice(&c0.to_be_bytes());
        buf[28..32].copy_from_slice(&c1.to_be_bytes());
        buf
    }

    /// Build a single frame: 24-byte header + page_size bytes of data, with running checksum.
    fn build_frame(
        page_size: u32,
        page_number: u32,
        commit_size: u32,
        salt1: u32,
        salt2: u32,
        running: &mut [u32; 2],
        data: &[u8],
    ) -> Vec<u8> {
        let mut buf = vec![0u8; WAL_FRAME_HEADER_SIZE + page_size as usize];
        buf[0..4].copy_from_slice(&page_number.to_be_bytes());
        buf[4..8].copy_from_slice(&commit_size.to_be_bytes());
        buf[8..12].copy_from_slice(&salt1.to_be_bytes());
        buf[12..16].copy_from_slice(&salt2.to_be_bytes());
        buf[WAL_FRAME_HEADER_SIZE..].copy_from_slice(data);
        let (s0, s1) = wal_checksum(&buf[0..8], true, running[0], running[1]);
        let (s0, s1) = wal_checksum(&buf[WAL_FRAME_HEADER_SIZE..], true, s0, s1);
        buf[16..20].copy_from_slice(&s0.to_be_bytes());
        buf[20..24].copy_from_slice(&s1.to_be_bytes());
        *running = [s0, s1];
        buf
    }

    #[test]
    fn empty_wal_when_no_sidecar() {
        rt().block_on(async {
            let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
            let wal = Wal::open(vfs.clone(), "db", 4096).await.unwrap();
            assert_eq!(wal.page_size(), 4096);
            assert_eq!(wal.mx_frame(), 0);
            assert_eq!(wal.n_page(), 0);
            // No page is in the WAL.
            assert_eq!(wal.find_frame(1), 0);
        });
    }

    #[test]
    fn recover_single_commit_frame() {
        rt().block_on(async {
            let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
            // Build a WAL with one commit frame for page 1.
            let wal_path = "db-wal";
            let page_size: u32 = 4096;
            let salt1 = 0x11111111;
            let salt2 = 0x22222222;
            let hdr = build_wal_header(page_size, salt1, salt2);
            let (c0, c1) = wal_checksum(&hdr[0..24], true, 0, 0);
            let mut running = [c0, c1];
            let mut data = vec![0xABu8; page_size as usize];
            data[0] = 0x42;
            let frame = build_frame(page_size, 1, 5, salt1, salt2, &mut running, &data);
            let file = vfs.open(wal_path, OpenFlags::READWRITE_CREATE).await.unwrap();
            file.write_at(0, &hdr).await.unwrap();
            file.write_at(WAL_HEADER_SIZE as u64, &frame).await.unwrap();

            let wal = Wal::open(vfs.clone(), "db", page_size).await.unwrap();
            assert_eq!(wal.page_size(), page_size);
            assert_eq!(wal.mx_frame(), 1);
            assert_eq!(wal.n_page(), 5);
            assert_eq!(wal.find_frame(1), 1);
            assert_eq!(wal.find_frame(2), 0);

            // Read the frame back.
            let mut out = vec![0u8; page_size as usize];
            wal.read_frame(1, &mut out).await.unwrap();
            assert_eq!(out[0], 0x42);
        });
    }

    #[test]
    fn recover_multiple_frames_latest_wins() {
        rt().block_on(async {
            let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
            let page_size: u32 = 4096;
            let salt1 = 0xaaaaaaaa;
            let salt2 = 0xbbbbbbbb;
            let hdr = build_wal_header(page_size, salt1, salt2);
            let (c0, c1) = wal_checksum(&hdr[0..24], true, 0, 0);
            let mut running = [c0, c1];

            // Frame 1: page 1, commit_size=0 (uncommitted).
            let data1 = vec![0x11u8; page_size as usize];
            let frame1 = build_frame(page_size, 1, 0, salt1, salt2, &mut running, &data1);
            // Frame 2: page 2, commit_size=0.
            let data2 = vec![0x22u8; page_size as usize];
            let frame2 = build_frame(page_size, 2, 0, salt1, salt2, &mut running, &data2);
            // Frame 3: page 1 again (newer version), commit_size=2 (commit).
            let data3 = vec![0x33u8; page_size as usize];
            let frame3 = build_frame(page_size, 1, 2, salt1, salt2, &mut running, &data3);

            let file = vfs.open("db-wal", OpenFlags::READWRITE_CREATE).await.unwrap();
            file.write_at(0, &hdr).await.unwrap();
            let mut off = WAL_HEADER_SIZE as u64;
            file.write_at(off, &frame1).await.unwrap();
            off += frame1.len() as u64;
            file.write_at(off, &frame2).await.unwrap();
            off += frame2.len() as u64;
            file.write_at(off, &frame3).await.unwrap();

            let wal = Wal::open(vfs.clone(), "db", page_size).await.unwrap();
            assert_eq!(wal.mx_frame(), 3);
            assert_eq!(wal.n_page(), 2);
            // Page 1's latest frame is 3.
            assert_eq!(wal.find_frame(1), 3);
            // Page 2's only frame is 2.
            assert_eq!(wal.find_frame(2), 2);

            let mut out = vec![0u8; page_size as usize];
            wal.read_frame(3, &mut out).await.unwrap();
            assert_eq!(out[0], 0x33);
        });
    }

    #[test]
    fn uncommitted_tail_is_dropped() {
        rt().block_on(async {
            let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
            let page_size: u32 = 4096;
            let salt1 = 0xcccccccc;
            let salt2 = 0xdddddddd;
            let hdr = build_wal_header(page_size, salt1, salt2);
            let (c0, c1) = wal_checksum(&hdr[0..24], true, 0, 0);
            let mut running = [c0, c1];

            // Frame 1: page 1, commit_size=1 (commit). Visible.
            let f1 = build_frame(page_size, 1, 1, salt1, salt2, &mut running, &vec![0x11u8; page_size as usize]);
            // Frame 2: page 2, commit_size=0 (uncommitted). Should be dropped.
            let f2 = build_frame(page_size, 2, 0, salt1, salt2, &mut running, &vec![0x22u8; page_size as usize]);

            let file = vfs.open("db-wal", OpenFlags::READWRITE_CREATE).await.unwrap();
            file.write_at(0, &hdr).await.unwrap();
            file.write_at(WAL_HEADER_SIZE as u64, &f1).await.unwrap();
            file.write_at(WAL_HEADER_SIZE as u64 + f1.len() as u64, &f2).await.unwrap();

            let wal = Wal::open(vfs.clone(), "db", page_size).await.unwrap();
            assert_eq!(wal.mx_frame(), 1);
            assert_eq!(wal.n_page(), 1);
            assert_eq!(wal.find_frame(1), 1);
            // Page 2 was in an uncommitted frame — not visible.
            assert_eq!(wal.find_frame(2), 0);
        });
    }

    #[test]
    fn salt_mismatch_stops_recovery() {
        rt().block_on(async {
            let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
            let page_size: u32 = 4096;
            let salt1 = 0xeeeeeeee;
            let salt2 = 0xffffffff;
            let hdr = build_wal_header(page_size, salt1, salt2);
            let (c0, c1) = wal_checksum(&hdr[0..24], true, 0, 0);
            let mut running = [c0, c1];

            // Frame 1: page 1, commit_size=1, correct salts. Visible.
            let f1 = build_frame(page_size, 1, 1, salt1, salt2, &mut running, &vec![0x11u8; page_size as usize]);
            // Frame 2: page 2, wrong salts (stale frame from before a checkpoint).
            let f2 = build_frame(page_size, 2, 0, 0x12345678, 0x9abcdef0, &mut running, &vec![0x22u8; page_size as usize]);

            let file = vfs.open("db-wal", OpenFlags::READWRITE_CREATE).await.unwrap();
            file.write_at(0, &hdr).await.unwrap();
            file.write_at(WAL_HEADER_SIZE as u64, &f1).await.unwrap();
            file.write_at(WAL_HEADER_SIZE as u64 + f1.len() as u64, &f2).await.unwrap();

            let wal = Wal::open(vfs.clone(), "db", page_size).await.unwrap();
            assert_eq!(wal.mx_frame(), 1);
            assert_eq!(wal.find_frame(1), 1);
            assert_eq!(wal.find_frame(2), 0);
        });
    }

    #[test]
    fn bad_checksum_stops_recovery() {
        rt().block_on(async {
            let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
            let page_size: u32 = 4096;
            let salt1 = 0x01020304;
            let salt2 = 0x05060708;
            let hdr = build_wal_header(page_size, salt1, salt2);
            let (c0, c1) = wal_checksum(&hdr[0..24], true, 0, 0);
            let mut running = [c0, c1];

            // Frame 1: valid, commit_size=1.
            let f1 = build_frame(page_size, 1, 1, salt1, salt2, &mut running, &vec![0x11u8; page_size as usize]);
            // Frame 2: invalid checksum (corrupt). Recovery stops here.
            let mut f2 = build_frame(page_size, 2, 0, salt1, salt2, &mut running, &vec![0x22u8; page_size as usize]);
            // Corrupt the checksum bytes.
            f2[16] = 0xff;

            let file = vfs.open("db-wal", OpenFlags::READWRITE_CREATE).await.unwrap();
            file.write_at(0, &hdr).await.unwrap();
            file.write_at(WAL_HEADER_SIZE as u64, &f1).await.unwrap();
            file.write_at(WAL_HEADER_SIZE as u64 + f1.len() as u64, &f2).await.unwrap();

            let wal = Wal::open(vfs.clone(), "db", page_size).await.unwrap();
            assert_eq!(wal.mx_frame(), 1);
            assert_eq!(wal.find_frame(1), 1);
            assert_eq!(wal.find_frame(2), 0);
        });
    }

    #[test]
    fn empty_wal_file_treated_as_empty() {
        rt().block_on(async {
            let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
            let page_size: u32 = 4096;
            // Create an empty -wal file (just the header, no frames).
            let hdr = build_wal_header(page_size, 0, 0);
            let file = vfs.open("db-wal", OpenFlags::READWRITE_CREATE).await.unwrap();
            file.write_at(0, &hdr).await.unwrap();

            let wal = Wal::open(vfs.clone(), "db", page_size).await.unwrap();
            assert_eq!(wal.mx_frame(), 0);
            assert_eq!(wal.n_page(), 0);
        });
    }

    #[test]
    fn write_frames_to_fresh_wal() {
        rt().block_on(async {
            let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
            let page_size: u32 = 4096;
            // Open an empty WAL (no -wal file yet).
            let mut wal = Wal::open(vfs.clone(), "db", page_size).await.unwrap();
            assert_eq!(wal.mx_frame(), 0);

            // Write two frames committing a 2-page database.
            let data1 = vec![0x11u8; page_size as usize];
            let data2 = vec![0x22u8; page_size as usize];
            wal.write_frames(&[(1, data1.clone()), (2, data2.clone())], 2)
                .await
                .unwrap();
            assert_eq!(wal.mx_frame(), 2);
            assert_eq!(wal.n_page(), 2);
            // The frames are visible via find_frame.
            assert_eq!(wal.find_frame(1), 1);
            assert_eq!(wal.find_frame(2), 2);

            // Reopen the WAL from the same file and verify the frames survive recovery.
            let wal2 = Wal::open(vfs.clone(), "db", page_size).await.unwrap();
            assert_eq!(wal2.mx_frame(), 2);
            assert_eq!(wal2.n_page(), 2);
            assert_eq!(wal2.find_frame(1), 1);
            assert_eq!(wal2.find_frame(2), 2);

            // Read the frame data back and verify the bytes.
            let mut out = vec![0u8; page_size as usize];
            wal2.read_frame(1, &mut out).await.unwrap();
            assert_eq!(out[0], 0x11);
            wal2.read_frame(2, &mut out).await.unwrap();
            assert_eq!(out[0], 0x22);
        });
    }

    #[test]
    fn write_frames_appends_to_existing_wal() {
        rt().block_on(async {
            let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
            let page_size: u32 = 4096;
            // First transaction: write page 1, commit_size=1.
            let mut wal = Wal::open(vfs.clone(), "db", page_size).await.unwrap();
            wal.write_frames(&[(1, vec![0x11u8; page_size as usize])], 1)
                .await
                .unwrap();
            assert_eq!(wal.mx_frame(), 1);

            // Second transaction: write page 2, commit_size=2.
            wal.write_frames(&[(2, vec![0x22u8; page_size as usize])], 2)
                .await
                .unwrap();
            assert_eq!(wal.mx_frame(), 2);
            assert_eq!(wal.n_page(), 2);

            // Reopen and verify both frames survive.
            let wal2 = Wal::open(vfs.clone(), "db", page_size).await.unwrap();
            assert_eq!(wal2.mx_frame(), 2);
            assert_eq!(wal2.n_page(), 2);
            assert_eq!(wal2.find_frame(1), 1);
            assert_eq!(wal2.find_frame(2), 2);
        });
    }

    #[test]
    fn write_frames_overwrites_page_in_new_frame() {
        rt().block_on(async {
            let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
            let page_size: u32 = 4096;
            // Transaction 1: write page 1 with value 0x11, commit_size=1.
            let mut wal = Wal::open(vfs.clone(), "db", page_size).await.unwrap();
            wal.write_frames(&[(1, vec![0x11u8; page_size as usize])], 1)
                .await
                .unwrap();
            // Transaction 2: write page 1 again with value 0x22, commit_size=1.
            wal.write_frames(&[(1, vec![0x22u8; page_size as usize])], 1)
                .await
                .unwrap();
            assert_eq!(wal.mx_frame(), 2);
            // The latest frame for page 1 is frame 2.
            assert_eq!(wal.find_frame(1), 2);

            // Reopen and verify recovery sees the latest frame.
            let wal2 = Wal::open(vfs.clone(), "db", page_size).await.unwrap();
            assert_eq!(wal2.find_frame(1), 2);
            let mut out = vec![0u8; page_size as usize];
            wal2.read_frame(2, &mut out).await.unwrap();
            assert_eq!(out[0], 0x22);
        });
    }
}