//! The default OS-backed VFS, using positioned I/O on tokio's blocking thread pool.
//!
//! Mirrors `os_unix.c`: positioned `pread`/`pwrite` (via the Unix [`FileExt`]) so a single
//! file handle can serve many concurrent positioned reads without a shared seek cursor. The
//! blocking syscalls run on `tokio::task::spawn_blocking`, keeping the async surface honest.
//!
//! Real OS byte-range locking is deferred to the write/transaction milestone; `lock`/`unlock`
//! currently track state in-process.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::{Error, Result};

use super::{LockLevel, OpenFlags, Vfs, VfsFile};

#[cfg(unix)]
use std::os::unix::fs::FileExt;

/// The default filesystem-backed VFS.
#[derive(Default)]
pub struct OsTokioVfs;

impl OsTokioVfs {
    pub fn new() -> OsTokioVfs {
        OsTokioVfs
    }
}

#[async_trait]
impl Vfs for OsTokioVfs {
    async fn open(&self, path: &str, flags: OpenFlags) -> Result<Box<dyn VfsFile>> {
        let path = path.to_string();
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
            opts.open(&path)
        })
        .await?
        .map_err(|e| Error::cant_open(e.to_string()))?;

        Ok(Box::new(OsTokioFile {
            file: Arc::new(file),
            lock_level: AtomicU8::new(LockLevel::Unlocked as u8),
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
        self.lock_level.store(level as u8, Ordering::SeqCst);
        Ok(())
    }

    async fn unlock(&self, level: LockLevel) -> Result<()> {
        self.lock_level.store(level as u8, Ordering::SeqCst);
        Ok(())
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
}
