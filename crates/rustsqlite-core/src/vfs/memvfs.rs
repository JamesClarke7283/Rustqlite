//! In-memory VFS for `:memory:` databases and fast tests.
//!
//! Files are byte vectors behind a shared mutex. A registry keyed by path lets repeated opens
//! of the same name share storage (handy for tests that write then reopen); `:memory:` and
//! empty paths get a private, unregistered file.
//!
//! In-process multi-connection locking mirrors `os_unix.c`'s POSIX byte-range locking: each
//! named file carries a shared [`super::LockState`] tracking how many SHARED locks are held and
//! whether a RESERVED/PENDING/EXCLUSIVE lock is held. A second `MemVfs` connection to the
//! same path sees the contention (RESERVED/EXCLUSIVE blocks the same level on another
//! connection), matching what real POSIX `fcntl(F_SETLK)` locks do across processes. This
//! lets transaction locking be exercised by tests without spawning real processes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::error::Result;

use super::{LockLevel, LockState, OpenFlags, Vfs, VfsFile};

/// An in-memory virtual filesystem.
#[derive(Default)]
pub struct MemVfs {
    files: Mutex<HashMap<String, Arc<Mutex<Vec<u8>>>>>,
    locks: Mutex<HashMap<String, Arc<Mutex<LockState>>>>,
}

impl MemVfs {
    pub fn new() -> MemVfs {
        MemVfs::default()
    }

    /// Look up (or create) the shared lock state for `path`.
    fn lock_state_for(&self, path: &str) -> Option<Arc<Mutex<LockState>>> {
        if path.is_empty() || path == ":memory:" {
            return None;
        }
        let mut locks = self.locks.lock().unwrap();
        Some(
            locks
                .entry(path.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(LockState::default())))
                .clone(),
        )
    }
}

#[async_trait]
impl Vfs for MemVfs {
    async fn open(&self, path: &str, _flags: OpenFlags) -> Result<Box<dyn VfsFile>> {
        let data = if path.is_empty() || path == ":memory:" {
            Arc::new(Mutex::new(Vec::new()))
        } else {
            let mut files = self.files.lock().unwrap();
            files
                .entry(path.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(Vec::new())))
                .clone()
        };
        let lock_state = self.lock_state_for(path);
        Ok(Box::new(MemFile {
            data,
            lock_level: AtomicU8::new(LockLevel::Unlocked as u8),
            lock_state,
        }))
    }

    async fn delete(&self, path: &str) -> Result<()> {
        self.files.lock().unwrap().remove(path);
        self.locks.lock().unwrap().remove(path);
        Ok(())
    }

    async fn exists(&self, path: &str) -> Result<bool> {
        Ok(self.files.lock().unwrap().contains_key(path))
    }
}

struct MemFile {
    data: Arc<Mutex<Vec<u8>>>,
    lock_level: AtomicU8,
    /// Shared per-path lock state for named files; `None` for `:memory:` (no contention
    /// possible — a private file).
    lock_state: Option<Arc<Mutex<LockState>>>,
}

#[async_trait]
impl VfsFile for MemFile {
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let data = self.data.lock().unwrap();
        let start = offset as usize;
        if start >= data.len() {
            return Ok(0);
        }
        let n = buf.len().min(data.len() - start);
        buf[..n].copy_from_slice(&data[start..start + n]);
        Ok(n)
    }

    async fn write_at(&self, offset: u64, src: &[u8]) -> Result<()> {
        let mut data = self.data.lock().unwrap();
        let end = offset as usize + src.len();
        if data.len() < end {
            data.resize(end, 0);
        }
        data[offset as usize..end].copy_from_slice(src);
        Ok(())
    }

    async fn truncate(&self, size: u64) -> Result<()> {
        self.data.lock().unwrap().resize(size as usize, 0);
        Ok(())
    }

    async fn sync(&self) -> Result<()> {
        Ok(())
    }

    async fn file_size(&self) -> Result<u64> {
        Ok(self.data.lock().unwrap().len() as u64)
    }

    async fn lock(&self, level: LockLevel) -> Result<()> {
        let current = LockLevel::from_u8(self.lock_level.load(Ordering::SeqCst));
        if current >= level {
            return Ok(());
        }
        if let Some(state) = &self.lock_state {
            let mut st = state.lock().unwrap();
            st.apply_lock(current, level)?;
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
        self.lock_level.store(level as u8, Ordering::SeqCst);
        Ok(())
    }

    async fn check_reserved_lock(&self) -> Result<bool> {
        if let Some(state) = &self.lock_state {
            let st = state.lock().unwrap();
            return Ok(st.writer.is_some());
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ResultCode;

    #[test]
    fn write_then_read_back() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let vfs = MemVfs::new();
            let f = vfs
                .open("test.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            f.write_at(0, b"hello world").await.unwrap();
            assert_eq!(f.file_size().await.unwrap(), 11);

            let mut buf = [0u8; 5];
            let n = f.read_at(6, &mut buf).await.unwrap();
            assert_eq!(n, 5);
            assert_eq!(&buf, b"world");

            // A short read at EOF returns fewer bytes.
            let mut buf = [0u8; 10];
            let n = f.read_at(8, &mut buf).await.unwrap();
            assert_eq!(n, 3);
            assert_eq!(&buf[..3], b"rld");
        });
    }

    #[test]
    fn named_files_share_storage() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let vfs = MemVfs::new();
            let a = vfs
                .open("shared.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            a.write_at(0, b"abc").await.unwrap();
            let b = vfs.open("shared.db", OpenFlags::READONLY).await.unwrap();
            let mut buf = [0u8; 3];
            b.read_at(0, &mut buf).await.unwrap();
            assert_eq!(&buf, b"abc");
        });
    }

    #[test]
    fn shared_locks_coexist() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let vfs = MemVfs::new();
            let a = vfs
                .open("lock.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            let b = vfs.open("lock.db", OpenFlags::READWRITE).await.unwrap();

            a.lock(LockLevel::Shared).await.unwrap();
            b.lock(LockLevel::Shared).await.unwrap();
            a.unlock(LockLevel::Unlocked).await.unwrap();
            b.unlock(LockLevel::Unlocked).await.unwrap();
        });
    }

    #[test]
    fn reserved_blocks_reserved() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let vfs = MemVfs::new();
            let a = vfs
                .open("lock.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            let b = vfs.open("lock.db", OpenFlags::READWRITE).await.unwrap();

            a.lock(LockLevel::Shared).await.unwrap();
            a.lock(LockLevel::Reserved).await.unwrap();

            // `b` SHARED is still allowed (RESERVED doesn't block new SHARED).
            b.lock(LockLevel::Shared).await.unwrap();
            // `b` RESERVED should fail.
            let err = b.lock(LockLevel::Reserved).await.unwrap_err();
            assert_eq!(err.code, ResultCode::Busy);

            a.unlock(LockLevel::Unlocked).await.unwrap();
            b.unlock(LockLevel::Unlocked).await.unwrap();
        });
    }

    #[test]
    fn exclusive_blocks_shared() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let vfs = MemVfs::new();
            let a = vfs
                .open("lock.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            let b = vfs.open("lock.db", OpenFlags::READWRITE).await.unwrap();

            a.lock(LockLevel::Shared).await.unwrap();
            a.lock(LockLevel::Exclusive).await.unwrap();

            let err = b.lock(LockLevel::Shared).await.unwrap_err();
            assert_eq!(err.code, ResultCode::Busy);

            a.unlock(LockLevel::Unlocked).await.unwrap();
            b.lock(LockLevel::Shared).await.unwrap();
            b.unlock(LockLevel::Unlocked).await.unwrap();
        });
    }
}