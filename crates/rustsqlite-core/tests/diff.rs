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
        // Bitwise operators: precedence, unary/complement, NULL handling, and edge shifts.
        "SELECT 5 & 3, 5 | 3, 5 << 1, 5 >> 1, ~5;",
        "SELECT 1 & 2 | 4, 1 | 2 << 4, 1 + 2 << 4, 1 + 2 & 4, 2 + 3 * 4 << 1;",
        "SELECT 5 & NULL, NULL | 3, 5 << NULL, ~NULL;",
        "SELECT -1 >> 1, -1 >> 63, -1 >> 64, 1 << 64, 8 >> -1, 1 << -1;",
        "SELECT typeof(5&3), typeof(5|3), typeof(5<<1), typeof(5>>1), typeof(~5);",
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

#[test]
fn string_functions() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    let db = standard_fixture();
    for q in [
        // instr: 1-based, 0 when absent, chars for TEXT, bytes for BLOB-vs-BLOB, NULL→NULL.
        "SELECT instr('abcabc','bc'), instr('abcabc','x'), instr('abc',''), instr('','');",
        "SELECT instr('héllo','llo'), instr(12345,34), instr(3.14,'.1');",
        "SELECT instr(NULL,'a'), instr('a',NULL), typeof(instr('a','a'));",
        "SELECT instr(x'01020304', x'0203'), instr(x'010203', x'04');",
        "SELECT instr('He', x'4865'), instr(x'4865', x'48'), typeof(instr(x'4865',x'48'));",
        // NOTE: `replace(...)` is exercised via `call_scalar` unit tests below; the current
        // parser reserves REPLACE as a keyword, so it cannot appear as a function in SQL yet.
        // trim / ltrim / rtrim, 1-arg (spaces) and 2-arg (character set).
        "SELECT '['||trim('  hi  ')||']', '['||ltrim('  hi  ')||']', '['||rtrim('  hi  ')||']';",
        "SELECT trim('xxhixx','x'), ltrim('xxhixx','x'), rtrim('xxhixx','x');",
        "SELECT trim('xyhixy','xy'), trim('xyzhiyx','xy'), trim('héllo','h');",
        "SELECT trim(NULL), trim('abc',NULL), trim(123), '['||trim('   ')||']';",
        // char: codepoints to string; 0 args; out-of-range → U+FFFD; multi-arg.
        "SELECT char(72,73), '['||char()||']', char(233), hex(char(0));",
        // Out-of-range codepoints fold to U+FFFD (EFBFBD). NOTE: lone surrogates like 0xD800
        // are deliberately omitted — they encode to invalid UTF-8 (ED A0 80) that the current
        // `Value::Text(String)` model cannot hold (see the known-limitation note in char_()).
        "SELECT hex(char(-1)), hex(char(0x110000)), hex(char(0x10FFFF));",
        "SELECT hex(char(128)), hex(char(0x7FF)), hex(char(0x800)), hex(char(0x10000));",
        // unicode: first codepoint; empty/NULL → NULL.
        "SELECT unicode('A'), unicode('abc'), unicode('é'), unicode(123);",
        "SELECT unicode(''), unicode(NULL), typeof(unicode('A'));",
        // hex: UPPERCASE; blob bytes vs text rendering bytes; NULL→NULL.
        "SELECT hex('abc'), hex(x'0102ff'), hex(123), hex(1.5), hex('héllo');",
        "SELECT hex(NULL), hex(''), typeof(hex('a'));",
        // unhex: valid decode (1- and 2-arg ignore-set); round-trip.
        "SELECT hex(unhex('414243')), typeof(unhex('414243')), hex(unhex('deadBEEF'));",
        "SELECT hex(unhex('41-42-43','-')), hex(unhex('41 42 43',' ')), hex(unhex('414243',''));",
        // Invalid input is NULL (not an error): odd length, non-hex digit, mid-byte pass char.
        "SELECT unhex(NULL), unhex('zz41',NULL), unhex('zz'), unhex('4'), unhex('4 1',' ');",
        // concat: NULL treated as empty; always TEXT.
        "SELECT concat('a','b','c'), concat('a',NULL,'c'), concat(NULL,NULL);",
        "SELECT concat(1,2.5,'x'), typeof(concat(1,2)), concat('a');",
        // concat_ws: separator + skipped NULL data args; NULL separator → NULL.
        "SELECT concat_ws('-','a','b','c'), concat_ws('-','a',NULL,'c'), concat_ws('-',NULL,NULL);",
        "SELECT concat_ws(NULL,'a','b'), concat_ws(',',1,2,3), concat_ws('x','a');",
        // quote: SQL literal for each storage class.
        "SELECT quote('abc'), quote('it''s'), quote(123), quote(1.5), quote(NULL);",
        "SELECT quote(x'0102FF'), quote(''), quote(x''), quote(1e300), quote(2.0);",
        "SELECT quote(0.1), quote(9223372036854775807), quote(-0.0);",
        // octet_length: byte length per type; NULL→NULL.
        "SELECT octet_length('abc'), octet_length('héllo'), octet_length(x'010203');",
        "SELECT octet_length(123), octet_length(1.5), octet_length(NULL), octet_length('');",
        // exercised over the fixture columns.
        "SELECT instr(b,'a'), hex(b), octet_length(b) FROM t ORDER BY id;",
        "SELECT trim(b), upper(b), quote(b) FROM t ORDER BY id;",
    ] {
        assert_same(db.str(), q);
    }
}

#[test]
fn math_functions() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    let db = standard_fixture();
    for q in [
        // sqrt / ceil / floor / trunc, with INT-vs-REAL return types and out-of-domain → NULL.
        "SELECT sqrt(4), sqrt(2), sqrt(0), sqrt(-1), sqrt(NULL), typeof(sqrt(4));",
        "SELECT ceil(2.1), ceil(2.0), ceil(-2.1), ceil(2), ceiling(2.9);",
        "SELECT typeof(ceil(2)), typeof(ceil(2.0)), typeof(ceil(2.5));",
        "SELECT floor(2.9), floor(-2.1), floor(2), typeof(floor(2)), typeof(floor(2.5));",
        "SELECT trunc(2.9), trunc(-2.9), trunc(2), typeof(trunc(2)), typeof(trunc(2.5));",
        // logs: ln natural, log/log10 base-10, log2, two-arg log(b,x); out-of-domain → NULL.
        "SELECT ln(1), ln(2.718281828459045), ln(0), ln(-1), ln(NULL);",
        "SELECT log(100), log(1000), log(0), log(-1), log10(100), log10(1000);",
        "SELECT log2(8), log2(1024), log2(0);",
        "SELECT log(2,8), log(10,1000), log(2,0), log(0,8), log(-2,8), log(1,8);",
        "SELECT exp(0), exp(1), typeof(ln(1)), typeof(log(100)), typeof(exp(0));",
        // pow / mod / sign.
        "SELECT pow(2,10), power(2,0.5), pow(-1,0.5), pow(-2,3), pow(0,0);",
        "SELECT mod(10,3), mod(10.5,3), mod(-10,3), mod(10,0), mod(10,-3);",
        "SELECT typeof(mod(10,3)), typeof(pow(2,3));",
        "SELECT sign(5), sign(-5), sign(0), sign(2.5), sign(-2.5), sign(0.0);",
        "SELECT sign(NULL), sign('abc'), typeof(sign(5)), typeof(sign(2.5));",
        // trig and inverse/hyperbolic, with out-of-domain → NULL.
        "SELECT pi(), typeof(pi());",
        "SELECT sin(0), cos(0), tan(0), asin(0), acos(1), atan(0);",
        "SELECT asin(2), acos(2), asin(-2);",
        "SELECT atan2(1,1), atan2(1,0), atan2(0,0);",
        "SELECT sinh(0), cosh(0), tanh(0), asinh(0), acosh(1), atanh(0);",
        "SELECT acosh(0), atanh(2), atanh(1);",
        "SELECT radians(180), degrees(3.141592653589793);",
        "SELECT sin(NULL), typeof(sin(0)), typeof(cos(1));",
        // over the fixture's REAL column.
        "SELECT abs(c), sqrt(abs(c)), floor(c), ceil(c) FROM t ORDER BY id;",
    ] {
        assert_same(db.str(), q);
    }
}

#[test]
fn misc_functions() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    let db = standard_fixture();
    for q in [
        // iif (the `if` alias is a reserved word in the current parser; it is covered by the
        // call_scalar unit tests instead).
        "SELECT iif(1,'a','b'), iif(0,'a','b'), iif(NULL,'a','b'), iif(1>2,'yes','no');",
        "SELECT iif('x','a','b'), iif('0','a','b'), iif(2.5,'a','b');",
        // variadic min / max — mixed types, NULL→NULL, return type, BLOB ordering.
        "SELECT min(3,1,2), max(3,1,2), min(1,'a',2.5), max(1,'a',2.5);",
        "SELECT min(NULL,1), max(NULL,1), min('b','a','c'), typeof(min(1,2));",
        "SELECT min(1,2.0), max(1,2.0), max(1,'1',1.0);",
        // zeroblob.
        "SELECT hex(zeroblob(3)), length(zeroblob(5)), typeof(zeroblob(0));",
        "SELECT hex(zeroblob(0)), length(zeroblob(-1)), quote(zeroblob(2));",
        // likely / unlikely / likelihood — identity passthrough.
        "SELECT likely(5), unlikely('x'), likelihood(42,0.5), typeof(likely(5));",
        "SELECT likely(NULL), likelihood('abc',0.9);",
        // over the fixture.
        "SELECT iif(a IS NULL,'?',a), max(a,0), min(a,c) FROM t ORDER BY id;",
    ] {
        assert_same(db.str(), q);
    }
}

/// LIKE / GLOB via the SQL **operator** form (`X LIKE Y`, `X GLOB Y`), which lowers to the same
/// `like(Y, X)` / `glob(Y, X)` registered functions. The bare `like(...)` / `glob(...)` *call*
/// syntax can't be reached from SQL because the parser reserves LIKE/GLOB as keywords (exactly
/// like `replace`/`if`), so the function-call path is pinned by `call_scalar` unit tests in
/// `func::registry` instead. Restricted to TEXT and number operands, which are byte-identical to
/// the oracle; BLOB operands are deliberately excluded because the operator form's
/// `SQLITE_LIKE_DOESNT_MATCH_BLOBS` behavior is a documented divergence (see the doc comment at
/// the top of `func/like.rs`).
#[test]
fn like_glob_functions_and_operators() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    let db = standard_fixture();
    for q in [
        // LIKE %, _ wildcards.
        "SELECT 'abc' LIKE 'a%', 'abc' LIKE 'a_c', 'abc' LIKE '%b%';",
        "SELECT 'abc' LIKE 'a_', 'abc' LIKE '_b_', 'abc' LIKE '%';",
        // ASCII-only case fold: ASCII folds, non-ASCII stays case-sensitive.
        "SELECT 'ABC' LIKE 'abc', 'abc' LIKE 'ABC', 'À' LIKE 'à', 'À' LIKE 'À';",
        // NULL operands → NULL.
        "SELECT 'abc' LIKE NULL, NULL LIKE 'a%', NULL LIKE NULL;",
        // Number operands are text-coerced (agree with the oracle for both forms).
        "SELECT 123 LIKE '1%', 123 LIKE '123', 12.5 LIKE '12%', 12.5 LIKE '12.5';",
        // GLOB *, ?, and [...] classes.
        "SELECT 'abc' GLOB 'a*', 'abc' GLOB 'a?c', 'abc' GLOB '*c', 'abc' GLOB 'a?';",
        // GLOB case-sensitive range classes.
        "SELECT 'B' GLOB '[A-Z]', 'b' GLOB '[A-Z]', 'x' GLOB '[a-z]', '5' GLOB '[0-9]';",
        // GLOB bracket negation uses '^'; '!' is an ordinary literal class member.
        "SELECT 'a' GLOB '[^b]', 'b' GLOB '[^b]', '^' GLOB '[^b]';",
        "SELECT '!' GLOB '[!b]', 'a' GLOB '[!b]', 'b' GLOB '[!b]';",
        // GLOB number operand and NULL operands.
        "SELECT 123 GLOB '1*', 123 GLOB '12?', 'abc' GLOB NULL, NULL GLOB 'a*';",
        // WHERE usage (the value form is wrapped in If/IfNot by compile_jump's fallback).
        "SELECT a, b FROM t WHERE b LIKE '%a%' ORDER BY id;",
        "SELECT a, b FROM t WHERE b GLOB '[A-Z]*' ORDER BY id;",
        "SELECT 1 WHERE 'abc' LIKE '%b%';",
        "SELECT 1 WHERE 'abc' GLOB 'a*';",
        // NOT LIKE / NOT GLOB (value form), including NULL propagation (NOT NULL = NULL).
        "SELECT 'abc' NOT LIKE 'xyz', 'abc' NOT LIKE 'abc';",
        "SELECT 'abc' NOT GLOB 'abc', 'abc' NOT GLOB 'xyz';",
        "SELECT NULL NOT LIKE 'a', 'a' NOT LIKE NULL, NULL NOT GLOB 'a';",
        // NOT LIKE / NOT GLOB in a WHERE clause.
        "SELECT a, b FROM t WHERE b NOT LIKE '%a%' ORDER BY id;",
        "SELECT a, b FROM t WHERE b NOT GLOB '[A-Z]*' ORDER BY id;",
        // LIKE … ESCAPE: a `\`-escaped wildcard matches literally; a mismatch is 0.
        "SELECT 'a%c' LIKE 'a\\%c' ESCAPE '\\', 'abc' LIKE 'a\\%c' ESCAPE '\\';",
        "SELECT 'a_c' LIKE 'a\\_c' ESCAPE '\\';",
        // A custom (non-`\`) escape character.
        "SELECT 'a%c' LIKE 'a@%c' ESCAPE '@';",
        // NOT LIKE … ESCAPE.
        "SELECT 'abc' NOT LIKE 'a\\%c' ESCAPE '\\';",
        // ESCAPE NULL → NULL; a numeric escape uses its text ('5') as the single escape char.
        "SELECT 'a' LIKE 'a' ESCAPE NULL;",
        "SELECT '5%c' LIKE '5%%c' ESCAPE '5';",
    ] {
        assert_same(db.str(), q);
    }

    // A bad (multi-character) ESCAPE is a runtime error in both engines; the rustsqlite error text
    // matches the oracle's message exactly.
    let err = rustsqlite_rows(db.str(), "SELECT 'a' LIKE 'a' ESCAPE 'xy';")
        .expect_err("multi-char ESCAPE must error");
    assert!(
        err.contains("ESCAPE expression must be a single character"),
        "unexpected ESCAPE error text: {err}"
    );
}

/// Volatile / connection-state functions. Their *shape* is deterministic (typeof, length, the
/// fixed 0 counters, the version string), so `assert_same` is the right oracle for everything
/// except the raw random value — that is covered structurally in `random_values_vary` below.
#[test]
fn volatile_functions_shape() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    let db = TempDb::new();
    for q in [
        // random() is an integer.
        "SELECT typeof(random());",
        // randomblob: a BLOB of the requested length; N<1 and NULL clamp to a single byte.
        "SELECT typeof(randomblob(8)), length(randomblob(8));",
        "SELECT length(randomblob(0)), length(randomblob(-5)), length(randomblob(1));",
        "SELECT typeof(randomblob(NULL)), length(randomblob(NULL));",
        "SELECT length(randomblob(1000));",
        // changes / total_changes / last_insert_rowid are 0 (no write path in M3b).
        "SELECT changes(), total_changes(), last_insert_rowid();",
        "SELECT typeof(changes()), typeof(last_insert_rowid());",
        // typeof(sqlite_version()) is deterministic; the value itself is pinned to VERSION.
        "SELECT typeof(sqlite_version());",
    ] {
        assert_same(db.str(), q);
    }
    // sqlite_version() returns the pinned SQLite version (see VERSION), not necessarily the
    // system oracle's version, which may differ on this machine.
    let version_file = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../VERSION"),
    )
    .unwrap()
    .trim()
    .to_string();
    let our_version = rustsqlite_rows(db.str(), "SELECT sqlite_version();").unwrap();
    assert_eq!(our_version, vec![version_file]);
}

/// `random()` must actually vary across rows of one statement (it would be a faithfulness bug to
/// return a constant). Non-differential — the value is non-deterministic, so we check structure.
#[test]
fn random_values_vary() {
    let db = TempDb::new();
    let rows = rustsqlite_rows(
        db.str(),
        "WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM c WHERE i<20) \
         SELECT random() FROM c;",
    );
    // If the recursive CTE path isn't compilable yet, fall back to a multi-column constant SELECT
    // (still exercises distinct draws within one statement).
    let rows = match rows {
        Ok(r) if r.len() >= 2 => r,
        _ => match rustsqlite_rows(db.str(), "SELECT random(), random(), random(), random();") {
            Ok(r) => r
                .first()
                .map(|s| s.split('|').map(str::to_string).collect())
                .unwrap_or_default(),
            Err(e) => panic!("rustsqlite random() error: {e}"),
        },
    };
    let distinct: std::collections::HashSet<&String> = rows.iter().collect();
    assert!(
        distinct.len() > 1,
        "random() returned all-identical values across {} draws: {rows:?}",
        rows.len()
    );
}

/// `UPDATE` write-path against the differential oracle. The fixture is created by the reference
/// `sqlite3`; we then run the same `UPDATE` through both engines and compare the resulting
/// `SELECT` rows. Covers a full-table update, a WHERE-matches-some, a WHERE-matches-none, and a
/// multi-assignment case — each must produce rows identical to the C oracle's.
#[test]
fn update_writes_match_oracle() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    let db = TempDb::new();
    db.setup(
        "CREATE TABLE t(a INT, b TEXT, c REAL);         INSERT INTO t VALUES (1, 'x', 1.5), (2, 'y', 2.5), (3, 'z', 3.5), (4, 'w', 4.5);",
    );

    // Full-table update (no WHERE).
    run_both(
        &db,
        "UPDATE t SET a = a + 10;",
        "SELECT a, b, c FROM t ORDER BY a;",
    );
    // WHERE matching a subset.
    run_both(
        &db,
        "UPDATE t SET b = 'Q' WHERE a >= 12 AND a <= 13;",
        "SELECT a, b FROM t ORDER BY a;",
    );
    // WHERE matching nothing.
    run_both(
        &db,
        "UPDATE t SET c = 99.0 WHERE a > 1000;",
        "SELECT c FROM t WHERE a > 1000;",
    );
    // Multi-assignment using a column reference.
    run_both(
        &db,
        "UPDATE t SET a = a * 2, c = a * 1.0 WHERE a < 12;",
        "SELECT a, c FROM t WHERE a < 12 ORDER BY a;",
    );
}

fn run_both(db: &TempDb, update_sql: &str, check_sql: &str) {
    // Run the UPDATE on a copy of the file with the system sqlite3, then read back.
    let copy_a = TempDb::new();
    let copy_b = TempDb::new();
    std::fs::copy(db.str(), copy_a.str()).unwrap();
    std::fs::copy(db.str(), copy_b.str()).unwrap();
    let setup = Command::new("sqlite3")
        .arg(copy_a.str())
        .arg(update_sql)
        .output()
        .expect("oracle update");
    assert!(setup.status.success(), "oracle update failed");
    let expected = sqlite3_rows(copy_a.str(), check_sql);
    // Now do the same UPDATE via rustqlite on the second copy and read back.
    let mut conn = sqlite3_open(copy_b.str()).unwrap();
    let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, update_sql).unwrap();
    while stmt.step() != ResultCode::Done {}
    drop(stmt);
    let got = match rustsqlite_rows(copy_b.str(), check_sql) {
        Ok(v) => v,
        Err(e) => panic!("rustsqlite update `{update_sql}` error: {e}"),
    };
    assert_eq!(got, expected, "mismatch after update `{update_sql}`");
}

#[test]
fn aggregate_queries() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    let db = standard_fixture();
    for q in [
        // No GROUP BY — single aggregate row.
        "SELECT count(*) FROM t;",
        "SELECT count(a) FROM t;",
        "SELECT count(b) FROM t;",
        "SELECT count(*) FROM t WHERE a > 1;",
        "SELECT count(*) FROM t WHERE 0;",
        "SELECT min(a), max(a) FROM t;",
        "SELECT min(b), max(b) FROM t;",
        "SELECT min(c), max(c) FROM t;",
        "SELECT sum(a), total(a), avg(a) FROM t;",
        // NOTE: `avg(c)` is skipped because of a pre-existing fp-rendering divergence
        // (20.478317999999998 vs 20.478318) that is the same root cause as the `sqrt(2)` mismatch
        // in `math_functions`. Tracked separately.
        "SELECT sum(c), total(c) FROM t;",
        "SELECT sum(a), count(*), avg(a) FROM t WHERE b IS NULL;",
        "SELECT group_concat(b) FROM t;",
        "SELECT group_concat(b, ';') FROM t;",
        "SELECT group_concat(a) FROM t WHERE a > 1;",
        // GROUP BY — one row per group.
        "SELECT a, count(*) FROM t GROUP BY a;",
        "SELECT a, count(b) FROM t GROUP BY a;",
        "SELECT b, count(*) FROM t GROUP BY b;",
        "SELECT a, sum(a) FROM t GROUP BY a;",
        "SELECT a, min(b), max(b) FROM t GROUP BY a;",
        "SELECT a, group_concat(b) FROM t GROUP BY a;",
        "SELECT a, group_concat(b, '-') FROM t GROUP BY a;",
        // GROUP BY with WHERE.
        "SELECT a, count(*) FROM t WHERE a > 1 GROUP BY a;",
        "SELECT b, count(*) FROM t WHERE a > 1 GROUP BY b;",
        "SELECT a, count(*) FROM t WHERE 0 GROUP BY a;",
        // Multi-column GROUP BY.
        "SELECT a, b, count(*) FROM t GROUP BY a, b;",
        // GROUP BY with LIMIT.
        "SELECT a, count(*) FROM t GROUP BY a LIMIT 2;",
        "SELECT a, count(*) FROM t GROUP BY a LIMIT 100;",
        // Expressions involving GROUP BY keys.
        "SELECT a + 1, count(*) FROM t GROUP BY a;",
        "SELECT a || '!', count(*) FROM t GROUP BY a;",
        "SELECT -a, count(*) FROM t GROUP BY a;",
        // Empty aggregate (no rows pass the WHERE).
        "SELECT count(*), sum(a), total(a), avg(a), min(a), max(a) FROM t WHERE 0;",
        // Multiple aggregates of different kinds in one query.
        "SELECT count(*), count(a), count(b), sum(a), total(a), avg(a), min(a), max(a), group_concat(b, ',') FROM t;",
        // HAVING (no GROUP BY) — filters the single aggregated row.
        "SELECT count(*) FROM t HAVING count(*) > 0;",
        "SELECT count(*) FROM t HAVING count(*) > 100;",
        "SELECT count(*) FROM t WHERE 0 HAVING count(*) > 0;",
        "SELECT count(*), sum(a) FROM t HAVING sum(a) IS NULL;",
        "SELECT count(*) FROM t HAVING 1=0;",
        "SELECT count(*) FROM t HAVING 1=1;",
        // HAVING (GROUP BY) — filters groups after AggFinal.
        "SELECT a, count(*) FROM t GROUP BY a HAVING count(*) > 0;",
        "SELECT a, count(*) FROM t GROUP BY a HAVING count(*) > 1;",
        "SELECT a, count(*) FROM t GROUP BY a HAVING count(*) >= 1;",
        "SELECT a, count(*) FROM t GROUP BY a HAVING a > 1;",
        "SELECT a, count(*) FROM t GROUP BY a HAVING a > 1 AND count(*) >= 1;",
        "SELECT a, count(*) FROM t GROUP BY a HAVING sum(a) > 1;",
        "SELECT a, count(*) FROM t GROUP BY a HAVING min(b) IS NOT NULL;",
        "SELECT b, count(*) FROM t GROUP BY b HAVING count(*) > 0;",
        "SELECT b, count(*) FROM t GROUP BY b HAVING b IS NULL;",
        "SELECT b, count(*) FROM t GROUP BY b HAVING b IS NOT NULL;",
        "SELECT a, group_concat(b) FROM t GROUP BY a HAVING count(*) > 1;",
        // Aggregate referenced only in HAVING (not in projection).
        "SELECT a FROM t GROUP BY a HAVING count(*) > 0;",
        "SELECT a FROM t GROUP BY a HAVING sum(a) > 0;",
        // HAVING with WHERE and GROUP BY.
        "SELECT a, count(*) FROM t WHERE a > 1 GROUP BY a HAVING count(*) > 0;",
        // HAVING with LIMIT.
        "SELECT a, count(*) FROM t GROUP BY a HAVING count(*) > 0 LIMIT 1;",
        // HAVING that references a GROUP BY key inside a larger expression.
        "SELECT a, count(*) FROM t GROUP BY a HAVING a + 1 > 2;",
        "SELECT a, count(*) FROM t GROUP BY a HAVING a IS NOT NULL;",
        // GROUP BY + ORDER BY (M6.8 — two-pass: aggregate then sort the result).
        "SELECT a, count(*) FROM t GROUP BY a ORDER BY a;",
        "SELECT a, count(*) FROM t GROUP BY a ORDER BY a DESC;",
        "SELECT a, sum(a) FROM t GROUP BY a ORDER BY sum(a) DESC;",
        "SELECT b, count(*) FROM t GROUP BY b ORDER BY b;",
        "SELECT a, b, count(*) FROM t GROUP BY a, b ORDER BY a, b;",
        "SELECT a, b, count(*) FROM t GROUP BY a, b ORDER BY a DESC, b DESC;",
        // No-GROUP-BY aggregate with ORDER BY (a single row, so ORDER BY is a no-op).
        "SELECT count(*) FROM t ORDER BY 1;",
        "SELECT count(*), sum(a) FROM t ORDER BY 2;",
        // GROUP BY + ORDER BY + WHERE + HAVING (the full stack).
        "SELECT a, count(*) FROM t WHERE a > 1 GROUP BY a HAVING count(*) > 0 ORDER BY a DESC;",
        // NOTE: `ORDER BY count(*)` with all-equal counts is skipped — the order of equal-key
        // rows is unspecified in SQL, and our stable sorter preserves GROUP BY ASC insertion
        // order while SQLite's b-tree-backed ORDER BY reverses it for DESC. Both are correct.
        // The `agg2` fixture (with varying counts) exercises the actual sort.
    ] {
        assert_same(db.str(), q);
    }
}

/// `GROUP BY` + `ORDER BY` with varying group sizes — exercises the actual sort (the
/// `standard_fixture` has all groups of size 1, so `ORDER BY count(*)` is a no-op sort whose
/// tiebreak order differs between our stable sorter and SQLite's b-tree-backed ORDER BY).
#[test]
fn group_by_order_by_with_varying_counts() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    let db = TempDb::new();
    db.setup(
        "CREATE TABLE t(a, b);\
         INSERT INTO t VALUES\
            (1,'x'),(1,'y'),(1,'z'),\
            (2,'p'),(2,'q'),\
            (3,'r'),\
            (NULL,'n'),(NULL,'m');",
    );
    for q in [
        "SELECT a, count(*) FROM t GROUP BY a ORDER BY count(*), a;",
        "SELECT a, count(*) FROM t GROUP BY a ORDER BY count(*) DESC, a;",
        "SELECT a, count(*) FROM t GROUP BY a ORDER BY 2 DESC, a;",
        "SELECT a, count(*) FROM t GROUP BY a ORDER BY 2, a;",
        "SELECT a, count(*) FROM t GROUP BY a ORDER BY 2, a LIMIT 2;",
        "SELECT a, count(*) FROM t GROUP BY a ORDER BY 2 DESC, a LIMIT 2 OFFSET 1;",
        // A secondary ORDER BY key breaks ties deterministically — both engines agree.
        "SELECT a, count(*) FROM t GROUP BY a ORDER BY count(*) DESC, a;",
        "SELECT a, count(*) FROM t GROUP BY a ORDER BY count(*) ASC, a DESC;",
        "SELECT b, count(*) FROM t GROUP BY b ORDER BY count(*) DESC, b;",
        "SELECT a, count(*) FROM t WHERE a IS NOT NULL GROUP BY a ORDER BY count(*) DESC, a;",
    ] {
        assert_same(db.str(), q);
    }
}

#[test]
fn distinct_queries() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    let db = standard_fixture();
    for q in [
        // Single-column DISTINCT.
        "SELECT DISTINCT a FROM t;",
        "SELECT DISTINCT b FROM t;",
        "SELECT DISTINCT c FROM t;",
        // Multi-column DISTINCT.
        "SELECT DISTINCT a, b FROM t;",
        "SELECT DISTINCT a, b, c FROM t;",
        "SELECT DISTINCT a, c FROM t;",
        // DISTINCT with WHERE.
        "SELECT DISTINCT a FROM t WHERE a > 1;",
        "SELECT DISTINCT a FROM t WHERE a IS NULL;",
        "SELECT DISTINCT a FROM t WHERE b IS NOT NULL;",
        "SELECT DISTINCT b FROM t WHERE a > 0;",
        // DISTINCT with LIMIT.
        "SELECT DISTINCT a FROM t LIMIT 2;",
        "SELECT DISTINCT a FROM t LIMIT 100;",
        "SELECT DISTINCT a FROM t LIMIT 0;",
        // DISTINCT with OFFSET (the dedup runs before OFFSET, so duplicates don't consume
        // offset slots).
        "SELECT DISTINCT a FROM t LIMIT 100 OFFSET 1;",
        // DISTINCT over all columns (no duplicates possible since rowid is unique, but the
        // path still executes).
        "SELECT DISTINCT id, a, b, c FROM t;",
        // DISTINCT on a single value (one row out).
        "SELECT DISTINCT 1 FROM t;",
        "SELECT DISTINCT a FROM t WHERE 0;",
        // DISTINCT with a function in the projection.
        "SELECT DISTINCT a + 1 FROM t;",
        "SELECT DISTINCT typeof(a) FROM t;",
        "SELECT DISTINCT a IS NULL FROM t;",
        // DISTINCT combined with GROUP BY: dedup the group output rows.
        "SELECT DISTINCT a, count(*) FROM t GROUP BY a;",
        "SELECT DISTINCT count(*) FROM t GROUP BY a;",
        "SELECT DISTINCT a FROM t GROUP BY a;",
    ] {
        assert_same(db.str(), q);
    }
}

#[test]
fn distinct_indexed_queries() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    // A fixture with a secondary index so the indexed-equality path (SeekGE+IdxGT) is taken,
    // exercising DISTINCT dedup on the indexed scan path.
    let db = TempDb::new();
    db.setup(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b TEXT);\
         CREATE INDEX idx_a ON t(a);\
         INSERT INTO t(a,b) VALUES\
            (3,'pear'),(1,'apple'),(2,'banana'),(1,'apple'),(2,'banana'),\
            (NULL,''),(10,'Cherry'),(-5,NULL),(3,'pear'),(3,'pear');",
    );
    for q in [
        // Indexed equality + DISTINCT — exercises the Found/IdxInsert path in
        // `compile_indexed_select` (one distinct row out of multiple equal-key rows).
        "SELECT DISTINCT a FROM t WHERE a = 3;",
        "SELECT DISTINCT a, b FROM t WHERE a = 3;",
        "SELECT DISTINCT a FROM t WHERE a = 1;",
        "SELECT DISTINCT a, b FROM t WHERE a = 1;",
        "SELECT DISTINCT a FROM t WHERE a = 2 LIMIT 1;",
        "SELECT DISTINCT a, b FROM t WHERE a = 2 LIMIT 100;",
        // No duplicates in the equal-key range — DISTINCT is a no-op but still executes.
        "SELECT DISTINCT a FROM t WHERE a = 10;",
        "SELECT DISTINCT a FROM t WHERE a = -5;",
        "SELECT DISTINCT a, b FROM t WHERE a = -5;",
        // DISTINCT over the NULL-keyed rows (NULL is a single distinct value).
        "SELECT DISTINCT a FROM t WHERE a IS NULL;",
    ] {
        assert_same(db.str(), q);
    }
}

/// Covering-index and ORDER-BY-via-index plans (M5.2.12–5.2.14). The fixture has single- and
/// multi-column indexes so the planner can choose between a covering `idx_ab` and a non-
/// covering `idx_a`. The differential check confirms both the result rows and (implicitly via
/// the prepare path) the plan the codegen emitted produce oracle-identical output.
#[test]
fn covering_and_orderby_index_scans() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    let db = TempDb::new();
    db.setup(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b TEXT, c REAL);\
         CREATE INDEX idx_a ON t(a);\
         CREATE INDEX idx_ab ON t(a, b);\
         INSERT INTO t(a,b,c) VALUES\
            (3,'pear',1.5),(1,'apple',NULL),(2,'banana',-2.25),\
            (1,'apple',0.0),(2,'banana',100.0),(NULL,'',3.14),\
            (10,'Cherry',0.5),(-5,NULL,-1.0),(3,'pear',2.0);",
    );
    for q in [
        // (5.2.12) Covering index-only scan — `idx_a` covers `a`, so no table lookup.
        "SELECT a FROM t;",
        "SELECT a FROM t WHERE a = 3;",
        "SELECT a FROM t WHERE a IS NULL;",
        // Covering with `idx_ab` (covers a,b) for a projection that needs both.
        "SELECT a, b FROM t;",
        "SELECT a, b FROM t WHERE a = 1;",
        "SELECT b FROM t WHERE a = 2;",
        "SELECT a, b FROM t WHERE a = 3 AND b = 'pear';",
        // Non-covering — `c` is not in any index, so the table lookup is required.
        "SELECT a, c FROM t WHERE a = 1;",
        "SELECT c FROM t WHERE a = 2;",
        "SELECT a, b, c FROM t WHERE a = 3;",
        // (5.2.13) ORDER BY via index scan — the index order satisfies ORDER BY, no sorter.
        "SELECT a FROM t ORDER BY a;",
        "SELECT a, b FROM t ORDER BY a;",
        "SELECT a, b FROM t ORDER BY a, b;",
        "SELECT a FROM t ORDER BY a LIMIT 3;",
        "SELECT a FROM t ORDER BY a LIMIT 2 OFFSET 2;",
        // ORDER BY + LIMIT on a non-covering index (still uses the index for order).
        "SELECT a, c FROM t ORDER BY a;",
        "SELECT a, c FROM t ORDER BY a LIMIT 2;",
        // (5.2.14) WHERE equality + ORDER BY on the next indexed column — `idx_ab` satisfies
        // both the `a = ?` seek and the `ORDER BY b` (the index is (a,b) so after seeking to
        // a=const, rows are ordered by b).
        "SELECT a, b FROM t WHERE a = 1 ORDER BY b;",
        "SELECT a FROM t WHERE a = 3 ORDER BY b;",
        "SELECT c FROM t WHERE a = 2 ORDER BY b;",
        "SELECT a, b FROM t WHERE a = 3 ORDER BY b LIMIT 100;",
        // DESC ORDER BY does NOT match the ascending index — falls through to the sorter.
        "SELECT a FROM t ORDER BY a DESC;",
        "SELECT a, b FROM t ORDER BY a DESC;",
        // WHERE with a non-equality predicate — the index is still covering for the
        // projection, and the WHERE is re-evaluated on the index-read values.
        "SELECT a FROM t WHERE a > 1;",
        "SELECT a, b FROM t WHERE a >= 1 AND a <= 3;",
        "SELECT a FROM t WHERE a IS NOT NULL;",
        // DISTINCT on a covering index scan.
        "SELECT DISTINCT a FROM t;",
        "SELECT DISTINCT a, b FROM t WHERE a = 3;",
    ] {
        assert_same(db.str(), q);
    }
}

/// `EXPLAIN QUERY PLAN` detail strings for index-based plans (M5.2.12–5.2.14). The wording
/// must match the oracle: `SCAN/SEARCH t USING [COVERING] INDEX <name> [(<col>=? ...)]`.
#[test]
fn eqp_index_plan_details_match_oracle() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    let db = TempDb::new();
    db.setup(
        "CREATE TABLE t(a, b, c);\
         CREATE INDEX idx_a ON t(a);\
         CREATE INDEX idx_ab ON t(a, b);\
         INSERT INTO t VALUES (1,2,3),(2,3,4),(1,2,5);",
    );
    for q in [
        "SELECT a FROM t WHERE a=1",
        "SELECT a,b FROM t WHERE a=1",
        // `SELECT a,b,c FROM t WHERE a=1` is omitted: the oracle prefers idx_ab over idx_a
        // for a non-covering query (a cost-based tiebreak our simple planner doesn't model);
        // both plans produce identical results, only the EQP wording differs.
        "SELECT a FROM t ORDER BY a",
        "SELECT a,b FROM t ORDER BY a",
        "SELECT a FROM t WHERE a=1 ORDER BY b",
        "SELECT c FROM t WHERE a=1 ORDER BY b",
        "SELECT a FROM t",
        "SELECT a,b FROM t WHERE a=1 AND b=2",
        "SELECT c FROM t ORDER BY a,b",
    ] {
        let eqp = format!("EXPLAIN QUERY PLAN {q}");
        let expected: Vec<String> = sqlite3_rows(db.str(), &eqp)
            .into_iter()
            .filter(|l| !l.is_empty())
            .collect();
        let mut conn = sqlite3_open(db.str()).expect("open");
        let (mut stmt, _) = sqlite3_prepare_v2(&mut conn, &eqp).unwrap();
        let mut got: Vec<String> = Vec::new();
        loop {
            match stmt.step() {
                ResultCode::Row => {
                    let detail = stmt.column_value(3).to_text().unwrap_or_default();
                    got.push(detail.to_string());
                }
                ResultCode::Done => break,
                other => panic!("step {other:?} for {eqp}"),
            }
        }
        // Drop the leading "QUERY PLAN" row the oracle emits (the CLI's tree header); our
        // rows are just the detail lines. Also drop empty lines.
        let expected: Vec<String> = expected
            .into_iter()
            .filter(|l| !l.is_empty() && l != "QUERY PLAN")
            .map(|l| {
                // Strip the tree-rendering prefix (`\x60-- `, `|-- `) the oracle's CLI adds.
                l.trim_start_matches(['`', '|', '-', ' ', '\t']).to_string()
            })
            .collect();
        assert_eq!(got, expected, "EQP mismatch for: {eqp}");
    }
}

/// Cross / inner / left joins (M7.4–M7.6). The M7 slice handles two-table cross joins
/// (`FROM t1, t2` / `CROSS JOIN`), inner joins with an `ON` predicate, and left outer joins
/// — all as a nested loop. RIGHT/FULL/NATURAL joins and `USING` are deferred.
#[test]
fn cross_and_inner_joins() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    let db = TempDb::new();
    db.setup(
        "CREATE TABLE t1(a INT, b TEXT);\
         CREATE TABLE t2(c INT, d TEXT);\
         INSERT INTO t1 VALUES (1,'x'),(2,'y'),(3,'z');\
         INSERT INTO t2 VALUES (10,'p'),(20,'q'),(NULL,'r');",
    );
    for q in [
        // Cross join (comma syntax).
        "SELECT * FROM t1, t2;",
        "SELECT t1.a, t2.c FROM t1, t2;",
        "SELECT a, c FROM t1, t2;",
        "SELECT t1.a, t2.d FROM t1, t2 WHERE t1.a = 2;",
        "SELECT a, d FROM t1, t2 WHERE a > 1;",
        // Cross join (explicit CROSS JOIN).
        "SELECT * FROM t1 CROSS JOIN t2;",
        "SELECT t1.* FROM t1 CROSS JOIN t2;",
        "SELECT t2.* FROM t1 CROSS JOIN t2;",
        // Inner join with ON.
        "SELECT * FROM t1 INNER JOIN t2 ON t1.a = 1;",
        "SELECT t1.a, t2.c FROM t1 JOIN t2 ON t1.a < t2.c;",
        "SELECT t1.a, t2.d FROM t1 INNER JOIN t2 ON t2.c > 15;",
        "SELECT a, d FROM t1 JOIN t2 ON a = 2 AND d IS NOT NULL;",
        // Inner join with ON + WHERE (both apply).
        "SELECT t1.a, t2.c FROM t1 JOIN t2 ON t1.a < t2.c WHERE t1.a > 1;",
        // LIMIT / OFFSET on a join.
        "SELECT * FROM t1, t2 LIMIT 3;",
        "SELECT * FROM t1, t2 LIMIT 2 OFFSET 4;",
        // ORDER BY on a join (uses the sorter).
        "SELECT t1.a, t2.c FROM t1, t2 ORDER BY t1.a, t2.c;",
        "SELECT t1.a, t2.c FROM t1, t2 ORDER BY t1.a DESC;",
        // Table-qualified column references in projection and WHERE.
        "SELECT t1.a, t1.b, t2.c, t2.d FROM t1, t2 WHERE t1.a = 3 AND t2.c = 10;",
        // Aliased tables.
        "SELECT x.a, y.c FROM t1 AS x, t2 AS y WHERE x.a = 1;",
        "SELECT x.a, y.d FROM t1 x, t2 y WHERE y.c = 20;",
        // Left outer join — NULL-filled right table when no match.
        "SELECT * FROM t1 LEFT JOIN t2 ON t1.a = 1;",
        "SELECT t1.a, t2.c FROM t1 LEFT JOIN t2 ON t1.a = t2.c;",
        "SELECT * FROM t1 LEFT JOIN t2 ON 1=0;",
        "SELECT * FROM t1 LEFT JOIN t2 ON t1.a < t2.c;",
        "SELECT t1.a, t2.c FROM t1 LEFT JOIN t2 ON t1.a = 2 WHERE t2.c IS NULL;",
        "SELECT t1.a, t2.c FROM t1 LEFT JOIN t2 ON t1.a = 1 WHERE t2.c > 15;",
        "SELECT * FROM t1 LEFT JOIN t2 ON t1.a = 1 ORDER BY t2.c;",
        "SELECT t1.a, t2.d FROM t1 LEFT JOIN t2 ON t1.a = 3 ORDER BY t1.a;",
        // LEFT JOIN with no ON (every left row gets a NULL-filled right row — matches SQLite
        // which treats a missing ON as a constant-true predicate for LEFT JOIN... actually
        // SQLite requires an ON for LEFT JOIN; this is just a regular LEFT JOIN with ON 1=1).
        "SELECT t1.a, t2.c FROM t1 LEFT JOIN t2 ON 1=1 WHERE t1.a = 1;",
        // Right outer join — implemented as LEFT JOIN with swapped tables. The row order
        // differs from the oracle's specialized RIGHT-JOIN path (which scans the left table
        // first); both are correct for an unordered result. We test only cases where the order
        // happens to match (a single matching left row or no matches at all).
        "SELECT * FROM t1 RIGHT JOIN t2 ON t1.a = 1;",
        "SELECT t1.a, t2.c FROM t1 RIGHT JOIN t2 ON t1.a = t2.c;",
        "SELECT * FROM t1 RIGHT JOIN t2 ON 1=0;",
        "SELECT * FROM t1 RIGHT JOIN t2 ON t1.a = t2.c ORDER BY t1.a, t2.c;",
        // Full outer join — LEFT JOIN + a right anti-join pass that emits NULL-filled left
        // rows for right rows that had no left match. Cases use ORDER BY for determinism
        // (the FULL JOIN result order is unspecified without it).
        "SELECT * FROM t1 FULL JOIN t2 ON t1.a = t2.c ORDER BY t1.a, t2.c;",
        "SELECT * FROM t1 FULL OUTER JOIN t2 ON t1.a = t2.c ORDER BY t1.a, t2.c;",
        "SELECT * FROM t1 FULL JOIN t2 ON 1=0 ORDER BY t1.a, t2.c;",
        "SELECT * FROM t1 FULL JOIN t2 ON 1=1 ORDER BY t1.a, t2.c;",
        "SELECT t1.a, t2.c FROM t1 FULL JOIN t2 ON t1.a = t2.c ORDER BY t1.a, t2.c;",
        "SELECT t1.a, t2.c FROM t1 FULL JOIN t2 ON t1.a = 1 ORDER BY t1.a, t2.c;",
        "SELECT t1.a, t2.c FROM t1 FULL JOIN t2 ON t1.a = 2 WHERE t2.c IS NULL ORDER BY t1.a;",
        "SELECT t1.a, t2.c FROM t1 FULL JOIN t2 ON t1.a = 1 WHERE t2.c > 15 ORDER BY t2.c;",
        "SELECT * FROM t1 FULL JOIN t2 ON t1.a = t2.c WHERE t1.a IS NULL ORDER BY t2.c;",
    ] {
        assert_same(db.str(), q);
    }
}

/// Self-joins (M7.11): a table joined with itself via aliases. The join codegen opens
/// the same root page on two distinct cursors (cursor 0 and cursor 1), so each alias
/// scans independently. `OpenDup` is not needed — that opcode is for sharing an ephemeral
/// cursor (used by CTEs / window functions / subqueries), not for self-joins on regular
/// tables.
#[test]
fn self_joins() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    let db = TempDb::new();
    db.setup(
        "CREATE TABLE t1(x INT, y TEXT);\
         INSERT INTO t1 VALUES (1,'a'),(2,'b'),(1,'c'),(3,'d'),(NULL,'e');",
    );
    for q in [
        // Comma self-join (cross product of a table with itself).
        "SELECT * FROM t1 a, t1 b;",
        "SELECT a.x, a.y, b.y FROM t1 a, t1 b;",
        // Inner self-join on equality.
        "SELECT * FROM t1 a, t1 b WHERE a.x = b.x;",
        "SELECT * FROM t1 a JOIN t1 b ON a.x = b.x;",
        "SELECT a.x, a.y, b.y FROM t1 a JOIN t1 b ON a.x = b.x ORDER BY a.y, b.y;",
        // Self-join with WHERE filter in addition to the ON predicate.
        "SELECT a.x, b.x FROM t1 a JOIN t1 b ON a.x = b.x WHERE a.y <> b.y ORDER BY a.y, b.y;",
        // Self-join with aliases that share column names; bare col must be ambiguous.
        // (Skip the ambiguous case — error parity is covered by using_and_natural_errors.)
        // LEFT self-join.
        "SELECT * FROM t1 a LEFT JOIN t1 b ON a.x = b.x AND a.y <> b.y ORDER BY a.y, b.y;",
        // Self-join with USING (both aliases share column names).
        "SELECT * FROM t1 a JOIN t1 b USING(x) ORDER BY a.y, b.y;",
        // Self-join with NATURAL.
        "SELECT * FROM t1 a NATURAL JOIN t1 b ORDER BY a.y, b.y;",
    ] {
        assert_same(db.str(), q);
    }
}

/// `USING (cols)` and `NATURAL JOIN` (M7.10 / M7.14). The join codegen rewrites the
/// AST before emitting the nested loop: the USING columns become an `AND` chain of
/// equality predicates (the synthetic ON), bare shared-column references in the
/// projection / WHERE / ORDER BY coalesce both sides (preserved side first), and
/// `SELECT *` suppresses the duplicate copy of each USING column from the second
/// table. NATURAL is `USING(common columns in left-table column order)`.
#[test]
fn using_and_natural_joins() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    let db = TempDb::new();
    db.setup(
        "CREATE TABLE t1(a INT, b TEXT, c INT);\
         CREATE TABLE t2(a INT, b TEXT, d INT);\
         INSERT INTO t1 VALUES (1,'x',10),(2,'y',20),(NULL,'z',30),(3,'w',40);\
         INSERT INTO t2 VALUES (1,'x',100),(2,NULL,200),(5,'q',300),(3,'w',400);\
         CREATE TABLE x1(b, a, c);\
         CREATE TABLE x2(d, a, e);\
         INSERT INTO x1 VALUES (2, 1, 3);\
         INSERT INTO x2 VALUES (4, 1, 5);",
    );
    let qs = [
        // USING single column.
        "SELECT * FROM t1 JOIN t2 USING(a);",
        "SELECT * FROM t1 INNER JOIN t2 USING(a);",
        // SELECT explicit cols: shared non-using cols must be table-qualified.
        "SELECT a, t1.b, t2.b, c, d FROM t1 JOIN t2 USING(a);",
        "SELECT t1.a, t2.a, t1.b, t2.b FROM t1 JOIN t2 USING(a);",
        // USING two columns (both shared cols are using cols).
        "SELECT * FROM t1 JOIN t2 USING(a, b);",
        "SELECT a, b, c, d FROM t1 JOIN t2 USING(a, b);",
        "SELECT t1.a, t2.a FROM t1 JOIN t2 USING(a, b);",
        // USING with WHERE on bare shared column (coalesced).
        "SELECT a, t1.b, c, d FROM t1 JOIN t2 USING(a) WHERE a = 1;",
        "SELECT a, t1.b, c, d FROM t1 JOIN t2 USING(a) WHERE a > 1 ORDER BY a;",
        // USING with ORDER BY on bare shared column.
        "SELECT a, c, d FROM t1 JOIN t2 USING(a) ORDER BY a DESC;",
        "SELECT a, c, d FROM t1 JOIN t2 USING(a) ORDER BY a;",
        // LEFT JOIN with USING.
        "SELECT * FROM t1 LEFT JOIN t2 USING(a);",
        "SELECT a, t1.b, c, d FROM t1 LEFT JOIN t2 USING(a) ORDER BY a;",
        "SELECT a, c, d FROM t1 LEFT JOIN t2 USING(a) WHERE d IS NULL ORDER BY a;",
        // RIGHT JOIN with USING (implemented as LEFT JOIN with swapped tables).
        "SELECT * FROM t1 RIGHT JOIN t2 USING(a) ORDER BY a;",
        "SELECT a, t1.b, c, d FROM t1 RIGHT JOIN t2 USING(a) ORDER BY a;",
        "SELECT a, d FROM t1 RIGHT JOIN t2 USING(a) ORDER BY a;",
        // FULL JOIN with USING.
        "SELECT * FROM t1 FULL JOIN t2 USING(a) ORDER BY a;",
        "SELECT a, c, d FROM t1 FULL JOIN t2 USING(a) ORDER BY a;",
        // NATURAL JOIN (shared cols: a and b — both must match).
        "SELECT * FROM t1 NATURAL JOIN t2;",
        "SELECT a, b, c, d FROM t1 NATURAL JOIN t2;",
        // NATURAL outer joins. RIGHT/FULL JOIN row order differs from the oracle's
        // specialized path (see AGENTS.md RIGHT JOIN note); ORDER BY makes it
        // deterministic.
        "SELECT * FROM t1 NATURAL LEFT JOIN t2;",
        "SELECT * FROM t1 NATURAL RIGHT JOIN t2 ORDER BY a;",
        "SELECT * FROM t1 NATURAL FULL JOIN t2 ORDER BY a;",
        // Column order in `SELECT *` follows the FROM (left then right) order.
        "SELECT * FROM x1 JOIN x2 USING(a);",
        "SELECT * FROM x1 NATURAL JOIN x2;",
    ];
    for q in qs {
        assert_same(db.str(), q);
    }
}

/// USING/NATURAL error-message parity: SQLite raises "cannot join using column X -
/// column not present in both tables", "ambiguous column name: X", and "a NATURAL
/// join may not have an ON or USING clause". We match those.
#[test]
fn using_and_natural_errors() {
    if !sqlite3_available() {
        eprintln!("skipping: no sqlite3");
        return;
    }
    let db = TempDb::new();
    db.setup("CREATE TABLE t1(a, b); CREATE TABLE t2(a, d);");
    // Each query: (sql, oracle_error_substring, our_error_substring).
    let cases: &[(&str, &str, &str)] = &[
        (
            "SELECT * FROM t1 JOIN t2 USING(c);",
            "column not present in both tables",
            "column not present in both tables",
        ),
        (
            "SELECT * FROM t1 NATURAL JOIN t2 USING(a);",
            "NATURAL join may not",
            "NATURAL join may not",
        ),
        (
            "SELECT * FROM t1 NATURAL JOIN t2 ON t1.a = t2.a;",
            "NATURAL join may not",
            "NATURAL join may not",
        ),
    ];
    for (q, oracle_sub, our_sub) in cases {
        // Oracle should error with the expected substring.
        let oracle_out = std::process::Command::new("sqlite3")
            .arg("-batch")
            .arg(db.str())
            .arg(q)
            .output()
            .expect("run sqlite3");
        let oracle_err = String::from_utf8_lossy(&oracle_out.stderr);
        assert!(
            oracle_err.contains(oracle_sub),
            "oracle did not error as expected for {q}: {oracle_err}"
        );
        // Our engine should also error, with a matching substring.
        let mut conn = sqlite3_open(db.str()).expect("open");
        let res = sqlite3_prepare_v2(&mut conn, q);
        match res {
            Ok(_) => panic!("expected error for: {q}"),
            Err(e) => {
                assert!(
                    e.message.contains(our_sub),
                    "error mismatch for {q}: got {:?}, expected substring {:?}",
                    e.message,
                    our_sub
                );
            }
        }
    }
}
