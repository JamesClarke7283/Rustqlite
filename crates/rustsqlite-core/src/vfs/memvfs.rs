//! In-memory VFS for `:memory:` databases and fast tests.
//!
//! Files are byte vectors behind a shared mutex. A registry keyed by path lets repeated opens
//! of the same name share storage (handy for tests that write then reopen); `:memory:` and
//! empty paths get a private, unregistered file.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::error::Result;

use super::{LockLevel, OpenFlags, Vfs, VfsFile};

/// An in-memory virtual filesystem.
#[derive(Default)]
pub struct MemVfs {
    files: Mutex<HashMap<String, Arc<Mutex<Vec<u8>>>>>,
}

impl MemVfs {
    pub fn new() -> MemVfs {
        MemVfs::default()
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
        Ok(Box::new(MemFile {
            data,
            lock_level: AtomicU8::new(LockLevel::Unlocked as u8),
        }))
    }

    async fn delete(&self, path: &str) -> Result<()> {
        self.files.lock().unwrap().remove(path);
        Ok(())
    }

    async fn exists(&self, path: &str) -> Result<bool> {
        Ok(self.files.lock().unwrap().contains_key(path))
    }
}

struct MemFile {
    data: Arc<Mutex<Vec<u8>>>,
    lock_level: AtomicU8,
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
        self.lock_level.store(level as u8, Ordering::SeqCst);
        Ok(())
    }

    async fn unlock(&self, level: LockLevel) -> Result<()> {
        self.lock_level.store(level as u8, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
