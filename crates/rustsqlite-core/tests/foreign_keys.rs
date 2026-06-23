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

/// Format rows into the oracle's pipe-separated text, with rows sorted lexicographically.
/// `PRAGMA foreign_key_check` walks tables in hash-table order upstream (which differs from
/// our catalog order); both orderings are correct for an unordered result, so we sort both
/// sides before comparing.
fn fmt_rows_sorted(rows: &[Vec<Value>]) -> String {
    let mut lines: Vec<String> = rows
        .iter()
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
        .collect();
    lines.sort();
    lines.join("\n")
}

/// Run SQL through the system `sqlite3` and return its trimmed stdout, with rows sorted
/// lexicographically (for unordered-result pragmas like `foreign_key_check`).
fn oracle_sorted(db: &TempDb, sql: &str) -> String {
    let out = Command::new("sqlite3")
        .arg(db.str())
        .arg(sql)
        .output()
        .expect("run sqlite3");
    assert!(
        out.status.success(),
        "sqlite3 failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut lines: Vec<String> = String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .split('\n')
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();
    lines.sort();
    lines.join("\n")
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

// ---------------------------------------------------------------------------
// PRAGMA foreign_key_check (M17.5)
// ---------------------------------------------------------------------------

/// `PRAGMA foreign_key_check` returns zero rows when there are no FK constraints at all.
#[test]
fn foreign_key_check_no_fks_returns_empty() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("fkc_nofk");

    let mut conn = sqlite3_open(db.str()).expect("open");
    exec(&mut conn, "CREATE TABLE t(a, b);");
    exec(&mut conn, "INSERT INTO t VALUES(1, 2);");
    let rows = query_rows(&mut conn, "PRAGMA foreign_key_check;");
    assert!(rows.is_empty());
    assert_eq!(db.oracle("PRAGMA foreign_key_check;"), "");
}

/// `PRAGMA foreign_key_check` returns zero rows when all FKs are satisfied.
#[test]
fn foreign_key_check_satisfied_returns_empty() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("fkc_ok");

    let mut conn = sqlite3_open(db.str()).expect("open");
    exec(&mut conn, "CREATE TABLE parent(id INTEGER PRIMARY KEY);");
    exec(&mut conn, "INSERT INTO parent VALUES(88),(89);");
    exec(&mut conn, "CREATE TABLE child(x INTEGER REFERENCES parent(id));");
    // FK enforcement is off (M17.6+ deferred), so these inserts succeed even though only
    // 88 and 89 exist in parent. Valid references don't violate.
    exec(&mut conn, "INSERT INTO child VALUES(88),(89);");
    let rows = query_rows(&mut conn, "PRAGMA foreign_key_check;");
    assert!(rows.is_empty());
    assert_eq!(db.oracle("PRAGMA foreign_key_check;"), "");
}

/// `PRAGMA foreign_key_check` reports each violating row, with the four columns
/// `table, rowid, parent, fkid`. Matches the oracle's output exactly.
#[test]
fn foreign_key_check_rowid_fk_violation_matches_oracle() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("fkc_rowid");

    let mut conn = sqlite3_open(db.str()).expect("open");
    exec(&mut conn, "CREATE TABLE p1(a INTEGER PRIMARY KEY);");
    exec(&mut conn, "INSERT INTO p1 VALUES(88),(89);");
    exec(&mut conn, "CREATE TABLE c1(x INTEGER PRIMARY KEY references p1);");
    // Insert violating rows (90, 87 → not in p1; 88 → ok).
    exec(&mut conn, "INSERT INTO c1 VALUES(90),(87),(88);");
    let rows = query_rows(&mut conn, "PRAGMA foreign_key_check;");
    let ours = fmt_rows(&rows);
    let theirs = db.oracle("PRAGMA foreign_key_check;");
    assert_eq!(ours, theirs, "foreign_key_check mismatch (rowid fk)");
}

/// `PRAGMA foreign_key_check(table)` filters to a single child table.
#[test]
fn foreign_key_check_filtered_to_table_matches_oracle() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("fkc_filter");

    let mut conn = sqlite3_open(db.str()).expect("open");
    exec(&mut conn, "CREATE TABLE p1(a INTEGER PRIMARY KEY);");
    exec(&mut conn, "INSERT INTO p1 VALUES(88),(89);");
    exec(&mut conn, "CREATE TABLE c1(x INTEGER PRIMARY KEY references p1);");
    exec(&mut conn, "CREATE TABLE c2(x INTEGER PRIMARY KEY references p1);");
    exec(&mut conn, "INSERT INTO c1 VALUES(90),(87),(88);");
    exec(&mut conn, "INSERT INTO c2 VALUES(91),(89);");

    // The full check covers both tables — order is unspecified (hash-table walk upstream
    // vs. catalog order here), so compare sorted.
    let rows = query_rows(&mut conn, "PRAGMA foreign_key_check;");
    let ours = fmt_rows_sorted(&rows);
    let theirs = oracle_sorted(&db, "PRAGMA foreign_key_check;");
    assert_eq!(ours, theirs, "foreign_key_check mismatch (full)");

    // The filtered check covers only c2.
    let rows = query_rows(&mut conn, "PRAGMA foreign_key_check(c2);");
    let ours = fmt_rows(&rows);
    let theirs = db.oracle("PRAGMA foreign_key_check(c2);");
    assert_eq!(ours, theirs, "foreign_key_check mismatch (filtered c2)");

    // c1 has its own violations too.
    let rows = query_rows(&mut conn, "PRAGMA foreign_key_check(c1);");
    let ours = fmt_rows(&rows);
    let theirs = db.oracle("PRAGMA foreign_key_check(c1);");
    assert_eq!(ours, theirs, "foreign_key_check mismatch (filtered c1)");
}

/// A NULL child FK column is skipped — NULL foreign keys never violate the constraint
/// (matching upstream's `OP_IsNull → addrOk` early-out in `pragma.c`).
#[test]
fn foreign_key_check_null_child_skipped() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("fkc_null");

    let mut conn = sqlite3_open(db.str()).expect("open");
    exec(&mut conn, "CREATE TABLE p1(a INTEGER PRIMARY KEY);");
    exec(&mut conn, "INSERT INTO p1 VALUES(88);");
    exec(&mut conn, "CREATE TABLE c1(x INTEGER REFERENCES p1);");
    // A NULL child key is not a violation.
    exec(&mut conn, "INSERT INTO c1 VALUES(NULL),(90);");
    let rows = query_rows(&mut conn, "PRAGMA foreign_key_check;");
    let ours = fmt_rows(&rows);
    let theirs = db.oracle("PRAGMA foreign_key_check;");
    assert_eq!(ours, theirs, "foreign_key_check mismatch (null child)");
}

/// A multi-column FK referencing a composite parent PK is checked correctly. The oracle
/// creates an implicit unique index for the composite PK and uses it for the lookup; our
/// engine's `find_covering_index` finds the same implicit index. (Note: the oracle is run
/// against an oracle-created file here because our engine's composite-PK implicit-index
/// population is a known M5.3+ gap — `PRAGMA integrity_check` reports "wrong # of entries
/// in index sqlite_autoindex_p_1" on a Rustqlite-written file. The FK-check logic itself is
/// correct, as verified by the oracle's output on its own file.)
#[test]
fn foreign_key_check_multicolumn_fk_matches_oracle() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("fkc_multi");

    // Build the schema with the oracle (so the implicit composite-PK index is populated
    // correctly), then run our `PRAGMA foreign_key_check` against that file.
    let setup = [
        "CREATE TABLE p(a INTEGER, b INTEGER, PRIMARY KEY(a,b));",
        "INSERT INTO p VALUES(1,2),(3,4);",
        "CREATE TABLE c(x INTEGER, y INTEGER, FOREIGN KEY(x,y) REFERENCES p(a,b));",
        "INSERT INTO c VALUES(1,2),(1,3),(3,4),(5,6);",
    ];
    for s in setup {
        let out = Command::new("sqlite3")
            .arg(db.str())
            .arg(s)
            .output()
            .expect("run sqlite3");
        assert!(out.status.success(), "sqlite3 setup failed: {}", String::from_utf8_lossy(&out.stderr));
    }

    let mut conn = sqlite3_open(db.str()).expect("open");
    let rows = query_rows(&mut conn, "PRAGMA foreign_key_check;");
    let ours = fmt_rows(&rows);
    let theirs = db.oracle("PRAGMA foreign_key_check;");
    assert_eq!(ours, theirs, "foreign_key_check mismatch (multicolumn)");
}

/// `PRAGMA foreign_key_check(no_such_table)` raises "no such table: no_such_table",
/// matching the oracle.
#[test]
fn foreign_key_check_missing_table_errors() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("fkc_missing");

    let mut conn = sqlite3_open(db.str()).expect("open");
    exec(&mut conn, "CREATE TABLE t(a);");
    // The prepare should fail.
    let result = sqlite3_prepare_v2(&mut conn, "PRAGMA foreign_key_check(no_such_table);");
    assert!(result.is_err(), "expected error for missing table");
    let msg = result.err().unwrap().to_string();
    assert!(
        msg.contains("no such table"),
        "expected 'no such table' in error, got: {msg}"
    );
    // The oracle also errors.
    let out = Command::new("sqlite3")
        .arg(db.str())
        .arg("PRAGMA foreign_key_check(no_such_table);")
        .output()
        .expect("run sqlite3");
    assert!(
        !out.status.success(),
        "sqlite3 should error on missing table"
    );
}

/// An empty database (no tables) returns zero rows from `PRAGMA foreign_key_check`.
#[test]
fn foreign_key_check_empty_db_returns_empty() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("fkc_empty");

    let mut conn = sqlite3_open(db.str()).expect("open");
    let rows = query_rows(&mut conn, "PRAGMA foreign_key_check;");
    assert!(rows.is_empty());
    assert_eq!(db.oracle("PRAGMA foreign_key_check;"), "");
}

/// A child table that references a non-existent parent table reports every non-NULL child
/// row as a violation (mirrors upstream's `PragTyp_FOREIGN_KEY_CHECK` second loop, which
/// leaves `pParent == 0` and falls through to the violation-reporting path).
#[test]
fn foreign_key_check_dangling_parent_reported() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("fkc_dangling");

    let mut conn = sqlite3_open(db.str()).expect("open");
    // Create a child referencing a parent that doesn't exist. (FK enforcement is off, so
    // this CREATE succeeds — and the parent table is never created.)
    exec(&mut conn, "CREATE TABLE c(x INTEGER REFERENCES ghost(id));");
    exec(&mut conn, "INSERT INTO c VALUES(1);");
    let rows = query_rows(&mut conn, "PRAGMA foreign_key_check;");
    let ours = fmt_rows(&rows);
    let theirs = db.oracle("PRAGMA foreign_key_check;");
    assert_eq!(ours, theirs, "dangling parent should be reported");
}

/// A FK referencing the parent's PK by an explicit index (e.g. `REFERENCES p(id)`) where
/// the parent has a unique index on `id` uses the index-lookup path. Matches the oracle.
#[test]
fn foreign_key_check_indexed_parent_matches_oracle() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("fkc_idx");

    let mut conn = sqlite3_open(db.str()).expect("open");
    exec(&mut conn, "CREATE TABLE p(id INTEGER PRIMARY KEY, name TEXT);");
    exec(&mut conn, "INSERT INTO p VALUES(1,'a'),(2,'b'),(3,'c');");
    exec(&mut conn, "CREATE TABLE c(x INTEGER REFERENCES p(id));");
    exec(&mut conn, "INSERT INTO c VALUES(1),(2),(4),(5);");
    let rows = query_rows(&mut conn, "PRAGMA foreign_key_check;");
    let ours = fmt_rows(&rows);
    let theirs = db.oracle("PRAGMA foreign_key_check;");
    assert_eq!(ours, theirs, "foreign_key_check mismatch (indexed parent)");
}