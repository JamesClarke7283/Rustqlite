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
    // INSERT ... OR REPLACE is still out of subset; prepare should error.
    assert!(sqlite3_prepare_v2(&mut conn, "INSERT OR REPLACE INTO t VALUES (1);").is_err());

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
