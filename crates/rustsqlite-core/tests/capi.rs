//! C-API behavior tests beyond the differential row checks: `sqlite3_reset` re-runs a
//! statement, `column_name`/`column_count` report the projection, and out-of-subset SQL fails
//! cleanly. Plain `#[test]`s (drive the engine via `block_on`); skip if `sqlite3` is absent.

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
    p.push(format!("rustsqlite_capi_{}_{tag}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p.to_str().unwrap().to_string()
}

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

#[test]
fn reset_reruns_statement() {
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("reset");
    Command::new("sqlite3")
        .arg(&db)
        .arg("CREATE TABLE t(a); INSERT INTO t VALUES (3),(1),(2);")
        .output()
        .unwrap();

    let mut conn = sqlite3_open(&db).unwrap();
    let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT a FROM t ORDER BY a;").unwrap();

    assert_eq!(stmt.column_count(), 1);
    assert_eq!(stmt.column_name(0), Some("a"));

    let first = collect(&mut stmt);
    assert_eq!(
        first,
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(2)],
            vec![Value::Int(3)]
        ]
    );

    // After reset, stepping again must produce the identical sequence.
    assert_eq!(stmt.reset(), ResultCode::Ok);
    let second = collect(&mut stmt);
    assert_eq!(first, second, "reset did not reproduce the result");

    let _ = std::fs::remove_file(&db);
}

#[test]
fn out_of_subset_sql_errors_cleanly() {
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("err");
    Command::new("sqlite3")
        .arg(&db)
        .arg("CREATE TABLE t(a, b);")
        .output()
        .unwrap();
    let mut conn = sqlite3_open(&db).unwrap();

    // No such table / column / function should be a prepare error, not a panic.
    assert!(sqlite3_prepare_v2(&mut conn, "SELECT * FROM nope;").is_err());
    assert!(sqlite3_prepare_v2(&mut conn, "SELECT zzz FROM t;").is_err());
    assert!(sqlite3_prepare_v2(&mut conn, "SELECT nosuchfn(a) FROM t;").is_err());
    // INSERT ... OR REPLACE now compiles (M12.8 conflict resolution).
    assert!(sqlite3_prepare_v2(&mut conn, "INSERT OR REPLACE INTO t VALUES (1, 2);").is_ok());

    let _ = std::fs::remove_file(&db);
}

#[test]
fn update_changes_counting_and_last_insert_rowid_preserved() {
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("upd");
    Command::new("sqlite3")
        .arg(&db)
        .arg("CREATE TABLE t(a, b); INSERT INTO t VALUES (1, 2), (3, 4), (5, 6);")
        .output()
        .unwrap();

    let mut conn = sqlite3_open(&db).unwrap();
    let last_before = conn.last_insert_rowid();

    // UPDATE with no WHERE matches all three rows; changes() must report 3, and
    // last_insert_rowid() must NOT be clobbered (it is set by INSERT, not UPDATE).
    let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "UPDATE t SET a = a + 1;").unwrap();
    assert_eq!(stmt.step(), ResultCode::Done);
    assert_eq!(conn.changes(), 3);
    assert_eq!(conn.last_insert_rowid(), last_before);

    // A second UPDATE that matches no rows must report 0 changes.
    let (mut stmt2, _) =
        sqlite3_prepare_v2(&mut conn, "UPDATE t SET a = 0 WHERE a > 1000;").unwrap();
    assert_eq!(stmt2.step(), ResultCode::Done);
    assert_eq!(conn.changes(), 0);

    // The file should reflect the update when read back.
    let (mut read, _) = sqlite3_prepare_v2(&mut conn, "SELECT a FROM t ORDER BY a;").unwrap();
    let rows = collect(&mut read);
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(2)],
            vec![Value::Int(4)],
            vec![Value::Int(6)]
        ]
    );

    let _ = std::fs::remove_file(&db);
}

#[test]
fn schema_change_returns_sqlite_schema() {
    // M12.10: a prepared statement whose schema cookie no longer matches the database's
    // current cookie must return SQLITE_SCHEMA from sqlite3_step() so the caller can
    // re-prepare. Mirrors the legacy `sqlite3_prepare` behavior (the `prepare_v2` auto-
    // reprepare of upstream is M12.11; for now we surface SQLITE_SCHEMA directly).
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("schema");
    Command::new("sqlite3")
        .arg(&db)
        .arg("CREATE TABLE t(a INTEGER); INSERT INTO t VALUES (1), (2), (3);")
        .output()
        .unwrap();

    let mut conn = sqlite3_open(&db).unwrap();
    // Prepare a SELECT; do not step it yet.
    let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT a FROM t ORDER BY a;").unwrap();
    // Sanity: the statement is not expired at prepare time.
    assert!(!stmt.expired(), "fresh statement should not be expired");

    // Run DDL on the same connection — this bumps the schema cookie.
    let (mut ddl, _) = sqlite3_prepare_v2(&mut conn, "CREATE TABLE u(b INTEGER);").unwrap();
    assert_eq!(ddl.step(), ResultCode::Done);

    // The previously-prepared SELECT should now be expired.
    assert!(stmt.expired(), "statement should be expired after DDL");

    // Stepping it must return SQLITE_SCHEMA, not Row/Done.
    assert_eq!(stmt.step(), ResultCode::Schema);
    // The error message should reflect the schema change.
    assert_eq!(stmt.errmsg(), "database schema has changed");

    // After re-prepare, the statement works again.
    let (mut stmt2, _) = sqlite3_prepare_v2(&mut conn, "SELECT a FROM t ORDER BY a;").unwrap();
    assert!(!stmt2.expired(), "re-prepared statement should not be expired");
    let rows = collect(&mut stmt2);
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(2)],
            vec![Value::Int(3)]
        ]
    );

    let _ = std::fs::remove_file(&db);
}

#[test]
fn schema_unchanged_does_not_expire_statement() {
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("schema_stable");
    Command::new("sqlite3")
        .arg(&db)
        .arg("CREATE TABLE t(a INTEGER); INSERT INTO t VALUES (1), (2);")
        .output()
        .unwrap();

    let mut conn = sqlite3_open(&db).unwrap();
    let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT a FROM t ORDER BY a;").unwrap();
    assert_eq!(stmt.step(), ResultCode::Row);
    assert_eq!(stmt.column_value(0), Value::Int(1));
    assert_eq!(stmt.step(), ResultCode::Row);
    assert_eq!(stmt.column_value(0), Value::Int(2));
    assert_eq!(stmt.step(), ResultCode::Done);
    assert!(!stmt.expired(), "stable statement should not be expired");

    // Reset and re-run — still not expired (no DDL happened).
    assert_eq!(stmt.reset(), ResultCode::Ok);
    assert!(!stmt.expired());
    assert_eq!(stmt.step(), ResultCode::Row);

    // DML (not DDL) does not bump the schema cookie, so the statement is still valid.
    let (mut ins, _) = sqlite3_prepare_v2(&mut conn, "INSERT INTO t VALUES (4);").unwrap();
    assert_eq!(ins.step(), ResultCode::Done);
    assert!(!stmt.expired(), "DML must not expire the statement");
    assert_eq!(stmt.reset(), ResultCode::Ok);
    let rows = collect(&mut stmt);
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(2)],
            vec![Value::Int(4)],
        ]
    );

    let _ = std::fs::remove_file(&db);
}

#[test]
fn step_with_db_auto_reprepares_on_schema_change() {
    // M12.11: step_with_db detects the schema cookie has changed and re-prepares the
    // statement against the current schema, transparently retrying the step (mirrors
    // upstream's sqlite3Reprepare+sqlite3Step retry loop). The legacy step() returns
    // SQLITE_SCHEMA; step_with_db re-prepares and returns Row/Done.
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("schema_reprepare");
    Command::new("sqlite3")
        .arg(&db)
        .arg("CREATE TABLE t(a INTEGER); INSERT INTO t VALUES (1), (2);")
        .output()
        .unwrap();

    let mut conn = sqlite3_open(&db).unwrap();
    let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT a FROM t ORDER BY a;").unwrap();

    // Run DDL on the same connection — bumps the schema cookie.
    let (mut ddl, _) = sqlite3_prepare_v2(&mut conn, "CREATE TABLE u(b INTEGER);").unwrap();
    assert_eq!(ddl.step(), ResultCode::Done);

    // The legacy step() would return SQLITE_SCHEMA here. step_with_db should re-prepare
    // transparently and return Row/Done.
    assert!(stmt.expired(), "statement should be expired after DDL");
    assert_eq!(stmt.step_with_db(&mut conn), ResultCode::Row);
    assert_eq!(stmt.column_value(0), Value::Int(1));
    assert_eq!(stmt.step_with_db(&mut conn), ResultCode::Row);
    assert_eq!(stmt.column_value(0), Value::Int(2));
    assert_eq!(stmt.step_with_db(&mut conn), ResultCode::Done);
    assert!(!stmt.expired(), "re-prepared statement should not be expired");

    let _ = std::fs::remove_file(&db);
}

#[test]
fn step_with_db_surfaces_error_when_table_dropped() {
    // When a re-prepare fails (e.g. the table was dropped), step_with_db surfaces the
    // underlying error rather than retrying forever (mirrors upstream's behavior of
    // returning the prepare error after the retry budget is exhausted).
    if !sqlite3_available() {
        return;
    }
    let db = temp_db("schema_drop");
    Command::new("sqlite3")
        .arg(&db)
        .arg("CREATE TABLE t(a INTEGER); INSERT INTO t VALUES (1);")
        .output()
        .unwrap();

    let mut conn = sqlite3_open(&db).unwrap();
    let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, "SELECT a FROM t;").unwrap();

    // Drop the table — re-prepare will fail with "no such table: t".
    let (mut ddl, _) = sqlite3_prepare_v2(&mut conn, "DROP TABLE t;").unwrap();
    assert_eq!(ddl.step(), ResultCode::Done);

    // step_with_db should surface the prepare error (SQLITE_ERROR with "no such table").
    let rc = stmt.step_with_db(&mut conn);
    assert!(
        matches!(rc, ResultCode::Error | ResultCode::Schema),
        "expected Error or Schema, got {rc:?}"
    );
    let msg = stmt.errmsg().to_string();
    assert!(
        msg.contains("no such table") || msg.contains("schema has changed"),
        "unexpected error message: {msg}"
    );

    let _ = std::fs::remove_file(&db);
}
