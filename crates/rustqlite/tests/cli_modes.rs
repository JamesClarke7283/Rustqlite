//! CLI-level differential tests: run the same query + `.mode`/`.headers` through BOTH the real
//! `sqlite3` shell (the oracle, at `/usr/bin/sqlite3`) and our `rustsqlite` binary, and assert the
//! stdout bytes are identical. These tests are skipped (treated as passing) when the oracle binary
//! is unavailable, so they never block a build that lacks the oracle.
//!
//! Both engines open the fixture read-only so repeated invocations on the same file never create
//! journal/WAL side files that could perturb a later read.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

const ORACLE: &str = "/usr/bin/sqlite3";

/// The fixture used by every case: three columns mixing INTEGER / TEXT / NULL / REAL.
const SCHEMA: &[&str] = &[
    "CREATE TABLE t(a, b, c);",
    "INSERT INTO t VALUES(2,'yy',NULL),(1,'x',3.5),(100,'zzz','hi');",
];

/// All output modes the shell supports.
const MODES: &[&str] = &[
    "list", "csv", "column", "line", "quote", "tabs", "ascii", "html", "markdown", "box", "table",
    "json", "insert",
];

/// A per-call counter so concurrent tests never share a temp directory.
static SEQ: AtomicU32 = AtomicU32::new(0);

/// Create a fresh temp directory holding a freshly-built oracle fixture DB. Returns `None` (so the
/// caller skips) when the oracle binary cannot build it.
fn make_fixture(tag: &str) -> Option<(PathBuf, PathBuf)> {
    if !Path::new(ORACLE).exists() {
        eprintln!("skipping: oracle {ORACLE} not present");
        return None;
    }
    let nonce = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rustqlite_{tag}_{}_{nonce}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let db = dir.join("fixture.db");
    let _ = std::fs::remove_file(&db);

    let mut cmd = Command::new(ORACLE);
    cmd.arg(&db);
    for stmt in SCHEMA {
        cmd.arg(stmt);
    }
    match cmd.status() {
        Ok(s) if s.success() => Some((dir, db)),
        _ => {
            eprintln!("skipping: could not build oracle fixture");
            let _ = std::fs::remove_dir_all(&dir);
            None
        }
    }
}

/// Run a sequence of shell args through `program` (with a leading read-only flag), returning raw
/// stdout bytes.
fn run(program: &str, readonly_flag: &str, db: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new(program)
        .arg(readonly_flag)
        .arg(db)
        .args(args)
        .output()
        .expect("spawn failed");
    out.stdout
}

#[test]
fn output_modes_match_oracle() {
    let Some((dir, db)) = make_fixture("diff") else {
        return;
    };
    let rustqlite = env!("CARGO_BIN_EXE_rustsqlite");
    let query = "SELECT * FROM t;";
    for &mode in MODES {
        for &hdr in &["on", "off"] {
            let mode_arg = format!(".mode {mode}");
            let hdr_arg = format!(".headers {hdr}");
            let args = [mode_arg.as_str(), hdr_arg.as_str(), query];

            let oracle = run(ORACLE, "-readonly", &db, &args);
            let ours = run(rustqlite, "--readonly", &db, &args);
            assert_eq!(
                ours,
                oracle,
                "mode={mode} headers={hdr}\n oracle={:?}\n  ours ={:?}",
                String::from_utf8_lossy(&oracle),
                String::from_utf8_lossy(&ours),
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn json_mode_real_exponents_match_oracle() {
    // REAL rendering in `.mode json`: the decimal/exponential boundary (1e15 decimal, 1e16
    // exponential), positive exponents (kept, e.g. `1.0e+17`), the zero-padded negative exponent
    // (`1.0e-07`), multi-digit mantissas, signed values, -0.0, and the Inf sentinel — all must be
    // byte-identical to the oracle. Aliased so column names are stable across both engines.
    let Some((dir, db)) = make_fixture("jsonexp") else {
        return;
    };
    let rustqlite = env!("CARGO_BIN_EXE_rustsqlite");
    let query = "SELECT 1e15 AS a, 1e16 AS b, 1e17 AS c, 5e22 AS d, 9.99e19 AS e, \
                 -1e25 AS f, 1e-7 AS g, 1e-308 AS h, 2.5e-100 AS i, 1.5e300 AS j, 1.0 AS k, \
                 -0.0 AS l, 3.14 AS m, 0.0001 AS n, 1e1000 AS o, -1e1000 AS p;";
    let args = [".mode json", query];
    let oracle = run(ORACLE, "-readonly", &db, &args);
    let ours = run(rustqlite, "--readonly", &db, &args);
    assert_eq!(
        ours,
        oracle,
        "\n oracle={:?}\n  ours ={:?}",
        String::from_utf8_lossy(&oracle),
        String::from_utf8_lossy(&ours)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mode_insert_table_arg_matches_oracle() {
    let Some((dir, db)) = make_fixture("ins") else {
        return;
    };
    let rustqlite = env!("CARGO_BIN_EXE_rustsqlite");
    // `.mode insert mytab` + `.headers on` exercises the column-list path and the table-name arg.
    let args = [".mode insert mytab", ".headers on", "SELECT * FROM t;"];
    let oracle = run(ORACLE, "-readonly", &db, &args);
    let ours = run(rustqlite, "--readonly", &db, &args);
    assert_eq!(
        ours,
        oracle,
        "\n oracle={:?}\n  ours ={:?}",
        String::from_utf8_lossy(&oracle),
        String::from_utf8_lossy(&ours)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
