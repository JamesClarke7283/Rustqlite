# WAL shared-memory VFS methods (M13.9)

Rustqlite's VFS shared-memory (`xShmMap` / `xShmLock` / `xShmBarrier` / `xShmUnmap`)
implementation, mirroring the upstream `sqlite3_io_methods` shm entries and `os_unix.c`'s
`unixShmMap` / `unixShmLock` / `unixShmBarrier` / `unixShmUnmap`.

## Scope

M13.9 adds the shm trait surface and a per-path shared wal-index backing store to both
VFS implementations. The `Wal` runtime still keeps the in-memory `IndexBlock` vec as the
authoritative wal-index (it is rebuilt on recovery); the shm methods are the
infrastructure for the eventual cross-process reader path (M13.12). The lock semantics
match upstream so a second Rustqlite connection in the same process contends correctly,
and a cross-process `sqlite3` binary holding a WAL lock would block a Rustqlite writer
and vice versa.

## API surface

`crates/rustsqlite-core/src/vfs/mod.rs`:

* `SQLITE_SHM_NLOCK = 8` (mirrors `sqlite3.h`).
* `shm_flags::{SHM_UNLOCK, SHM_LOCK, SHARED, EXCLUSIVE}` (the `flags` bit values for
  `xShmLock`).
* `VfsFile::shm_map(i_region, sz_region, b_extend) -> Result<Option<Arc<Mutex<Vec<u8>>>>>`
  — returns the mapped region, or `Ok(None)` when `!b_extend` and the region is not
  allocated. Default: `SQLITE_IOERR_SHMMAP` (a non-WAL VFS refuses).
* `VfsFile::shm_lock(ofst, n, flags) -> Result<()>` — acquire/release shared or exclusive
  on `ofst..ofst+n` slots. Default: `SQLITE_IOERR_SHMLOCK`.
* `VfsFile::shm_barrier()` — memory barrier (default no-op, sufficient for in-process).
* `VfsFile::shm_unmap(delete_flag) -> Result<()>` — drop this connection's mapping; when
  `delete_flag`, remove the `-shm` file (default no-op).

The region type is `Arc<Mutex<Vec<u8>>>` (Rust's safe analogue of `volatile void *`).
`shm_map` returns an `Arc` clone; readers and writers lock the `Mutex` to read/write the
region bytes (mirrors how upstream's `walIndexPage` returns a `volatile u32 *` that
callers walk without a per-access lock).

## `MemVfs`

Per-path `ShmNode` (in `MemVfs::shms: Mutex<HashMap<String, Arc<Mutex<ShmNode>>>>`):

* `regions: Vec<Arc<Mutex<Vec<u8>>>>` — the mapped regions.
* `a_lock: [i32; SQLITE_SHM_NLOCK]` — `0` = unlocked, `>0` = N shared holders, `-1` =
  one exclusive holder (mirrors `unixShmNode.aLock`).

`MemFile` carries `shm_shared_mask: AtomicU8` + `shm_excl_mask: AtomicU8` (the
per-connection snapshots, mirrors `unixShm.sharedMask`/`exclMask`).

`shm_lock` walks the four cases (unlock-shared, unlock-exclusive, lock-shared,
lock-exclusive) exactly like `unixShmLock`:

* SHARED unlock with N>1 in-process holders just decrements `a_lock[ofst]` (the OS lock
  stays; mirrors `bUnlock = 0`).
* SHARED lock when `a_lock[ofst] == 0` is the first in-process SHARED — no OS call needed
  (in-process only VFS).
* EXCLUSIVE lock refuses if any `a_lock[slot] != 0` (Busy).
* EXCLUSIVE unlock clears all `n` slots.
* SHARED → EXCLUSIVE direct upgrade is refused (IoErr), matching upstream's rule.

`:memory:` databases have no `ShmNode` (`shm_node_for` returns `None`), so `shm_map`
returns `Ok(None)` and `shm_lock` returns `Err(IoErr)` — a private database has no WAL.

## `OsTokioVfs`

Per-path `ShmNode` in a process-global registry (`shm_list()`), mirroring `unixShmNode`:

* `shm_file: Option<Arc<std::fs::File>>` — the open `<db>-shm` file (lazily opened on
  first `shm_map` extend or first `shm_lock`).
* `shm_path: String` — `<db>-shm`.
* `regions: Vec<Arc<Mutex<Vec<u8>>>>` — the in-memory region cache; `shm_map` reads each
  new region from the `-shm` file (zero-filled if past EOF, mirroring mmap of an
  unallocated region).
* `a_lock: [i32; SQLITE_SHM_NLOCK]` — same as `MemVfs`.

`shm_lock` performs two-level locking (mirrors `unixShmLock`):

1. In-process `a_lock` array tracks shared counts / exclusive holder so two connections
   in this process contend correctly (POSIX `fcntl` locks are per-process and would miss
   same-process contention).
2. OS-level `fcntl(F_SETLK)` byte-range locks on bytes
   `WALINDEX_LOCK_OFFSET..WALINDEX_LOCK_OFFSET+SQLITE_SHM_NLOCK` of the `-shm` file
   (offset 120..128, matching `UNIX_SHM_BASE == WALINDEX_LOCK_OFFSET == 120`).

   * `F_RDLCK` for a SHARED lock.
   * `F_WRLCK` for an EXCLUSIVE lock.
   * `F_UNLCK` to release.

   Returns `SQLITE_BUSY` when a cross-process lock conflicts.

`shm_unmap(delete_flag)` drops this connection's locks (clearing the `a_lock`
contribution and issuing `F_UNLCK` for each slot this connection held), and when
`delete_flag` is set it `remove_file`s the `-shm` path (mirrors `sqlite3OsDelete` after
`sqlite3WalClose`).

## Lock slot indices (mirrors `wal.c`)

| Slot | Constant | Purpose |
|------|---------|---------|
| 0 | `WAL_WRITE_LOCK` | held by the active writer |
| 1 | `WAL_CKPT_LOCK` | held by the active checkpointer |
| 2 | `WAL_RECOVER_LOCK` | held during wal-index recovery |
| 3 | `WAL_READ_LOCK(0)` | reader read-mark 0 (always reads the whole WAL) |
| 4..7 | `WAL_READ_LOCK(1..=4)` | reader read-marks 1..4 |

The constants live in `crates/rustsqlite-core/src/format/wal_index.rs`.

## What M13.9 does NOT do

* The `Wal` runtime (`crates/rustsqlite-core/src/pager/wal.rs`) is not yet migrated to
  read/write the wal-index via `shm_map` — it still uses the in-memory `IndexBlock` vec.
  The shm methods are infrastructure for the cross-process reader path (M13.12).
* No `mmap` — regions are read/written via positioned I/O (`read_at`/`write_at`). The
  in-memory region cache is the "mapping". This is functionally equivalent for our
  single-process engine; a multi-process engine would need real `mmap` to share the
  region bytes across processes.
* WAL recovery (M13.11) and concurrent readers (M13.12) are separate tasks.