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
    // UPDATE is still on the M4.6 milestone; the parser doesn't produce a `Stmt` for it
    // yet, so `prepare` returns the parse error.
    assert!(sqlite3_prepare_v2(&mut conn, "UPDATE t SET a = 1;").is_err());

    let _ = std::fs::remove_file(&db);
}
