//! Foreign-key pragma tests: `PRAGMA foreign_keys` (read/write) and
//! `PRAGMA foreign_key_list(tbl)` — the M17.3/M17.4 introspection surface. FK enforcement
//! itself (M17.6+) is deferred; these tests cover the read/write flag and the
//! constraint-listing pragma, differential-checked against the system `sqlite3` oracle.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use rustsqlite_core::capi::ResultCode;
use rustsqlite_core::{sqlite3_open, sqlite3_prepare_v2, Sqlite3, Sqlite3Stmt, Value};

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
        path.push(format!("rustqlite_fk_{}_{tag}_{n}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        TempDb { path }
    }
    fn str(&self) -> &str {
        self.path.to_str().unwrap()
    }
    /// Run SQL through the system `sqlite3` and return its trimmed stdout.
    fn oracle(&self, sql: &str) -> String {
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

fn exec(conn: &mut Sqlite3, sql: &str) {
    let (mut stmt, _) =
        sqlite3_prepare_v2(conn, sql).unwrap_or_else(|e| panic!("prepare {sql}: {e}"));
    match stmt.step() {
        ResultCode::Done => {}
        ResultCode::Row => panic!("unexpected row from {sql}"),
        other => panic!(
            "unexpected step result {other:?} from {sql}: {}",
            stmt.errmsg()
        ),
    }
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

fn query_rows(conn: &mut Sqlite3, sql: &str) -> Vec<Vec<Value>> {
    let (mut stmt, _) =
        sqlite3_prepare_v2(conn, sql).unwrap_or_else(|e| panic!("prepare {sql}: {e}"));
    collect(&mut stmt)
}

/// Format rows into the oracle's pipe-separated text for diffing.
fn fmt_rows(rows: &[Vec<Value>]) -> String {
    rows.iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    Value::Null => "".to_string(),
                    Value::Int(n) => n.to_string(),
                    Value::Real(f) => f.to_string(),
                    Value::Text(s) => s.clone(),
                    Value::Blob(b) => {
                        b.iter().map(|c| format!("{:02x}", c)).collect::<String>()
                    }
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn foreign_keys_pragma_default_off_and_toggle() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("fk_toggle");

    // Default is OFF (0), matching upstream without SQLITE_DEFAULT_FOREIGN_KEYS.
    let mut conn = sqlite3_open(db.str()).expect("open");
    let rows = query_rows(&mut conn, "PRAGMA foreign_keys;");
    assert_eq!(rows, vec![vec![Value::Int(0)]]);

    // ON via the keyword.
    exec(&mut conn, "PRAGMA foreign_keys = ON;");
    let rows = query_rows(&mut conn, "PRAGMA foreign_keys;");
    assert_eq!(rows, vec![vec![Value::Int(1)]]);

    // OFF via 0.
    exec(&mut conn, "PRAGMA foreign_keys = 0;");
    let rows = query_rows(&mut conn, "PRAGMA foreign_keys;");
    assert_eq!(rows, vec![vec![Value::Int(0)]]);

    // ON via 1, then OFF via "false".
    exec(&mut conn, "PRAGMA foreign_keys = 1;");
    exec(&mut conn, "PRAGMA foreign_keys = false;");
    let rows = query_rows(&mut conn, "PRAGMA foreign_keys;");
    assert_eq!(rows, vec![vec![Value::Int(0)]]);

    // The oracle reads the same final state.
    assert_eq!(db.oracle("PRAGMA foreign_keys;"), "0");
}

#[test]
fn foreign_keys_pragma_silently_ignored_inside_transaction() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("fk_tx");

    let mut conn = sqlite3_open(db.str()).expect("open");
    exec(&mut conn, "PRAGMA foreign_keys = ON;");
    exec(&mut conn, "BEGIN;");
    // Upstream masks the FK bit out inside a transaction, so this is a no-op.
    exec(&mut conn, "PRAGMA foreign_keys = OFF;");
    let rows = query_rows(&mut conn, "PRAGMA foreign_keys;");
    assert_eq!(rows, vec![vec![Value::Int(1)]]);
    exec(&mut conn, "COMMIT;");

    // After COMMIT the toggle works again.
    exec(&mut conn, "PRAGMA foreign_keys = OFF;");
    let rows = query_rows(&mut conn, "PRAGMA foreign_keys;");
    assert_eq!(rows, vec![vec![Value::Int(0)]]);
}

#[test]
fn foreign_key_list_matches_oracle() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("fk_list");

    let setup = [
        "CREATE TABLE parent(id INTEGER PRIMARY KEY, x TEXT);",
        "CREATE TABLE child(a INTEGER, b INTEGER REFERENCES parent(id) ON DELETE CASCADE ON UPDATE SET NULL, c TEXT, FOREIGN KEY(c) REFERENCES parent(x));",
    ];

    let mut conn = sqlite3_open(db.str()).expect("open");
    for s in setup {
        exec(&mut conn, s);
    }
    let rows = query_rows(&mut conn, "PRAGMA foreign_key_list(child);");
    let ours = fmt_rows(&rows);

    // The setup rows are already in the file (our engine wrote them); the oracle reads the
    // same file. C-SQLite can read Rustqlite-written schema rows because the file format is
    // compatible.
    let theirs = db.oracle("PRAGMA foreign_key_list(child);");
    assert_eq!(ours, theirs, "foreign_key_list mismatch");
}

#[test]
fn foreign_key_list_mixed_column_and_table_constraints() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("fk_mixed");

    let setup = [
        "CREATE TABLE parent(id INTEGER PRIMARY KEY, x TEXT);",
        "CREATE TABLE t1(a INTEGER REFERENCES parent(id), b TEXT REFERENCES parent(x) ON DELETE CASCADE, c INTEGER, FOREIGN KEY(c) REFERENCES parent(id) ON UPDATE SET NULL);",
    ];

    let mut conn = sqlite3_open(db.str()).expect("open");
    for s in setup {
        exec(&mut conn, s);
    }
    let rows = query_rows(&mut conn, "PRAGMA foreign_key_list(t1);");
    let ours = fmt_rows(&rows);

    let theirs = db.oracle("PRAGMA foreign_key_list(t1);");
    assert_eq!(ours, theirs, "foreign_key_list mismatch (mixed)");
}

#[test]
fn foreign_key_list_multicolumn_fk() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("fk_mc");

    let setup = [
        "CREATE TABLE parent(a, b, PRIMARY KEY(a, b));",
        "CREATE TABLE child(x, y, FOREIGN KEY(x, y) REFERENCES parent(a, b));",
    ];

    let mut conn = sqlite3_open(db.str()).expect("open");
    for s in setup {
        exec(&mut conn, s);
    }
    let rows = query_rows(&mut conn, "PRAGMA foreign_key_list(child);");
    let ours = fmt_rows(&rows);

    let theirs = db.oracle("PRAGMA foreign_key_list(child);");
    assert_eq!(ours, theirs, "foreign_key_list mismatch (multicolumn)");
}

#[test]
fn foreign_key_list_no_fks_returns_empty() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("fk_empty");

    let mut conn = sqlite3_open(db.str()).expect("open");
    exec(&mut conn, "CREATE TABLE t(a, b);");
    let rows = query_rows(&mut conn, "PRAGMA foreign_key_list(t);");
    assert!(rows.is_empty());
    assert_eq!(db.oracle("PRAGMA foreign_key_list(t);"), "");
}

#[test]
fn foreign_key_list_missing_table_returns_empty() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("fk_missing");

    let mut conn = sqlite3_open(db.str()).expect("open");
    let rows = query_rows(&mut conn, "PRAGMA foreign_key_list(no_such_table);");
    assert!(rows.is_empty());
    assert_eq!(db.oracle("PRAGMA foreign_key_list(no_such_table);"), "");
}