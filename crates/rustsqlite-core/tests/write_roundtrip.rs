//! End-to-end write-path round-trip tests through the PUBLIC C-API.
//!
//! The first write vertical: on a fresh `OsTokioVfs` tempfile, `CREATE TABLE` + `INSERT` +
//! `SELECT` round-trip through `sqlite3_open` / `sqlite3_prepare_v2` / `sqlite3_step` /
//! the column accessors. The INVERSE ORACLE then opens the same file with the system `sqlite3`
//! binary and checks `PRAGMA integrity_check` and that the rows read back identically — the
//! headline M4 guarantee that a rustqlite-written file is byte-format-valid to C SQLite.
//!
//! Plain `#[test]`s (the C-API drives the engine via `block_on`, so they must not run inside a
//! tokio runtime). They SKIP when the system `sqlite3` binary is absent.

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

/// A temp database path that cleans itself (and its sidecar files) up on drop.
struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new(tag: &str) -> TempDb {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!("rustqlite_wr_{}_{tag}_{n}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        TempDb { path }
    }

    fn str(&self) -> &str {
        self.path.to_str().unwrap()
    }

    /// Run SQL through the system `sqlite3` and return its trimmed stdout.
    fn query(&self, sql: &str) -> String {
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

/// Run a non-query (CREATE/INSERT) statement to completion, asserting it reaches Done.
fn exec(conn: &mut Sqlite3, sql: &str) {
    let (mut stmt, _) = sqlite3_prepare_v2(conn, sql).unwrap_or_else(|e| panic!("prepare {sql}: {e}"));
    loop {
        match stmt.step() {
            ResultCode::Done => break,
            ResultCode::Row => panic!("unexpected row from {sql}"),
            other => panic!("unexpected step result {other:?} from {sql}: {}", stmt.errmsg()),
        }
    }
}

/// Collect all result rows of a query.
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

#[test]
fn create_insert_select_roundtrip_and_c_oracle() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("basic");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE t(a, b);");
        exec(&mut conn, "INSERT INTO t VALUES (1, 'x'), (2, 'y');");

        // changes() == 2 after the insert; last_insert_rowid() == 2.
        assert_eq!(conn.changes(), 2, "changes() after INSERT");
        assert_eq!(conn.last_insert_rowid(), 2, "last_insert_rowid() after INSERT");

        // SELECT a, b FROM t returns the two rows through the engine itself.
        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT a, b FROM t;").unwrap();
        let rows = collect(&mut stmt);
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Text("x".into())],
                vec![Value::Int(2), Value::Text("y".into())],
            ]
        );
        // Close the connection (drops the pager/file) before the C oracle opens the file.
        let _ = conn;
    }

    // INVERSE ORACLE: the C sqlite3 binary validates and reads the same file.
    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
    assert_eq!(db.query("SELECT a, b FROM t;"), "1|x\n2|y");
    // The stored CREATE text is byte-verbatim and the schema cookie was bumped to 1.
    assert_eq!(
        db.query("SELECT quote(sql) FROM sqlite_schema WHERE name='t';"),
        "'CREATE TABLE t(a, b)'"
    );
    assert_eq!(db.query("PRAGMA schema_version;"), "1");
}

#[test]
fn integer_primary_key_rowid_alias_roundtrip() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("pk");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE t(id INTEGER PRIMARY KEY, v);");
        exec(&mut conn, "INSERT INTO t VALUES(5,'x'),(NULL,'y');");
        // The explicit rowid 5, then NULL auto-assigns max+1 = 6 → last_insert_rowid() == 6.
        assert_eq!(conn.changes(), 2);
        assert_eq!(conn.last_insert_rowid(), 6);
        let _ = conn;
    }

    // C oracle: rowid alias substitution and auto-assignment match upstream.
    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
    assert_eq!(db.query("SELECT rowid, id, v FROM t;"), "5|5|x\n6|6|y");
}

#[test]
fn second_table_appends_to_existing_database() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("two");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE t(a, b);");
        exec(&mut conn, "INSERT INTO t VALUES (1, 'x');");
        // A second CREATE TABLE on the now-non-empty database must also work.
        exec(&mut conn, "CREATE TABLE u(c);");
        exec(&mut conn, "INSERT INTO u VALUES (42);");
        let _ = conn;
    }

    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
    assert_eq!(db.query("SELECT a, b FROM t;"), "1|x");
    assert_eq!(db.query("SELECT c FROM u;"), "42");
    // Both tables are present; the second schema row bumped the cookie to 2.
    assert_eq!(
        db.query("SELECT count(*) FROM sqlite_schema WHERE type='table';"),
        "2"
    );
    assert_eq!(db.query("PRAGMA schema_version;"), "2");
}

#[test]
fn delete_roundtrip_and_c_oracle() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("delete");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE t(a, b);");
        for n in 1..=10 {
            exec(&mut conn, &format!("INSERT INTO t VALUES ({n}, 'r{n}');"));
        }
        // Full-table delete (no WHERE).
        exec(&mut conn, "DELETE FROM t;");
        let _ = conn;
    }
    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
    assert_eq!(db.query("SELECT count(*) FROM t;"), "0");

    // Repopulate, then a partial delete.
    let mut conn = sqlite3_open(db.str()).expect("open");
    for n in 1..=10 {
        exec(&mut conn, &format!("INSERT INTO t VALUES ({n}, 'r{n}');"));
    }
    exec(&mut conn, "DELETE FROM t WHERE a > 5;");
    assert_eq!(
        db.query("SELECT a FROM t ORDER BY a;"),
        "1\n2\n3\n4\n5"
    );
    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
}

#[test]
fn drop_table_roundtrip_and_c_oracle() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("drop");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE keepme(a);");
        exec(&mut conn, "CREATE TABLE dropme(b, c);");
        for n in 1..=5 {
            exec(
                &mut conn,
                &format!("INSERT INTO dropme VALUES ({n}, 'r{n}');"),
            );
        }
        // Drop the table; C oracle should then see only `keepme` in `sqlite_schema`.
        exec(&mut conn, "DROP TABLE dropme;");
        let _ = conn;
    }

    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
    assert_eq!(
        db.query("SELECT count(*) FROM sqlite_schema WHERE type='table';"),
        "1"
    );
    assert_eq!(
        db.query("SELECT name FROM sqlite_schema WHERE type='table';"),
        "keepme"
    );
    // The schema cookie was bumped to 3 (one DDL per statement: CREATE keepme, CREATE dropme,
    // DROP dropme). C's `PRAGMA schema_version` agrees.
    assert_eq!(db.query("PRAGMA schema_version;"), "3");
}

#[test]
fn update_roundtrip_and_c_oracle() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("update");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE t(a, b);");
        for n in 1..=6 {
            exec(
                &mut conn,
                &format!("INSERT INTO t VALUES ({n}, 'r{n}');"),
            );
        }
        // UPDATE with WHERE — change every row whose `a` is in 2..=4.
        exec(&mut conn, "UPDATE t SET b = 'X' WHERE a >= 2 AND a <= 4;");
        assert_eq!(conn.changes(), 3);
        // last_insert_rowid() must NOT be clobbered by an UPDATE.
        assert_eq!(conn.last_insert_rowid(), 6);
        let _ = conn;
    }

    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
    assert_eq!(
        db.query("SELECT a, b FROM t ORDER BY a;"),
        "1|r1
2|X
3|X
4|X
5|r5
6|r6"
    );
}

#[test]
fn drop_table_if_exists_unknown_is_silent() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("dropif");
    // A fresh database: we need at least one DDL for the pager to have page 1, otherwise
    // the codegen is invoked on an empty file. (CREATE TABLE then DROP TABLE IF EXISTS of a
    // different name.)
    let mut conn = sqlite3_open(db.str()).expect("open");
    exec(&mut conn, "CREATE TABLE real(a);");
    exec(
        &mut conn,
        "DROP TABLE IF EXISTS nosuch;",
    );
    let _ = conn;
    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
    assert_eq!(
        db.query("SELECT count(*) FROM sqlite_schema WHERE type='table';"),
        "1"
    );
}
