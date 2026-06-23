//! The database connection handle — `sqlite3 *` (mirrors `main.c`).

use std::sync::{Arc, Mutex};

use crate::error::{Error, Result, ResultCode};
use crate::pager::Pager;
use crate::schema::{read_catalog, Catalog};
use crate::types::Value;
use crate::vfs::{MemVfs, OpenFlags, OsTokioVfs, Vfs};

use super::runtime::block_on;

/// The default page size for a freshly created database (`SQLITE_DEFAULT_PAGE_SIZE` is 4096 in the
/// 3.x default build).
const DEFAULT_PAGE_SIZE: u32 = 4096;

/// The connection's change counters, shared with the in-flight statement so it can publish its
/// results back when it finishes (mirrors `db->nChange` / `db->lastRowid`). A C `sqlite3_stmt`
/// updates these on its parent `sqlite3` directly; we share them by `Arc` since the Rust statement
/// does not borrow the connection.
#[derive(Default)]
pub(crate) struct ChangeCounts {
    /// Rows changed by the most recently executed statement.
    pub changes: i64,
    /// Rows changed since the connection opened.
    pub total_changes: i64,
    /// Rowid of the last successful insert (persists until the next insert).
    pub last_insert_rowid: i64,
}

/// A database connection. The Rust analogue of `sqlite3 *`.
///
/// Opened with [`sqlite3_open`]/[`sqlite3_open_v2`]. The `sqlite3_*` free functions and the
/// methods here mirror the C API; richer error information (a [`ResultCode`] plus a message)
/// is returned as [`Error`] rather than a bare integer.
///
/// The [`Pager`] is held behind an `Arc` so that a prepared statement
/// ([`super::stmt::Sqlite3Stmt`]) can own a cheap clone of it and drive the async read path
/// from `sqlite3_step` without borrowing the connection — mirroring how a C `sqlite3_stmt`
/// holds a pointer back to its `sqlite3`.
pub struct Sqlite3 {
    pager: Option<Arc<Pager>>,
    /// The VFS this connection opened through, retained so a write to a still-empty database can
    /// lazily create the file's page 1 (header + empty `sqlite_schema` leaf) on the first DDL.
    vfs: Arc<dyn Vfs>,
    filename: String,
    read_only: bool,
    /// The change counters, shared by `Arc` with the in-flight statement so it can publish its
    /// `changes`/`last_insert_rowid` back when it steps to completion.
    counts: Arc<Mutex<ChangeCounts>>,
    /// The autocommit flag, shared by `Arc` with the in-flight VDBE so `OP_AutoCommit` and
    /// `OP_Halt` can consult and mutate it. `true` (the default) means the connection is in
    /// autocommit mode — each statement commits independently. `BEGIN` sets this to `false`;
    /// `COMMIT`/`ROLLBACK` set it back to `true` and commit/roll back the pending transaction.
    /// Mirrors `db->autoCommit` in `main.c`. See [`Self::autocommit`] / [`Self::set_autocommit`].
    autocommit: Arc<Mutex<bool>>,
    /// `db->isTransactionSavepoint` from `main.c`: `true` when the outermost savepoint on the
    /// stack was created by `SAVEPOINT …` while the connection was in autocommit mode (so it
    /// auto-started an implicit transaction). `RELEASE` of that outermost savepoint commits the
    /// transaction; any other release/rollback just pops the stack. Shared by `Arc` with the
    /// in-flight VDBE so `OP_Savepoint` and `OP_AutoCommit` can consult/mutate it.
    is_transaction_savepoint: Arc<Mutex<bool>>,
    /// `db->flags & SQLITE_ForeignKeys` from `main.c` — `true` when foreign-key constraint
    /// enforcement is enabled via `PRAGMA foreign_keys = ON`. Default is OFF (matching upstream
    /// unless `SQLITE_DEFAULT_FOREIGN_KEYS` is defined). The flag may only be toggled outside a
    /// transaction (upstream's `PragTyp_FLAG` path masks `SQLITE_ForeignKeys` out when
    /// `db->autoCommit == 0`). Enforcement itself is M17.6+; this flag is the read/write
    /// surface for `PRAGMA foreign_keys`.
    foreign_keys: Arc<Mutex<bool>>,
    /// `PRAGMA synchronous` — 0=OFF, 1=NORMAL, 2=FULL (default), 3=EXTRA. Per-connection
    /// (not persisted in the header). Currently informational — the pager always syncs on
    /// commit.
    synchronous: Arc<Mutex<u8>>,
    /// `PRAGMA cache_size` — the page cache size in pages (negative = kibibytes, matching
    /// the legacy `PRAGMA default_cache_size`). Per-connection (the header's
    /// `default_cache_size` is the persistent default). Currently informational — the pager
    /// does not enforce a page cache limit yet (M32.1).
    cache_size: Arc<Mutex<i32>>,
    last_error: Option<Error>,
}

/// `sqlite3_open()` — open (creating if necessary) a database for reading and writing.
pub fn sqlite3_open(filename: &str) -> Result<Sqlite3> {
    sqlite3_open_v2(filename, OpenFlags::READWRITE_CREATE)
}

/// `sqlite3_open_v2()` — open a database with explicit flags.
///
/// `:memory:` and the empty string open a private in-memory database. A brand-new or empty file
/// has no pages yet; the pager is created lazily on the first write (`CREATE TABLE`) — see
/// [`Sqlite3::ensure_pager`] — so opening an empty file and immediately creating a table works.
pub fn sqlite3_open_v2(filename: &str, flags: OpenFlags) -> Result<Sqlite3> {
    block_on(async move {
        let is_memory = filename.is_empty() || filename == ":memory:";
        let vfs: Arc<dyn Vfs> = if is_memory {
            Arc::new(MemVfs::new())
        } else {
            Arc::new(OsTokioVfs::new())
        };

        let file = vfs.open(filename, flags).await?;
        let size = file.file_size().await?;
        // An empty file has no header yet (a brand-new or in-memory database). Defer pager
        // creation until the first write (`ensure_pager`); a read of such a handle still errors.
        let pager = if size == 0 {
            None
        } else {
            let opened = Pager::open(vfs.clone(), filename.to_string(), file).await?;
            Some(Arc::new(opened))
        };

        Ok(Sqlite3 {
            pager,
            vfs: vfs.clone(),
            filename: filename.to_string(),
            read_only: flags.is_readonly(),
            counts: Arc::new(Mutex::new(ChangeCounts::default())),
            autocommit: Arc::new(Mutex::new(true)),
            is_transaction_savepoint: Arc::new(Mutex::new(false)),
            foreign_keys: Arc::new(Mutex::new(false)),
            synchronous: Arc::new(Mutex::new(2)),
            cache_size: Arc::new(Mutex::new(-2000)),
            last_error: None,
        })
    })
}

impl Sqlite3 {
    /// `sqlite3_close()` — close the connection. Resources are freed on drop; this exists for
    /// C-API symmetry and always reports success for a read-only handle.
    pub fn close(self) -> ResultCode {
        ResultCode::Ok
    }

    /// `sqlite3_errmsg()` — the message of the most recent error on this connection.
    pub fn errmsg(&self) -> &str {
        match &self.last_error {
            Some(e) => &e.message,
            None => "not an error",
        }
    }

    /// `sqlite3_errcode()` — the primary result code of the most recent error.
    pub fn errcode(&self) -> ResultCode {
        self.last_error.as_ref().map_or(ResultCode::Ok, |e| e.code)
    }

    /// `sqlite3_extended_errcode()` — the extended result code of the most recent error.
    pub fn extended_errcode(&self) -> i32 {
        self.last_error
            .as_ref()
            .map_or(ResultCode::Ok.code(), |e| e.extended_code)
    }

    /// `sqlite3_changes()` — rows modified by the most recently executed statement.
    pub fn changes(&self) -> i64 {
        self.counts.lock().unwrap().changes
    }

    /// `sqlite3_total_changes()` — rows modified since the connection opened.
    pub fn total_changes(&self) -> i64 {
        self.counts.lock().unwrap().total_changes
    }

    /// `sqlite3_last_insert_rowid()` — rowid of the last successful insert (persists across
    /// statements until the next insert).
    pub fn last_insert_rowid(&self) -> i64 {
        self.counts.lock().unwrap().last_insert_rowid
    }

    /// The filename this connection was opened with.
    pub fn filename(&self) -> &str {
        &self.filename
    }

    pub fn is_readonly(&self) -> bool {
        self.read_only
    }

    /// A clone of the database header, if the database has been opened with content. Used by
    /// the shell's `.dbinfo`. (Engine-internal convenience, not part of the C API.)
    pub fn db_header(&self) -> Option<crate::format::DbHeader> {
        self.pager.as_ref().map(|p| p.header())
    }

    /// The number of pages in the database file (0 if there is no content yet).
    pub fn page_count(&self) -> u32 {
        self.pager.as_ref().map_or(0, |p| p.page_count())
    }

    fn pager(&self) -> Result<&Pager> {
        self.pager.as_deref().ok_or_else(|| {
            Error::msg(format!(
                "database \"{}\" has no pages yet (write path pending)",
                self.filename
            ))
        })
    }

    /// Record an error as the connection's most recent (so `errmsg`/`errcode` report it).
    /// Engine-internal (used by `sqlite3_prepare_v2`).
    pub(crate) fn set_last_error(&mut self, e: Error) {
        self.last_error = Some(e);
    }

    /// A cheap `Arc` clone of the pager, for handing to a prepared statement so it can drive
    /// the async read path from `sqlite3_step` without borrowing the connection. Engine-internal
    /// (used by [`super::stmt::sqlite3_prepare_v2`]).
    pub(crate) fn pager_arc(&self) -> Result<Arc<Pager>> {
        self.pager.clone().ok_or_else(|| {
            Error::msg(format!(
                "database \"{}\" has no pages yet (write path pending)",
                self.filename
            ))
        })
    }

    /// Ensure a pager exists, creating a fresh database file (page 1 = header + an empty
    /// `sqlite_schema` leaf) on the first write to an empty/new file. Returns an `Arc` clone of
    /// the pager. Engine-internal (used by the write prepare path for `CREATE TABLE`).
    ///
    /// Mirrors how C SQLite lays down page 1 the first time a connection writes to a zero-length
    /// file. A read-only connection cannot create the database.
    pub(crate) fn ensure_pager(&mut self) -> Result<Arc<Pager>> {
        if let Some(p) = &self.pager {
            return Ok(p.clone());
        }
        if self.read_only {
            return Err(Error::msg("attempt to write a readonly database"));
        }
        let vfs = self.vfs.clone();
        let filename = self.filename.clone();
        let pager = block_on(async move {
            let file = vfs.open(&filename, OpenFlags::READWRITE_CREATE).await?;
            Pager::create_fresh(vfs.clone(), filename.clone(), file, DEFAULT_PAGE_SIZE).await
        })?;
        let pager = Arc::new(pager);
        self.pager = Some(pager.clone());
        Ok(pager)
    }

    /// A clone of the shared change-counter handle, for a write statement to publish its results
    /// into when it steps to completion. Engine-internal (used by [`super::stmt`]).
    pub(crate) fn counts_handle(&self) -> Arc<Mutex<ChangeCounts>> {
        self.counts.clone()
    }

    /// `sqlite3_get_autocommit()` — return `true` if the connection is in autocommit mode (no
    /// explicit `BEGIN` is active). Mirrors `db->autoCommit` in `main.c`.
    pub fn autocommit(&self) -> bool {
        *self.autocommit.lock().unwrap()
    }

    /// A clone of the shared autocommit-flag handle, for the in-flight VDBE so `OP_AutoCommit`
    /// and `OP_Halt` can consult and mutate it. Engine-internal (used by [`super::stmt`]).
    pub(crate) fn autocommit_handle(&self) -> Arc<Mutex<bool>> {
        self.autocommit.clone()
    }

    /// A clone of the shared `is_transaction_savepoint` handle, for the in-flight VDBE so
    /// `OP_Savepoint` and `OP_AutoCommit` can consult/mutate it. Engine-internal (used by
    /// [`super::stmt`]). Mirrors `db->isTransactionSavepoint` in `main.c`.
    pub(crate) fn is_transaction_savepoint_handle(&self) -> Arc<Mutex<bool>> {
        self.is_transaction_savepoint.clone()
    }

    /// `PRAGMA foreign_keys` read — `true` when FK enforcement is enabled. Mirrors
    /// `db->flags & SQLITE_ForeignKeys`. Default is OFF.
    pub fn foreign_keys(&self) -> bool {
        *self.foreign_keys.lock().unwrap()
    }

    /// `PRAGMA foreign_keys = ON/OFF` write — sets the FK enforcement flag. Mirrors the
    /// `PragTyp_FLAG` path in `pragma.c`. Caller must reject the change inside a transaction
    /// (upstream masks `SQLITE_ForeignKeys` out when `db->autoCommit == 0`).
    pub(crate) fn set_foreign_keys(&self, on: bool) {
        *self.foreign_keys.lock().unwrap() = on;
    }

    /// `PRAGMA synchronous` read — 0=OFF, 1=NORMAL, 2=FULL, 3=EXTRA. Per-connection.
    pub fn synchronous(&self) -> u8 {
        *self.synchronous.lock().unwrap()
    }

    /// `PRAGMA synchronous = N` write.
    pub(crate) fn set_synchronous(&self, v: u8) {
        *self.synchronous.lock().unwrap() = v;
    }

    /// `PRAGMA cache_size` read — the page cache size in pages (negative = kibibytes).
    /// Per-connection; the header's `default_cache_size` is the persistent default.
    pub fn cache_size(&self) -> i32 {
        *self.cache_size.lock().unwrap()
    }

    /// `PRAGMA cache_size = N` write.
    pub(crate) fn set_cache_size(&self, v: i32) {
        *self.cache_size.lock().unwrap() = v;
    }

    // ---- Interim engine read helpers (until the VDBE prepare/step path lands in M3) ----
    //
    // These are NOT part of the C API surface; they let the CLI's `.tables`/`.schema` and the
    // file-format round-trip tests read real databases today. They will be superseded by
    // `sqlite3_prepare_v2` + `sqlite3_step`.

    /// Read the full `sqlite_schema` catalog.
    pub fn read_schema(&mut self) -> Result<Catalog> {
        let result = block_on(read_catalog_for(&self.pager));
        if let Err(e) = &result {
            self.last_error = Some(e.clone());
        }
        result
    }

    /// Read every row of a table by name, returning decoded record values. Columns that are
    /// the rowid alias (`INTEGER PRIMARY KEY`) currently read back as NULL — substituting the
    /// rowid requires the schema-aware decode that arrives with the query path (M3).
    pub fn read_table(&mut self, name: &str) -> Result<Vec<Vec<Value>>> {
        let result = block_on(async {
            let pager = self.pager.as_ref().ok_or_else(|| {
                Error::msg(format!("database \"{}\" has no pages yet", self.filename))
            })?;
            let catalog = read_catalog(pager).await?;
            let obj = catalog
                .find_table(name)
                .ok_or_else(|| Error::msg(format!("no such table: {name}")))?;
            let rows = crate::btree::scan_table(pager, obj.rootpage as u32).await?;
            let encoding = pager.text_encoding();
            let mut out = Vec::with_capacity(rows.len());
            for (_rowid, payload) in rows {
                out.push(crate::format::decode_record(&payload, encoding)?);
            }
            Ok::<Vec<Vec<Value>>, Error>(out)
        });
        if let Err(e) = &result {
            self.last_error = Some(e.clone());
        }
        result
    }
}

/// Helper so `read_schema` can borrow `self.pager` immutably while still recording errors.
async fn read_catalog_for(pager: &Option<Arc<Pager>>) -> Result<Catalog> {
    let pager = pager
        .as_deref()
        .ok_or_else(|| Error::msg("database has no pages yet (write path pending)"))?;
    read_catalog(pager).await
}
