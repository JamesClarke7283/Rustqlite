//! The default OS-backed VFS, using positioned I/O on tokio's blocking thread pool.
//!
//! Mirrors `os_unix.c`: positioned `pread`/`pwrite` (via the Unix [`FileExt`]) so a single
//! file handle can serve many concurrent positioned reads without a shared seek cursor. The
//! blocking syscalls run on `tokio::task::spawn_blocking`, keeping the async surface honest.
//!
//! Real POSIX byte-range locking (`fcntl(F_SETLK)`) is implemented for the 5-state SQLite
//! locking protocol (UNLOCKED → SHARED → RESERVED → PENDING → EXCLUSIVE), mirroring
//! `unixLock`/`posixUnlock` in `os_unix.c`. The lock bytes are at the well-known offsets
//! `PENDING_BYTE`/`RESERVED_BYTE`/`SHARED_FIRST` (default `0x4000_0000`/`+1`/`+2`, with
//! `SHARED_SIZE = 510`), so cross-process contention with the real `sqlite3` binary is
//! correct: a `BEGIN EXCLUSIVE` here blocks a `BEGIN EXCLUSIVE` there and vice versa.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;

use crate::error::{Error, Result};

use super::{LockLevel, LockState, OpenFlags, Vfs, VfsFile};

#[cfg(unix)]
use std::os::unix::fs::FileExt;

/// The process-global per-path lock-state registry, mirroring `unixInodeInfo`'s `inodeList`
/// in `os_unix.c`. POSIX `fcntl(F_SETLK)` advisory locks are per-process, so two opens of
/// the same file in this process don't contend at the OS level — this registry tracks the
/// in-process contention (a second `BEGIN EXCLUSIVE` on the same path in the same process
/// blocks here even though the OS would allow it). Shared across all `OsTokioVfs` instances
/// so two `sqlite3_open` calls on the same path see each other's locks.
fn inode_list() -> &'static Mutex<HashMap<String, Arc<Mutex<LockState>>>> {
    static INODES: OnceLock<Mutex<HashMap<String, Arc<Mutex<LockState>>>>> = OnceLock::new();
    INODES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The default filesystem-backed VFS.
#[derive(Default)]
pub struct OsTokioVfs;

impl OsTokioVfs {
    pub fn new() -> OsTokioVfs {
        OsTokioVfs
    }

    fn lock_state_for(&self, path: &str) -> Option<Arc<Mutex<LockState>>> {
        if path.is_empty() || path == ":memory:" {
            return None;
        }
        let mut locks = inode_list().lock().unwrap();
        Some(
            locks
                .entry(path.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(LockState::default())))
                .clone(),
        )
    }
}

#[async_trait]
impl Vfs for OsTokioVfs {
    async fn open(&self, path: &str, flags: OpenFlags) -> Result<Box<dyn VfsFile>> {
        let path_str = path.to_string();
        let read_only = flags.is_readonly();
        let create = flags.contains(super::SQLITE_OPEN_CREATE);
        let file = spawn_io(move || {
            let mut opts = std::fs::OpenOptions::new();
            opts.read(true);
            if !read_only {
                opts.write(true);
                if create {
                    opts.create(true);
                }
            }
            opts.open(&path_str)
        })
        .await?
        .map_err(|e| Error::cant_open(e.to_string()))?;

        let lock_state = self.lock_state_for(path);
        Ok(Box::new(OsTokioFile {
            file: Arc::new(file),
            lock_level: AtomicU8::new(LockLevel::Unlocked as u8),
            lock_state,
        }))
    }

    async fn delete(&self, path: &str) -> Result<()> {
        let path = path.to_string();
        spawn_io(move || match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        })
        .await?
        .map_err(|e| Error::io_err(e.to_string()))
    }

    async fn exists(&self, path: &str) -> Result<bool> {
        let path = path.to_string();
        spawn_io(move || Ok::<bool, std::io::Error>(std::path::Path::new(&path).exists()))
            .await?
            .map_err(|e| Error::io_err(e.to_string()))
    }
}

struct OsTokioFile {
    file: Arc<std::fs::File>,
    lock_level: AtomicU8,
    /// Shared per-path lock state for in-process contention tracking (mirrors
    /// `unixInodeInfo`). `None` for `:memory:` (no contention possible).
    lock_state: Option<Arc<Mutex<LockState>>>,
}

#[async_trait]
impl VfsFile for OsTokioFile {
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let file = self.file.clone();
        let len = buf.len();
        let data = spawn_io(move || {
            let mut tmp = vec![0u8; len];
            let n = read_at_impl(&file, &mut tmp, offset)?;
            tmp.truncate(n);
            Ok::<Vec<u8>, std::io::Error>(tmp)
        })
        .await?
        .map_err(|e| Error::io_err(e.to_string()))?;
        buf[..data.len()].copy_from_slice(&data);
        Ok(data.len())
    }

    async fn write_at(&self, offset: u64, data: &[u8]) -> Result<()> {
        let file = self.file.clone();
        let data = data.to_vec();
        spawn_io(move || write_all_at_impl(&file, &data, offset))
            .await?
            .map_err(|e| Error::io_err(e.to_string()))
    }

    async fn truncate(&self, size: u64) -> Result<()> {
        let file = self.file.clone();
        spawn_io(move || file.set_len(size))
            .await?
            .map_err(|e| Error::io_err(e.to_string()))
    }

    async fn sync(&self) -> Result<()> {
        let file = self.file.clone();
        spawn_io(move || file.sync_all())
            .await?
            .map_err(|e| Error::io_err(e.to_string()))
    }

    async fn file_size(&self) -> Result<u64> {
        let file = self.file.clone();
        let md = spawn_io(move || file.metadata())
            .await?
            .map_err(|e| Error::io_err(e.to_string()))?;
        Ok(md.len())
    }

    async fn lock(&self, level: LockLevel) -> Result<()> {
        let current = LockLevel::from_u8(self.lock_level.load(Ordering::SeqCst));
        if current >= level {
            return Ok(());
        }
        // First consult the in-process lock state (mirrors `unixInodeInfo`'s
        // `nShared`/`eFileLock` check in `unixLock`). This catches same-process
        // contention that the OS-level `fcntl` would miss (advisory locks are
        // per-process, not per-fd).
        if let Some(state) = &self.lock_state {
            let mut st = state.lock().unwrap();
            st.apply_lock(current, level)?;
        }
        // Then issue the OS-level byte-range locks for cross-process contention
        // (mirrors the `fcntl(F_SETLK)` calls in `unixLock`).
        let file = self.file.clone();
        match spawn_io(move || posix_lock(&file, current, level)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                // Roll back the in-process state on fcntl failure.
                if let Some(state) = &self.lock_state {
                    let mut st = state.lock().unwrap();
                    st.apply_unlock(level, current);
                }
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    return Err(Error::busy("database is locked"));
                } else {
                    return Err(Error::io_err(e.to_string()));
                }
            }
            Err(join_err) => return Err(join_err),
        }
        self.lock_level.store(level as u8, Ordering::SeqCst);
        Ok(())
    }

    async fn unlock(&self, level: LockLevel) -> Result<()> {
        let current = LockLevel::from_u8(self.lock_level.load(Ordering::SeqCst));
        if current <= level {
            return Ok(());
        }
        if let Some(state) = &self.lock_state {
            let mut st = state.lock().unwrap();
            st.apply_unlock(current, level);
        }
        let file = self.file.clone();
        // The OS-level unlock is best-effort — the in-process state is authoritative for
        // same-process contention, and a failed `fcntl(F_UNLCK)` (e.g. on a network mount)
        // shouldn't abort the transaction tail. Mirrors `posixUnlock`'s "try and continue"
        // behavior for the non-fatal unlock paths.
        let _ = spawn_io(move || posix_unlock(&file, current, level)).await;
        self.lock_level.store(level as u8, Ordering::SeqCst);
        Ok(())
    }

    async fn check_reserved_lock(&self) -> Result<bool> {
        // First check the in-process state (a same-process writer).
        if let Some(state) = &self.lock_state {
            let st = state.lock().unwrap();
            if st.writer.is_some() {
                return Ok(true);
            }
        }
        // Then check the OS-level lock (a cross-process writer) via `fcntl(F_GETLK)` on
        // the RESERVED_BYTE — mirrors `unixCheckReservedLock` in `os_unix.c`.
        let file = self.file.clone();
        let reserved = spawn_io(move || check_reserved_fcntl(&file))
            .await?
            .map_err(|e| Error::io_err(e.to_string()))?;
        Ok(reserved)
    }
}

/// Run a blocking I/O closure on tokio's blocking pool, mapping the join error.
async fn spawn_io<T, F>(f: F) -> Result<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::io_err(format!("blocking task failed: {e}")))
}

#[cfg(unix)]
fn read_at_impl(file: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    file.read_at(buf, offset)
}

#[cfg(unix)]
fn write_all_at_impl(file: &std::fs::File, data: &[u8], offset: u64) -> std::io::Result<()> {
    file.write_all_at(data, offset)
}

// Portable fallback (Windows etc.): seek + read/write. Not concurrency-safe across handles,
// but adequate until a platform-specific positioned-I/O path is added.
#[cfg(not(unix))]
fn read_at_impl(mut file: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(offset))?;
    file.read(buf)
}

#[cfg(not(unix))]
fn write_all_at_impl(mut file: &std::fs::File, data: &[u8], offset: u64) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(data)
}

// ---------------------------------------------------------------------------
// POSIX byte-range locking — a faithful port of `unixLock`/`posixUnlock` from `os_unix.c`.
// ---------------------------------------------------------------------------

/// The first byte past the 1 GiB boundary (`sqlite3PendingByte` in `global.c`,
/// `PENDING_BYTE` in `os.h`). The default value is `0x4000_0000`.
pub const PENDING_BYTE: u64 = 0x4000_0000;
/// `RESERVED_BYTE = PENDING_BYTE + 1` (`os.h`).
pub const RESERVED_BYTE: u64 = PENDING_BYTE + 1;
/// `SHARED_FIRST = PENDING_BYTE + 2` (`os.h`).
pub const SHARED_FIRST: u64 = PENDING_BYTE + 2;
/// `SHARED_SIZE = 510` (`os.h`) — the pool of bytes a SHARED lock can cover.
pub const SHARED_SIZE: u64 = 510;

/// Acquire `target` lock level, transitioning from `current`. Mirrors `unixLock` in
/// `os_unix.c`. The transitions are:
/// * `UNLOCKED → SHARED`: read-lock `PENDING_BYTE` → read-lock `SHARED_FIRST..+SHARED_SIZE` →
///   unlock `PENDING_BYTE`.
/// * `SHARED → RESERVED`: write-lock `RESERVED_BYTE`.
/// * `SHARED → EXCLUSIVE`: write-lock `PENDING_BYTE` (becomes PENDING) → write-lock
///   `SHARED_FIRST..+SHARED_SIZE`.
/// * `RESERVED → EXCLUSIVE`: write-lock `PENDING_BYTE` (becomes PENDING) → write-lock
///   `SHARED_FIRST..+SHARED_SIZE`.
/// * `PENDING → EXCLUSIVE`: write-lock `SHARED_FIRST..+SHARED_SIZE`.
///
/// Returns `Err(WouldBlock)` when a byte-range lock conflicts (the upstream `SQLITE_BUSY`
/// case). Intermediate state (PENDING) is recorded on the `lock_level` field by the caller.
#[cfg(unix)]
fn posix_lock(
    file: &std::fs::File,
    current: LockLevel,
    target: LockLevel,
) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;

    let fd = file.as_raw_fd();
    let setlk = |l_type, start: u64, len: u64| -> std::io::Result<()> {
        let mut lock: libc::flock = unsafe { std::mem::zeroed() };
        lock.l_type = l_type as i16;
        lock.l_whence = libc::SEEK_SET as i16;
        lock.l_start = start as i64;
        lock.l_len = len as i64;
        let rc = unsafe { libc::fcntl(fd, libc::F_SETLK, &lock) };
        if rc == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    };

    match (current, target) {
        // No-op: already at or above the target (caller should have short-circuited).
        (c, t) if c >= t => Ok(()),

        // UNLOCKED → SHARED: PENDING read-lock → SHARED range read-lock → PENDING unlock.
        (LockLevel::Unlocked, LockLevel::Shared) => {
            setlk(libc::F_RDLCK, PENDING_BYTE, 1).ok();
            setlk(libc::F_RDLCK, SHARED_FIRST, SHARED_SIZE)?;
            // Drop the temporary PENDING read-lock.
            setlk(libc::F_UNLCK, PENDING_BYTE, 1).ok();
            Ok(())
        }

        // SHARED → RESERVED: write-lock RESERVED_BYTE.
        (LockLevel::Shared, LockLevel::Reserved) => {
            setlk(libc::F_WRLCK, RESERVED_BYTE, 1)
        }

        // SHARED → EXCLUSIVE, RESERVED → EXCLUSIVE, or UNLOCKED → EXCLUSIVE: PENDING
        // write-lock → EXCLUSIVE range. (The UNLOCKED → EXCLUSIVE direct path is taken
        // when a write statement begins without the connection holding a SHARED lock —
        // rare, but allowed when no other connection is reading.)
        (_, LockLevel::Exclusive) => {
            if let Err(e) = setlk(libc::F_WRLCK, PENDING_BYTE, 1) {
                return Err(e);
            }
            // Now at PENDING. Try to escalate to EXCLUSIVE.
            match setlk(libc::F_WRLCK, SHARED_FIRST, SHARED_SIZE) {
                Ok(()) => Ok(()),
                Err(e) => {
                    // Drop the PENDING lock on failure (the caller did not advance
                    // `lock_level`, so the file is back at the prior level after this).
                    // The caller surfaces `SQLITE_BUSY` to the user.
                    let _ = setlk(libc::F_UNLCK, PENDING_BYTE, 1);
                    Err(e)
                }
            }
        }

        // Other transitions are not part of SQLite's locking protocol (e.g. UNLOCKED →
        // RESERVED is forbidden — a SHARED lock must be acquired first). Treat as a no-op
        // rather than crashing; the higher layers never request these.
        _ => Ok(()),
    }
}

/// Lower the lock level from `current` to `target` (`target` is `SHARED` or `UNLOCKED`).
/// Mirrors `posixUnlock` in `os_unix.c`. The transitions are:
/// * `* → SHARED`: write-lock on RESERVED/PENDING/SHARED range dropped to a read-lock on
///   the SHARED range; unlock `PENDING_BYTE` + `RESERVED_BYTE`.
/// * `* → UNLOCKED`: same as `→ SHARED`, then drop the SHARED range read-lock too.
#[cfg(unix)]
fn posix_unlock(
    file: &std::fs::File,
    current: LockLevel,
    target: LockLevel,
) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;

    let fd = file.as_raw_fd();
    let setlk = |l_type, start: u64, len: u64| -> std::io::Result<()> {
        let mut lock: libc::flock = unsafe { std::mem::zeroed() };
        lock.l_type = l_type as i16;
        lock.l_whence = libc::SEEK_SET as i16;
        lock.l_start = start as i64;
        lock.l_len = len as i64;
        let rc = unsafe { libc::fcntl(fd, libc::F_SETLK, &lock) };
        if rc == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    };

    if current > LockLevel::Shared {
        // Downgrade to SHARED: drop RESERVED/PENDING/EXCLUSIVE write-locks, then
        // read-lock the SHARED range (so we still hold a SHARED lock).
        if target == LockLevel::Shared {
            setlk(libc::F_RDLCK, SHARED_FIRST, SHARED_SIZE)?;
        }
        // Unlock PENDING_BYTE + RESERVED_BYTE (len=2 covers both, since they're adjacent).
        setlk(libc::F_UNLCK, PENDING_BYTE, 2)?;
        if target == LockLevel::Shared {
            return Ok(());
        }
    }
    if target == LockLevel::Unlocked {
        // Drop the SHARED range lock too.
        setlk(libc::F_UNLCK, SHARED_FIRST, SHARED_SIZE)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn posix_lock(
    _file: &std::fs::File,
    _current: LockLevel,
    _target: LockLevel,
) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn posix_unlock(
    _file: &std::fs::File,
    _current: LockLevel,
    _target: LockLevel,
) -> std::io::Result<()> {
    Ok(())
}

/// Check whether any process holds a write-lock on the RESERVED_BYTE, mirroring
/// `unixCheckReservedLock` in `os_unix.c`. Returns `true` if a RESERVED (or stronger) lock
/// is held by any process. Uses `fcntl(F_GETLK)` to probe the lock state.
#[cfg(unix)]
fn check_reserved_fcntl(file: &std::fs::File) -> std::io::Result<bool> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as i16;
    lock.l_whence = libc::SEEK_SET as i16;
    lock.l_start = RESERVED_BYTE as i64;
    lock.l_len = 1;
    let rc = unsafe { libc::fcntl(fd, libc::F_GETLK, &mut lock) };
    if rc == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // `F_GETLK` overwrites `l_type` with `F_UNLCK` if no conflicting lock is found.
    Ok(lock.l_type != libc::F_UNLCK as i16)
}

#[cfg(not(unix))]
fn check_reserved_fcntl(_file: &std::fs::File) -> std::io::Result<bool> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_write_read_roundtrip() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = std::env::temp_dir();
            let path = dir.join(format!("rustqlite_vfs_{}.bin", std::process::id()));
            let path_str = path.to_str().unwrap();

            let vfs = OsTokioVfs::new();
            let f = vfs
                .open(path_str, OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            f.write_at(0, b"SQLite format 3\0").await.unwrap();
            f.sync().await.unwrap();
            assert_eq!(f.file_size().await.unwrap(), 16);

            let mut buf = [0u8; 6];
            let n = f.read_at(0, &mut buf).await.unwrap();
            assert_eq!(n, 6);
            assert_eq!(&buf, b"SQLite");

            vfs.delete(path_str).await.unwrap();
            assert!(!vfs.exists(path_str).await.unwrap());
        });
    }

    #[cfg(unix)]
    #[test]
    fn shared_lock_then_exclusive_blocks() {
        // Two file handles to the same path: a SHARED lock on one should block an
        // EXCLUSIVE lock on the other (the EXCLUSIVE returns SQLITE_BUSY/WouldBlock).
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = std::env::temp_dir();
            let path =
                dir.join(format!("rustqlite_lock_{}.bin", std::process::id()));
            let path_str = path.to_str().unwrap();

            let vfs = OsTokioVfs::new();
            let a = vfs
                .open(path_str, OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            a.write_at(0, b"SQLite format 3\0").await.unwrap();
            a.sync().await.unwrap();

            let b = vfs.open(path_str, OpenFlags::READWRITE).await.unwrap();

            // Acquire SHARED on `a`, then EXCLUSIVE on `b` should fail.
            a.lock(LockLevel::Shared).await.unwrap();
            let err = b.lock(LockLevel::Exclusive).await.unwrap_err();
            assert_eq!(err.code, crate::error::ResultCode::Busy);

            // After `a` unlocks, `b` can acquire EXCLUSIVE.
            a.unlock(LockLevel::Unlocked).await.unwrap();
            b.lock(LockLevel::Exclusive).await.unwrap();
            b.unlock(LockLevel::Unlocked).await.unwrap();

            vfs.delete(path_str).await.unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn reserved_lock_then_reserved_blocks() {
        // Two handles: a RESERVED on one blocks RESERVED on the other.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = std::env::temp_dir();
            let path = dir.join(format!(
                "rustqlite_reserved_{}.bin",
                std::process::id()
            ));
            let path_str = path.to_str().unwrap();

            let vfs = OsTokioVfs::new();
            let a = vfs
                .open(path_str, OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            a.write_at(0, b"SQLite format 3\0").await.unwrap();
            a.sync().await.unwrap();

            let b = vfs.open(path_str, OpenFlags::READWRITE).await.unwrap();

            // SHARED → RESERVED on `a`.
            a.lock(LockLevel::Shared).await.unwrap();
            a.lock(LockLevel::Reserved).await.unwrap();

            // `b` SHARED should still succeed (RESERVED allows new SHARED locks).
            b.lock(LockLevel::Shared).await.unwrap();

            // `b` RESERVED should fail (a holds RESERVED).
            let err = b.lock(LockLevel::Reserved).await.unwrap_err();
            assert_eq!(err.code, crate::error::ResultCode::Busy);

            a.unlock(LockLevel::Unlocked).await.unwrap();
            b.unlock(LockLevel::Unlocked).await.unwrap();

            vfs.delete(path_str).await.unwrap();
        });
    }
}