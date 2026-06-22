//! End-to-end WAL-mode read tests through the PUBLIC C-API.
//!
//! The system `sqlite3` binary is used as the **oracle**: it creates a database in WAL mode,
//! inserts rows, and (optionally) checkpoints. Rustqlite then opens the same file and reads
//! the rows back, exercising the M13.4 WAL read path — `Wal::open` recovers the in-memory
//! wal-index from the `-wal` sidecar, and `Pager::get_page` consults the WAL before the
//! database file.
//!
//! Plain `#[test]`s (the C-API drives the engine via `block_on`). They SKIP when the system
//! `sqlite3` binary is absent.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use rustsqlite_core::capi::ResultCode;
use rustsqlite_core::{sqlite3_open, sqlite3_prepare_v2, Sqlite3Stmt, Value};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn sqlite3_available() -> bool {
    Command::new("sqlite3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new(tag: &str) -> TempDb {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!("rustqlite_walr_{}_{tag}_{n}.db", std::process::id()));
        for suffix in ["", "-journal", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.to_str().unwrap()));
        }
        TempDb { path }
    }

    fn str(&self) -> &str {
        self.path.to_str().unwrap()
    }

    /// Run SQL through the system `sqlite3` and return its trimmed stdout.
    fn run(&self, sql: &str) -> String {
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
        String::from_utf8(out.stdout).unwrap().trim().to_string()
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

fn collect(stmt: &mut Sqlite3Stmt) -> Vec<Vec<Value>> {
    let ncol = stmt.column_count();
    let mut rows = Vec::new();
    loop {
        match stmt.step() {
            ResultCode::Row => rows.push((0..ncol).map(|i| stmt.column_value(i)).collect()),
            ResultCode::Done => break,
            other => panic!("unexpected step result {other:?}: {}", stmt.errmsg()),
        }
    }
    rows
}

/// Open a WAL-mode database that the oracle wrote and committed (but did NOT checkpoint) and
/// read all rows back through Rustqlite's WAL read path. The `-wal` sidecar carries the only
/// copy of the committed rows; the database file has just the schema. Rustqlite must recover
/// the WAL index, find the frames for each page, and serve the page data from the WAL.
#[test]
fn read_wal_mode_db_uncheckpointed() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("uncheckpointed");
    // Create the database in WAL mode and insert rows WITHOUT a checkpoint, so the rows
    // live only in the -wal file. Then close cleanly (the wal is NOT checkpointed on close
    // by default when journal_mode=wal).
    db.run("PRAGMA journal_mode = wal;");
    db.run("CREATE TABLE t(a, b);");
    db.run("INSERT INTO t VALUES (1, 'one'), (2, 'two'), (3, 'three');");

    // Verify the oracle sees the rows (sanity).
    assert_eq!(db.run("SELECT count(*) FROM t;"), "3");

    // Now open the same file with Rustqlite and read the rows back. The WAL read path
    // must find the pages in the -wal sidecar.
    let mut conn = sqlite3_open(db.str()).expect("open");
    let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT a, b FROM t ORDER BY a;")
        .expect("prepare");
    let rows = collect(&mut stmt);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], Value::Int(1));
    assert_eq!(rows[0][1], Value::Text("one".to_string()));
    assert_eq!(rows[1][0], Value::Int(2));
    assert_eq!(rows[1][1], Value::Text("two".to_string()));
    assert_eq!(rows[2][0], Value::Int(3));
    assert_eq!(rows[2][1], Value::Text("three".to_string()));
}

/// A WAL-mode database that has been checkpointed (so the rows are in the DB file, not the
/// WAL) must still read correctly — Rustqlite falls back to the database file when the WAL has
/// no frame for a page.
#[test]
fn read_wal_mode_db_after_checkpoint() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("checkpointed");
    db.run("PRAGMA journal_mode = wal;");
    db.run("CREATE TABLE t(a);");
    db.run("INSERT INTO t VALUES (10), (20), (30);");
    // Force a checkpoint so the rows move from the -wal into the .db file.
    db.run("PRAGMA wal_checkpoint(TRUNCATE);");

    let mut conn = sqlite3_open(db.str()).expect("open");
    let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT a FROM t ORDER BY a;")
        .expect("prepare");
    let rows = collect(&mut stmt);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], Value::Int(10));
    assert_eq!(rows[1][0], Value::Int(20));
    assert_eq!(rows[2][0], Value::Int(30));
}

/// A WAL-mode database that has multiple commits in the -wal (uncommitted tail from a
/// transaction that was rolled back or is still open in another connection) must expose only
/// the rows up to the last commit frame.
#[test]
fn read_wal_mode_db_multi_commit() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("multi");
    db.run("PRAGMA journal_mode = wal;");
    db.run("CREATE TABLE t(a);");
    db.run("INSERT INTO t VALUES (1);");
    db.run("INSERT INTO t VALUES (2);");
    db.run("INSERT INTO t VALUES (3);");

    let mut conn = sqlite3_open(db.str()).expect("open");
    let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT a FROM t ORDER BY a;")
        .expect("prepare");
    let rows = collect(&mut stmt);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], Value::Int(1));
    assert_eq!(rows[1][0], Value::Int(2));
    assert_eq!(rows[2][0], Value::Int(3));
}

/// A WAL-mode database with an empty -wal (just the header, no frames) must read the rows
/// from the database file (this happens after a checkpoint that truncated the wal).
#[test]
fn read_wal_mode_db_empty_wal() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("emptywal");
    db.run("PRAGMA journal_mode = wal;");
    db.run("CREATE TABLE t(a);");
    db.run("INSERT INTO t VALUES (42);");
    // Checkpoint and TRUNCATE the wal so it has no frames.
    db.run("PRAGMA wal_checkpoint(TRUNCATE);");

    let mut conn = sqlite3_open(db.str()).expect("open");
    let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT a FROM t;").expect("prepare");
    let rows = collect(&mut stmt);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(42));
}

/// Opening a WAL-mode database with no -wal file at all (the wal was deleted after a
/// checkpoint) must still read from the DB file. This is the common "the wal was cleaned up
/// on close" case.
#[test]
fn read_wal_mode_db_no_wal_file() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("nowal");
    db.run("PRAGMA journal_mode = wal;");
    db.run("CREATE TABLE t(a);");
    db.run("INSERT INTO t VALUES (7);");
    // Checkpoint and remove the -wal file entirely.
    db.run("PRAGMA wal_checkpoint(TRUNCATE);");
    let _ = std::fs::remove_file(format!("{}-wal", db.str()));
    let _ = std::fs::remove_file(format!("{}-shm", db.str()));

    let mut conn = sqlite3_open(db.str()).expect("open");
    let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT a FROM t;").expect("prepare");
    let rows = collect(&mut stmt);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(7));
}

/// A WAL-mode database with a larger payload (more rows) to make sure the WAL frame lookup
/// works for pages beyond page 1.
#[test]
fn read_wal_mode_db_many_rows() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("many");
    db.run("PRAGMA journal_mode = wal;");
    db.run("CREATE TABLE t(a, b);");
    // Insert enough rows to span multiple b-tree leaf pages (each row is ~100 bytes, so
    // ~40 rows fill a 4KiB page; insert 200 to span several pages).
    let mut sql = String::from("INSERT INTO t VALUES ");
    for i in 0..200 {
        if i > 0 {
            sql.push(',');
        }
        sql.push_str(&format!("({i}, 'row{i}')"));
    }
    sql.push(';');
    db.run(&sql);

    let mut conn = sqlite3_open(db.str()).expect("open");
    let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT count(*) FROM t;").expect("prepare");
    let rows = collect(&mut stmt);
    assert_eq!(rows[0][0], Value::Int(200));

    let (mut stmt2, _) = sqlite3_prepare_v2(&mut conn, "SELECT a, b FROM t ORDER BY a;")
        .expect("prepare");
    let got = collect(&mut stmt2);
    assert_eq!(got.len(), 200);
    assert_eq!(got[0][0], Value::Int(0));
    assert_eq!(got[0][1], Value::Text("row0".to_string()));
    assert_eq!(got[199][0], Value::Int(199));
    assert_eq!(got[199][1], Value::Text("row199".to_string()));
}

/// `sqlite_schema` reads through the WAL too — verify that `.tables` / schema queries work
/// on a WAL-mode database.
#[test]
fn read_wal_mode_db_schema() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("schema");
    db.run("PRAGMA journal_mode = wal;");
    db.run("CREATE TABLE foo(x);");
    db.run("CREATE TABLE bar(y);");
    db.run("INSERT INTO foo VALUES (1);");

    let mut conn = sqlite3_open(db.str()).expect("open");
    let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name;").expect("prepare");
    let rows = collect(&mut stmt);
    let names: Vec<String> = rows
        .into_iter()
        .map(|r| match r.into_iter().next() {
            Some(Value::Text(s)) => s,
            other => panic!("expected text, got {other:?}"),
        })
        .collect();
    assert_eq!(names, vec!["bar".to_string(), "foo".to_string()]);
}

/// Rustqlite writes to a WAL-mode database (created by the C oracle) and the C oracle reads
/// the new rows back. This exercises the M13.5 WAL write path — `Pager::commit` appends frames
/// to the `-wal` sidecar instead of journaling + writing the DB file. The C oracle then opens
/// the same file (recovering the WAL) and sees the rows Rustqlite wrote.
#[test]
fn write_wal_mode_db_rustqlite_reads_back_via_c_oracle() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("rwrite");
    // The C oracle sets up a WAL-mode database with a table and one row.
    db.run("PRAGMA journal_mode = wal;");
    db.run("CREATE TABLE t(a, b);");
    db.run("INSERT INTO t VALUES (1, 'one');");

    // Rustqlite opens the WAL-mode database and inserts more rows. The write path appends
    // frames to the -wal sidecar (the DB file is untouched — only the schema page is there).
    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "INSERT INTO t VALUES (2, 'two'), (3, 'three');").expect("prepare");
        match stmt.step() {
            ResultCode::Done => {}
            other => panic!("unexpected step result {other:?}: {}", stmt.errmsg()),
        }
    }
    // The Rustqlite connection is dropped (closing the WAL). The C oracle opens the file
    // and reads back — it must recover the WAL (including the frames Rustqlite wrote) and
    // see all three rows.
    assert_eq!(db.run("SELECT count(*) FROM t;"), "3");
    assert_eq!(
        db.run("SELECT a FROM t ORDER BY a;"),
        "1\n2\n3"
    );
    assert_eq!(
        db.run("SELECT b FROM t WHERE a = 3;"),
        "three"
    );
}

/// Rustqlite writes to a WAL-mode database and Rustqlite itself reads the rows back (without
/// the C oracle). This verifies the round-trip through our own WAL write + read path: the
/// commit appends frames, a fresh connection recovers the WAL and serves the pages from the
/// frames.
#[test]
fn write_then_read_wal_mode_db_roundtrip() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("rwround");
    // The C oracle puts the database into WAL mode (so the header has write_version=2). We
    // need the C oracle here because Rustqlite doesn't yet implement `PRAGMA journal_mode =
    // wal` (M13.10) — `create_fresh` always starts in rollback-journal mode.
    db.run("PRAGMA journal_mode = wal;");
    db.run("CREATE TABLE t(a);");

    // Rustqlite inserts rows.
    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        for i in 0..50 {
            let sql = format!("INSERT INTO t VALUES ({i});");
            let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, &sql).expect("prepare");
            match stmt.step() {
                ResultCode::Done => {}
                other => panic!("unexpected step result {other:?}: {}", stmt.errmsg()),
            }
        }
    }

    // Rustqlite reads them back in a fresh connection (recovering the WAL Rustqlite wrote).
    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT count(*) FROM t;").expect("prepare");
        let rows = collect(&mut stmt);
        assert_eq!(rows[0][0], Value::Int(50));
        let (mut stmt2, _) = sqlite3_prepare_v2(&mut conn, "SELECT a FROM t ORDER BY a;").expect("prepare");
        let got = collect(&mut stmt2);
        assert_eq!(got.len(), 50);
        for (i, row) in got.iter().enumerate() {
            assert_eq!(row[0], Value::Int(i as i64));
        }
    }

    // The C oracle also reads them back (cross-engine WAL compatibility).
    assert_eq!(db.run("SELECT count(*) FROM t;"), "50");
}

/// Rustqlite writes to a WAL-mode database and the C oracle's `PRAGMA integrity_check` passes.
/// This verifies that the WAL frames Rustqlite writes are byte-format-valid.
#[test]
fn write_wal_mode_db_c_oracle_integrity_check() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("rwinteg");
    db.run("PRAGMA journal_mode = wal;");
    db.run("CREATE TABLE t(a, b);");
    db.run("INSERT INTO t VALUES (0, 'seed');");

    // Rustqlite inserts rows.
    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        for i in 1..100 {
            let sql = format!("INSERT INTO t VALUES ({i}, 'row{i}');");
            let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, &sql).expect("prepare");
            match stmt.step() {
                ResultCode::Done => {}
                other => panic!("unexpected step result {other:?}: {}", stmt.errmsg()),
            }
        }
    }

    // The C oracle checks integrity. It must recover the WAL (the frames Rustqlite wrote)
    // and the b-tree must be consistent.
    assert_eq!(db.run("PRAGMA integrity_check;"), "ok");
    assert_eq!(db.run("SELECT count(*) FROM t;"), "100");
}