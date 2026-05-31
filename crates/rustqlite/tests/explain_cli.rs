//! CLI parity for `EXPLAIN` / `EXPLAIN QUERY PLAN`.
//!
//! The headline guarantee for A2: the rustqlite shell's `EXPLAIN QUERY PLAN` tree output
//! byte-matches the `sqlite3` shell for the supported plan shapes. We build a fixture database
//! with the oracle, then run the SAME query through both shells and assert the stdout is
//! identical. Plain `EXPLAIN` is only checked to be a columnar table (it is NOT byte-compared to
//! the oracle — our bytecode legitimately differs).
//!
//! Skips when `sqlite3` is not installed.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Path to the built `rustsqlite` binary (Cargo sets this for integration tests).
const RUSTSQLITE: &str = env!("CARGO_BIN_EXE_rustsqlite");

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
    fn new() -> TempDb {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "rustqlite_explaincli_{}_{n}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        TempDb { path }
    }
    fn str(&self) -> &str {
        self.path.to_str().unwrap()
    }
    fn setup(&self, sql: &str) {
        let out = Command::new("sqlite3")
            .arg(self.str())
            .arg(sql)
            .output()
            .expect("run sqlite3 setup");
        assert!(out.status.success());
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-journal", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.str()));
        }
    }
}

fn oracle_stdout(db: &str, query: &str) -> String {
    let out = Command::new("sqlite3")
        .arg(db)
        .arg(query)
        .output()
        .expect("run sqlite3");
    assert!(out.status.success());
    String::from_utf8(out.stdout).expect("utf8")
}

fn rustqlite_stdout(db: &str, query: &str) -> String {
    let out = Command::new(RUSTSQLITE)
        .arg(db)
        .arg(query)
        .output()
        .expect("run rustsqlite");
    String::from_utf8(out.stdout).expect("utf8")
}

#[test]
fn eqp_tree_byte_matches_oracle() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    let db = TempDb::new();
    db.setup("CREATE TABLE t(a,b);");

    for query in [
        "EXPLAIN QUERY PLAN SELECT * FROM t;",
        "EXPLAIN QUERY PLAN SELECT * FROM t ORDER BY a;",
        "EXPLAIN QUERY PLAN SELECT 1;",
        "EXPLAIN QUERY PLAN SELECT * FROM t WHERE a=1;",
    ] {
        let oracle = oracle_stdout(db.str(), query);
        let rust = rustqlite_stdout(db.str(), query);
        assert_eq!(rust, oracle, "EQP tree mismatch for: {query}");
    }
}

#[test]
fn plain_explain_is_columnar_and_mode_independent() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    let db = TempDb::new();
    db.setup("CREATE TABLE t(a,b);");

    // Even with `.mode json` set, plain EXPLAIN must render the fixed column table (headers row
    // with the 8 EXPLAIN columns, then a dashed rule), NOT a JSON array.
    let out = Command::new(RUSTSQLITE)
        .arg(db.str())
        .arg(".mode json")
        .arg("EXPLAIN SELECT a FROM t;")
        .output()
        .expect("run rustsqlite");
    let text = String::from_utf8(out.stdout).expect("utf8");
    let mut lines = text.lines();
    let header = lines.next().unwrap_or("");
    assert!(
        header.starts_with("addr") && header.contains("opcode") && header.contains("comment"),
        "expected EXPLAIN column header, got: {header:?}"
    );
    assert!(
        !text.trim_start().starts_with('['),
        "plain EXPLAIN must not honor .mode json"
    );
}
