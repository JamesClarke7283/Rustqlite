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

/// Collect all result rows of a RETURNING statement and assert it reaches Done.
fn collect_returning(conn: &mut Sqlite3, sql: &str) -> Vec<Vec<Value>> {
    let (mut stmt, _) = sqlite3_prepare_v2(conn, sql)
        .unwrap_or_else(|e| panic!("prepare {sql}: {e}"));
    let rows = collect(&mut stmt);
    match stmt.step() {
        ResultCode::Done => {}
        other => panic!("unexpected step result {other:?} from {sql}: {}", stmt.errmsg()),
    }
    rows
}

#[test]
fn create_insert_select_basic_roundtrip_and_c_oracle() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("basic");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE t(a, b);");
        exec(&mut conn, "INSERT INTO t VALUES (1, 'x'), (2, 'y');");

        // changes() == 2 after the insert; last_insert_rowid() == 2.
        assert_eq!(conn.changes(), 2, "changes() after INSERT");
        assert_eq!(
            conn.last_insert_rowid(),
            2,
            "last_insert_rowid() after INSERT"
        );

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
        assert_eq!(conn.changes(), 10, "changes() after full DELETE");
        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT count(*) FROM t;").unwrap();
        let rows = collect(&mut stmt);
        assert_eq!(rows, vec![vec![Value::Int(0)]]);

        // Re-populate and test a filtered delete.
        for n in 1..=5 {
            exec(&mut conn, &format!("INSERT INTO t VALUES ({n}, 'r{n}');"));
        }
        exec(&mut conn, "DELETE FROM t WHERE a > 3;");
        assert_eq!(conn.changes(), 2, "changes() after filtered DELETE");

        let _ = conn;
    }

    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
    assert_eq!(db.query("SELECT a, b FROM t;"), "1|r1\n2|r2\n3|r3");
}

#[test]
fn update_roundtrip_and_c_oracle() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("update");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE t(a, b);");
        for n in 1..=5 {
            exec(&mut conn, &format!("INSERT INTO t VALUES ({n}, 'r{n}');"));
        }

        // Full-table update (no WHERE).
        exec(&mut conn, "UPDATE t SET b = 'x';");
        assert_eq!(conn.changes(), 5, "changes() after full UPDATE");
        assert_eq!(conn.last_insert_rowid(), 0, "UPDATE does not set last_insert_rowid");

        // Filtered update.
        exec(&mut conn, "UPDATE t SET b = 'y' WHERE a > 3;");
        assert_eq!(conn.changes(), 2, "changes() after filtered UPDATE");

        let _ = conn;
    }

    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
    assert_eq!(
        db.query("SELECT a, b FROM t ORDER BY a;"),
        "1|x\n2|x\n3|x\n4|y\n5|y"
    );
}

#[test]
fn index_maintained_on_insert_update_delete() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("index_maintained");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE t(a, b);");
        exec(&mut conn, "CREATE INDEX idx_a ON t(a);");
        exec(&mut conn, "INSERT INTO t VALUES (1, 'one'), (2, 'two'), (3, 'three');");

        // Use the index for a point lookup.
        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT b FROM t WHERE a = 2;").unwrap();
        let rows = collect(&mut stmt);
        assert_eq!(rows, vec![vec![Value::Text("two".into())]]);

        // Update an indexed column and verify the new key is reachable.
        exec(&mut conn, "UPDATE t SET a = 20 WHERE a = 2;");
        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT b FROM t WHERE a = 20;").unwrap();
        let rows = collect(&mut stmt);
        assert_eq!(rows, vec![vec![Value::Text("two".into())]]);

        // Delete a row and verify the key is gone.
        exec(&mut conn, "DELETE FROM t WHERE a = 20;");
        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT count(*) FROM t WHERE a = 20;").unwrap();
        let rows = collect(&mut stmt);
        assert_eq!(rows, vec![vec![Value::Int(0)]]);

        let _ = conn;
    }

    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
    assert_eq!(db.query("SELECT a, b FROM t ORDER BY a;"), "1|one\n3|three");
}

#[test]
fn create_index_roundtrip_and_c_oracle() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("create_index");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE t(a, b);");
        exec(&mut conn, "INSERT INTO t VALUES (3, 'three'), (1, 'one'), (2, 'two');");
        exec(&mut conn, "CREATE INDEX idx_a ON t(a);");

        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT b FROM t WHERE a = 2;").unwrap();
        let rows = collect(&mut stmt);
        assert_eq!(rows, vec![vec![Value::Text("two".into())]]);

        let _ = conn;
    }

    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
    assert_eq!(db.query("SELECT b FROM t WHERE a = 1;"), "one");
}

#[test]
fn create_insert_select_roundtrip_and_c_oracle() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("insert_select");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE src(a, b);");
        exec(&mut conn, "CREATE TABLE dst(a, b);");
        exec(&mut conn, "INSERT INTO src VALUES (1, 'x'), (2, 'y'), (3, 'z');");
        exec(&mut conn, "INSERT INTO dst SELECT * FROM src WHERE a > 1;");
        assert_eq!(conn.changes(), 2, "changes() after INSERT ... SELECT");

        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT a, b FROM dst ORDER BY a;").unwrap();
        let rows = collect(&mut stmt);
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(2), Value::Text("y".into())],
                vec![Value::Int(3), Value::Text("z".into())],
            ]
        );

        let _ = conn;
    }

    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
    assert_eq!(db.query("SELECT a, b FROM dst ORDER BY a;"), "2|y\n3|z");
}

#[test]
fn unique_index_rejects_duplicate_insert() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("unique_insert");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE t(a, b);");
        exec(&mut conn, "CREATE UNIQUE INDEX idx_a ON t(a);");
        exec(&mut conn, "INSERT INTO t VALUES (1, 'x');");

        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "INSERT INTO t VALUES (1, 'y');").unwrap();
        let rc = stmt.step();
        assert_eq!(rc, ResultCode::Abort);
        assert!(stmt.errmsg().contains("UNIQUE constraint failed"));

        let _ = conn;
    }
}

#[test]
fn unique_index_rejects_duplicate_update() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("unique_update");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE t(a, b);");
        exec(&mut conn, "CREATE UNIQUE INDEX idx_a ON t(a);");
        exec(&mut conn, "INSERT INTO t VALUES (1, 'x'), (2, 'y');");

        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "UPDATE t SET a = 1 WHERE a = 2;").unwrap();
        let rc = stmt.step();
        assert_eq!(rc, ResultCode::Abort);
        assert!(stmt.errmsg().contains("UNIQUE constraint failed"));

        let _ = conn;
    }
}

#[test]
fn drop_table_roundtrip_and_c_oracle() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("drop_table");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE t(a);");
        exec(&mut conn, "INSERT INTO t VALUES (1);");
        exec(&mut conn, "DROP TABLE t;");

        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT count(*) FROM sqlite_schema WHERE name='t';").unwrap();
        let rows = collect(&mut stmt);
        assert_eq!(rows, vec![vec![Value::Int(0)]]);

        let _ = conn;
    }

    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
    assert_eq!(db.query("SELECT count(*) FROM sqlite_schema WHERE name='t';"), "0");
}

#[test]
fn drop_index_roundtrip_and_c_oracle() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("drop_index");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE t(a);");
        exec(&mut conn, "CREATE INDEX idx_a ON t(a);");
        exec(&mut conn, "DROP INDEX idx_a;");

        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT count(*) FROM sqlite_schema WHERE name='idx_a';").unwrap();
        let rows = collect(&mut stmt);
        assert_eq!(rows, vec![vec![Value::Int(0)]]);

        let _ = conn;
    }

    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
    assert_eq!(db.query("SELECT count(*) FROM sqlite_schema WHERE name='idx_a';"), "0");
}

#[test]
fn multi_column_index_select() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("multi_col_index_select");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE t(a, b, c);");
        exec(&mut conn, "CREATE INDEX idx_ab ON t(a, b);");
        exec(&mut conn, "INSERT INTO t VALUES (1, 2, 'x'), (1, 3, 'y'), (2, 2, 'z');");

        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT c FROM t WHERE a = 1 AND b = 3;").unwrap();
        let rows = collect(&mut stmt);
        assert_eq!(rows, vec![vec![Value::Text("y".into())]]);

        let _ = conn;
    }

    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
    assert_eq!(db.query("SELECT c FROM t WHERE a = 1 AND b = 2;"), "x");
}

#[test]
fn multi_column_index_maintained_on_writes() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("multi_col_index_writes");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE t(a, b, c);");
        exec(&mut conn, "CREATE INDEX idx_ab ON t(a, b);");
        exec(&mut conn, "INSERT INTO t VALUES (1, 2, 'x'), (1, 3, 'y');");
        exec(&mut conn, "UPDATE t SET b = 4 WHERE a = 1 AND b = 2;");
        exec(&mut conn, "DELETE FROM t WHERE a = 1 AND b = 3;");

        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT c FROM t WHERE a = 1 AND b = 4;").unwrap();
        let rows = collect(&mut stmt);
        assert_eq!(rows, vec![vec![Value::Text("x".into())]]);

        let _ = conn;
    }

    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
    assert_eq!(db.query("SELECT c FROM t WHERE a = 1 AND b = 3;"), "");
}

#[test]
fn multi_column_index_with_collation_roundtrip_and_c_oracle() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("multi_col_index_coll");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE t(a TEXT, b TEXT);");
        exec(&mut conn, "CREATE INDEX idx_ab ON t(a COLLATE NOCASE, b);");
        exec(&mut conn, "INSERT INTO t VALUES ('A', 'one'), ('a', 'two'), ('B', 'three');");

        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT b FROM t WHERE a = 'A';").unwrap();
        let rows = collect(&mut stmt);
        assert_eq!(
            rows,
            vec![
                vec![Value::Text("one".into())],
                vec![Value::Text("two".into())],
            ]
        );

        let _ = conn;
    }

    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
    assert_eq!(db.query("SELECT b FROM t WHERE a = 'b';"), "three");
}

#[test]
fn multi_column_unique_index_rejects_duplicate() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("multi_col_unique");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE t(a, b);");
        exec(&mut conn, "CREATE UNIQUE INDEX idx_ab ON t(a, b);");
        exec(&mut conn, "INSERT INTO t VALUES (1, 2);");

        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "INSERT INTO t VALUES (1, 2);").unwrap();
        let rc = stmt.step();
        assert_eq!(rc, ResultCode::Abort);
        assert!(stmt.errmsg().contains("UNIQUE constraint failed"));

        let _ = conn;
    }
}

#[test]
fn partial_index_create_populate_select_and_maintain() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("partial_index");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE t(a, b);");
        exec(&mut conn, "CREATE INDEX idx_a ON t(a) WHERE a > 0;");
        exec(&mut conn, "INSERT INTO t VALUES (-1, 'neg'), (1, 'pos'), (2, 'pos2');");

        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT b FROM t WHERE a = 1;").unwrap();
        let rows = collect(&mut stmt);
        assert_eq!(rows, vec![vec![Value::Text("pos".into())]]);

        exec(&mut conn, "UPDATE t SET a = -2 WHERE a = 1;");
        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT b FROM t WHERE a = 1;").unwrap();
        let rows = collect(&mut stmt);
        assert_eq!(rows, Vec::<Vec<Value>>::new());

        let _ = conn;
    }

    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
}

#[test]
fn indexed_select_where_equality() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("indexed_select");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE t(a, b);");
        exec(&mut conn, "CREATE INDEX idx_a ON t(a);");
        exec(&mut conn, "INSERT INTO t VALUES (1, 'one'), (2, 'two'), (3, 'three');");

        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT b FROM t WHERE a = 2;").unwrap();
        let rows = collect(&mut stmt);
        assert_eq!(rows, vec![vec![Value::Text("two".into())]]);

        let _ = conn;
    }

    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
    assert_eq!(db.query("SELECT b FROM t WHERE a = 3;"), "three");
}

#[test]
fn create_index_if_not_exists() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("create_index_ifne");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE t(a);");
        exec(&mut conn, "CREATE INDEX idx_a ON t(a);");
        exec(&mut conn, "CREATE INDEX IF NOT EXISTS idx_a ON t(a);"); // no-op

        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT count(*) FROM sqlite_schema WHERE name='idx_a';").unwrap();
        let rows = collect(&mut stmt);
        assert_eq!(rows, vec![vec![Value::Int(1)]]);

        let _ = conn;
    }

    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
}

#[test]
fn drop_table_if_exists_unknown_is_silent() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("drop_table_ifexists");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "DROP TABLE IF EXISTS no_such_table;");

        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT count(*) FROM sqlite_schema;").unwrap();
        let rows = collect(&mut stmt);
        assert_eq!(rows, vec![vec![Value::Int(0)]]);

        let _ = conn;
    }
}

#[test]
fn delete_triggers_leaf_merge_and_c_oracle() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("delete_leaf_merge");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE t(a INTEGER PRIMARY KEY, b);");
        // Insert enough rows to trigger an interior page.
        for n in 1..=50 {
            exec(&mut conn, &format!("INSERT INTO t VALUES ({n}, 'r{n}');"));
        }
        // Delete most rows, exercising leaf merge / redistribution.
        exec(&mut conn, "DELETE FROM t WHERE a > 5;");
        assert_eq!(conn.changes(), 45);

        let _ = conn;
    }

    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
    assert_eq!(
        db.query("SELECT a, b FROM t ORDER BY a;"),
        "1|r1\n2|r2\n3|r3\n4|r4\n5|r5"
    );
}

/// M2.22 / M4: `INSERT ... DEFAULT VALUES` uses each column's default (or NULL) and works
/// end-to-end through the C-API. The C oracle validates the file format and the row content.
#[test]
fn insert_default_values_roundtrip_and_c_oracle() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("default_values");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(
            &mut conn,
            "CREATE TABLE t(a INT DEFAULT 42, b TEXT DEFAULT 'hello', c);",
        );
        exec(&mut conn, "INSERT INTO t DEFAULT VALUES;");
        assert_eq!(conn.changes(), 1);
        assert_eq!(conn.last_insert_rowid(), 1);

        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT a, b, c FROM t;").unwrap();
        let rows = collect(&mut stmt);
        assert_eq!(
            rows,
            vec![vec![
                Value::Int(42),
                Value::Text("hello".into()),
                Value::Null,
            ]]
        );

        // Rowid-alias table with a default on the stored column only: auto-assign rowid.
        exec(
            &mut conn,
            "CREATE TABLE u(id INTEGER PRIMARY KEY, v INT DEFAULT 99);",
        );
        exec(&mut conn, "INSERT INTO u DEFAULT VALUES;");
        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT rowid, id, v FROM u;").unwrap();
        let rows = collect(&mut stmt);
        assert_eq!(
            rows,
            vec![vec![Value::Int(1), Value::Int(1), Value::Int(99)]]
        );

        // Rowid-alias table with an explicit default on the alias column: that value becomes rowid.
        exec(
            &mut conn,
            "CREATE TABLE w(id INTEGER PRIMARY KEY DEFAULT 123, v);",
        );
        exec(&mut conn, "INSERT INTO w DEFAULT VALUES;");
        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT rowid, id, v FROM w;").unwrap();
        let rows = collect(&mut stmt);
        assert_eq!(
            rows,
            vec![vec![Value::Int(123), Value::Int(123), Value::Null]]
        );

        let _ = conn;
    }

    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
    assert_eq!(db.query("SELECT a, b, c FROM t;"), "42|hello|");
    assert_eq!(db.query("SELECT rowid, id, v FROM u;"), "1|1|99");
    assert_eq!(db.query("SELECT rowid, id, v FROM w;"), "123|123|");
}

// -----------------------------------------------------------------------------
// M2.24: RETURNING clause
// -----------------------------------------------------------------------------

#[test]
fn insert_returning_matches_oracle() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("insert_returning");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE t(a INTEGER PRIMARY KEY, b, c);");
        // Basic RETURNING with rowid and stored columns.
        let rows = collect_returning(
            &mut conn,
            "INSERT INTO t(b, c) VALUES (10, 99) RETURNING a, b, c, rowid;",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 4);
        assert_eq!(rows[0][0], rows[0][3]); // a == rowid alias
        assert_eq!(rows[0][1], Value::Int(10));
        assert_eq!(rows[0][2], Value::Int(99));

        // Multi-row VALUES with * expansion.
        let rows = collect_returning(
            &mut conn,
            "INSERT INTO t(b, c) VALUES ('hello', 1), ('world', 2) RETURNING *;",
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(2), Value::Text("hello".into()), Value::Int(1)],
                vec![Value::Int(3), Value::Text("world".into()), Value::Int(2)],
            ]
        );

        // DEFAULT VALUES with RETURNING *.
        let rows = collect_returning(
            &mut conn,
            "INSERT INTO t DEFAULT VALUES RETURNING a, b, c;",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], vec![Value::Int(4), Value::Null, Value::Null]);

        let _ = conn;
    }

    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
    assert_eq!(db.query("SELECT a, b, c FROM t;"), "1|10|99\n2|hello|1\n3|world|2\n4||");
}

#[test]
fn update_delete_returning_matches_oracle() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("update_delete_returning");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE t(x, y);");
        exec(&mut conn, "INSERT INTO t VALUES (1, 'one'), (2, 'two'), (3, 'three');");

        let rows = collect_returning(
            &mut conn,
            "UPDATE t SET y = 'changed' WHERE x = 2 RETURNING rowid, x, y;",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1], Value::Int(2));
        assert_eq!(rows[0][2], Value::Text("changed".into()));

        // DELETE with RETURNING * and WHERE.
        let rows = collect_returning(
            &mut conn,
            "DELETE FROM t WHERE x > 1 RETURNING *;",
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(2), Value::Text("changed".into())],
                vec![Value::Int(3), Value::Text("three".into())],
            ]
        );

        let _ = conn;
    }

    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
    assert_eq!(db.query("SELECT x, y FROM t;"), "1|one");
}

#[test]
fn returning_real_affinity() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("returning_real_affinity");

    {
        let mut conn = sqlite3_open(db.str()).expect("open");
        exec(&mut conn, "CREATE TABLE t(x REAL);");
        let rows = collect_returning(
            &mut conn,
            "INSERT INTO t(x) VALUES (5.0) RETURNING x, typeof(x);",
        );
        assert_eq!(
            rows,
            vec![vec![Value::Real(5.0), Value::Text("real".into())]]
        );
        let rows = collect_returning(
            &mut conn,
            "UPDATE t SET x = x + 1 RETURNING x, typeof(x);",
        );
        assert_eq!(
            rows,
            vec![vec![Value::Real(6.0), Value::Text("real".into())]]
        );
        let rows = collect_returning(
            &mut conn,
            "DELETE FROM t RETURNING x, typeof(x);",
        );
        assert_eq!(
            rows,
            vec![vec![Value::Real(6.0), Value::Text("real".into())]]
        );
        let _ = conn;
    }

    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
}
