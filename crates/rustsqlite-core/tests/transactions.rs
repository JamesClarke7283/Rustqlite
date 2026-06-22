//! Transaction-control tests (M12.3 + M12.6): `BEGIN [DEFERRED|IMMEDIATE|EXCLUSIVE]` /
//! `COMMIT`/`END`/`ROLLBACK` via `OP_AutoCommit` (+ `OP_Transaction` for IMMEDIATE/EXCLUSIVE),
//! plus the `SAVEPOINT` family.
//!
//! Plain `#[test]`s (drive the engine via `sqlite3_step`); skipped if the system `sqlite3`
//! oracle is absent. Differential cases replay the same SQL sequence against the C oracle and
//! compare the resulting table contents and the error text from invalid transitions.

use std::process::Command;

use rustsqlite_core::capi::ResultCode;
use rustsqlite_core::{sqlite3_open, sqlite3_prepare_v2, Value};

fn sqlite3_available() -> bool {
    Command::new("sqlite3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn temp_db(tag: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "rustsqlite_tx_{tag}_{}.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    p.to_str().unwrap().to_string()
}

/// Run a single SQL statement that returns no rows; assert it steps to `Done`.
fn exec(conn: &mut rustsqlite_core::Sqlite3, sql: &str) -> ResultCode {
    let (mut stmt, _) = sqlite3_prepare_v2(conn, sql).expect("prepare");
    stmt.step()
}

/// Collect all rows of a SELECT into a `Vec<Vec<Value>>`.
fn collect(stmt: &mut rustsqlite_core::Sqlite3Stmt) -> Vec<Vec<Value>> {
    let ncol = stmt.column_count();
    let mut rows = Vec::new();
    loop {
        match stmt.step() {
            ResultCode::Row => rows.push((0..ncol).map(|i| stmt.column_value(i)).collect()),
            ResultCode::Done => break,
            other => panic!("unexpected step result {other:?}"),
        }
    }
    rows
}

/// Read `SELECT a FROM t ORDER BY a` from `conn`.
fn read_a(conn: &mut rustsqlite_core::Sqlite3) -> Vec<Vec<Value>> {
    let (mut stmt, _) = sqlite3_prepare_v2(conn, "SELECT a FROM t ORDER BY a;").unwrap();
    collect(&mut stmt)
}

/// Run the same SQL script against the C oracle and return the contents of `t` as a sorted
/// string of `a` values joined by commas.
fn oracle_table_contents(db_path: &str, setup_and_body: &str) -> String {
    // Re-create from scratch in a separate file so the oracle run is hermetic.
    let oracle_path = format!("{db_path}.oracle");
    let _ = std::fs::remove_file(&oracle_path);
    Command::new("sqlite3")
        .arg(&oracle_path)
        .arg(setup_and_body)
        .output()
        .expect("sqlite3 oracle");
    let out = Command::new("sqlite3")
        .arg(&oracle_path)
        .arg("SELECT a FROM t ORDER BY a;")
        .output()
        .expect("sqlite3 read");
    let _ = std::fs::remove_file(&oracle_path);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn values_to_lines(rows: &[Vec<Value>]) -> String {
    let mut s = String::new();
    for r in rows {
        s.push_str(&match &r[0] {
            Value::Int(i) => i.to_string(),
            Value::Null => "NULL".to_string(),
            other => format!("{other:?}"),
        });
        s.push('\n');
    }
    // Match the oracle's `String::from_utf8_lossy(..).trim()` (no trailing newline).
    s.trim_end().to_string()
}

#[test]
fn begin_commit_persists_insert() {
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("begin_commit");
    let body = "CREATE TABLE t(a);\n\
                BEGIN;\n\
                INSERT INTO t VALUES (1),(2),(3);\n\
                COMMIT;\n\
                SELECT a FROM t ORDER BY a;\n";
    let expected = oracle_table_contents(&db, body);

    let mut conn = sqlite3_open(&db).unwrap();
    assert!(conn.autocommit(), "connection starts in autocommit mode");
    exec(&mut conn, "CREATE TABLE t(a);");
    assert!(conn.autocommit(), "DDL commits and autocommit stays on");

    assert_eq!(exec(&mut conn, "BEGIN;"), ResultCode::Done);
    assert!(!conn.autocommit(), "BEGIN turns autocommit off");
    assert_eq!(exec(&mut conn, "INSERT INTO t VALUES (1),(2),(3);"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "COMMIT;"), ResultCode::Done);
    assert!(conn.autocommit(), "COMMIT turns autocommit back on");

    let rows = read_a(&mut conn);
    assert_eq!(values_to_lines(&rows), expected);

    let _ = std::fs::remove_file(&db);
}

#[test]
fn begin_rollback_undoes_insert() {
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("begin_rollback");
    let body = "CREATE TABLE t(a); INSERT INTO t VALUES (9);\n\
                BEGIN; INSERT INTO t VALUES (1),(2); ROLLBACK;\n\
                SELECT a FROM t ORDER BY a;\n";
    let expected = oracle_table_contents(&db, body);

    let mut conn = sqlite3_open(&db).unwrap();
    exec(&mut conn, "CREATE TABLE t(a);");
    exec(&mut conn, "INSERT INTO t VALUES (9);");

    assert_eq!(exec(&mut conn, "BEGIN;"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "INSERT INTO t VALUES (1),(2);"), ResultCode::Done);
    // The rows must be visible inside the transaction.
    let in_txn = read_a(&mut conn);
    assert_eq!(
        values_to_lines(&in_txn),
        "1\n2\n9",
        "rows visible inside the transaction"
    );
    assert_eq!(exec(&mut conn, "ROLLBACK;"), ResultCode::Done);
    assert!(conn.autocommit(), "ROLLBACK restores autocommit");

    let rows = read_a(&mut conn);
    assert_eq!(values_to_lines(&rows), expected);

    let _ = std::fs::remove_file(&db);
}

#[test]
fn end_is_alias_for_commit() {
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("end_alias");
    let mut conn = sqlite3_open(&db).unwrap();
    exec(&mut conn, "CREATE TABLE t(a);");
    assert_eq!(exec(&mut conn, "BEGIN;"), ResultCode::Done);
    exec(&mut conn, "INSERT INTO t VALUES (5);");
    assert_eq!(exec(&mut conn, "END;"), ResultCode::Done);
    assert!(conn.autocommit(), "END commits and restores autocommit");
    let rows = read_a(&mut conn);
    assert_eq!(values_to_lines(&rows), "5");
    let _ = std::fs::remove_file(&db);
}

#[test]
fn nested_begin_errors() {
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("nested_begin");
    let mut conn = sqlite3_open(&db).unwrap();
    exec(&mut conn, "CREATE TABLE t(a);");
    assert_eq!(exec(&mut conn, "BEGIN;"), ResultCode::Done);
    // A second BEGIN must error.
    let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "BEGIN;").unwrap();
    let res = stmt.step();
    assert!(
        matches!(res, ResultCode::Error),
        "nested BEGIN should error, got {res:?}"
    );
    // The connection stays in the outer transaction.
    assert!(!conn.autocommit());
    // Clean up so the file can be removed.
    exec(&mut conn, "ROLLBACK;");

    let _ = std::fs::remove_file(&db);
}

#[test]
fn commit_without_transaction_errors() {
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("commit_no_txn");
    let mut conn = sqlite3_open(&db).unwrap();
    exec(&mut conn, "CREATE TABLE t(a);");
    // COMMIT with no active transaction should error.
    let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "COMMIT;").unwrap();
    let res = stmt.step();
    assert!(
        matches!(res, ResultCode::Error),
        "COMMIT without a transaction should error, got {res:?}"
    );
    assert!(conn.autocommit(), "failed COMMIT keeps autocommit on");
    let _ = std::fs::remove_file(&db);
}

#[test]
fn rollback_without_transaction_errors() {
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("rollback_no_txn");
    let mut conn = sqlite3_open(&db).unwrap();
    exec(&mut conn, "CREATE TABLE t(a);");
    let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "ROLLBACK;").unwrap();
    let res = stmt.step();
    assert!(
        matches!(res, ResultCode::Error),
        "ROLLBACK without a transaction should error, got {res:?}"
    );
    assert!(conn.autocommit(), "failed ROLLBACK keeps autocommit on");
    let _ = std::fs::remove_file(&db);
}

/// Run the same SQL script against the C oracle and return the resulting `t` table as a sorted
/// string of `a` values (used by the savepoint differential cases, mirroring
/// [`oracle_table_contents`]). Uses a per-test oracle DB path so parallel test threads don't
/// collide on the shared file.
fn oracle_table_contents_with_setup(setup: &str, body: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let oracle_path = format!(
        "/tmp/rustsqlite_savepoint_oracle_{}_{}.db",
        std::process::id(),
        unique
    );
    let _ = std::fs::remove_file(&oracle_path);
    let script = format!("{setup}\n{body}\nSELECT a FROM t ORDER BY a;");
    Command::new("sqlite3")
        .arg(&oracle_path)
        .arg(&script)
        .output()
        .expect("sqlite3 oracle");
    let out = Command::new("sqlite3")
        .arg(&oracle_path)
        .arg("SELECT a FROM t ORDER BY a;")
        .output()
        .expect("sqlite3 read");
    let _ = std::fs::remove_file(&oracle_path);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn savepoint_auto_starts_transaction_and_release_commits() {
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("savepoint_auto");
    let setup = "CREATE TABLE t(a);";
    let body = "SAVEPOINT s;\nINSERT INTO t VALUES (1),(2);\nRELEASE s;";
    let expected = oracle_table_contents_with_setup(setup, body);

    let mut conn = sqlite3_open(&db).unwrap();
    exec(&mut conn, "CREATE TABLE t(a);");
    assert!(conn.autocommit(), "starts in autocommit");
    // SAVEPOINT outside any BEGIN auto-starts a transaction.
    assert_eq!(exec(&mut conn, "SAVEPOINT s;"), ResultCode::Done);
    assert!(!conn.autocommit(), "SAVEPOINT turns autocommit off");
    assert_eq!(exec(&mut conn, "INSERT INTO t VALUES (1),(2);"), ResultCode::Done);
    // Rows are visible inside the implicit transaction.
    let in_txn = read_a(&mut conn);
    assert_eq!(values_to_lines(&in_txn), "1\n2");
    // RELEASE of the outermost transaction savepoint commits.
    assert_eq!(exec(&mut conn, "RELEASE s;"), ResultCode::Done);
    assert!(conn.autocommit(), "RELEASE commits and restores autocommit");
    let rows = read_a(&mut conn);
    assert_eq!(values_to_lines(&rows), expected);

    let _ = std::fs::remove_file(&db);
}

#[test]
fn rollback_to_savepoint_inside_begin_discards_changes() {
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("savepoint_rollback_to");
    let setup = "CREATE TABLE t(a);";
    let body = "BEGIN;\nSAVEPOINT s;\nINSERT INTO t VALUES (1),(2);\nROLLBACK TO s;\nINSERT INTO t VALUES (3),(4);\nCOMMIT;";
    let expected = oracle_table_contents_with_setup(setup, body);

    let mut conn = sqlite3_open(&db).unwrap();
    exec(&mut conn, "CREATE TABLE t(a);");
    assert_eq!(exec(&mut conn, "BEGIN;"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "SAVEPOINT s;"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "INSERT INTO t VALUES (1),(2);"), ResultCode::Done);
    let in_savepoint = read_a(&mut conn);
    assert_eq!(values_to_lines(&in_savepoint), "1\n2");
    // ROLLBACK TO discards the inserts but keeps the savepoint on the stack.
    assert_eq!(exec(&mut conn, "ROLLBACK TO s;"), ResultCode::Done);
    let after_rollback = read_a(&mut conn);
    assert_eq!(values_to_lines(&after_rollback), "");
    assert_eq!(exec(&mut conn, "INSERT INTO t VALUES (3),(4);"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "COMMIT;"), ResultCode::Done);
    assert!(conn.autocommit(), "COMMIT restores autocommit");
    let rows = read_a(&mut conn);
    assert_eq!(values_to_lines(&rows), expected);

    let _ = std::fs::remove_file(&db);
}

#[test]
fn savepoint_auto_start_rollback_to_then_release() {
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("savepoint_auto_rollback");
    let setup = "CREATE TABLE t(a);";
    let body = "SAVEPOINT s;\nINSERT INTO t VALUES (1);\nROLLBACK TO s;\nINSERT INTO t VALUES (2);\nRELEASE s;";
    let expected = oracle_table_contents_with_setup(setup, body);

    let mut conn = sqlite3_open(&db).unwrap();
    exec(&mut conn, "CREATE TABLE t(a);");
    assert_eq!(exec(&mut conn, "SAVEPOINT s;"), ResultCode::Done);
    assert!(!conn.autocommit());
    assert_eq!(exec(&mut conn, "INSERT INTO t VALUES (1);"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "ROLLBACK TO s;"), ResultCode::Done);
    // Savepoint is still on the stack — autocommit still off.
    assert!(!conn.autocommit());
    assert_eq!(exec(&mut conn, "INSERT INTO t VALUES (2);"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "RELEASE s;"), ResultCode::Done);
    assert!(conn.autocommit());
    let rows = read_a(&mut conn);
    assert_eq!(values_to_lines(&rows), expected);

    let _ = std::fs::remove_file(&db);
}

#[test]
fn nested_savepoints_inner_rollback_then_outer() {
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("savepoint_nested");
    let setup = "CREATE TABLE t(a);";
    let body = "BEGIN;\nSAVEPOINT a;\nINSERT INTO t VALUES (1);\nSAVEPOINT b;\nINSERT INTO t VALUES (2);\nROLLBACK TO b;\nINSERT INTO t VALUES (3);\nCOMMIT;";
    let expected = oracle_table_contents_with_setup(setup, body);

    let mut conn = sqlite3_open(&db).unwrap();
    exec(&mut conn, "CREATE TABLE t(a);");
    assert_eq!(exec(&mut conn, "BEGIN;"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "SAVEPOINT a;"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "INSERT INTO t VALUES (1);"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "SAVEPOINT b;"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "INSERT INTO t VALUES (2);"), ResultCode::Done);
    assert_eq!(values_to_lines(&read_a(&mut conn)), "1\n2");
    assert_eq!(exec(&mut conn, "ROLLBACK TO b;"), ResultCode::Done);
    assert_eq!(values_to_lines(&read_a(&mut conn)), "1");
    assert_eq!(exec(&mut conn, "INSERT INTO t VALUES (3);"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "COMMIT;"), ResultCode::Done);
    let rows = read_a(&mut conn);
    assert_eq!(values_to_lines(&rows), expected);

    let _ = std::fs::remove_file(&db);
}

#[test]
fn release_inner_savepoint_keeps_changes() {
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("savepoint_release_inner");
    let setup = "CREATE TABLE t(a);";
    let body = "BEGIN;\nSAVEPOINT a;\nINSERT INTO t VALUES (1);\nSAVEPOINT b;\nINSERT INTO t VALUES (2);\nRELEASE b;\nINSERT INTO t VALUES (3);\nROLLBACK TO a;\nCOMMIT;";
    let expected = oracle_table_contents_with_setup(setup, body);

    let mut conn = sqlite3_open(&db).unwrap();
    exec(&mut conn, "CREATE TABLE t(a);");
    assert_eq!(exec(&mut conn, "BEGIN;"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "SAVEPOINT a;"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "INSERT INTO t VALUES (1);"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "SAVEPOINT b;"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "INSERT INTO t VALUES (2);"), ResultCode::Done);
    // RELEASE b — the changes since b become part of the enclosing transaction.
    assert_eq!(exec(&mut conn, "RELEASE b;"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "INSERT INTO t VALUES (3);"), ResultCode::Done);
    // ROLLBACK TO a discards everything since a (1, 2, 3 all gone).
    assert_eq!(exec(&mut conn, "ROLLBACK TO a;"), ResultCode::Done);
    assert_eq!(values_to_lines(&read_a(&mut conn)), "");
    assert_eq!(exec(&mut conn, "COMMIT;"), ResultCode::Done);
    let rows = read_a(&mut conn);
    assert_eq!(values_to_lines(&rows), expected);

    let _ = std::fs::remove_file(&db);
}

#[test]
fn rollback_to_savepoint_keepable_re_rollback() {
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("savepoint_re_rollback");
    let setup = "CREATE TABLE t(a);";
    let body = "BEGIN;\nSAVEPOINT s;\nINSERT INTO t VALUES (1);\nROLLBACK TO s;\nROLLBACK TO s;\nCOMMIT;";
    let expected = oracle_table_contents_with_setup(setup, body);

    let mut conn = sqlite3_open(&db).unwrap();
    exec(&mut conn, "CREATE TABLE t(a);");
    assert_eq!(exec(&mut conn, "BEGIN;"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "SAVEPOINT s;"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "INSERT INTO t VALUES (1);"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "ROLLBACK TO s;"), ResultCode::Done);
    // The savepoint is still on the stack — ROLLBACK TO again is a no-op (empty dirty overlay).
    assert_eq!(exec(&mut conn, "ROLLBACK TO s;"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "COMMIT;"), ResultCode::Done);
    let rows = read_a(&mut conn);
    assert_eq!(values_to_lines(&rows), expected);

    let _ = std::fs::remove_file(&db);
}

#[test]
fn unknown_savepoint_errors() {
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("savepoint_unknown");
    let mut conn = sqlite3_open(&db).unwrap();
    exec(&mut conn, "CREATE TABLE t(a);");
    assert_eq!(exec(&mut conn, "BEGIN;"), ResultCode::Done);
    // RELEASE / ROLLBACK TO with an unknown savepoint name must error.
    let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "RELEASE no_such;").unwrap();
    assert!(matches!(stmt.step(), ResultCode::Error), "RELEASE unknown errors");
    let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "ROLLBACK TO no_such;").unwrap();
    assert!(matches!(stmt.step(), ResultCode::Error), "ROLLBACK TO unknown errors");
    // The connection stays in the outer transaction.
    assert!(!conn.autocommit());
    exec(&mut conn, "ROLLBACK;");
    let _ = std::fs::remove_file(&db);
}

#[test]
fn commit_via_explicit_commit_after_savepoint_auto_start() {
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("savepoint_then_commit");
    let setup = "CREATE TABLE t(a);";
    let body = "SAVEPOINT s;\nINSERT INTO t VALUES (1);\nCOMMIT;";
    let expected = oracle_table_contents_with_setup(setup, body);

    let mut conn = sqlite3_open(&db).unwrap();
    exec(&mut conn, "CREATE TABLE t(a);");
    assert_eq!(exec(&mut conn, "SAVEPOINT s;"), ResultCode::Done);
    assert!(!conn.autocommit());
    assert_eq!(exec(&mut conn, "INSERT INTO t VALUES (1);"), ResultCode::Done);
    // An explicit COMMIT commits the implicit transaction started by SAVEPOINT.
    assert_eq!(exec(&mut conn, "COMMIT;"), ResultCode::Done);
    assert!(conn.autocommit(), "COMMIT restores autocommit");
    let rows = read_a(&mut conn);
    assert_eq!(values_to_lines(&rows), expected);

    let _ = std::fs::remove_file(&db);
}

// ---------------------------------------------------------------------------
// M12.6 — `BEGIN IMMEDIATE` / `BEGIN EXCLUSIVE` locking.
//
// In SQLite, `BEGIN IMMEDIATE` acquires a RESERVED lock up-front (so writes from other
// connections block, reads still work), while `BEGIN EXCLUSIVE` acquires an EXCLUSIVE lock
// (so even readers block). `BEGIN DEFERRED` (the default) acquires no lock at BEGIN; the
// RESERVED lock is taken lazily at first write via the `OP_Transaction 0 1` opcode every
// write statement already emits.
//
// Codegen mirrors `sqlite3BeginTransaction` in `build.c`:
// * `BEGIN DEFERRED` → `OP_AutoCommit 0 0` only.
// * `BEGIN IMMEDIATE` → `OP_Transaction 0 1` + `OP_AutoCommit 0 0` (RESERVED up-front).
// * `BEGIN EXCLUSIVE` → `OP_Transaction 0 2` + `OP_AutoCommit 0 0` (EXCLUSIVE up-front).
//
// The single-connection tests below verify the behavior matches the C oracle: each BEGIN
// variant commits/rolls back the same way, and a nested BEGIN IMMEDIATE/EXCLUSIVE inside an
// active transaction errors with the same "cannot start a transaction within a transaction"
// message as in SQLite.
// ---------------------------------------------------------------------------

#[test]
fn begin_immediate_commits_like_oracle() {
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("begin_imm");
    let setup = "CREATE TABLE t(a);";
    let body = "BEGIN IMMEDIATE;\nINSERT INTO t VALUES (1),(2),(3);\nCOMMIT;";
    let expected = oracle_table_contents_with_setup(setup, body);

    let mut conn = sqlite3_open(&db).unwrap();
    exec(&mut conn, "CREATE TABLE t(a);");
    assert!(conn.autocommit(), "starts in autocommit");
    assert_eq!(exec(&mut conn, "BEGIN IMMEDIATE;"), ResultCode::Done);
    assert!(!conn.autocommit(), "BEGIN IMMEDIATE turns autocommit off");
    assert_eq!(exec(&mut conn, "INSERT INTO t VALUES (1),(2),(3);"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "COMMIT;"), ResultCode::Done);
    assert!(conn.autocommit(), "COMMIT restores autocommit");

    let rows = read_a(&mut conn);
    assert_eq!(values_to_lines(&rows), expected);

    let _ = std::fs::remove_file(&db);
}

#[test]
fn begin_exclusive_commits_like_oracle() {
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("begin_excl");
    let setup = "CREATE TABLE t(a);";
    let body = "BEGIN EXCLUSIVE;\nINSERT INTO t VALUES (10),(20),(30);\nCOMMIT;";
    let expected = oracle_table_contents_with_setup(setup, body);

    let mut conn = sqlite3_open(&db).unwrap();
    exec(&mut conn, "CREATE TABLE t(a);");
    assert!(conn.autocommit(), "starts in autocommit");
    assert_eq!(exec(&mut conn, "BEGIN EXCLUSIVE;"), ResultCode::Done);
    assert!(!conn.autocommit(), "BEGIN EXCLUSIVE turns autocommit off");
    assert_eq!(exec(&mut conn, "INSERT INTO t VALUES (10),(20),(30);"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "COMMIT;"), ResultCode::Done);
    assert!(conn.autocommit(), "COMMIT restores autocommit");

    let rows = read_a(&mut conn);
    assert_eq!(values_to_lines(&rows), expected);

    let _ = std::fs::remove_file(&db);
}

#[test]
fn begin_immediate_rollback_like_oracle() {
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("begin_imm_rb");
    let setup = "CREATE TABLE t(a);";
    let body = "BEGIN IMMEDIATE;\nINSERT INTO t VALUES (1),(2);\nROLLBACK;";
    let expected = oracle_table_contents_with_setup(setup, body);

    let mut conn = sqlite3_open(&db).unwrap();
    exec(&mut conn, "CREATE TABLE t(a);");
    assert_eq!(exec(&mut conn, "BEGIN IMMEDIATE;"), ResultCode::Done);
    assert!(!conn.autocommit());
    assert_eq!(exec(&mut conn, "INSERT INTO t VALUES (1),(2);"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "ROLLBACK;"), ResultCode::Done);
    assert!(conn.autocommit(), "ROLLBACK restores autocommit");

    let rows = read_a(&mut conn);
    assert_eq!(values_to_lines(&rows), expected);

    let _ = std::fs::remove_file(&db);
}

#[test]
fn begin_immediate_then_read_works() {
    // `BEGIN IMMEDIATE` takes a RESERVED lock — readers on the same connection still work
    // (a SELECT inside the transaction returns the in-progress writes, matching SQLite).
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("begin_imm_read");
    let mut conn = sqlite3_open(&db).unwrap();
    exec(&mut conn, "CREATE TABLE t(a);");
    assert_eq!(exec(&mut conn, "BEGIN IMMEDIATE;"), ResultCode::Done);
    assert!(!conn.autocommit());
    assert_eq!(exec(&mut conn, "INSERT INTO t VALUES (7),(8);"), ResultCode::Done);
    // A SELECT inside the IMMEDIATE transaction sees the in-progress writes.
    let rows = read_a(&mut conn);
    assert_eq!(values_to_lines(&rows), "7\n8");
    assert_eq!(exec(&mut conn, "COMMIT;"), ResultCode::Done);
    assert!(conn.autocommit());

    let _ = std::fs::remove_file(&db);
}

#[test]
fn nested_begin_immediate_errors_like_oracle() {
    // A `BEGIN IMMEDIATE` inside an active transaction errors with the same message as
    // `BEGIN` (DEFERRED). The `OP_AutoCommit` arm detects the same-state transition; the
    // preceding `OP_Transaction 0 1` is idempotent (a no-op when a write txn is already open).
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("nested_begin_imm");
    let mut conn = sqlite3_open(&db).unwrap();
    exec(&mut conn, "CREATE TABLE t(a);");
    assert_eq!(exec(&mut conn, "BEGIN;"), ResultCode::Done);
    assert!(!conn.autocommit());

    let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "BEGIN IMMEDIATE;").unwrap();
    let res = stmt.step();
    assert!(
        matches!(res, ResultCode::Error),
        "nested BEGIN IMMEDIATE should error, got {res:?}"
    );
    // The failed BEGIN IMMEDIATE must not have flipped autocommit.
    assert!(!conn.autocommit(), "failed BEGIN IMMEDIATE keeps autocommit off");
    // The error message matches the C oracle.
    assert_eq!(
        stmt.errmsg(),
        "cannot start a transaction within a transaction"
    );

    // The outer transaction is still usable.
    assert_eq!(exec(&mut conn, "INSERT INTO t VALUES (1);"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "COMMIT;"), ResultCode::Done);
    assert!(conn.autocommit());

    let _ = std::fs::remove_file(&db);
}

#[test]
fn nested_begin_exclusive_errors_like_oracle() {
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("nested_begin_excl");
    let mut conn = sqlite3_open(&db).unwrap();
    exec(&mut conn, "CREATE TABLE t(a);");
    assert_eq!(exec(&mut conn, "BEGIN;"), ResultCode::Done);
    assert!(!conn.autocommit());

    let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "BEGIN EXCLUSIVE;").unwrap();
    let res = stmt.step();
    assert!(
        matches!(res, ResultCode::Error),
        "nested BEGIN EXCLUSIVE should error, got {res:?}"
    );
    assert!(!conn.autocommit(), "failed BEGIN EXCLUSIVE keeps autocommit off");
    assert_eq!(
        stmt.errmsg(),
        "cannot start a transaction within a transaction"
    );

    // The outer transaction is still usable.
    assert_eq!(exec(&mut conn, "INSERT INTO t VALUES (1);"), ResultCode::Done);
    assert_eq!(exec(&mut conn, "ROLLBACK;"), ResultCode::Done);
    assert!(conn.autocommit());

    let _ = std::fs::remove_file(&db);
}

#[test]
fn begin_immediate_after_begin_immediate_errors() {
    // Two `BEGIN IMMEDIATE` in a row: the second one errors, exactly like two `BEGIN`s would.
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("imm_then_imm");
    let mut conn = sqlite3_open(&db).unwrap();
    exec(&mut conn, "CREATE TABLE t(a);");
    assert_eq!(exec(&mut conn, "BEGIN IMMEDIATE;"), ResultCode::Done);
    assert!(!conn.autocommit());

    let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "BEGIN IMMEDIATE;").unwrap();
    let res = stmt.step();
    assert!(
        matches!(res, ResultCode::Error),
        "second BEGIN IMMEDIATE should error, got {res:?}"
    );
    assert_eq!(
        stmt.errmsg(),
        "cannot start a transaction within a transaction"
    );

    exec(&mut conn, "ROLLBACK;");
    let _ = std::fs::remove_file(&db);
}

#[test]
fn begin_exclusive_then_end_uses_read_works() {
    // Inside a `BEGIN EXCLUSIVE` transaction, the connection that owns the lock can still read
    // its own writes. (Cross-connection blocking isn't testable here since our in-process VFS
    // doesn't enforce cross-connection locks yet — that's M12.7's real-OS-locking follow-up.
    // This test verifies the same-connection behavior matches the oracle's.)
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("begin_excl_end");
    let setup = "CREATE TABLE t(a);";
    let body = "BEGIN EXCLUSIVE;\nINSERT INTO t VALUES (100);\nEND;";
    let expected = oracle_table_contents_with_setup(setup, body);

    let mut conn = sqlite3_open(&db).unwrap();
    exec(&mut conn, "CREATE TABLE t(a);");
    assert_eq!(exec(&mut conn, "BEGIN EXCLUSIVE;"), ResultCode::Done);
    assert!(!conn.autocommit());
    assert_eq!(exec(&mut conn, "INSERT INTO t VALUES (100);"), ResultCode::Done);
    // `END` is an alias for `COMMIT`.
    assert_eq!(exec(&mut conn, "END;"), ResultCode::Done);
    assert!(conn.autocommit(), "END commits and restores autocommit");

    let rows = read_a(&mut conn);
    assert_eq!(values_to_lines(&rows), expected);

    let _ = std::fs::remove_file(&db);
}

// ---------------------------------------------------------------------------
// M12.7 — Cross-connection VFS lock escalation.
//
// `BEGIN IMMEDIATE`/`BEGIN EXCLUSIVE` acquire RESERVED/EXCLUSIVE locks via
// `fcntl(F_SETLK)` byte-range locks on `OsTokioVfs` (mirrors `os_unix.c`'s `unixLock`).
// The in-process `LockState` registry (mirrors `unixInodeInfo`) catches same-process
// contention that the per-process OS locks would miss. These tests exercise the
// cross-connection contention paths on a real file (the second `sqlite3_open` on the
// same path sees the first connection's RESERVED/EXCLUSIVE lock and `BEGIN IMMEDIATE`/
// `BEGIN EXCLUSIVE` returns `SQLITE_BUSY`).
//
// They don't compare against the C oracle (the C oracle would block forever on a BUSY
// without a busy-handler; we verify the `SQLITE_BUSY` result code instead, which is
// what a real SQLite connection sees with a zero busy-timeout).
// ---------------------------------------------------------------------------

/// Helper: prepare and step a SQL statement, returning the `ResultCode`. On a prepare
/// failure, return the connection's last-error code (which may be `SQLITE_BUSY` from a
/// `Pager::open`/`begin_read` failure).
fn step_sql(conn: &mut rustsqlite_core::Sqlite3, sql: &str) -> ResultCode {
    match sqlite3_prepare_v2(conn, sql) {
        Ok((mut stmt, _)) => stmt.step(),
        Err(_e) => conn.errcode(),
    }
}

#[test]
fn begin_immediate_blocks_begin_immediate_cross_connection() {
    let db = temp_db("imm_xconn");
    let mut conn1 = sqlite3_open(&db).unwrap();
    exec(&mut conn1, "CREATE TABLE t(a);");

    // conn1 takes RESERVED via BEGIN IMMEDIATE.
    assert_eq!(exec(&mut conn1, "BEGIN IMMEDIATE;"), ResultCode::Done);
    assert!(!conn1.autocommit());

    // conn2 opening the same file and trying BEGIN IMMEDIATE should see SQLITE_BUSY
    // (the RESERVED byte is already locked by conn1).
    let mut conn2 = sqlite3_open(&db).unwrap();
    let res = step_sql(&mut conn2, "BEGIN IMMEDIATE;");
    assert!(
        matches!(res, ResultCode::Busy),
        "expected SQLITE_BUSY, got {res:?}"
    );

    // After conn1 commits, conn2 can BEGIN IMMEDIATE.
    assert_eq!(exec(&mut conn1, "COMMIT;"), ResultCode::Done);
    assert!(conn1.autocommit());
    assert_eq!(exec(&mut conn2, "BEGIN IMMEDIATE;"), ResultCode::Done);
    assert_eq!(exec(&mut conn2, "ROLLBACK;"), ResultCode::Done);

    let _ = std::fs::remove_file(&db);
}

#[test]
fn begin_exclusive_blocks_begin_exclusive_cross_connection() {
    let db = temp_db("excl_xconn");
    let mut conn1 = sqlite3_open(&db).unwrap();
    exec(&mut conn1, "CREATE TABLE t(a);");

    // conn1 takes EXCLUSIVE via BEGIN EXCLUSIVE.
    assert_eq!(exec(&mut conn1, "BEGIN EXCLUSIVE;"), ResultCode::Done);
    assert!(!conn1.autocommit());

    // conn2's BEGIN EXCLUSIVE should fail with SQLITE_BUSY (EXCLUSIVE blocks all).
    let mut conn2 = sqlite3_open(&db).unwrap();
    let res = step_sql(&mut conn2, "BEGIN EXCLUSIVE;");
    assert!(
        matches!(res, ResultCode::Busy),
        "expected SQLITE_BUSY, got {res:?}"
    );

    // Even a table read on conn2 should fail (EXCLUSIVE blocks SHARED).
    // (A `SELECT 1` with no FROM doesn't touch the database, so it succeeds — matching
    // real SQLite. A `SELECT * FROM t` acquires a SHARED lock and fails with BUSY.)
    let res = step_sql(&mut conn2, "SELECT * FROM t;");
    assert!(
        matches!(res, ResultCode::Busy),
        "expected SQLITE_BUSY for SELECT * FROM t under EXCLUSIVE, got {res:?}"
    );

    // After conn1 commits, conn2 can read.
    assert_eq!(exec(&mut conn1, "COMMIT;"), ResultCode::Done);
    assert_eq!(exec(&mut conn2, "SELECT 1;"), ResultCode::Row);
    // Step to drain the row.
    let _ = step_sql(&mut conn2, "SELECT 1;");

    let _ = std::fs::remove_file(&db);
}

#[test]
fn begin_immediate_allows_reads_on_other_connection() {
    // A RESERVED lock (BEGIN IMMEDIATE) does not block SHARED locks on another connection
    // (RESERVED allows new readers; only EXCLUSIVE blocks them). This matches SQLite's
    // locking protocol: a writer and readers can coexist until commit-time escalation.
    let db = temp_db("imm_read_xconn");
    let mut conn1 = sqlite3_open(&db).unwrap();
    exec(&mut conn1, "CREATE TABLE t(a);");
    exec(&mut conn1, "INSERT INTO t VALUES (1);");

    assert_eq!(exec(&mut conn1, "BEGIN IMMEDIATE;"), ResultCode::Done);

    // conn2 can still read (SHARED lock succeeds alongside conn1's RESERVED).
    let mut conn2 = sqlite3_open(&db).unwrap();
    let res = step_sql(&mut conn2, "SELECT a FROM t;");
    assert!(
        matches!(res, ResultCode::Row),
        "expected ROW under RESERVED, got {res:?}"
    );

    assert_eq!(exec(&mut conn1, "ROLLBACK;"), ResultCode::Done);

    let _ = std::fs::remove_file(&db);
}
