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
use std::sync::{Arc, Mutex};

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
    /// Open an existing database for read/write (file must already exist).
    pub const READWRITE: OpenFlags = OpenFlags(SQLITE_OPEN_READWRITE);
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

impl LockLevel {
    /// Decode a stored level byte (the inverse of `as u8`). Used by the VFS implementations
    /// to read the atomic `lock_level` field; out-of-range values fall back to `Unlocked`.
    pub fn from_u8(v: u8) -> LockLevel {
        match v {
            0 => LockLevel::Unlocked,
            1 => LockLevel::Shared,
            2 => LockLevel::Reserved,
            3 => LockLevel::Pending,
            4 => LockLevel::Exclusive,
            _ => LockLevel::Unlocked,
        }
    }
}

/// The shared per-path in-process lock state, mirroring `unixInodeInfo` in `os_unix.c`.
///
/// POSIX `fcntl(F_SETLK)` advisory locks are per-process, not per-file-descriptor: a second
/// open of the same file in the same process does NOT contend with the first at the OS level.
/// SQLite bridges this by tracking the lock state per-inode in-process (`unixInodeInfo`:
/// `nShared` + `eFileLock`), and only issuing `fcntl` for cross-process contention. We do
/// the same: a [`LockState`] is shared (via `Arc<Mutex>`) by all opens of the same path in
/// this process, and the VFS consults it before issuing the OS-level `fcntl`.
///
/// Many SHARED locks coexist; at most one of RESERVED/PENDING/EXCLUSIVE may be held at a
/// time; an EXCLUSIVE blocks all SHARED lockers; a PENDING blocks new SHARED but allows
/// existing SHARED holders to release.
#[derive(Default)]
pub struct LockState {
    /// The number of currently-held SHARED locks (across all handles on this path).
    pub n_shared: u32,
    /// The strongest non-SHARED lock currently held (`None` = none).
    pub writer: Option<LockLevel>,
}

impl LockState {
    /// Acquire `target`, transitioning from `current`. Mirrors the contention semantics
    /// of `unixLock` (without the byte-range ceremony): a SHARED lock is granted if no
    /// PENDING/EXCLUSIVE is held; a RESERVED is granted if no other writer is held; an
    /// EXCLUSIVE is granted only if no SHARED (other than this handle's own) and no other
    /// writer is held. The handle's own SHARED is "upgraded" in place (its contribution to
    /// `n_shared` is dropped when the writer is taken).
    pub fn apply_lock(&mut self, current: LockLevel, target: LockLevel) -> Result<()> {
        use crate::error::Error;
        match (current, target) {
            (c, t) if c >= t => Ok(()),

            // UNLOCKED → SHARED: granted unless a PENDING or EXCLUSIVE writer is held.
            (LockLevel::Unlocked, LockLevel::Shared) => {
                if let Some(w) = self.writer {
                    if w == LockLevel::Pending || w == LockLevel::Exclusive {
                        return Err(Error::busy("database is locked"));
                    }
                }
                self.n_shared += 1;
                Ok(())
            }

            // SHARED → RESERVED: granted unless another writer is held.
            (LockLevel::Shared, LockLevel::Reserved) => {
                if self.writer.is_some() {
                    return Err(Error::busy("database is locked"));
                }
                self.writer = Some(LockLevel::Reserved);
                // This handle's SHARED is subsumed by the RESERVED writer.
                self.n_shared = self.n_shared.saturating_sub(1);
                Ok(())
            }

            // * → EXCLUSIVE: granted only if no SHARED holders and no other writer.
            // This covers `SHARED → EXCLUSIVE`, `RESERVED → EXCLUSIVE`, `PENDING →
            // EXCLUSIVE`, and the `UNLOCKED → EXCLUSIVE` direct path (taken when a write
            // statement begins without the connection holding a SHARED lock — rare, but
            // the protocol allows it when no other connection is reading).
            (_, LockLevel::Exclusive) => {
                if current == LockLevel::Shared {
                    self.n_shared = self.n_shared.saturating_sub(1);
                }
                if self.n_shared > 0 || self.writer.is_some() {
                    return Err(Error::busy("database is locked"));
                }
                self.writer = Some(LockLevel::Exclusive);
                Ok(())
            }

            _ => Ok(()),
        }
    }

    /// Release `current` down to `target` (`target` is `SHARED` or `UNLOCKED`). Mirrors
    /// `posixUnlock`.
    pub fn apply_unlock(&mut self, current: LockLevel, target: LockLevel) {
        if current > LockLevel::Shared {
            // Dropping a writer: clear the writer slot (we held it).
            self.writer = None;
            if target == LockLevel::Shared {
                // Downgrade to SHARED: re-add this handle's SHARED contribution.
                self.n_shared += 1;
            }
        } else if current == LockLevel::Shared && target == LockLevel::Unlocked {
            // Dropping a SHARED: remove this handle's contribution.
            self.n_shared = self.n_shared.saturating_sub(1);
        }
    }
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

/// The number of wal-index lock bytes (mirrors `SQLITE_SHM_NLOCK` in `sqlite3.h`). The WAL
/// uses slots 0..=2 for the writer/checkpointer/recovery locks and slots 3..=7 for the five
/// reader read-marks (`WAL_READ_LOCK(0..=4)`). See `format::wal_index` for the indices.
pub const SQLITE_SHM_NLOCK: usize = 8;

/// `xShmLock` flag bit values (mirrors `SQLITE_SHM_*` in `sqlite3.h`).
///
/// `flags` is the bitwise OR of one of `{LOCK, UNLOCK}` and one of `{SHARED, EXCLUSIVE}`:
/// * `LOCK | SHARED`     — acquire a shared lock on `ofst..ofst+n`.
/// * `LOCK | EXCLUSIVE`  — acquire an exclusive lock on `ofst..ofst+n`.
/// * `UNLOCK | SHARED`   — release a shared lock on `ofst..ofst+n`.
/// * `UNLOCK | EXCLUSIVE`— release an exclusive lock on `ofst..ofst+n`.
///
/// Upstream forbids transitions between SHARED and EXCLUSIVE directly (you must unlock to
/// NONE first); this matches `unixShmLock`'s "one may not go from shared to exclusive or
/// from exclusive to shared" rule.
pub mod shm_flags {
    pub const SHM_UNLOCK: u32 = 1;
    pub const SHM_LOCK: u32 = 2;
    pub const SHM_SHARED: u32 = 4;
    pub const SHM_EXCLUSIVE: u32 = 8;
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

    /// Check whether any connection (this one or another) holds a RESERVED or stronger lock
    /// on the file. Mirrors `sqlite3OsCheckReservedLock` / `unixCheckReservedLock` in
    /// `os_unix.c`. Used by the hot-journal recovery path to skip recovery when the journal
    /// belongs to an active transaction (a RESERVED lock means another connection is the
    /// writer — the journal is not hot).
    async fn check_reserved_lock(&self) -> Result<bool>;

    /// Map (and optionally extend) the wal-index shared-memory region `i_region` of
    /// `sz_region` bytes. Returns the mapped slice (a view of the underlying `-shm` file or
    /// in-memory buffer shared between all opens of the same database path). Mirrors
    /// `xShmMap` / `sqlite3OsShmMap` in `os.h`.
    ///
    /// When `b_extend` is `false` and the region has not yet been allocated, returns
    /// `Ok(None)` (a non-extending request for a region that doesn't exist). When
    /// `b_extend` is `true`, the region is allocated (zero-filled) if absent.
    ///
    /// The default implementation refuses with `SQLITE_IOERR_SHMMAP`, matching a VFS that
    /// does not support WAL (upstream's "if not WAL-capable" early-out).
    async fn shm_map(&self, _i_region: usize, _sz_region: usize, _b_extend: bool) -> Result<Option<Arc<Mutex<Vec<u8>>>>> {
        Err(crate::error::Error::io_err("xShmMap not supported by this VFS"))
    }

    /// Acquire or release wal-index locks (mirrors `xShmLock` / `sqlite3OsShmLock`). See
    /// [`shm_flags`] for the `flags` bit values. `ofst` is the first lock slot (0..SQLITE_SHM_NLOCK)
    /// and `n` is the count of consecutive slots to acquire/release as a unit (n==1 for
    /// SHARED locks; n>=1 for EXCLUSIVE locks).
    ///
    /// The default implementation refuses with `SQLITE_IOERR_SHMLOCK`.
    async fn shm_lock(&self, _ofst: usize, _n: usize, _flags: u32) -> Result<()> {
        Err(crate::error::Error::io_err("xShmLock not supported by this VFS"))
    }

    /// Memory barrier over the wal-index (mirrors `xShmBarrier` / `sqlite3OsShmBarrier`).
    /// All loads/stores before the barrier complete before any load/store after it. The
    /// default is a no-op (sufficient for single-threaded tests); VFS implementations that
    /// share the `-shm` across threads/processes issue a real fence.
    async fn shm_barrier(&self) {}

    /// Close the wal-index shared-memory mapping for this connection (mirrors `xShmUnmap` /
    /// `sqlite3OsShmUnmap`). When `delete_flag` is true, the underlying `-shm` file is
    /// removed (mirrors `sqlite3OsDelete` after `sqlite3WalClose`). The default is a no-op.
    async fn shm_unmap(&self, _delete_flag: bool) -> Result<()> {
        Ok(())
    }
}
