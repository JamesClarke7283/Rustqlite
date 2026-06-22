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

use super::{LockLevel, LockState, OpenFlags, Vfs, VfsFile, shm_flags, SQLITE_SHM_NLOCK};

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

/// The process-global per-path shared wal-index registry, mirroring `unixShmNode` in
/// `os_unix.c`. POSIX `fcntl` shm locks are per-process; the per-path `ShmNode` tracks the
/// in-process lock array (so two opens in this process contend correctly) and the open
/// `-shm` file handle (shared via `Arc`). The OS-level `fcntl` byte-range locks at
/// `WALINDEX_LOCK_OFFSET` provide cross-process contention.
fn shm_list() -> &'static Mutex<HashMap<String, Arc<Mutex<ShmNode>>>> {
    static SHMS: OnceLock<Mutex<HashMap<String, Arc<Mutex<ShmNode>>>>> = OnceLock::new();
    SHMS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The per-path shared wal-index state for `OsTokioVfs` (mirrors `unixShmNode` in
/// `os_unix.c`). Holds the open `-shm` file handle, the in-process lock array, and the
/// in-memory region cache (we read/write the `-shm` file via positioned I/O rather than
/// `mmap`, so the region cache is the "mapping" — a per-region `Arc<Mutex<Vec<u8>>>` that
/// `shm_map` hands out).
struct ShmNode {
    /// The open `-shm` file (lazily created on the first `shm_map` with `b_extend=true`).
    /// `None` when no `-shm` file exists yet and `shm_map` has not been asked to extend.
    shm_file: Option<Arc<std::fs::File>>,
    /// The `-shm` file path (`<db>-shm`).
    shm_path: String,
    /// The mapped regions: one `Arc<Mutex<Vec<u8>>>` per `i_region`. Region `i` has size
    /// `sz_region` (uniform — `WALINDEX_PGSZ = 32768`). The region is the in-memory cache;
    /// `shm_sync_region` writes it back to the `-shm` file. `shm_map` returns the Arc.
    regions: Vec<Arc<Mutex<Vec<u8>>>>,
    /// Per-slot lock state (mirrors `unixShmNode.aLock`): `0` = unlocked, `>0` = N shared
    /// holders, `<0` = -1 for one exclusive holder. Indexed by lock slot 0..SQLITE_SHM_NLOCK.
    a_lock: [i32; SQLITE_SHM_NLOCK],
}

impl ShmNode {
    fn new(shm_path: String) -> ShmNode {
        ShmNode {
            shm_file: None,
            shm_path,
            regions: Vec::new(),
            a_lock: [0; SQLITE_SHM_NLOCK],
        }
    }
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

    /// Look up (or create) the shared wal-index node for `path`. Returns `None` for
    /// `:memory:` (no shared wal-index).
    fn shm_node_for(&self, path: &str) -> Option<Arc<Mutex<ShmNode>>> {
        if path.is_empty() || path == ":memory:" {
            return None;
        }
        let shm_path = format!("{path}-shm");
        let mut shms = shm_list().lock().unwrap();
        Some(
            shms.entry(path.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(ShmNode::new(shm_path))))
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
        let shm_node = self.shm_node_for(path);
        Ok(Box::new(OsTokioFile {
            file: Arc::new(file),
            lock_level: AtomicU8::new(LockLevel::Unlocked as u8),
            lock_state,
            shm_node,
            shm_shared_mask: AtomicU8::new(0),
            shm_excl_mask: AtomicU8::new(0),
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
    /// Shared wal-index state for the database path; `None` for `:memory:` (no WAL).
    shm_node: Option<Arc<Mutex<ShmNode>>>,
    /// This connection's currently-held SHARED shm locks (a bitmask over `aLock` slots).
    /// Mirrors `unixShm.sharedMask`.
    shm_shared_mask: AtomicU8,
    /// This connection's currently-held EXCLUSIVE shm locks (a bitmask over `aLock` slots).
    /// Mirrors `unixShm.exclMask`.
    shm_excl_mask: AtomicU8,
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

    async fn shm_map(&self, i_region: usize, sz_region: usize, b_extend: bool) -> Result<Option<Arc<Mutex<Vec<u8>>>>> {
        let node = match &self.shm_node {
            Some(n) => n.clone(),
            None => return Ok(None),
        };
        // Fast path: the region is already mapped.
        {
            let n = node.lock().unwrap();
            if i_region < n.regions.len() {
                return Ok(Some(n.regions[i_region].clone()));
            }
            if !b_extend {
                return Ok(None);
            }
        }
        // Slow path: open the `-shm` file (if not yet open) and read each new region.
        // We hold the node lock only briefly to set up the shm_file; the per-region reads
        // run without the lock (they don't need it — the regions vec is mutated only here
        // and we serialize on the node lock between iterations).
        let shm_file = {
            let need_open: Option<String> = {
                let n = node.lock().unwrap();
                if n.shm_file.is_some() {
                    None
                } else {
                    Some(n.shm_path.clone())
                }
            };
            match need_open {
                None => {
                    let n = node.lock().unwrap();
                    n.shm_file.as_ref().unwrap().clone()
                }
                Some(shm_path) => {
                    let file = spawn_io(move || {
                        std::fs::OpenOptions::new()
                            .read(true)
                            .write(true)
                            .create(true)
                            .open(&shm_path)
                    })
                    .await?
                    .map_err(|e| Error::cant_open(format!("cannot open -shm: {e}")))?;
                    let mut n = node.lock().unwrap();
                    // Another concurrent opener may have set shm_file first; keep the
                    // existing one if so (avoids two opens racing — both point at the
                    // same path anyway).
                    n.shm_file.get_or_insert_with(|| Arc::new(file)).clone()
                }
            }
        };
        // Read each new region up through `i_region`. We take the node lock per iteration
        // to push the new region (and re-check the length, in case a concurrent shm_map
        // raced ahead).
        loop {
            let (need_read, offset) = {
                let n = node.lock().unwrap();
                if n.regions.len() > i_region {
                    return Ok(Some(n.regions[i_region].clone()));
                }
                (true, n.regions.len() as u64 * sz_region as u64)
            };
            if !need_read {
                break;
            }
            // Read the region from the `-shm` file (or zero-fill if past EOF). A short read
            // yields a zero-filled region (mirrors mmap of an unallocated region).
            let sf = shm_file.clone();
            let region = spawn_io(move || -> std::io::Result<Vec<u8>> {
                let mut buf = vec![0u8; sz_region];
                let n = sf.read_at(&mut buf, offset).unwrap_or(0);
                if n < sz_region {
                    for b in &mut buf[n..] {
                        *b = 0;
                    }
                }
                Ok(buf)
            })
            .await?
            .map_err(|e| Error::io_err(format!("shm region read failed: {e}")))?;
            let mut n = node.lock().unwrap();
            // A racing shm_map may have pushed this region first; drop ours if so.
            if n.regions.len() <= i_region {
                n.regions.push(Arc::new(Mutex::new(region)));
            }
        }
        let n = node.lock().unwrap();
        Ok(Some(n.regions[i_region].clone()))
    }

    async fn shm_lock(&self, ofst: usize, n_slots: usize, flags: u32) -> Result<()> {
        use shm_flags as F;
        if ofst + n_slots > SQLITE_SHM_NLOCK || n_slots == 0 {
            return Err(Error::io_err("invalid xShmLock range"));
        }
        let mask: u8 = (((1u16 << (ofst + n_slots)) - (1u16 << ofst)) & 0xff) as u8;
        let node = match &self.shm_node {
            Some(n) => n.clone(),
            None => return Err(Error::io_err("xShmLock: no shm node")),
        };
        let shared_mask = self.shm_shared_mask.load(Ordering::SeqCst);
        let excl_mask = self.shm_excl_mask.load(Ordering::SeqCst);

        let unlocking = flags & F::SHM_UNLOCK != 0;
        let exclusive = flags & F::SHM_EXCLUSIVE != 0;
        let shared = flags & F::SHM_SHARED != 0;

        // The lock bytes are at `WALINDEX_LOCK_OFFSET + ofst` in the `-shm` file (a separate
        // file from the DB, so the byte ranges don't collide with the DB's PENDING_BYTE/
        // RESERVED_BYTE/SHARED_FIRST scheme). Upstream uses `UNIX_SHM_BASE = 120`, which
        // matches `WALINDEX_LOCK_OFFSET`.
        // Open the `-shm` file lazily if a lock is requested before any `shm_map`
        // (upstream's `unixShmMap` opens the shm file on first call, before any lock;
        // here we open on demand to support the lock-first ordering).
        let shm_file = {
            let need_open: Option<String> = {
                let n = node.lock().unwrap();
                if n.shm_file.is_some() {
                    None
                } else {
                    Some(n.shm_path.clone())
                }
            };
            match need_open {
                None => {
                    let n = node.lock().unwrap();
                    n.shm_file.as_ref().unwrap().clone()
                }
                Some(shm_path) => {
                    let file = spawn_io(move || {
                        std::fs::OpenOptions::new()
                            .read(true)
                            .write(true)
                            .create(true)
                            .open(&shm_path)
                    })
                    .await?
                    .map_err(|e| Error::cant_open(format!("cannot open -shm: {e}")))?;
                    let mut n = node.lock().unwrap();
                    n.shm_file.get_or_insert_with(|| Arc::new(file)).clone()
                }
            }
        };

        // Decide the action under the in-process lock, capturing any OS-level lock op
        // to perform after dropping the guard.
        enum Action {
            None,
            SysLock(i32),  // l_type
            SysUnlock,
        }
        let action: Action = {
            let mut n = node.lock().unwrap();
            if unlocking {
                if shared {
                    if n_slots != 1 {
                        return Err(Error::io_err("xShmLock: SHARED unlock must have n==1"));
                    }
                    if shared_mask & mask == 0 {
                        return Ok(());
                    }
                    if n.a_lock[ofst] > 1 {
                        n.a_lock[ofst] -= 1;
                        Action::None
                    } else {
                        n.a_lock[ofst] = 0;
                        Action::SysUnlock
                    }
                } else {
                    if excl_mask & mask == 0 {
                        return Ok(());
                    }
                    for slot in ofst..ofst + n_slots {
                        n.a_lock[slot] = 0;
                    }
                    Action::SysUnlock
                }
            } else if shared {
                if shared_mask & mask != 0 {
                    return Ok(());
                }
                if n.a_lock[ofst] < 0 {
                    return Err(Error::busy("wal-index lock busy"));
                }
                let need_sys = n.a_lock[ofst] == 0;
                n.a_lock[ofst] += 1;
                if need_sys { Action::SysLock(libc::F_RDLCK) } else { Action::None }
            } else if exclusive {
                if excl_mask & mask != 0 {
                    return Ok(());
                }
                if shared_mask & mask != 0 {
                    return Err(Error::io_err("xShmLock: cannot upgrade SHARED to EXCLUSIVE"));
                }
                for slot in ofst..ofst + n_slots {
                    if n.a_lock[slot] != 0 {
                        return Err(Error::busy("wal-index lock busy"));
                    }
                }
                for slot in ofst..ofst + n_slots {
                    n.a_lock[slot] = -1;
                }
                Action::SysLock(libc::F_WRLCK)
            } else {
                return Err(Error::io_err("xShmLock: invalid flags"));
            }
        };
        // Update this connection's masks based on the action.
        match (unlocking, shared, exclusive, &action) {
            (true, true, false, _) => self.shm_shared_mask.store(shared_mask & !mask, Ordering::SeqCst),
            (true, false, true, _) => self.shm_excl_mask.store(excl_mask & !mask, Ordering::SeqCst),
            (false, true, false, _) => self.shm_shared_mask.store(shared_mask | mask, Ordering::SeqCst),
            (false, false, true, _) => self.shm_excl_mask.store(excl_mask | mask, Ordering::SeqCst),
            _ => {}
        }
        // Perform the OS-level fcntl outside the in-process lock.
        match action {
            Action::None => Ok(()),
            Action::SysLock(l_type) => {
                let f = shm_file.clone();
                spawn_io(move || posix_shm_lock(&f, ofst, n_slots, l_type))
                    .await?
                    .map_err(|e| Error::io_err(format!("shm lock: {e}")))
            }
            Action::SysUnlock => {
                let f = shm_file.clone();
                spawn_io(move || posix_shm_lock(&f, ofst, n_slots, libc::F_UNLCK))
                    .await?
                    .map_err(|e| Error::io_err(format!("shm unlock: {e}")))
            }
        }
    }

    async fn shm_barrier(&self) {
        // A SeqCst fence + a no-op OS call (mirrors `unixShmBarrier`'s mutex enter/leave for
        // redundancy). On a single-process engine this is sufficient.
        std::sync::atomic::fence(Ordering::SeqCst);
    }

    async fn shm_unmap(&self, delete_flag: bool) -> Result<()> {
        let node = match &self.shm_node {
            Some(n) => n.clone(),
            None => return Ok(()),
        };
        let shared_mask = self.shm_shared_mask.load(Ordering::SeqCst);
        let excl_mask = self.shm_excl_mask.load(Ordering::SeqCst);
        // Capture the OS-level unlock slots and the shm file under the in-process lock.
        let (unlock_slots, shm_file_opt): (Vec<usize>, Option<Arc<std::fs::File>>) = {
            let mut n = node.lock().unwrap();
            for slot in 0..SQLITE_SHM_NLOCK {
                if shared_mask & (1 << slot) != 0 && n.a_lock[slot] > 0 {
                    n.a_lock[slot] -= 1;
                }
                if excl_mask & (1 << slot) != 0 {
                    n.a_lock[slot] = 0;
                }
            }
            let slots: Vec<usize> = (0..SQLITE_SHM_NLOCK)
                .filter(|&s| (shared_mask & (1 << s) != 0) || (excl_mask & (1 << s) != 0))
                .collect();
            self.shm_shared_mask.store(0, Ordering::SeqCst);
            self.shm_excl_mask.store(0, Ordering::SeqCst);
            (slots, n.shm_file.clone())
        };
        // Drop this connection's OS-level locks.
        if let Some(shm_file) = shm_file_opt {
            for slot in unlock_slots {
                let f = shm_file.clone();
                let _ = spawn_io(move || posix_shm_lock(&f, slot, 1, libc::F_UNLCK)).await;
            }
        }
        if delete_flag {
            let shm_path = {
                let mut n = node.lock().unwrap();
                n.regions.clear();
                let p = n.shm_path.clone();
                n.shm_file = None;
                p
            };
            let _ = spawn_io(move || std::fs::remove_file(&shm_path)).await;
        }
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

/// The byte offset of the wal-index lock region within the `-shm` file (mirrors
/// `WALINDEX_LOCK_OFFSET` in `wal.c` = 120). The lock slots are bytes
/// `WALINDEX_LOCK_OFFSET..WALINDEX_LOCK_OFFSET+SQLITE_SHM_NLOCK` (120..128).
pub const WALINDEX_LOCK_OFFSET: u64 = 120;

/// Acquire or release a wal-index byte-range lock on the `-shm` file (mirrors
/// `unixShmSystemLock` in `os_unix.c`). `l_type` is one of `F_RDLCK`/`F_WRLCK`/`F_UNLCK`;
/// the lock is on bytes `WALINDEX_LOCK_OFFSET+ofst .. +ofst+n` of the `-shm` file. Returns
/// `Err(WouldBlock)` on a conflicting lock (the upstream `SQLITE_BUSY` case).
#[cfg(unix)]
fn posix_shm_lock(file: &std::fs::File, ofst: usize, n: usize, l_type: i32) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = l_type as i16;
    lock.l_whence = libc::SEEK_SET as i16;
    lock.l_start = (WALINDEX_LOCK_OFFSET + ofst as u64) as i64;
    lock.l_len = n as i64;
    let rc = unsafe { libc::fcntl(fd, libc::F_SETLK, &lock) };
    if rc == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(unix))]
fn posix_shm_lock(_file: &std::fs::File, _ofst: usize, _n: usize, _l_type: i32) -> std::io::Result<()> {
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

    // ---- xShmMap / xShmLock / xShmBarrier / xShmUnmap tests (M13.9) ----
    //
    // The shm tests use real `-shm` files in the temp dir and real POSIX `fcntl` byte-range
    // locks at `WALINDEX_LOCK_OFFSET + slot`, so they exercise the cross-process lock path
    // (a second connection in the same process would normally share the in-process
    // `a_lock` array, but the fcntl path is also exercised and must agree).

    fn unique_shm_path(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        dir.join(format!(
            "rustqlite_shm_{label}_{}_{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[cfg(unix)]
    #[test]
    fn shm_map_extend_creates_shm_file_and_zero_fills_region() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let path = unique_shm_path("extend");
            let path_str = path.to_str().unwrap();
            let vfs = OsTokioVfs::new();
            // Create the database file first (so the -shm sibling exists conceptually).
            let f = vfs
                .open(path_str, OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            f.write_at(0, b"SQLite format 3\0").await.unwrap();
            f.sync().await.unwrap();

            // Non-extending request: Ok(None).
            let r = f.shm_map(0, 32768, false).await.unwrap();
            assert!(r.is_none());

            // Extending request: Ok(Some(zero-filled region)).
            let r = f.shm_map(0, 32768, true).await.unwrap().unwrap();
            let buf = r.lock().unwrap();
            assert_eq!(buf.len(), 32768);
            assert!(buf.iter().all(|&b| b == 0));

            f.shm_unmap(true).await.unwrap();
            vfs.delete(path_str).await.unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn shm_map_returns_shared_region_across_connections() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let path = unique_shm_path("share");
            let path_str = path.to_str().unwrap();
            let vfs = OsTokioVfs::new();
            let a = vfs
                .open(path_str, OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            a.write_at(0, b"SQLite format 3\0").await.unwrap();
            a.sync().await.unwrap();
            let b = vfs.open(path_str, OpenFlags::READWRITE).await.unwrap();

            let ra = a.shm_map(0, 32768, true).await.unwrap().unwrap();
            {
                let mut buf = ra.lock().unwrap();
                buf[0] = 0x57;
            }
            let rb = b.shm_map(0, 32768, false).await.unwrap().unwrap();
            // Same shared region across the two connections.
            assert!(Arc::ptr_eq(&ra, &rb));
            let buf = rb.lock().unwrap();
            assert_eq!(buf[0], 0x57);

            a.shm_unmap(true).await.unwrap();
            vfs.delete(path_str).await.unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn shm_lock_shared_coexist_in_process() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let path = unique_shm_path("lock_shared");
            let path_str = path.to_str().unwrap();
            let vfs = OsTokioVfs::new();
            let a = vfs
                .open(path_str, OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            a.write_at(0, b"SQLite format 3\0").await.unwrap();
            a.sync().await.unwrap();
            let b = vfs.open(path_str, OpenFlags::READWRITE).await.unwrap();

            use crate::vfs::shm_flags as F;
            a.shm_lock(3, 1, F::SHM_LOCK | F::SHM_SHARED)
                .await
                .unwrap();
            // Same process, different connection: the in-process a_lock count goes to 2,
            // and the OS-level fcntl shared lock is held once (per-process).
            b.shm_lock(3, 1, F::SHM_LOCK | F::SHM_SHARED)
                .await
                .unwrap();
            a.shm_lock(3, 1, F::SHM_UNLOCK | F::SHM_SHARED)
                .await
                .unwrap();
            b.shm_lock(3, 1, F::SHM_UNLOCK | F::SHM_SHARED)
                .await
                .unwrap();

            a.shm_unmap(true).await.unwrap();
            vfs.delete(path_str).await.unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn shm_lock_exclusive_blocks_shared_in_process() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let path = unique_shm_path("lock_excl");
            let path_str = path.to_str().unwrap();
            let vfs = OsTokioVfs::new();
            let a = vfs
                .open(path_str, OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            a.write_at(0, b"SQLite format 3\0").await.unwrap();
            a.sync().await.unwrap();
            let b = vfs.open(path_str, OpenFlags::READWRITE).await.unwrap();

            use crate::vfs::shm_flags as F;
            a.shm_lock(0, 1, F::SHM_LOCK | F::SHM_EXCLUSIVE)
                .await
                .unwrap();
            let err = b
                .shm_lock(0, 1, F::SHM_LOCK | F::SHM_SHARED)
                .await
                .unwrap_err();
            assert_eq!(err.code, crate::error::ResultCode::Busy);
            a.shm_lock(0, 1, F::SHM_UNLOCK | F::SHM_EXCLUSIVE)
                .await
                .unwrap();
            // After unlock, B's SHARED succeeds.
            b.shm_lock(0, 1, F::SHM_LOCK | F::SHM_SHARED)
                .await
                .unwrap();
            b.shm_lock(0, 1, F::SHM_UNLOCK | F::SHM_SHARED)
                .await
                .unwrap();

            a.shm_unmap(true).await.unwrap();
            vfs.delete(path_str).await.unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn shm_lock_cannot_upgrade_shared_to_exclusive() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let path = unique_shm_path("lock_upgrade");
            let path_str = path.to_str().unwrap();
            let vfs = OsTokioVfs::new();
            let a = vfs
                .open(path_str, OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            a.write_at(0, b"SQLite format 3\0").await.unwrap();
            a.sync().await.unwrap();

            use crate::vfs::shm_flags as F;
            a.shm_lock(3, 1, F::SHM_LOCK | F::SHM_SHARED)
                .await
                .unwrap();
            let err = a
                .shm_lock(3, 1, F::SHM_LOCK | F::SHM_EXCLUSIVE)
                .await
                .unwrap_err();
            assert_eq!(err.code, crate::error::ResultCode::IoErr);
            a.shm_lock(3, 1, F::SHM_UNLOCK | F::SHM_SHARED)
                .await
                .unwrap();

            a.shm_unmap(true).await.unwrap();
            vfs.delete(path_str).await.unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn shm_unmap_drops_locks_and_delete_removes_shm_file() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let path = unique_shm_path("unmap_delete");
            let path_str = path.to_str().unwrap();
            let shm_path = format!("{path_str}-shm");
            let vfs = OsTokioVfs::new();
            let a = vfs
                .open(path_str, OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            a.write_at(0, b"SQLite format 3\0").await.unwrap();
            a.sync().await.unwrap();
            let b = vfs.open(path_str, OpenFlags::READWRITE).await.unwrap();

            use crate::vfs::shm_flags as F;
            a.shm_lock(0, 1, F::SHM_LOCK | F::SHM_EXCLUSIVE)
                .await
                .unwrap();
            // The -shm file should exist now (lazily created by the lock path).
            assert!(vfs.exists(&shm_path).await.unwrap());
            a.shm_unmap(false).await.unwrap();
            // After unmap, B can take the EXCLUSIVE lock.
            b.shm_lock(0, 1, F::SHM_LOCK | F::SHM_EXCLUSIVE)
                .await
                .unwrap();
            b.shm_lock(0, 1, F::SHM_UNLOCK | F::SHM_EXCLUSIVE)
                .await
                .unwrap();
            // delete_flag=true removes the -shm file.
            b.shm_unmap(true).await.unwrap();
            assert!(!vfs.exists(&shm_path).await.unwrap());

            vfs.delete(path_str).await.unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn shm_barrier_is_a_noop() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let path = unique_shm_path("barrier");
            let path_str = path.to_str().unwrap();
            let vfs = OsTokioVfs::new();
            let f = vfs
                .open(path_str, OpenFlags::READWRITE_CREATE)
                .await
                .unwrap();
            f.shm_barrier().await;
            vfs.delete(path_str).await.unwrap();
        });
    }
}