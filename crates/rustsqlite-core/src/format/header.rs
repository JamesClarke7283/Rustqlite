//! The 100-byte database file header (<https://www.sqlite.org/fileformat2.html#the_database_header>).
//!
//! All multi-byte integers in the SQLite file format are big-endian. This module parses and
//! serializes the header byte-for-byte; it is one of the first things validated against real
//! `.db` files because every other structure hangs off the page size and text encoding it
//! reports.

use crate::error::{Error, Result};

/// The 16-byte magic string at the start of every SQLite database.
pub const MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Text encoding of TEXT values in the database (header bytes 56–59).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextEncoding {
    Utf8,
    Utf16Le,
    Utf16Be,
}

impl TextEncoding {
    fn from_code(code: u32) -> TextEncoding {
        match code {
            2 => TextEncoding::Utf16Le,
            3 => TextEncoding::Utf16Be,
            // 1 (UTF-8) and 0 (unset, treated as the default) both map to UTF-8.
            _ => TextEncoding::Utf8,
        }
    }

    fn code(self) -> u32 {
        match self {
            TextEncoding::Utf8 => 1,
            TextEncoding::Utf16Le => 2,
            TextEncoding::Utf16Be => 3,
        }
    }
}

/// A parsed database header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DbHeader {
    /// Page size in bytes (a power of two, 512..=65536). The on-disk u16 value `1` means 65536.
    pub page_size: u32,
    /// File format write version: 1 = legacy (rollback journal), 2 = WAL.
    pub write_version: u8,
    /// File format read version: 1 = legacy, 2 = WAL.
    pub read_version: u8,
    /// Bytes of reserved space at the end of each page (usually 0).
    pub reserved_space: u8,
    /// File change counter.
    pub file_change_counter: u32,
    /// Size of the database file in pages (the "in-header database size"). Only authoritative
    /// when non-zero and `version_valid_for == file_change_counter`.
    pub db_size_pages: u32,
    /// Page number of the first freelist trunk page (0 if the freelist is empty).
    pub first_freelist_trunk: u32,
    /// Total number of freelist pages.
    pub freelist_count: u32,
    /// Schema cookie (bumped on every schema change).
    pub schema_cookie: u32,
    /// Schema format number (1–4).
    pub schema_format: u32,
    /// Default page cache size.
    pub default_cache_size: i32,
    /// Page number of the largest root b-tree page when in auto/incremental-vacuum mode, else 0.
    pub largest_root_page: u32,
    /// Database text encoding.
    pub text_encoding: TextEncoding,
    /// User version (set/read by `PRAGMA user_version`).
    pub user_version: i32,
    /// Non-zero in incremental-vacuum mode.
    pub incremental_vacuum: u32,
    /// Application ID (`PRAGMA application_id`).
    pub application_id: u32,
    /// The `file_change_counter` value when the version number below was written.
    pub version_valid_for: u32,
    /// `SQLITE_VERSION_NUMBER` of the library that last wrote the file.
    pub sqlite_version_number: u32,
}

impl DbHeader {
    /// The usable size of each page: `page_size - reserved_space`.
    pub fn usable_size(&self) -> u32 {
        self.page_size - self.reserved_space as u32
    }

    /// Parse a header from at least the first 100 bytes of a database file.
    pub fn parse(bytes: &[u8]) -> Result<DbHeader> {
        if bytes.len() < 100 {
            return Err(Error::not_a_db("file is shorter than the 100-byte header"));
        }
        if &bytes[0..16] != MAGIC {
            return Err(Error::not_a_db("file is not a database (bad header magic)"));
        }

        let raw_page_size = be_u16(&bytes[16..18]);
        let page_size: u32 = if raw_page_size == 1 {
            65_536
        } else {
            raw_page_size as u32
        };
        if !is_valid_page_size(page_size) {
            return Err(Error::corrupt(format!("invalid page size {page_size}")));
        }

        let reserved_space = bytes[20];
        // SQLite requires the usable page size (page_size - reserved_space) to be at least
        // 480 bytes; a smaller value means the file cannot be a valid database. SQLite
        // reports SQLITE_NOTADB for such a file, so match that (this also keeps usable_size
        // comfortably above the b-tree overflow-formula minimums, which assume usable >= 480).
        if page_size < 480 + reserved_space as u32 {
            return Err(Error::not_a_db(
                "usable page size is below the 480-byte minimum",
            ));
        }

        // The read version must be 1 (legacy) or 2 (WAL); a greater value means the file
        // format cannot be read. (A write version > 2 only forces read-only access, which is
        // not a parse error, so it is accepted.)
        let read_version = bytes[19];
        if read_version > 2 {
            return Err(Error::not_a_db(format!(
                "unsupported file format read version {read_version}"
            )));
        }

        Ok(DbHeader {
            page_size,
            write_version: bytes[18],
            read_version,
            reserved_space,
            file_change_counter: be_u32(&bytes[24..28]),
            db_size_pages: be_u32(&bytes[28..32]),
            first_freelist_trunk: be_u32(&bytes[32..36]),
            freelist_count: be_u32(&bytes[36..40]),
            schema_cookie: be_u32(&bytes[40..44]),
            schema_format: be_u32(&bytes[44..48]),
            default_cache_size: be_u32(&bytes[48..52]) as i32,
            largest_root_page: be_u32(&bytes[52..56]),
            text_encoding: TextEncoding::from_code(be_u32(&bytes[56..60])),
            user_version: be_u32(&bytes[60..64]) as i32,
            incremental_vacuum: be_u32(&bytes[64..68]),
            application_id: be_u32(&bytes[68..72]),
            // bytes 72..92 are reserved-for-expansion (must be zero); not stored.
            version_valid_for: be_u32(&bytes[92..96]),
            sqlite_version_number: be_u32(&bytes[96..100]),
        })
    }

    /// Serialize back to a 100-byte header, byte-for-byte.
    pub fn serialize(&self) -> [u8; 100] {
        let mut b = [0u8; 100];
        b[0..16].copy_from_slice(MAGIC);
        let raw_page_size: u16 = if self.page_size == 65_536 {
            1
        } else {
            self.page_size as u16
        };
        b[16..18].copy_from_slice(&raw_page_size.to_be_bytes());
        b[18] = self.write_version;
        b[19] = self.read_version;
        b[20] = self.reserved_space;
        b[21] = 64; // max embedded payload fraction (must be 64)
        b[22] = 32; // min embedded payload fraction (must be 32)
        b[23] = 32; // leaf payload fraction (must be 32)
        b[24..28].copy_from_slice(&self.file_change_counter.to_be_bytes());
        b[28..32].copy_from_slice(&self.db_size_pages.to_be_bytes());
        b[32..36].copy_from_slice(&self.first_freelist_trunk.to_be_bytes());
        b[36..40].copy_from_slice(&self.freelist_count.to_be_bytes());
        b[40..44].copy_from_slice(&self.schema_cookie.to_be_bytes());
        b[44..48].copy_from_slice(&self.schema_format.to_be_bytes());
        b[48..52].copy_from_slice(&(self.default_cache_size as u32).to_be_bytes());
        b[52..56].copy_from_slice(&self.largest_root_page.to_be_bytes());
        b[56..60].copy_from_slice(&self.text_encoding.code().to_be_bytes());
        b[60..64].copy_from_slice(&(self.user_version as u32).to_be_bytes());
        b[64..68].copy_from_slice(&self.incremental_vacuum.to_be_bytes());
        b[68..72].copy_from_slice(&self.application_id.to_be_bytes());
        b[92..96].copy_from_slice(&self.version_valid_for.to_be_bytes());
        b[96..100].copy_from_slice(&self.sqlite_version_number.to_be_bytes());
        b
    }
}

fn is_valid_page_size(page_size: u32) -> bool {
    (512..=65_536).contains(&page_size) && page_size.is_power_of_two()
}

fn be_u16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DbHeader {
        DbHeader {
            page_size: 4096,
            write_version: 1,
            read_version: 1,
            reserved_space: 0,
            file_change_counter: 3,
            db_size_pages: 2,
            first_freelist_trunk: 0,
            freelist_count: 0,
            schema_cookie: 1,
            schema_format: 4,
            default_cache_size: 0,
            largest_root_page: 0,
            text_encoding: TextEncoding::Utf8,
            user_version: 0,
            incremental_vacuum: 0,
            application_id: 0,
            version_valid_for: 3,
            sqlite_version_number: 3_053_001,
        }
    }

    #[test]
    fn serialize_parse_roundtrip() {
        let h = sample();
        let bytes = h.serialize();
        assert_eq!(&bytes[0..16], MAGIC);
        assert_eq!(bytes[21], 64);
        assert_eq!(bytes[22], 32);
        assert_eq!(bytes[23], 32);
        let parsed = DbHeader::parse(&bytes).unwrap();
        assert_eq!(parsed, h);
        assert_eq!(parsed.usable_size(), 4096);
    }

    #[test]
    fn page_size_65536_encodes_as_one() {
        let mut h = sample();
        h.page_size = 65_536;
        let bytes = h.serialize();
        assert_eq!(be_u16(&bytes[16..18]), 1);
        assert_eq!(DbHeader::parse(&bytes).unwrap().page_size, 65_536);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = sample().serialize().to_vec();
        bytes[0] = b'X';
        assert_eq!(
            DbHeader::parse(&bytes).unwrap_err().code,
            crate::error::ResultCode::NotADb
        );
    }

    #[test]
    fn rejects_non_power_of_two_page_size() {
        let mut bytes = sample().serialize();
        bytes[16..18].copy_from_slice(&3000u16.to_be_bytes());
        assert!(DbHeader::parse(&bytes).is_err());
    }

    #[test]
    fn rejects_usable_size_below_480() {
        // page_size 512 with reserved_space 33 => usable 479, which SQLite rejects (NOTADB).
        let mut h = sample();
        h.page_size = 512;
        h.reserved_space = 33;
        let bytes = h.serialize();
        assert_eq!(
            DbHeader::parse(&bytes).unwrap_err().code,
            crate::error::ResultCode::NotADb
        );

        // reserved_space 32 => usable 480 is the minimum and is accepted.
        h.reserved_space = 32;
        let bytes = h.serialize();
        assert_eq!(DbHeader::parse(&bytes).unwrap().usable_size(), 480);
    }

    #[test]
    fn rejects_unsupported_read_version() {
        let mut bytes = sample().serialize();
        bytes[19] = 3; // read version > 2 cannot be read
        assert_eq!(
            DbHeader::parse(&bytes).unwrap_err().code,
            crate::error::ResultCode::NotADb
        );
    }
}
