//! Pager — page cache + write buffering + the rollback journal (mirrors `pager.c`, `pcache.c`).
//!
//! The pager opens a database file through a [`VfsFile`], parses the [`DbHeader`], derives the
//! page size and page count, and serves page-sized byte buffers through a cache. Page numbers are
//! 1-based; page 1 carries the 100-byte database header before its b-tree page header.
//!
//! The write path layers a **dirty-page overlay** on top of the clean cache: a page being
//! modified is copied into the dirty map (a faithful stand-in for `sqlite3PagerWrite` making a
//! page writable). A write transaction ([`begin_write`](Pager::begin_write) → mutations →
//! [`commit`](Pager::commit) / [`rollback`](Pager::rollback)) journals each modified page's
//! pre-image to the `-journal` sidecar (see [`journal`]) and commits atomically by syncing the
//! journal, writing the new pages, syncing the database, and deleting the journal. A crash that
//! leaves a *hot* journal is recovered on the next [`open`](Pager::open). WAL lives in [`wal`].
//!
//! All mutable state lives behind a single [`Mutex`] (geometry like `page_size` is immutable and
//! kept in plain fields), so an `Arc<Pager>` — shared by the connection and every prepared
//! statement — can still be written through, exactly as `pager.c` mutates pages through a shared
//! `Pager*`.
//!
//! NOTE on the in-memory model: SQLite hands out a pointer into a pinned page buffer and the
//! caller mutates it in place. We use a copy-modify-write model instead — [`read_page_for_write`]
//! returns an owned copy, the caller mutates it, and [`write_page`] installs it in the dirty map.
//! The bytes written to the file are identical; only the in-RAM ownership differs (which keeps the
//! async/`Mutex` boundaries simple). Because modified pages live in the overlay until commit, the
//! database file is untouched mid-transaction, so an in-process rollback just discards the overlay.
//!
//! [`read_page_for_write`]: Pager::read_page_for_write
//! [`write_page`]: Pager::write_page
//! [`begin_write`]: Pager::begin_write

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};
use crate::format::{DbHeader, TextEncoding};
use crate::vfs::{LockLevel, OpenFlags, Vfs, VfsFile};

pub mod journal;
pub mod pcache;
pub mod wal;

/// A page's bytes (exactly `page_size` long), shared cheaply via `Arc`.
pub type PageRef = Arc<Vec<u8>>;

/// `SQLITE_VERSION_NUMBER` for the pinned 3.53.1 target, written into the header by a writer.
pub const SQLITE_VERSION_NUMBER: u32 = 3_053_001;

/// The pager. Immutable geometry (`page_size`/`usable_size`, the VFS + filename) sits in plain
/// fields; everything that changes during a write — the header, the page count, the clean/dirty
/// page maps — lives in [`PagerState`] behind a [`Mutex`], and the in-flight write transaction in
/// its own [`Mutex`] (so journal I/O, which is async, never contends the page-state lock).
pub struct Pager {
    vfs: Arc<dyn Vfs>,
    path: String,
    file: Box<dyn VfsFile>,
    page_size: usize,
    usable_size: usize,
    state: Mutex<PagerState>,
    txn: Mutex<Option<WriteTxn>>,
}

/// The mutable page-cache interior of a [`Pager`].
struct PagerState {
    header: DbHeader,
    page_count: u32,
    /// Clean pages exactly as read from (or last flushed to) the file.
    cache: HashMap<u32, PageRef>,
    /// Pages modified since the last flush/commit, pending write-back. A `get_page` reads through
    /// this overlay so a writer sees its own in-progress changes.
    dirty: HashMap<u32, PageRef>,
}

/// State of an in-progress write transaction (the rollback-journal half of `pager.c`).
struct WriteTxn {
    /// The open `-journal` sidecar file.
    journal: Arc<dyn VfsFile>,
    /// Seed mixed into every page checksum for this transaction's journal.
    cksum_init: u32,
    /// Database size in pages when the transaction began (the journal's `nDatabase`).
    db_orig_size: u32,
    /// Number of page records written to the journal so far.
    nrec: u32,
    /// Byte offset at which the next page record will be written.
    journal_off: u64,
    /// Pages whose pre-image is already in the journal (so each is journaled at most once).
    journaled: HashSet<u32>,
}

/// Quasi-random `cksumInit` seed for a journal (splitmix64 over pid + a process-global counter).
/// The exact value is irrelevant to file-format faithfulness — the journal is deleted on a clean
/// commit and only ever read back by our own recovery, which reads the seed from the header.
fn next_cksum_init() -> u32 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let bump = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut seed = (u64::from(std::process::id()) << 32) ^ bump.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    seed = (seed ^ (seed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    seed = (seed ^ (seed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    (seed ^ (seed >> 31)) as u32
}

impl Pager {
    /// Open a pager over an already-opened database file, reading and validating the header. If a
    /// **hot journal** (`<path>-journal`) is present, it is played back (and deleted) first, so the
    /// header is parsed from a consistent database.
    pub async fn open(vfs: Arc<dyn Vfs>, path: String, file: Box<dyn VfsFile>) -> Result<Pager> {
        // Crash recovery: a leftover, valid journal means the last writer did not finish. Restore
        // the pre-images before reading anything else (mirrors the hot-journal check in `pager.c`).
        recover_hot_journal(vfs.as_ref(), &path, file.as_ref()).await?;

        let mut head = [0u8; 100];
        let n = file.read_at(0, &mut head).await?;
        if n < 100 {
            return Err(Error::not_a_db("file is shorter than the 100-byte header"));
        }
        let header = DbHeader::parse(&head)?;
        let page_size = header.page_size as usize;
        let usable_size = header.usable_size() as usize;

        let file_size = file.file_size().await?;
        let page_count = (file_size / page_size as u64) as u32;

        Ok(Pager {
            vfs,
            path,
            file,
            page_size,
            usable_size,
            state: Mutex::new(PagerState {
                header,
                page_count,
                cache: HashMap::new(),
                dirty: HashMap::new(),
            }),
            txn: Mutex::new(None),
        })
    }

    /// Create a brand-new, empty database on `file`: a single page 1 holding the 100-byte header
    /// followed by an empty `sqlite_schema` leaf b-tree page, written and synced so the file can be
    /// reopened immediately. Mirrors the initial file `pager.c`/`btree.c` lay down for a fresh
    /// database (the 100-byte header via [`DbHeader::serialize`] + a `zeroPage`d leaf).
    pub async fn create_fresh(
        vfs: Arc<dyn Vfs>,
        path: String,
        file: Box<dyn VfsFile>,
        page_size: u32,
    ) -> Result<Pager> {
        if !(512..=65_536).contains(&page_size) || !page_size.is_power_of_two() {
            return Err(Error::msg(format!("invalid page size {page_size}")));
        }
        let header = fresh_header(page_size);
        let page1 = build_fresh_page1(&header, page_size as usize);

        // Persist page 1 and sync so a subsequent `open` sees a valid file.
        file.write_at(0, &page1).await?;
        file.sync().await?;

        let usable_size = header.usable_size() as usize;
        let mut cache = HashMap::new();
        cache.insert(1u32, Arc::new(page1));
        Ok(Pager {
            vfs,
            path,
            file,
            page_size: page_size as usize,
            usable_size,
            state: Mutex::new(PagerState {
                header,
                page_count: 1,
                cache,
                dirty: HashMap::new(),
            }),
            txn: Mutex::new(None),
        })
    }

    /// A clone of the current database header.
    pub fn header(&self) -> DbHeader {
        self.state.lock().unwrap().header.clone()
    }

    pub fn page_size(&self) -> usize {
        self.page_size
    }

    pub fn usable_size(&self) -> usize {
        self.usable_size
    }

    pub fn page_count(&self) -> u32 {
        self.state.lock().unwrap().page_count
    }

    pub fn text_encoding(&self) -> TextEncoding {
        self.state.lock().unwrap().header.text_encoding
    }

    /// The byte offset within a page at which its b-tree header begins. Page 1 reserves the
    /// first 100 bytes for the database header.
    pub fn btree_header_offset(&self, pgno: u32) -> usize {
        if pgno == 1 {
            100
        } else {
            0
        }
    }

    /// Fetch a page (1-based) as a shared byte buffer, reading through the dirty overlay, then the
    /// clean cache, then the file. The lock is never held across the file I/O.
    pub async fn get_page(&self, pgno: u32) -> Result<PageRef> {
        {
            let st = self.state.lock().unwrap();
            if pgno == 0 || pgno > st.page_count {
                return Err(Error::corrupt(format!(
                    "page {pgno} out of range (page count {})",
                    st.page_count
                )));
            }
            if let Some(page) = st.dirty.get(&pgno).or_else(|| st.cache.get(&pgno)).cloned() {
                return Ok(page);
            }
        }

        let mut buf = vec![0u8; self.page_size];
        let offset = (pgno as u64 - 1) * self.page_size as u64;
        let n = self.file.read_at(offset, &mut buf).await?;
        if n < self.page_size {
            return Err(Error::corrupt(format!(
                "short read for page {pgno}: got {n} of {} bytes",
                self.page_size
            )));
        }

        let page: PageRef = Arc::new(buf);
        // Re-check the overlay in case a concurrent writer installed the page while we read.
        let mut st = self.state.lock().unwrap();
        if let Some(page) = st.dirty.get(&pgno).or_else(|| st.cache.get(&pgno)).cloned() {
            return Ok(page);
        }
        st.cache.insert(pgno, page.clone());
        Ok(page)
    }

    /// Return an owned, mutable copy of page `pgno`'s current contents (dirty overlay, else clean
    /// cache, else the file). The caller mutates the copy and installs it with [`write_page`]; this
    /// is our copy-modify-write stand-in for `sqlite3PagerWrite` making a page writable. If a write
    /// transaction is active, the page's **pre-image** is captured to the journal here (before the
    /// caller can modify it), exactly once per page per transaction.
    ///
    /// [`write_page`]: Pager::write_page
    pub async fn read_page_for_write(&self, pgno: u32) -> Result<Vec<u8>> {
        let page = self.get_page(pgno).await?;
        self.journal_page(pgno, &page).await?;
        Ok((*page).clone())
    }

    /// Capture `pgno`'s pre-image into the journal if a transaction is active, the page existed
    /// before the transaction (`pgno <= db_orig_size`), and it has not been journaled yet. A newly
    /// allocated page (beyond the original size) needs no journal record — rollback simply truncates
    /// the file back to the original size.
    async fn journal_page(&self, pgno: u32, preimage: &[u8]) -> Result<()> {
        // Reserve the record slot under the txn lock, then write it without holding the lock.
        let (journal, offset, cksum_init) = {
            let mut guard = self.txn.lock().unwrap();
            let txn = match guard.as_mut() {
                Some(t) => t,
                None => return Ok(()), // not in a transaction → unjournaled (e.g. flush_dirty path)
            };
            if pgno > txn.db_orig_size || txn.journaled.contains(&pgno) {
                return Ok(());
            }
            let offset = txn.journal_off;
            txn.journaled.insert(pgno);
            txn.nrec += 1;
            txn.journal_off += journal::record_len(self.page_size) as u64;
            (txn.journal.clone(), offset, txn.cksum_init)
        };
        let record = journal::build_record(pgno, preimage, cksum_init);
        journal.write_at(offset, &record).await?;
        Ok(())
    }

    /// Install a modified page into the dirty overlay (pending the next commit/flush). The data
    /// must be exactly one page long.
    pub fn write_page(&self, pgno: u32, data: Vec<u8>) -> Result<()> {
        if data.len() != self.page_size {
            return Err(Error::corrupt(format!(
                "write_page: page {pgno} is {} bytes, expected {}",
                data.len(),
                self.page_size
            )));
        }
        self.state
            .lock()
            .unwrap()
            .dirty
            .insert(pgno, Arc::new(data));
        Ok(())
    }

    /// Allocate a new page at the end of the file, returning its (1-based) page number. The page is
    /// added to the dirty overlay zero-filled (mirrors `btree.c` extending the file then `zeroPage`
    /// preparing the new page); the caller writes its real contents with [`write_page`].
    ///
    /// [`write_page`]: Pager::write_page
    pub fn allocate_page(&self) -> u32 {
        let mut st = self.state.lock().unwrap();
        st.page_count += 1;
        let pgno = st.page_count;
        st.dirty.insert(pgno, Arc::new(vec![0u8; self.page_size]));
        pgno
    }

    /// Mutate the cached database header (e.g. to bump the schema cookie on DDL). The change is
    /// visible to `header()`/`text_encoding()` immediately; it is persisted into page 1 by
    /// [`commit`](Pager::commit).
    pub fn with_header_mut(&self, f: impl FnOnce(&mut DbHeader)) {
        f(&mut self.state.lock().unwrap().header);
    }

    /// Add a page to the freelist: the page becomes the new first trunk, its first 4 bytes
    /// hold the previous first-trunk page number, and the freelist count in the header is
    /// bumped by one. The page is journaled (so a rollback restores the freelist). The
    /// on-page content beyond the 4-byte next-pointer is zeroed, matching SQLite's freelist
    /// trunk layout.
    pub async fn free_page(&self, pgno: u32) -> Result<()> {
        // Capture the pre-image in the journal (the page may already have data on disk; this
        // records it so a rollback can restore the freelist state).
        let preimage = self.get_page(pgno).await?;
        self.journal_page(pgno, &preimage).await?;

        let prev_first = {
            let st = self.state.lock().unwrap();
            st.header.first_freelist_trunk
        };

        // Write the freelist trunk header into the page: first 4 bytes = previous first
        // trunk, rest = 0.
        let mut buf = preimage.to_vec();
        buf[0..4].copy_from_slice(&prev_first.to_be_bytes());
        for b in &mut buf[4..] {
            *b = 0;
        }
        self.write_page(pgno, buf)?;

        // Update the header to point at the new first trunk and bump the count.
        self.with_header_mut(|h| {
            h.first_freelist_trunk = pgno;
            h.freelist_count = h.freelist_count.wrapping_add(1);
        });
        Ok(())
    }

    /// Whether a write transaction is currently open.
    pub fn in_write_txn(&self) -> bool {
        self.txn.lock().unwrap().is_some()
    }

    /// Begin a write transaction: take the writer lock, snapshot the database size, and create the
    /// rollback journal with its header. A no-op if a transaction is already open. Mirrors the
    /// `PAGER_WRITER_LOCKED` → journal-open transition in `pager.c`.
    pub async fn begin_write(&self) -> Result<()> {
        if self.in_write_txn() {
            return Ok(());
        }
        self.file.lock(LockLevel::Exclusive).await?;

        let db_orig_size = self.page_count();
        let cksum_init = next_cksum_init();

        let jfile = self
            .vfs
            .open(&self.journal_path(), OpenFlags::READWRITE_CREATE)
            .await?;
        jfile.truncate(0).await?;
        let header = journal::build_header(0, cksum_init, db_orig_size, self.page_size as u32);
        jfile.write_at(0, &header).await?;
        jfile.sync().await?;

        let journal: Arc<dyn VfsFile> = Arc::from(jfile);
        *self.txn.lock().unwrap() = Some(WriteTxn {
            journal,
            cksum_init,
            db_orig_size,
            nrec: 0,
            journal_off: journal::JOURNAL_HDR_SZ as u64,
            journaled: HashSet::new(),
        });
        Ok(())
    }

    /// Commit the open write transaction atomically (`sqlite3PagerCommitPhaseOne`/`Two`):
    /// stamp page 1's header (change counter, version, size), sync the journal and patch in its
    /// final record count, write the dirty pages into the database and sync it, then delete the
    /// journal — the delete is the durable commit point — and release the writer lock. A commit
    /// with no changes simply ends the transaction.
    pub async fn commit(&self) -> Result<()> {
        if !self.in_write_txn() {
            return Ok(());
        }

        let has_changes = !self.state.lock().unwrap().dirty.is_empty();
        if !has_changes {
            return self.end_txn().await;
        }

        // Stamp page 1's header: the change counter advances on every write transaction, and the
        // in-header size / version-valid-for travel with it (`pager_write_changecounter`). The
        // schema cookie was already bumped by the DDL path via `with_header_mut`, if applicable.
        let page_count = self.page_count();
        self.with_header_mut(|h| {
            h.file_change_counter = h.file_change_counter.wrapping_add(1);
            h.version_valid_for = h.file_change_counter;
            h.sqlite_version_number = SQLITE_VERSION_NUMBER;
            h.db_size_pages = page_count;
        });
        let header_bytes = self.header().serialize();
        // read_page_for_write(1) journals page 1's pre-image (if not already) before we restamp it.
        let mut page1 = self.read_page_for_write(1).await?;
        page1[0..100].copy_from_slice(&header_bytes);
        self.write_page(1, page1)?;

        // CommitPhaseOne: make the journal durable, then record how many pages it holds.
        let (journal, nrec) = {
            let guard = self.txn.lock().unwrap();
            let txn = guard.as_ref().expect("in transaction");
            (txn.journal.clone(), txn.nrec)
        };
        journal.sync().await?;
        journal.write_at(8, &nrec.to_be_bytes()).await?; // patch nRec in the header
        journal.sync().await?;

        // Write the new page images into the database and make them durable.
        let pending: Vec<(u32, PageRef)> = {
            let mut st = self.state.lock().unwrap();
            st.dirty.drain().collect()
        };
        for (pgno, data) in &pending {
            let offset = (*pgno as u64 - 1) * self.page_size as u64;
            self.file.write_at(offset, data).await?;
        }
        // Ensure the file is exactly page_count pages long (it grew if pages were allocated).
        self.file
            .truncate(page_count as u64 * self.page_size as u64)
            .await?;
        self.file.sync().await?;

        // Promote the committed pages into the clean cache.
        {
            let mut st = self.state.lock().unwrap();
            for (pgno, data) in pending {
                st.cache.insert(pgno, data);
            }
        }

        // CommitPhaseTwo: delete the journal (the atomic commit) and drop the writer lock.
        self.end_txn().await
    }

    /// Roll back the open write transaction: discard the dirty overlay (the database file was never
    /// touched mid-transaction), shrink back to the original page count, delete the journal, and
    /// release the writer lock. A no-op if no transaction is open.
    pub async fn rollback(&self) -> Result<()> {
        let orig = {
            let guard = self.txn.lock().unwrap();
            match guard.as_ref() {
                Some(t) => t.db_orig_size,
                None => return Ok(()),
            }
        };
        {
            let mut st = self.state.lock().unwrap();
            st.dirty.clear();
            // Drop any pages that were allocated during the transaction from both maps.
            st.cache.retain(|&pgno, _| pgno <= orig);
            st.page_count = orig;
        }
        self.end_txn().await
    }

    /// Delete the journal and release the writer lock, ending the transaction. Shared by the commit
    /// and rollback tails.
    async fn end_txn(&self) -> Result<()> {
        let _ = self.vfs.delete(&self.journal_path()).await;
        self.file.unlock(LockLevel::Unlocked).await?;
        *self.txn.lock().unwrap() = None;
        Ok(())
    }

    /// Write all dirty pages back to the file and move them into the clean cache, then `sync`. This
    /// is the **unjournaled** flush, used outside a write transaction (e.g. by setup code that is
    /// not crash-sensitive); transactional durability goes through [`commit`](Pager::commit).
    pub async fn flush_dirty(&self) -> Result<()> {
        let pending: Vec<(u32, PageRef)> = {
            let mut st = self.state.lock().unwrap();
            st.dirty.drain().collect()
        };
        if pending.is_empty() {
            return Ok(());
        }
        for (pgno, data) in &pending {
            let offset = (*pgno as u64 - 1) * self.page_size as u64;
            self.file.write_at(offset, data).await?;
        }
        self.file.sync().await?;
        let mut st = self.state.lock().unwrap();
        for (pgno, data) in pending {
            st.cache.insert(pgno, data);
        }
        Ok(())
    }

    fn journal_path(&self) -> String {
        format!("{}-journal", self.path)
    }
}

/// Hot-journal recovery (`pager_playback` for `isHot=1`): if `<path>-journal` exists and carries a
/// valid header, the previous writer crashed mid-commit; copy each record's pre-image back over the
/// database, truncate it to the pre-transaction size, sync, and delete the journal. Missing or
/// header-invalid journals are silently ignored (nothing to recover). A record whose checksum does
/// not verify ends playback (a partially written tail from the crash), matching upstream.
async fn recover_hot_journal(vfs: &dyn Vfs, path: &str, db: &dyn VfsFile) -> Result<()> {
    let jpath = format!("{path}-journal");
    if !vfs.exists(&jpath).await? {
        return Ok(());
    }
    let jfile = vfs.open(&jpath, OpenFlags::READWRITE_CREATE).await?;

    let mut hdr = vec![0u8; journal::JOURNAL_HDR_SZ];
    let n = jfile.read_at(0, &mut hdr).await?;
    let header = match journal::parse_header(&hdr[..n.min(hdr.len())]) {
        Some(h) => h,
        None => {
            // Not a real journal (or empty) — drop it and carry on.
            let _ = vfs.delete(&jpath).await;
            return Ok(());
        }
    };

    let page_size = header.page_size as usize;
    if page_size == 0 {
        let _ = vfs.delete(&jpath).await;
        return Ok(());
    }
    let rec_len = journal::record_len(page_size);
    let mut off = journal::JOURNAL_HDR_SZ as u64;
    for _ in 0..header.nrec {
        let mut rec = vec![0u8; rec_len];
        let got = jfile.read_at(off, &mut rec).await?;
        if got < rec_len {
            break; // truncated tail from the crash
        }
        let pgno = u32::from_be_bytes([rec[0], rec[1], rec[2], rec[3]]);
        let data = &rec[4..4 + page_size];
        let stored = u32::from_be_bytes([
            rec[4 + page_size],
            rec[5 + page_size],
            rec[6 + page_size],
            rec[7 + page_size],
        ]);
        if pgno == 0 || journal::pager_cksum(header.cksum_init, data) != stored {
            break; // corrupt/partial record — stop replay here
        }
        if pgno <= header.db_orig_size {
            db.write_at((pgno as u64 - 1) * page_size as u64, data).await?;
        }
        off += rec_len as u64;
    }

    // Restore the original database size and make the restoration durable before removing the
    // journal (so a crash during recovery re-runs it).
    db.truncate(header.db_orig_size as u64 * page_size as u64).await?;
    db.sync().await?;
    let _ = vfs.delete(&jpath).await;
    Ok(())
}

/// Build the default header for a freshly created database of the given page size. Mirrors the
/// values C SQLite writes for a new file: legacy (rollback-journal) format, UTF-8, schema format 4,
/// and the pinned library version number. The change counter / schema cookie advance as writes
/// commit (handled by the commit path).
fn fresh_header(page_size: u32) -> DbHeader {
    DbHeader {
        page_size,
        write_version: 1,
        read_version: 1,
        reserved_space: 0,
        file_change_counter: 0,
        db_size_pages: 1,
        first_freelist_trunk: 0,
        freelist_count: 0,
        schema_cookie: 0,
        schema_format: 4,
        default_cache_size: 0,
        largest_root_page: 0,
        text_encoding: TextEncoding::Utf8,
        user_version: 0,
        incremental_vacuum: 0,
        application_id: 0,
        version_valid_for: 0,
        sqlite_version_number: SQLITE_VERSION_NUMBER,
    }
}

/// Lay out page 1 of a fresh database: the 100-byte header, then an empty leaf-table b-tree page
/// header (`sqlite_schema`'s root), with the rest of the page zeroed.
fn build_fresh_page1(header: &DbHeader, page_size: usize) -> Vec<u8> {
    let mut page = vec![0u8; page_size];
    page[0..100].copy_from_slice(&header.serialize());
    // Empty leaf-table b-tree header at offset 100 (page 1's b-tree header follows the db header).
    page[100] = 0x0d; // leaf table page
    page[101..103].copy_from_slice(&0u16.to_be_bytes()); // first freeblock = 0
    page[103..105].copy_from_slice(&0u16.to_be_bytes()); // num cells = 0
                                                         // Cell content area starts at the end of the page (stored as 0 when that is 65536).
    let ccs: u16 = if page_size == 65_536 {
        0
    } else {
        page_size as u16
    };
    page[105..107].copy_from_slice(&ccs.to_be_bytes());
    page[107] = 0; // fragmented free bytes
    page
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{MemVfs, OpenFlags, Vfs};

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
    }

    async fn fresh(vfs: &Arc<dyn Vfs>, name: &str) -> Pager {
        let file = vfs.open(name, OpenFlags::READWRITE_CREATE).await.unwrap();
        Pager::create_fresh(vfs.clone(), name.to_string(), file, 4096)
            .await
            .unwrap()
    }

    #[test]
    fn create_fresh_then_reopen_reads_valid_header() {
        rt().block_on(async {
            let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
            let pager = fresh(&vfs, "fresh.db").await;
            assert_eq!(pager.page_count(), 1);
            assert_eq!(pager.page_size(), 4096);
            assert_eq!(pager.header().schema_format, 4);

            // Page 1 is a valid empty leaf-table page.
            let p1 = pager.get_page(1).await.unwrap();
            assert_eq!(p1[100], 0x0d);

            // Reopen the same MemVfs file and re-parse the header.
            let file2 = vfs.open("fresh.db", OpenFlags::READONLY).await.unwrap();
            let reopened = Pager::open(vfs.clone(), "fresh.db".into(), file2)
                .await
                .unwrap();
            assert_eq!(reopened.page_count(), 1);
            assert_eq!(reopened.header().page_size, 4096);
            assert_eq!(
                reopened.header().sqlite_version_number,
                SQLITE_VERSION_NUMBER
            );
        });
    }

    #[test]
    fn allocate_write_flush_reopen_roundtrip() {
        rt().block_on(async {
            let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
            let pager = fresh(&vfs, "rw.db").await;

            // Allocate a fresh page and fill it with a recognizable pattern.
            let pgno = pager.allocate_page();
            assert_eq!(pgno, 2);
            let mut buf = pager.read_page_for_write(pgno).await.unwrap();
            buf[0] = 0xAB;
            buf[4095] = 0xCD;
            pager.write_page(pgno, buf).unwrap();

            // Before flush, the dirty overlay already serves the new bytes.
            assert_eq!(pager.get_page(2).await.unwrap()[0], 0xAB);

            pager.flush_dirty().await.unwrap();

            // Reopen and confirm the page persisted.
            let file2 = vfs.open("rw.db", OpenFlags::READONLY).await.unwrap();
            let reopened = Pager::open(vfs.clone(), "rw.db".into(), file2)
                .await
                .unwrap();
            assert_eq!(reopened.page_count(), 2);
            let p2 = reopened.get_page(2).await.unwrap();
            assert_eq!(p2[0], 0xAB);
            assert_eq!(p2[4095], 0xCD);
        });
    }

    #[test]
    fn commit_persists_and_deletes_journal() {
        rt().block_on(async {
            let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
            let pager = fresh(&vfs, "commit.db").await;
            let orig_change_counter = pager.header().file_change_counter;

            pager.begin_write().await.unwrap();
            // Allocate a page and modify page 1 (touch an existing page so it is journaled).
            let pgno = pager.allocate_page();
            let mut p = pager.read_page_for_write(pgno).await.unwrap();
            p[0] = 0x42;
            pager.write_page(pgno, p).unwrap();
            pager.commit().await.unwrap();

            // The journal is gone after a clean commit.
            assert!(!vfs.exists("commit.db-journal").await.unwrap());
            // The change counter advanced and the in-header size matches the file.
            let h = pager.header();
            assert_eq!(h.file_change_counter, orig_change_counter + 1);
            assert_eq!(h.version_valid_for, h.file_change_counter);
            assert_eq!(h.db_size_pages, 2);

            // Reopen: the committed page and the stamped header persisted.
            let file2 = vfs.open("commit.db", OpenFlags::READONLY).await.unwrap();
            let reopened = Pager::open(vfs.clone(), "commit.db".into(), file2)
                .await
                .unwrap();
            assert_eq!(reopened.page_count(), 2);
            assert_eq!(reopened.get_page(2).await.unwrap()[0], 0x42);
            assert_eq!(
                reopened.header().file_change_counter,
                orig_change_counter + 1
            );
        });
    }

    #[test]
    fn rollback_leaves_database_unchanged() {
        rt().block_on(async {
            let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
            let pager = fresh(&vfs, "rb.db").await;
            let orig_change_counter = pager.header().file_change_counter;

            pager.begin_write().await.unwrap();
            let pgno = pager.allocate_page();
            let mut p = pager.read_page_for_write(pgno).await.unwrap();
            p[0] = 0x99;
            pager.write_page(pgno, p).unwrap();
            // Also touch page 1's overlay so we'd notice if it leaked to disk.
            let mut p1 = pager.read_page_for_write(1).await.unwrap();
            p1[100] = 0x05;
            pager.write_page(1, p1).unwrap();
            pager.rollback().await.unwrap();

            // Page count is back to 1, the journal is gone, and nothing was written.
            assert_eq!(pager.page_count(), 1);
            assert!(!vfs.exists("rb.db-journal").await.unwrap());
            assert_eq!(pager.header().file_change_counter, orig_change_counter);

            let file2 = vfs.open("rb.db", OpenFlags::READONLY).await.unwrap();
            let reopened = Pager::open(vfs.clone(), "rb.db".into(), file2)
                .await
                .unwrap();
            assert_eq!(reopened.page_count(), 1);
            // The original empty-leaf page 1 is intact (byte 100 is the 0x0d page type).
            assert_eq!(reopened.get_page(1).await.unwrap()[100], 0x0d);
        });
    }

    #[test]
    fn hot_journal_is_replayed_on_open() {
        rt().block_on(async {
            let vfs: Arc<dyn Vfs> = Arc::new(MemVfs::new());
            // Build a 2-page database directly (page 1 fresh header, page 2 = all 0x11).
            {
                let pager = fresh(&vfs, "hot.db").await;
                let pgno = pager.allocate_page();
                let mut p = pager.read_page_for_write(pgno).await.unwrap();
                p.iter_mut().for_each(|b| *b = 0x11);
                pager.write_page(pgno, p).unwrap();
                pager.flush_dirty().await.unwrap();
            }

            // Simulate a crash mid-commit: a valid journal holds page 2's pre-image (0x11), but the
            // database file has already been overwritten with the new contents (0x22).
            let preimage = vec![0x11u8; 4096];
            let jname = "hot.db-journal";
            let jfile = vfs
                .open(jname, OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            let cksum_init = 0xabcd_1234u32;
            jfile
                .write_at(0, &journal::build_header(1, cksum_init, 2, 4096))
                .await
                .unwrap();
            jfile
                .write_at(
                    journal::JOURNAL_HDR_SZ as u64,
                    &journal::build_record(2, &preimage, cksum_init),
                )
                .await
                .unwrap();
            jfile.sync().await.unwrap();

            // Corrupt the live database page 2 (the "half-written commit").
            let dbfile = vfs.open("hot.db", OpenFlags::READWRITE_CREATE).await.unwrap();
            dbfile.write_at(4096, &vec![0x22u8; 4096]).await.unwrap();
            dbfile.sync().await.unwrap();

            // Opening triggers recovery: page 2 is restored to its pre-image and the journal removed.
            let file = vfs.open("hot.db", OpenFlags::READONLY).await.unwrap();
            let reopened = Pager::open(vfs.clone(), "hot.db".into(), file).await.unwrap();
            assert!(!vfs.exists(jname).await.unwrap());
            assert_eq!(reopened.get_page(2).await.unwrap()[0], 0x11);
        });
    }
}
