//! Pager — page cache + write buffering (mirrors `pager.c`, `pcache.c`).
//!
//! The pager opens a database file through a [`VfsFile`], parses the [`DbHeader`], derives the
//! page size and page count, and serves page-sized byte buffers through a cache. Page numbers are
//! 1-based; page 1 carries the 100-byte database header before its b-tree page header.
//!
//! The write path (M4) adds a **dirty-page overlay** on top of the clean cache: a page being
//! modified is copied into the dirty map (a faithful stand-in for `sqlite3PagerWrite` making a
//! page writable), and [`flush_dirty`](Pager::flush_dirty) writes the dirty pages back to the
//! file. All mutable state lives behind a single [`Mutex`] so an `Arc<Pager>` — shared by the
//! connection and every prepared statement — can still be written through (`pager.c` likewise
//! mutates pages through a shared `Pager*`). The rollback journal and atomic commit live in
//! [`journal`] and arrive with the next phase; WAL lives in [`wal`].
//!
//! NOTE on the in-memory model: SQLite hands out a pointer into a pinned page buffer and the
//! caller mutates it in place. We instead use a copy-modify-write model — [`read_page_for_write`]
//! returns an owned copy, the caller mutates it, and [`write_page`] installs it in the dirty map.
//! The bytes written to the file are identical; only the in-RAM ownership differs (which keeps the
//! async/`Mutex` boundaries simple and avoids handing a mutable borrow across an `.await`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};
use crate::format::{DbHeader, TextEncoding};
use crate::vfs::VfsFile;

pub mod journal;
pub mod pcache;
pub mod wal;

/// A page's bytes (exactly `page_size` long), shared cheaply via `Arc`.
pub type PageRef = Arc<Vec<u8>>;

/// `SQLITE_VERSION_NUMBER` for the pinned 3.53.1 target, written into the header by a writer.
pub const SQLITE_VERSION_NUMBER: u32 = 3_053_001;

/// The pager. Immutable geometry (`page_size`/`usable_size`) sits in plain fields; everything that
/// changes during a write — the header, the page count, and the clean/dirty page maps — lives in
/// [`PagerState`] behind a [`Mutex`].
pub struct Pager {
    file: Box<dyn VfsFile>,
    page_size: usize,
    usable_size: usize,
    state: Mutex<PagerState>,
}

/// The mutable interior of a [`Pager`].
struct PagerState {
    header: DbHeader,
    page_count: u32,
    /// Clean pages exactly as read from (or last flushed to) the file.
    cache: HashMap<u32, PageRef>,
    /// Pages modified since the last flush, pending write-back. A `get_page` reads through this
    /// overlay so a writer sees its own in-progress changes.
    dirty: HashMap<u32, PageRef>,
}

impl Pager {
    /// Open a pager over an already-opened database file, reading and validating the header.
    pub async fn open(file: Box<dyn VfsFile>) -> Result<Pager> {
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
            file,
            page_size,
            usable_size,
            state: Mutex::new(PagerState {
                header,
                page_count,
                cache: HashMap::new(),
                dirty: HashMap::new(),
            }),
        })
    }

    /// Create a brand-new, empty database on `file`: a single page 1 holding the 100-byte header
    /// followed by an empty `sqlite_schema` leaf b-tree page, written and synced so the file can be
    /// reopened immediately. Mirrors the initial file `pager.c`/`btree.c` lay down for a fresh
    /// database (the 100-byte header via [`DbHeader::serialize`] + a `zeroPage`d leaf).
    pub async fn create_fresh(file: Box<dyn VfsFile>, page_size: u32) -> Result<Pager> {
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
            file,
            page_size: page_size as usize,
            usable_size,
            state: Mutex::new(PagerState {
                header,
                page_count: 1,
                cache,
                dirty: HashMap::new(),
            }),
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
    /// is our copy-modify-write stand-in for `sqlite3PagerWrite` making a page writable. (The
    /// rollback journal will capture the page's pre-image at this point in the next phase.)
    pub async fn read_page_for_write(&self, pgno: u32) -> Result<Vec<u8>> {
        let page = self.get_page(pgno).await?;
        Ok((*page).clone())
    }

    /// Install a modified page into the dirty overlay (pending the next [`flush_dirty`]). The data
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
    pub fn allocate_page(&self) -> u32 {
        let mut st = self.state.lock().unwrap();
        st.page_count += 1;
        let pgno = st.page_count;
        st.dirty.insert(pgno, Arc::new(vec![0u8; self.page_size]));
        pgno
    }

    /// Mutate the cached database header (e.g. to bump the schema cookie or page count at commit).
    /// The change is reflected by `header()`/`text_encoding()` immediately; persisting it requires
    /// writing page 1 (its first 100 bytes) and flushing.
    pub fn with_header_mut(&self, f: impl FnOnce(&mut DbHeader)) {
        f(&mut self.state.lock().unwrap().header);
    }

    /// Write all dirty pages back to the file and move them into the clean cache, then `sync`. This
    /// is the unjournaled flush; the atomic-commit sequence (journal → write → sync → delete) lands
    /// with [`journal`] in the next phase.
    pub async fn flush_dirty(&self) -> Result<()> {
        // Snapshot and clear the dirty set under the lock; do the I/O without holding it.
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
        // Promote the flushed pages into the clean cache.
        let mut st = self.state.lock().unwrap();
        for (pgno, data) in pending {
            st.cache.insert(pgno, data);
        }
        Ok(())
    }
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

    #[test]
    fn create_fresh_then_reopen_reads_valid_header() {
        rt().block_on(async {
            let vfs = MemVfs::new();
            let file = vfs
                .open("fresh.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            let pager = Pager::create_fresh(file, 4096).await.unwrap();
            assert_eq!(pager.page_count(), 1);
            assert_eq!(pager.page_size(), 4096);
            assert_eq!(pager.header().schema_format, 4);

            // Page 1 is a valid empty leaf-table page.
            let p1 = pager.get_page(1).await.unwrap();
            assert_eq!(p1[100], 0x0d);

            // Reopen the same MemVfs file and re-parse the header.
            let file2 = vfs.open("fresh.db", OpenFlags::READONLY).await.unwrap();
            let reopened = Pager::open(file2).await.unwrap();
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
            let vfs = MemVfs::new();
            let file = vfs
                .open("rw.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            let pager = Pager::create_fresh(file, 4096).await.unwrap();

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
            let reopened = Pager::open(file2).await.unwrap();
            assert_eq!(reopened.page_count(), 2);
            let p2 = reopened.get_page(2).await.unwrap();
            assert_eq!(p2[0], 0xAB);
            assert_eq!(p2[4095], 0xCD);
        });
    }
}
