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

use crate::error::{Error, Result};

use super::{LockLevel, LockState, OpenFlags, Vfs, VfsFile, shm_flags, SQLITE_SHM_NLOCK};

/// The per-path shared wal-index state for `MemVfs` (mirrors `unixShmNode` in `os_unix.c`).
/// All opens of the same database path share one `ShmNode` so the wal-index regions and the
/// lock array are common across connections (matching the shared-memory semantics of the
/// `-shm` file). The lock array tracks per-slot shared-count / exclusive-holder state in
/// process memory.
struct ShmNode {
    /// The mapped regions: one `Arc<Mutex<Vec<u8>>>` per `i_region`. Grown by `shm_map` with
    /// `b_extend=true`. Region `i` has size `sz_region` (uniform — upstream uses
    /// `WALINDEX_PGSZ = 32768`).
    regions: Vec<Arc<Mutex<Vec<u8>>>>,
    /// Per-slot lock state (mirrors `unixShmNode.aLock`): `0` = unlocked, `>0` = N shared
    /// holders, `<0` = -1 for one exclusive holder. Indexed by lock slot 0..SQLITE_SHM_NLOCK.
    ///
    /// Per-connection shared-mask/excl-mask snapshots are kept in the `MemFile`; this struct
    /// holds only the shared `aLock` array that the connection snapshots mutate against.
    a_lock: [i32; SQLITE_SHM_NLOCK],
}

impl ShmNode {
    fn new() -> ShmNode {
        ShmNode {
            regions: Vec::new(),
            a_lock: [0; SQLITE_SHM_NLOCK],
        }
    }
}

/// An in-memory virtual filesystem.
#[derive(Default)]
pub struct MemVfs {
    files: Mutex<HashMap<String, Arc<Mutex<Vec<u8>>>>>,
    locks: Mutex<HashMap<String, Arc<Mutex<LockState>>>>,
    /// Per-path shared wal-index state (mirrors `unixShmNode` / the `-shm` file). `None` for
    /// `:memory:` (a private database has no shared wal-index).
    shms: Mutex<HashMap<String, Arc<Mutex<ShmNode>>>>,
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

    /// Look up (or create) the shared wal-index node for `path`. Returns `None` for
    /// `:memory:` (no shared wal-index).
    fn shm_node_for(&self, path: &str) -> Option<Arc<Mutex<ShmNode>>> {
        if path.is_empty() || path == ":memory:" {
            return None;
        }
        let mut shms = self.shms.lock().unwrap();
        Some(
            shms.entry(path.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(ShmNode::new())))
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
        let shm_node = self.shm_node_for(path);
        Ok(Box::new(MemFile {
            data,
            lock_level: AtomicU8::new(LockLevel::Unlocked as u8),
            lock_state,
            shm_node,
            shm_shared_mask: AtomicU8::new(0),
            shm_excl_mask: AtomicU8::new(0),
        }))
    }

    async fn delete(&self, path: &str) -> Result<()> {
        self.files.lock().unwrap().remove(path);
        self.locks.lock().unwrap().remove(path);
        self.shms.lock().unwrap().remove(path);
        Ok(())
    }

    async fn exists(&self, path: &str) -> Result<bool> {
        Ok(self.files.lock().unwrap().contains_key(path))
    }
}

pub struct MemFile {
    data: Arc<Mutex<Vec<u8>>>,
    lock_level: AtomicU8,
    /// Shared per-path lock state for named files; `None` for `:memory:` (no contention
    /// possible — a private file).
    lock_state: Option<Arc<Mutex<LockState>>>,
    /// Shared wal-index state for the database path; `None` for `:memory:` (no WAL).
    shm_node: Option<Arc<Mutex<ShmNode>>>,
    /// This connection's currently-held SHARED shm locks (a bitmask over `aLock` slots).
    /// Mirrors `unixShm.sharedMask`.
    shm_shared_mask: AtomicU8,
    /// This connection's currently-held EXCLUSIVE shm locks (a bitmask over `aLock` slots).
    /// Mirrors `unixShm.exclMask`.
    shm_excl_mask: AtomicU8,
}

impl MemFile {
    /// Construct an empty in-memory file (as a boxed `VfsFile`) with no lock state. Used as
    /// a no-op placeholder where a `VfsFile` is required by type but never actually read or
    /// written (e.g. the WAL-mode `WriteTxn` carries a dummy journal that is never touched).
    pub fn empty_boxed() -> Box<dyn VfsFile> {
        Box::new(MemFile {
            data: Arc::new(Mutex::new(Vec::new())),
            lock_level: AtomicU8::new(LockLevel::Unlocked as u8),
            lock_state: None,
            shm_node: None,
            shm_shared_mask: AtomicU8::new(0),
            shm_excl_mask: AtomicU8::new(0),
        })
    }
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

    async fn shm_map(&self, i_region: usize, sz_region: usize, b_extend: bool) -> Result<Option<Arc<Mutex<Vec<u8>>>>> {
        let node = match &self.shm_node {
            Some(n) => n.clone(),
            None => return Ok(None),
        };
        let mut node = node.lock().unwrap();
        if i_region >= node.regions.len() {
            if !b_extend {
                return Ok(None);
            }
            // Allocate zero-filled regions up through `i_region`.
            while node.regions.len() <= i_region {
                node.regions.push(Arc::new(Mutex::new(vec![0u8; sz_region])));
            }
        }
        Ok(Some(node.regions[i_region].clone()))
    }

    async fn shm_lock(&self, ofst: usize, n_slots: usize, flags: u32) -> Result<()> {
        use shm_flags as F;
        // Validate inputs (mirrors `unixShmLock`'s asserts).
        if ofst + n_slots > SQLITE_SHM_NLOCK || n_slots == 0 {
            return Err(Error::io_err("invalid xShmLock range"));
        }
        let mask: u8 = (((1u16 << (ofst + n_slots)) - (1u16 << ofst)) & 0xff) as u8;
        let node = match &self.shm_node {
            Some(n) => n.clone(),
            None => return Err(Error::io_err("xShmLock: no shm node")),
        };
        let mut node = node.lock().unwrap();
        let shared_mask = self.shm_shared_mask.load(Ordering::SeqCst);
        let excl_mask = self.shm_excl_mask.load(Ordering::SeqCst);

        let unlocking = flags & F::SHM_UNLOCK != 0;
        let exclusive = flags & F::SHM_EXCLUSIVE != 0;
        let shared = flags & F::SHM_SHARED != 0;

        if unlocking {
            // Case (a): unlock `ofst..ofst+n_slots`.
            if shared {
                // SHARED unlock: n_slots must be 1 (upstream asserts n==1 for SHARED).
                if n_slots != 1 {
                    return Err(Error::io_err("xShmLock: SHARED unlock must have n==1"));
                }
                // Verify the caller actually holds the SHARED lock.
                if shared_mask & mask == 0 {
                    return Ok(()); // nothing to release (idempotent)
                }
                // Drop this connection's contribution. If other connections within this
                // process also hold the SHARED lock, just decrement the count; otherwise
                // fully release the slot (mirrors `unixShmLock`'s `bUnlock` decision).
                if node.a_lock[ofst] > 1 {
                    node.a_lock[ofst] -= 1;
                } else {
                    node.a_lock[ofst] = 0;
                }
                self.shm_shared_mask.store(shared_mask & !mask, Ordering::SeqCst);
            } else {
                // EXCLUSIVE unlock.
                if excl_mask & mask == 0 {
                    return Ok(()); // idempotent
                }
                for slot in ofst..ofst + n_slots {
                    node.a_lock[slot] = 0;
                }
                self.shm_excl_mask.store(excl_mask & !mask, Ordering::SeqCst);
            }
            Ok(())
        } else if shared {
            // Case (b): acquire SHARED.
            if shared_mask & mask != 0 {
                return Ok(()); // already held by this connection
            }
            if node.a_lock[ofst] < 0 {
                // An exclusive lock is held by another connection.
                return Err(Error::busy("wal-index lock busy"));
            }
            node.a_lock[ofst] += 1;
            self.shm_shared_mask.store(shared_mask | mask, Ordering::SeqCst);
            Ok(())
        } else if exclusive {
            // Case (c): acquire EXCLUSIVE.
            if excl_mask & mask != 0 {
                // Already held by this connection — upstream forbids this (asserts), but we
                // return Ok to be forgiving.
                return Ok(());
            }
            if shared_mask & mask != 0 {
                // Upstream forbids going SHARED → EXCLUSIVE directly.
                return Err(Error::io_err("xShmLock: cannot upgrade SHARED to EXCLUSIVE"));
            }
            for slot in ofst..ofst + n_slots {
                if node.a_lock[slot] != 0 {
                    return Err(Error::busy("wal-index lock busy"));
                }
            }
            for slot in ofst..ofst + n_slots {
                node.a_lock[slot] = -1;
            }
            self.shm_excl_mask.store(excl_mask | mask, Ordering::SeqCst);
            Ok(())
        } else {
            Err(Error::io_err("xShmLock: invalid flags (neither LOCK nor UNLOCK)"))
        }
    }

    async fn shm_barrier(&self) {
        // In-process single-runtime: a no-op is sufficient because the tokio runtime
        // provides the memory fence via its own synchronization. Upstream uses
        // `sqlite3MemoryBarrier()` + the unix mutex for redundancy.
        std::sync::atomic::fence(Ordering::SeqCst);
    }

    async fn shm_unmap(&self, delete_flag: bool) -> Result<()> {
        // Drop this connection's locks (mirrors `unixShmUnmap` clearing `sharedMask`/`exclMask`).
        if let Some(node) = &self.shm_node {
            let shared_mask = self.shm_shared_mask.load(Ordering::SeqCst);
            let excl_mask = self.shm_excl_mask.load(Ordering::SeqCst);
            let mut node = node.lock().unwrap();
            for slot in 0..SQLITE_SHM_NLOCK {
                if shared_mask & (1 << slot) != 0 {
                    if node.a_lock[slot] > 0 {
                        node.a_lock[slot] -= 1;
                    }
                }
                if excl_mask & (1 << slot) != 0 {
                    node.a_lock[slot] = 0;
                }
            }
            self.shm_shared_mask.store(0, Ordering::SeqCst);
            self.shm_excl_mask.store(0, Ordering::SeqCst);
            if delete_flag {
                node.regions.clear();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ResultCode;
    use crate::vfs::shm_flags as F;

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

    // ---- xShmMap / xShmLock / xShmBarrier / xShmUnmap tests (M13.9) ----

    #[test]
    fn shm_map_returns_none_without_extend() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let vfs = MemVfs::new();
            let f = vfs
                .open("wal.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            // Non-extending request for region 0 of a fresh database: Ok(None).
            let r = f.shm_map(0, 32768, false).await.unwrap();
            assert!(r.is_none());
        });
    }

    #[test]
    fn shm_map_extend_allocates_zero_filled_region() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let vfs = MemVfs::new();
            let f = vfs
                .open("wal.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            let r = f.shm_map(0, 32768, true).await.unwrap().unwrap();
            let buf = r.lock().unwrap();
            assert_eq!(buf.len(), 32768);
            assert!(buf.iter().all(|&b| b == 0));
        });
    }

    #[test]
    fn shm_map_returns_shared_region_across_connections() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let vfs = MemVfs::new();
            let a = vfs
                .open("wal.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            let b = vfs.open("wal.db", OpenFlags::READWRITE).await.unwrap();
            // Connection A extends region 0; connection B's non-extending request sees it.
            let ra = a.shm_map(0, 32768, true).await.unwrap().unwrap();
            {
                let mut buf = ra.lock().unwrap();
                buf[0] = 42;
            }
            let rb = b.shm_map(0, 32768, false).await.unwrap().unwrap();
            // The two Arcs should be the same shared region (Arc::ptr_eq).
            assert!(Arc::ptr_eq(&ra, &rb));
            // B sees A's write.
            let buf = rb.lock().unwrap();
            assert_eq!(buf[0], 42);
        });
    }

    #[test]
    fn shm_map_returns_none_for_memory_db() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let vfs = MemVfs::new();
            let f = vfs
                .open(":memory:", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            let r = f.shm_map(0, 32768, true).await.unwrap();
            assert!(r.is_none());
        });
    }

    #[test]
    fn shm_lock_shared_coexist_across_connections() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let vfs = MemVfs::new();
            let a = vfs
                .open("wal.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            let b = vfs.open("wal.db", OpenFlags::READWRITE).await.unwrap();
            // Two SHARED locks on slot 3 (WAL_READ_LOCK(0)) coexist.
            a.shm_lock(3, 1, F::SHM_LOCK | F::SHM_SHARED)
                .await
                .unwrap();
            b.shm_lock(3, 1, F::SHM_LOCK | F::SHM_SHARED)
                .await
                .unwrap();
            a.shm_lock(3, 1, F::SHM_UNLOCK | F::SHM_SHARED)
                .await
                .unwrap();
            b.shm_lock(3, 1, F::SHM_UNLOCK | F::SHM_SHARED)
                .await
                .unwrap();
        });
    }

    #[test]
    fn shm_lock_exclusive_blocks_shared() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let vfs = MemVfs::new();
            let a = vfs
                .open("wal.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            let b = vfs.open("wal.db", OpenFlags::READWRITE).await.unwrap();
            // A takes EXCLUSIVE on slot 0 (WAL_WRITE_LOCK).
            a.shm_lock(0, 1, F::SHM_LOCK | F::SHM_EXCLUSIVE)
                .await
                .unwrap();
            // B's SHARED on slot 0 should fail with Busy.
            let err = b
                .shm_lock(0, 1, F::SHM_LOCK | F::SHM_SHARED)
                .await
                .unwrap_err();
            assert_eq!(err.code, ResultCode::Busy);
            // After A unlocks, B's SHARED succeeds.
            a.shm_lock(0, 1, F::SHM_UNLOCK | F::SHM_EXCLUSIVE)
                .await
                .unwrap();
            b.shm_lock(0, 1, F::SHM_LOCK | F::SHM_SHARED)
                .await
                .unwrap();
            b.shm_lock(3, 1, F::SHM_UNLOCK | F::SHM_SHARED)
                .await
                .unwrap();
        });
    }

    #[test]
    fn shm_lock_shared_unlock_with_other_holders_keeps_slot() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let vfs = MemVfs::new();
            let a = vfs
                .open("wal.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            let b = vfs.open("wal.db", OpenFlags::READWRITE).await.unwrap();
            let c = vfs.open("wal.db", OpenFlags::READWRITE).await.unwrap();
            // Three SHARED holders on slot 3.
            a.shm_lock(3, 1, F::SHM_LOCK | F::SHM_SHARED)
                .await
                .unwrap();
            b.shm_lock(3, 1, F::SHM_LOCK | F::SHM_SHARED)
                .await
                .unwrap();
            c.shm_lock(3, 1, F::SHM_LOCK | F::SHM_SHARED)
                .await
                .unwrap();
            // B unlocks — the slot is still held by A and C (count goes 3 -> 2).
            b.shm_lock(3, 1, F::SHM_UNLOCK | F::SHM_SHARED)
                .await
                .unwrap();
            // A new connection D's EXCLUSIVE should still fail (slot still SHARED).
            let d = vfs.open("wal.db", OpenFlags::READWRITE).await.unwrap();
            let err = d
                .shm_lock(3, 1, F::SHM_LOCK | F::SHM_EXCLUSIVE)
                .await
                .unwrap_err();
            assert_eq!(err.code, ResultCode::Busy);
            a.shm_lock(3, 1, F::SHM_UNLOCK | F::SHM_SHARED)
                .await
                .unwrap();
            c.shm_lock(3, 1, F::SHM_UNLOCK | F::SHM_SHARED)
                .await
                .unwrap();
        });
    }

    #[test]
    fn shm_lock_cannot_upgrade_shared_to_exclusive() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let vfs = MemVfs::new();
            let a = vfs
                .open("wal.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            a.shm_lock(3, 1, F::SHM_LOCK | F::SHM_SHARED)
                .await
                .unwrap();
            // Same connection tries to upgrade to EXCLUSIVE — upstream forbids this.
            let err = a
                .shm_lock(3, 1, F::SHM_LOCK | F::SHM_EXCLUSIVE)
                .await
                .unwrap_err();
            assert_eq!(err.code, ResultCode::IoErr);
            a.shm_lock(3, 1, F::SHM_UNLOCK | F::SHM_SHARED)
                .await
                .unwrap();
        });
    }

    #[test]
    fn shm_lock_exclusive_multi_slot_n_greater_than_one() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let vfs = MemVfs::new();
            let a = vfs
                .open("wal.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            let b = vfs.open("wal.db", OpenFlags::READWRITE).await.unwrap();
            // A takes EXCLUSIVE on slots 3..8 (all 5 reader locks at once, mirrors
            // `walLockExclusive(pWal, WAL_READ_LOCK(0), WAL_NREADER)`).
            a.shm_lock(3, 5, F::SHM_LOCK | F::SHM_EXCLUSIVE)
                .await
                .unwrap();
            // B's SHARED on any reader slot should fail.
            for slot in 3..8 {
                let err = b
                    .shm_lock(slot, 1, F::SHM_LOCK | F::SHM_SHARED)
                    .await
                    .unwrap_err();
                assert_eq!(err.code, ResultCode::Busy);
            }
            a.shm_lock(3, 5, F::SHM_UNLOCK | F::SHM_EXCLUSIVE)
                .await
                .unwrap();
        });
    }

    #[test]
    fn shm_unmap_drops_this_connections_locks() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let vfs = MemVfs::new();
            let a = vfs
                .open("wal.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            let b = vfs.open("wal.db", OpenFlags::READWRITE).await.unwrap();
            a.shm_lock(0, 1, F::SHM_LOCK | F::SHM_EXCLUSIVE)
                .await
                .unwrap();
            // A unmaps (drops its locks).
            a.shm_unmap(false).await.unwrap();
            // B can now take the EXCLUSIVE lock on slot 0.
            b.shm_lock(0, 1, F::SHM_LOCK | F::SHM_EXCLUSIVE)
                .await
                .unwrap();
            b.shm_lock(0, 1, F::SHM_UNLOCK | F::SHM_EXCLUSIVE)
                .await
                .unwrap();
        });
    }

    #[test]
    fn shm_unmap_delete_clears_regions() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let vfs = MemVfs::new();
            let a = vfs
                .open("wal.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            a.shm_map(0, 32768, true).await.unwrap();
            a.shm_map(1, 32768, true).await.unwrap();
            a.shm_unmap(true).await.unwrap();
            // After delete-flag unmap, a fresh extend should return a zero-filled region
            // (the previous regions were dropped, not preserved).
            let b = vfs.open("wal.db", OpenFlags::READWRITE).await.unwrap();
            let r = b.shm_map(0, 32768, true).await.unwrap().unwrap();
            let buf = r.lock().unwrap();
            assert!(buf.iter().all(|&x| x == 0));
        });
    }

    #[test]
    fn shm_barrier_is_a_noop() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let vfs = MemVfs::new();
            let f = vfs
                .open("wal.db", OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            // Just verify it does not panic.
            f.shm_barrier().await;
        });
    }
}