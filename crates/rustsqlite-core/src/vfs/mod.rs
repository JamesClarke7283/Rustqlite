//! Virtual File System — the OS abstraction layer (mirrors `os.c` / `os_unix.c`).
//!
//! SQLite isolates all platform I/O behind a VFS so the rest of the engine never touches the
//! filesystem directly. Rustqlite keeps that boundary and makes it **async**: [`Vfs`] and
//! [`VfsFile`] expose async methods (object-safe via `async_trait`), so the pager's reads and
//! writes are async on tokio. The `sqlite3_*` C-API functions drive these to completion via a
//! process-global runtime.
//!
//! Two implementations ship:
//! * [`os_tokio::OsTokioVfs`] — real files via positioned I/O on a blocking thread pool.
//! * [`memvfs::MemVfs`] — in-memory files for `:memory:` databases and fast tests.

use async_trait::async_trait;

use crate::error::Result;

pub mod memvfs;
pub mod os_tokio;

pub use memvfs::MemVfs;
pub use os_tokio::OsTokioVfs;

// `sqlite3_open_v2` flags (`sqlite3.h`). Only the subset used so far is defined.
pub const SQLITE_OPEN_READONLY: i32 = 0x0000_0001;
pub const SQLITE_OPEN_READWRITE: i32 = 0x0000_0002;
pub const SQLITE_OPEN_CREATE: i32 = 0x0000_0004;
pub const SQLITE_OPEN_MEMORY: i32 = 0x0000_0080;

/// A set of `SQLITE_OPEN_*` flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenFlags(pub i32);

impl OpenFlags {
    /// Open an existing database read-only.
    pub const READONLY: OpenFlags = OpenFlags(SQLITE_OPEN_READONLY);
    /// Open read/write, creating the file if necessary (the `sqlite3_open` default).
    pub const READWRITE_CREATE: OpenFlags = OpenFlags(SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE);

    pub fn contains(self, flag: i32) -> bool {
        self.0 & flag != 0
    }

    /// True when opened read-only (READONLY set and READWRITE not set).
    pub fn is_readonly(self) -> bool {
        self.contains(SQLITE_OPEN_READONLY) && !self.contains(SQLITE_OPEN_READWRITE)
    }
}

/// SQLite's five file-lock states (`os.h`). Ordered from weakest to strongest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LockLevel {
    Unlocked = 0,
    Shared = 1,
    Reserved = 2,
    Pending = 3,
    Exclusive = 4,
}

/// A virtual filesystem: opens files and performs path-level operations.
#[async_trait]
pub trait Vfs: Send + Sync {
    /// Open (or create, per `flags`) the file at `path`.
    async fn open(&self, path: &str, flags: OpenFlags) -> Result<Box<dyn VfsFile>>;

    /// Delete the file at `path`. Missing files are not an error.
    async fn delete(&self, path: &str) -> Result<()>;

    /// Whether a file exists at `path`.
    async fn exists(&self, path: &str) -> Result<bool>;
}

/// An open file. All methods take `&self` and use interior mutability so a file can be shared
/// (the pager hands the same file to many readers). Positioned reads/writes mirror SQLite's
/// `pread`/`pwrite` usage — no shared seek cursor.
#[async_trait]
pub trait VfsFile: Send + Sync {
    /// Read into `buf` starting at `offset`. Returns the number of bytes read (which may be
    /// short at end-of-file).
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize>;

    /// Write `data` starting at `offset`, extending the file if necessary.
    async fn write_at(&self, offset: u64, data: &[u8]) -> Result<()>;

    /// Truncate (or extend) the file to `size` bytes.
    async fn truncate(&self, size: u64) -> Result<()>;

    /// Flush buffered writes durably to storage (`fsync`).
    async fn sync(&self) -> Result<()>;

    /// Current size of the file in bytes.
    async fn file_size(&self) -> Result<u64>;

    /// Acquire (raise to) the given lock level. NOTE: the current implementations track lock
    /// state in-process only; real OS-level byte-range locking lands with the write path.
    async fn lock(&self, level: LockLevel) -> Result<()>;

    /// Release down to the given lock level.
    async fn unlock(&self, level: LockLevel) -> Result<()>;
}
