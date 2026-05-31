//! File-format round-trip tests (the `tests/fileformat/` layer from TESTING.md).
//!
//! These create real databases with the **system `sqlite3` binary** and then open and read
//! them with rustqlite, asserting that the schema and row values come back identically. This
//! is the headline M1 guarantee: byte-compatible reading of C-SQLite databases.
//!
//! The tests shell out to `sqlite3`; if it is not installed they SKIP (print a notice and
//! return) rather than fail. They are plain `#[test]`s — `sqlite3_*` drive the engine via
//! `Runtime::block_on`, so they must not run inside another tokio runtime.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use rustsqlite_core::{sqlite3_open, Value};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Whether the reference `sqlite3` binary is available.
fn sqlite3_available() -> bool {
    Command::new("sqlite3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A temp database path that cleans itself (and its sidecar files) up on drop.
struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new(tag: &str) -> TempDb {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!("rustqlite_ff_{}_{tag}_{n}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        TempDb { path }
    }

    fn str(&self) -> &str {
        self.path.to_str().unwrap()
    }

    /// Run SQL through the system `sqlite3` against this database.
    fn exec(&self, sql: &str) {
        let out = Command::new("sqlite3")
            .arg(self.str())
            .arg(sql)
            .output()
            .expect("run sqlite3");
        assert!(
            out.status.success(),
            "sqlite3 failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-journal", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.str()));
        }
    }
}

macro_rules! skip_if_no_sqlite3 {
    () => {
        if !sqlite3_available() {
            eprintln!("skipping: system `sqlite3` binary not found");
            return;
        }
    };
}

#[test]
fn reads_schema_written_by_c_sqlite() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("schema");
    db.exec(
        "CREATE TABLE t(a, b);\
         CREATE TABLE other(x INTEGER, y TEXT);\
         CREATE INDEX idx_other_y ON other(y);",
    );

    let mut conn = sqlite3_open(db.str()).expect("open db");
    let catalog = conn.read_schema().expect("read schema");

    let table_names: Vec<&str> = catalog.tables().map(|o| o.name.as_str()).collect();
    assert!(table_names.contains(&"t"), "tables: {table_names:?}");
    assert!(table_names.contains(&"other"), "tables: {table_names:?}");

    // The stored CREATE text must match what we wrote (sqlite stores it verbatim).
    let t = catalog.find_table("t").unwrap();
    assert_eq!(t.sql.as_deref(), Some("CREATE TABLE t(a, b)"));
    assert!(t.rootpage > 0);

    // The index appears as a separate object referencing its table.
    let idx = catalog
        .objects
        .iter()
        .find(|o| o.name == "idx_other_y")
        .expect("index present");
    assert_eq!(idx.obj_type, "index");
    assert_eq!(idx.tbl_name, "other");
}

#[test]
fn reads_mixed_storage_classes() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("types");
    // Columns have no declared type => BLOB/NONE affinity => values stored as written.
    db.exec(
        "CREATE TABLE t(a, b, c);\
         INSERT INTO t VALUES (1, 'hello', 3.5);\
         INSERT INTO t VALUES (NULL, 'world', -2);\
         INSERT INTO t VALUES (9999999, x'01020304', 0);",
    );

    let mut conn = sqlite3_open(db.str()).expect("open db");
    let rows = conn.read_table("t").expect("read table");

    assert_eq!(
        rows,
        vec![
            vec![Value::Int(1), Value::Text("hello".into()), Value::Real(3.5)],
            vec![Value::Null, Value::Text("world".into()), Value::Int(-2)],
            vec![
                Value::Int(9_999_999),
                Value::Blob(vec![0x01, 0x02, 0x03, 0x04]),
                Value::Int(0),
            ],
        ]
    );
}

#[test]
fn reads_large_payload_via_overflow_pages() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("overflow");
    // 5000 bytes far exceeds the per-page local-payload threshold (page size 4096), forcing
    // the value onto an overflow-page chain that the cursor must follow.
    let big = "a".repeat(5000);
    db.exec(&format!(
        "CREATE TABLE big(x); INSERT INTO big VALUES ('{big}');"
    ));

    let mut conn = sqlite3_open(db.str()).expect("open db");
    let rows = conn.read_table("big").expect("read table");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0], vec![Value::Text(big)]);
}

#[test]
fn reads_through_interior_pages() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("interior");
    // 2000 rows overflow a single leaf page, producing interior table b-tree pages that the
    // scan must descend through. Rows are inserted in ascending order, so rowid == value.
    db.exec(
        "CREATE TABLE seq(n);\
         WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n < 2000)\
         INSERT INTO seq SELECT n FROM c;",
    );

    let mut conn = sqlite3_open(db.str()).expect("open db");
    let rows = conn.read_table("seq").expect("read table");

    assert_eq!(rows.len(), 2000);
    assert_eq!(rows[0], vec![Value::Int(1)]);
    assert_eq!(rows[1999], vec![Value::Int(2000)]);
    // Spot-check the middle and that the scan is in ascending rowid order.
    assert_eq!(rows[1000], vec![Value::Int(1001)]);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row, &vec![Value::Int(i as i64 + 1)]);
    }
}

#[test]
fn reads_nondefault_page_size() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("pagesize");
    db.exec(
        "PRAGMA page_size=8192;\
         CREATE TABLE t(a, b);\
         INSERT INTO t VALUES (42, 'answer');",
    );

    let mut conn = sqlite3_open(db.str()).expect("open db");
    assert_eq!(conn.db_header().unwrap().page_size, 8192);
    let rows = conn.read_table("t").expect("read table");
    assert_eq!(
        rows,
        vec![vec![Value::Int(42), Value::Text("answer".into())]]
    );
}
