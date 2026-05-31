//! The database connection handle — `sqlite3 *` (mirrors `main.c`).

use crate::error::{Error, Result, ResultCode};
use crate::pager::Pager;
use crate::schema::{read_catalog, Catalog};
use crate::types::Value;
use crate::vfs::{MemVfs, OpenFlags, OsTokioVfs, Vfs};

use super::runtime::block_on;

/// A database connection. The Rust analogue of `sqlite3 *`.
///
/// Opened with [`sqlite3_open`]/[`sqlite3_open_v2`]. The `sqlite3_*` free functions and the
/// methods here mirror the C API; richer error information (a [`ResultCode`] plus a message)
/// is returned as [`Error`] rather than a bare integer.
pub struct Sqlite3 {
    pager: Option<Pager>,
    filename: String,
    read_only: bool,
    last_error: Option<Error>,
}

/// `sqlite3_open()` — open (creating if necessary) a database for reading and writing.
pub fn sqlite3_open(filename: &str) -> Result<Sqlite3> {
    sqlite3_open_v2(filename, OpenFlags::READWRITE_CREATE)
}

/// `sqlite3_open_v2()` — open a database with explicit flags.
///
/// `:memory:` and the empty string open a private in-memory database. Note: at M1 the write
/// path does not yet exist, so an in-memory or freshly created (empty) database has no pages
/// to read until the write path lands; reading from such a handle returns an error.
pub fn sqlite3_open_v2(filename: &str, flags: OpenFlags) -> Result<Sqlite3> {
    block_on(async move {
        let is_memory = filename.is_empty() || filename == ":memory:";
        let vfs: Box<dyn Vfs> = if is_memory {
            Box::new(MemVfs::new())
        } else {
            Box::new(OsTokioVfs::new())
        };

        let file = vfs.open(filename, flags).await?;
        let size = file.file_size().await?;
        // An empty file has no header yet (a brand-new or in-memory database). Defer pager
        // creation until there is something to read/write.
        let pager = if size == 0 {
            None
        } else {
            Some(Pager::open(file).await?)
        };

        Ok(Sqlite3 {
            pager,
            filename: filename.to_string(),
            read_only: flags.is_readonly(),
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

    /// `sqlite3_changes()` — rows modified by the most recent statement. Always 0 until the
    /// write path lands.
    pub fn changes(&self) -> i64 {
        0
    }

    /// `sqlite3_last_insert_rowid()` — 0 until the write path lands.
    pub fn last_insert_rowid(&self) -> i64 {
        0
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
        self.pager.as_ref().map(|p| p.header().clone())
    }

    /// The number of pages in the database file (0 if there is no content yet).
    pub fn page_count(&self) -> u32 {
        self.pager.as_ref().map_or(0, |p| p.page_count())
    }

    fn pager(&self) -> Result<&Pager> {
        self.pager.as_ref().ok_or_else(|| {
            Error::msg(format!(
                "database \"{}\" has no pages yet (write path pending)",
                self.filename
            ))
        })
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
async fn read_catalog_for(pager: &Option<Pager>) -> Result<Catalog> {
    let pager = pager
        .as_ref()
        .ok_or_else(|| Error::msg("database has no pages yet (write path pending)"))?;
    read_catalog(pager).await
}
