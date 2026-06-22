//! Result codes and error type, mirroring SQLite's `SQLITE_*` codes (`sqlite3.h`).
//!
//! The primary, C-API-faithful surface is the [`ResultCode`] enum plus the `SQLITE_*`
//! integer constants. The engine-internal [`Error`] pairs a code with an extended code and a
//! human-readable message (as returned by `sqlite3_errmsg`).

use std::fmt;

/// Primary SQLite result codes (`sqlite3.h`). Values match the C API exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum ResultCode {
    Ok = 0,
    Error = 1,
    Internal = 2,
    Perm = 3,
    Abort = 4,
    Busy = 5,
    Locked = 6,
    NoMem = 7,
    ReadOnly = 8,
    Interrupt = 9,
    IoErr = 10,
    Corrupt = 11,
    NotFound = 12,
    Full = 13,
    CantOpen = 14,
    Protocol = 15,
    Empty = 16,
    Schema = 17,
    TooBig = 18,
    Constraint = 19,
    Mismatch = 20,
    Misuse = 21,
    NoLfs = 22,
    Auth = 23,
    Format = 24,
    Range = 25,
    NotADb = 26,
    Notice = 27,
    Warning = 28,
    Row = 100,
    Done = 101,
}

impl ResultCode {
    /// The integer value, identical to the corresponding `SQLITE_*` constant.
    pub fn code(self) -> i32 {
        self as i32
    }
}

// `SQLITE_*` integer constants, for callers that prefer the C spelling.
pub const SQLITE_OK: i32 = ResultCode::Ok as i32;
pub const SQLITE_ERROR: i32 = ResultCode::Error as i32;
pub const SQLITE_INTERNAL: i32 = ResultCode::Internal as i32;
pub const SQLITE_PERM: i32 = ResultCode::Perm as i32;
pub const SQLITE_ABORT: i32 = ResultCode::Abort as i32;
pub const SQLITE_BUSY: i32 = ResultCode::Busy as i32;
pub const SQLITE_LOCKED: i32 = ResultCode::Locked as i32;
pub const SQLITE_NOMEM: i32 = ResultCode::NoMem as i32;
pub const SQLITE_READONLY: i32 = ResultCode::ReadOnly as i32;
pub const SQLITE_INTERRUPT: i32 = ResultCode::Interrupt as i32;
pub const SQLITE_IOERR: i32 = ResultCode::IoErr as i32;
pub const SQLITE_CORRUPT: i32 = ResultCode::Corrupt as i32;
pub const SQLITE_NOTFOUND: i32 = ResultCode::NotFound as i32;
pub const SQLITE_FULL: i32 = ResultCode::Full as i32;
pub const SQLITE_CANTOPEN: i32 = ResultCode::CantOpen as i32;
pub const SQLITE_PROTOCOL: i32 = ResultCode::Protocol as i32;
pub const SQLITE_EMPTY: i32 = ResultCode::Empty as i32;
pub const SQLITE_SCHEMA: i32 = ResultCode::Schema as i32;
pub const SQLITE_TOOBIG: i32 = ResultCode::TooBig as i32;
pub const SQLITE_CONSTRAINT: i32 = ResultCode::Constraint as i32;
pub const SQLITE_MISMATCH: i32 = ResultCode::Mismatch as i32;
pub const SQLITE_MISUSE: i32 = ResultCode::Misuse as i32;
pub const SQLITE_NOLFS: i32 = ResultCode::NoLfs as i32;
pub const SQLITE_AUTH: i32 = ResultCode::Auth as i32;
pub const SQLITE_FORMAT: i32 = ResultCode::Format as i32;
pub const SQLITE_RANGE: i32 = ResultCode::Range as i32;
pub const SQLITE_NOTADB: i32 = ResultCode::NotADb as i32;
pub const SQLITE_NOTICE: i32 = ResultCode::Notice as i32;
pub const SQLITE_WARNING: i32 = ResultCode::Warning as i32;
pub const SQLITE_ROW: i32 = ResultCode::Row as i32;
pub const SQLITE_DONE: i32 = ResultCode::Done as i32;

/// An engine error: a primary [`ResultCode`], an extended code, and a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub code: ResultCode,
    pub extended_code: i32,
    pub message: String,
}

impl Error {
    pub fn new(code: ResultCode, message: impl Into<String>) -> Self {
        Error {
            code,
            extended_code: code.code(),
            message: message.into(),
        }
    }

    /// `SQLITE_CORRUPT` — the database image is malformed.
    pub fn corrupt(message: impl Into<String>) -> Self {
        Error::new(ResultCode::Corrupt, message)
    }

    /// `SQLITE_NOTADB` — the file is not a database (bad header magic).
    pub fn not_a_db(message: impl Into<String>) -> Self {
        Error::new(ResultCode::NotADb, message)
    }

    /// `SQLITE_CANTOPEN` — unable to open the database file.
    pub fn cant_open(message: impl Into<String>) -> Self {
        Error::new(ResultCode::CantOpen, message)
    }

    /// `SQLITE_IOERR` — an I/O error occurred at the VFS layer.
    pub fn io_err(message: impl Into<String>) -> Self {
        Error::new(ResultCode::IoErr, message)
    }

    /// `SQLITE_BUSY` — the database file is locked by another connection (VFS-level lock
    /// contention). Mirrors `SQLITE_BUSY` from `sqlite3.h` ("database is locked").
    pub fn busy(message: impl Into<String>) -> Self {
        Error::new(ResultCode::Busy, message)
    }

    /// `SQLITE_ERROR` — generic error (often a SQL/logic error).
    pub fn msg(message: impl Into<String>) -> Self {
        Error::new(ResultCode::Error, message)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Error {}

/// Engine-internal result alias.
pub type Result<T> = std::result::Result<T, Error>;
