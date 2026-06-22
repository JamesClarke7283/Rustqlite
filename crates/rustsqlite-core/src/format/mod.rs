//! On-disk format codecs — the byte-compatibility-critical layer.
//!
//! Everything here is validated first against real `.db` files because the rest of the
//! engine depends on reading these structures exactly as C SQLite writes them:
//!
//! * [`varint`] — the big-endian variable-length integer encoding.
//! * [`serial_type`] — record serial types (storage class + width).
//! * [`record`] — the record header/body codec used by table-leaf and index cells.
//! * [`header`] — the 100-byte database file header and text encoding.
//! * [`wal`] — the write-ahead log (`-wal` sidecar) header and frame-header codec.
//!
//! B-tree *page* and *cell* layout live in the [`crate::btree`] module (matching upstream's
//! split of the file format across `btree.c` and the record codec in `vdbeaux.c`).

pub mod header;
pub mod record;
pub mod serial_type;
pub mod varint;
pub mod wal;
pub mod wal_index;

pub use header::{DbHeader, TextEncoding};
pub use record::{decode_record, encode_record};
pub use serial_type::SerialType;
pub use varint::{read_varint, read_varint_i64, varint_len, write_varint};
pub use wal::{WalFrameHeader, WalHeader, WAL_HEADER_SIZE, WAL_FRAME_HEADER_SIZE};
pub use wal_index::{
    WalCkptInfo, WalIndexHdr, HASHTABLE_NPAGE, HASHTABLE_NPAGE_ONE, HASHTABLE_NSLOT,
    READMARK_NOT_USED, SQLITE_SHM_NLOCK, WALINDEX_LOCK_OFFSET, WAL_INDEX_HEADER_SIZE,
    WAL_NREADER,
};
