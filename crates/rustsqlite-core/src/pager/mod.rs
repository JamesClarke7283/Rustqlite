//! Pager — page cache + (eventually) transaction state and journaling (mirrors `pager.c`,
//! `pcache.c`).
//!
//! This is the **read-only** pager for M1: it opens a database file through a [`VfsFile`],
//! parses the [`DbHeader`], derives the page size and page count, and serves page-sized byte
//! buffers through a simple cache. Page numbers are 1-based; page 1 carries the 100-byte
//! database header before its b-tree page header.
//!
//! Write buffering, the rollback journal, and WAL live in [`journal`] and [`wal`] and arrive
//! with the write-path milestone.

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

/// The read-only pager.
pub struct Pager {
    file: Box<dyn VfsFile>,
    header: DbHeader,
    page_size: usize,
    usable_size: usize,
    page_count: u32,
    cache: Mutex<HashMap<u32, PageRef>>,
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
            header,
            page_size,
            usable_size,
            page_count,
            cache: Mutex::new(HashMap::new()),
        })
    }

    pub fn header(&self) -> &DbHeader {
        &self.header
    }

    pub fn page_size(&self) -> usize {
        self.page_size
    }

    pub fn usable_size(&self) -> usize {
        self.usable_size
    }

    pub fn page_count(&self) -> u32 {
        self.page_count
    }

    pub fn text_encoding(&self) -> TextEncoding {
        self.header.text_encoding
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

    /// Fetch a page (1-based) as a shared byte buffer, reading through the cache.
    pub async fn get_page(&self, pgno: u32) -> Result<PageRef> {
        if pgno == 0 || pgno > self.page_count {
            return Err(Error::corrupt(format!(
                "page {pgno} out of range (page count {})",
                self.page_count
            )));
        }

        if let Some(page) = self.cache.lock().unwrap().get(&pgno).cloned() {
            return Ok(page);
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
        self.cache.lock().unwrap().insert(pgno, page.clone());
        Ok(page)
    }
}
