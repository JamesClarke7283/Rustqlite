//! Parity test for the streaming [`TableCursor`] against the materializing [`scan_table`].
//!
//! Both must yield the identical `(rowid, payload)` sequence for a table that spans multiple
//! b-tree pages (interior pages) and includes an overflow-page chain. Driven directly against
//! the engine's async internals (NOT the `sqlite3_*` C-API), so it runs inside a tokio runtime.
//! The fixture database is built with the system `sqlite3`; the test skips if it is absent.

use std::process::Command;
use std::sync::Arc;

use rustsqlite_core::btree::{scan_table, TableCursor};
use rustsqlite_core::pager::Pager;
use rustsqlite_core::schema::read_catalog;
use rustsqlite_core::vfs::{OpenFlags, OsTokioVfs, Vfs};

fn sqlite3_available() -> bool {
    Command::new("sqlite3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn table_cursor_matches_scan_table() {
    if !sqlite3_available() {
        eprintln!("skipping: system `sqlite3` binary not found");
        return;
    }

    let mut path = std::env::temp_dir();
    path.push(format!("rustsqlite_cursor_{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let path_str = path.to_str().unwrap().to_string();

    // 3000 rows force interior b-tree pages; a few 5000-byte values force overflow chains.
    let sql = "CREATE TABLE t(n, big);\
               WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n < 3000)\
               INSERT INTO t SELECT n, CASE WHEN n%500=0 THEN hex(zeroblob(2500)) ELSE n END FROM c;";
    let out = Command::new("sqlite3")
        .arg(&path_str)
        .arg(sql)
        .output()
        .expect("run sqlite3");
    assert!(
        out.status.success(),
        "sqlite3 failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let vfs = OsTokioVfs::new();
    let file = vfs
        .open(&path_str, OpenFlags::READONLY)
        .await
        .expect("open");
    let pager = Arc::new(Pager::open(file).await.expect("pager"));

    let catalog = read_catalog(&pager).await.expect("catalog");
    let root = catalog.find_table("t").expect("table t").rootpage as u32;

    // Reference: the materializing DFS.
    let expected = scan_table(&pager, root).await.expect("scan_table");

    // Streaming cursor.
    let mut cursor = TableCursor::new(pager.clone(), root);
    let mut got: Vec<(i64, Vec<u8>)> = Vec::new();
    cursor.rewind().await.expect("rewind");
    while cursor.is_valid() {
        let rowid = cursor.rowid().expect("rowid");
        let payload = cursor.payload().await.expect("payload");
        got.push((rowid, payload));
        cursor.next().await.expect("next");
    }

    let _ = std::fs::remove_file(&path);

    assert_eq!(got.len(), 3000, "row count");
    assert_eq!(expected.len(), got.len(), "scan vs cursor length");
    assert!(got == expected, "cursor sequence differs from scan_table");
    // Spot-check the overflow rows decoded identically (rowid 500, 1000, ... have big values).
    assert!(got[499].1.len() > 4000, "overflow payload present");
}
