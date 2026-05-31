//! Differential oracle: run identical SQL through rustsqlite (the C-API prepare/step/column
//! path) and the system `sqlite3` (a subprocess in `.mode list`), and assert the result rows
//! are byte-identical. This is the headline correctness gate for the M3a read query path.
//!
//! These are plain `#[test]`s — the `sqlite3_*` functions drive the engine via `block_on`, so
//! they must not run inside another tokio runtime. They skip (rather than fail) when the
//! `sqlite3` binary is absent.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use rustsqlite_core::capi::ResultCode;
use rustsqlite_core::{sqlite3_open, sqlite3_prepare_v2, Value};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// The sentinel printed for NULL by both engines (chosen not to collide with data).
const NULL_SENTINEL: &str = "<<NULL>>";

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
        path.push(format!("rustsqlite_diff_{}_{n}.db", std::process::id()));
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
        assert!(
            out.status.success(),
            "sqlite3 setup failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-journal", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.str()));
        }
    }
}

/// Reference rows from `sqlite3` in `.mode list` with our NULL sentinel.
fn sqlite3_rows(db: &str, query: &str) -> Vec<String> {
    let out = Command::new("sqlite3")
        .arg("-batch")
        .arg(db)
        .arg(".mode list")
        .arg(format!(".nullvalue {NULL_SENTINEL}"))
        .arg(query)
        .output()
        .expect("run sqlite3 query");
    assert!(
        out.status.success(),
        "sqlite3 query failed ({query}): {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8")
        .lines()
        .map(str::to_string)
        .collect()
}

/// Render a value exactly as `.mode list` does: column text, or the NULL sentinel.
fn render(v: &Value) -> String {
    v.to_text().unwrap_or_else(|| NULL_SENTINEL.to_string())
}

/// Rows from rustsqlite via the C-API, formatted like `.mode list`.
fn rustsqlite_rows(db: &str, query: &str) -> Result<Vec<String>, String> {
    let mut conn = sqlite3_open(db).map_err(|e| e.message)?;
    let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, query).map_err(|e| e.message)?;
    let ncol = stmt.column_count();
    let mut rows = Vec::new();
    loop {
        match stmt.step() {
            ResultCode::Row => {
                let cols: Vec<String> = (0..ncol).map(|i| render(&stmt.column_value(i))).collect();
                rows.push(cols.join("|"));
            }
            ResultCode::Done => break,
            _ => return Err(stmt.errmsg().to_string()),
        }
    }
    Ok(rows)
}

/// Assert rustsqlite and sqlite3 produce identical rows for `query` against `db`.
fn assert_same(db: &str, query: &str) {
    let expected = sqlite3_rows(db, query);
    match rustsqlite_rows(db, query) {
        Ok(got) => assert_eq!(got, expected, "mismatch for query: {query}"),
        Err(e) => panic!("rustsqlite error for query `{query}`: {e}\n(sqlite3 gave: {expected:?})"),
    }
}

/// A standard fixture covering rowid alias, typed columns, NULLs, and mixed storage classes.
fn standard_fixture() -> TempDb {
    let db = TempDb::new();
    db.setup(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b TEXT, c REAL);\
         INSERT INTO t(a,b,c) VALUES\
            (3,'pear',1.5),\
            (1,'apple',NULL),\
            (2,'banana',-2.25),\
            (NULL,'',0.0),\
            (10,'Cherry',100.0),\
            (-5,NULL,3.14159);",
    );
    db
}

#[test]
fn literals_and_constant_selects() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    let db = TempDb::new(); // empty database; constant SELECTs need no table
    for q in [
        "SELECT 1;",
        "SELECT 1, 2, 3;",
        "SELECT 'hello', 42, 3.5, NULL;",
        "SELECT 2.0;",
        "SELECT 0.1+0.2;",
        "SELECT 1e20;",
        "SELECT -5, -2.5, +3;",
        "SELECT 9223372036854775807;",
        "SELECT 'it''s';",
    ] {
        assert_same(db.str(), q);
    }
}

#[test]
fn arithmetic_and_affinity() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    let db = TempDb::new();
    for q in [
        "SELECT 1+2*3;",
        "SELECT 7/2, 7%2, 7.0/2, -7/2;",
        "SELECT 1/0, 1%0;",
        "SELECT 5-3-1;",
        "SELECT 2*3+4;",
        "SELECT 'a'||'b'||'c';",
        "SELECT 1||2;",
        "SELECT 'x'||NULL;",
        "SELECT '5'+3, '5.5'+1, 'abc'+1;",
        "SELECT 1=1, 1=2, 1<2, 2<=2, 3>4, NULL=NULL, NULL=1;",
        "SELECT 1 AND 0, 1 AND 1, 0 OR 0, NULL AND 0, NULL AND 1, NULL OR 1;",
        "SELECT NOT 0, NOT 1, NOT NULL;",
        "SELECT 10 < '9', '10' < '9', 10 < 9;",
    ] {
        assert_same(db.str(), q);
    }
}

#[test]
fn projection_and_where() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    let db = standard_fixture();
    for q in [
        "SELECT * FROM t;",
        "SELECT a, b FROM t;",
        "SELECT b, a, c FROM t;",
        "SELECT a+1, b||'!' FROM t;",
        "SELECT * FROM t WHERE a > 1;",
        "SELECT * FROM t WHERE a >= 2 AND c IS NOT NULL;",
        "SELECT a FROM t WHERE b = 'apple';",
        "SELECT a FROM t WHERE a IS NULL;",
        "SELECT a FROM t WHERE a IS NOT NULL;",
        "SELECT a FROM t WHERE c > 0;",
        "SELECT a FROM t WHERE a < 0 OR a > 5;",
        "SELECT a, b FROM t WHERE NOT (a > 2);",
        "SELECT b FROM t WHERE b > 'b';",
    ] {
        assert_same(db.str(), q);
    }
}

#[test]
fn rowid_alias() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    let db = standard_fixture();
    for q in [
        "SELECT id, a FROM t;",
        "SELECT rowid, a FROM t;",
        "SELECT _rowid_ FROM t;",
        "SELECT id, rowid FROM t WHERE id = 3;",
        "SELECT a FROM t WHERE rowid = 2;",
        "SELECT * FROM t WHERE id > 4;",
    ] {
        assert_same(db.str(), q);
    }
}

#[test]
fn order_by_and_limit() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    let db = standard_fixture();
    for q in [
        "SELECT a FROM t ORDER BY a;",
        "SELECT a FROM t ORDER BY a ASC;",
        "SELECT a FROM t ORDER BY a DESC;",
        "SELECT b FROM t ORDER BY b;",
        "SELECT b FROM t ORDER BY b DESC;",
        "SELECT a, c FROM t ORDER BY c;",
        "SELECT a, b FROM t ORDER BY a DESC, b ASC;",
        "SELECT a FROM t ORDER BY a LIMIT 3;",
        "SELECT a FROM t ORDER BY a LIMIT 2 OFFSET 1;",
        "SELECT a FROM t ORDER BY a DESC LIMIT 2;",
        "SELECT * FROM t LIMIT 2;",
        "SELECT * FROM t LIMIT 0;",
        "SELECT * FROM t LIMIT 100;",
        "SELECT a FROM t WHERE a IS NOT NULL ORDER BY a LIMIT 2 OFFSET 1;",
        "SELECT a FROM t ORDER BY 1;",
    ] {
        assert_same(db.str(), q);
    }
}

#[test]
fn text_numeric_coercion() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    let db = TempDb::new();
    for q in [
        // Arithmetic uses SQLite's numeric *prefix* parsing.
        "SELECT '10garbage'+5, '1e'+0, '1.5e'*2, '  12.5xyz'+0, '.5'+1, '5.'+0;",
        "SELECT '0x10'+0, '1.2.3'+0, ''+0, 'abc'*5, '+5'+0, '5e2'+1;",
        "SELECT typeof('1e'+0), typeof('5.'+0), typeof('5e2'+0), typeof('10x'+0);",
        // Affinity comparison only coerces a *whole* numeric string.
        "SELECT '10garbage'=10, '1e'=1, '5.'=5.0, '10'=10, '5e1'=50;",
        // substr / round with NULL and extreme arguments.
        "SELECT substr('abc',NULL), substr('abc',1,NULL), substr('abc',NULL,2);",
        "SELECT round(9223372036854775807.0), round(1e300), round(-1e300, 5);",
    ] {
        assert_same(db.str(), q);
    }
}

#[test]
fn large_table_through_interior_pages() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    // 4000 rows force a multi-level b-tree, so the executor's cursor descends interior pages.
    let db = TempDb::new();
    db.setup(
        "CREATE TABLE big(id INTEGER PRIMARY KEY, n INT, s TEXT);\
         WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM c WHERE i<4000)\
         INSERT INTO big SELECT i, i*2, 'row'||i FROM c;",
    );
    for q in [
        "SELECT id, n, s FROM big WHERE id = 2500;",
        "SELECT n FROM big WHERE n > 7990;",
        "SELECT id FROM big WHERE id > 3990 ORDER BY id DESC;",
        "SELECT id, s FROM big ORDER BY id DESC LIMIT 5;",
        "SELECT id FROM big WHERE n % 1000 = 0 ORDER BY id;",
        "SELECT s FROM big WHERE id < 3 OR id = 4000;",
        "SELECT id FROM big LIMIT 3 OFFSET 3997;",
        "SELECT abs(n - 4000), id FROM big WHERE id <= 3 ORDER BY id;",
        "SELECT id FROM big WHERE s = 'row2500';",
        "SELECT id FROM big WHERE id > 3998 LIMIT 5;",
    ] {
        assert_same(db.str(), q);
    }
}

#[test]
fn scalar_functions() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    let db = standard_fixture();
    for q in [
        "SELECT abs(-5), abs(5), abs(-2.5), abs(NULL);",
        "SELECT length('héllo'), length('abc'), length(12345), length(NULL);",
        "SELECT lower('ÀBC'), upper('àbc'), lower('AbC123');",
        "SELECT substr('hello',2), substr('hello',2,2), substr('hello',-2), substr('hello',0,2);",
        "SELECT round(2.5), round(3.5), round(-2.5), round(2.567,2), round(2);",
        "SELECT coalesce(NULL,NULL,3,4), ifnull(NULL,'x'), ifnull(2,'x');",
        "SELECT nullif(1,1), nullif(1,2), nullif('a','a');",
        "SELECT typeof(1), typeof(1.0), typeof('x'), typeof(NULL), typeof(1+1), typeof(3/2);",
        "SELECT abs(a), length(b), upper(b) FROM t;",
        "SELECT typeof(c), round(c, 1) FROM t ORDER BY id;",
        "SELECT a, coalesce(a, -1) FROM t ORDER BY id;",
        "SELECT substr(b, 1, 3) FROM t WHERE b IS NOT NULL ORDER BY id;",
    ] {
        assert_same(db.str(), q);
    }
}
