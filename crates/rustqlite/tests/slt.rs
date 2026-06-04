//! `sqllogictest` test harness — runs the upstream `sqlite-test-suite` `.slt` corpus
//! against the rustqlite engine in-process.
//!
//! Why in-process: the engine is async-on-tokio and the C-API is the only fully-threaded
//! boundary. Driving each test query through a fresh subprocess would dominate the test
//! budget. The `AsyncDB` impl below drives the engine's public C-API directly, one query
//! per `run()` call, on a single connection per `.slt` file (the runner issues `connection`
//! records as fresh connections; the test files we ship use only the default).
//!
//! Files live in `target/slt/<file>.slt`, downloaded by `xtask/fetch-slt.sh` from the
//! upstream `sqlite-test-suite` (pinned SHA; see `TESTING.md` for the pin and update
//! procedure). The curated list in `tests/slt/manifest.txt` is what `slt_smoke` runs
//! (a tight subset that exercises the write path without booting the full 5-figure
//! corpus). `RUSTQLITE_FULL_SLT=1` extends the run to the full manifest.

use std::path::PathBuf;
use std::sync::Once;

use futures::executor::block_on;
use sqllogictest::{AsyncDB, DBOutput, DefaultColumnType, Runner};

use rustsqlite_core::capi::ResultCode;
use rustsqlite_core::{sqlite3_open, sqlite3_prepare_v2, Sqlite3, Value};

/// `futures::executor::block_on` (imported at the top of the file) is a sync runtime
/// that does NOT depend on tokio — it drives async DBs from a sync test thread even when
/// a tokio runtime is already live in the process. We use that here so the engine's own
/// `block_on` (which uses a process-global tokio runtime) can run from inside our async
/// DB methods without hitting "cannot start a runtime from within a runtime".

#[derive(Debug)]
enum DbError {
    Sql(String),
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbError::Sql(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for DbError {}

/// Adapter from the engine's public C-API to the `sqllogictest::AsyncDB` trait.
struct EngineDb {
    conn: Sqlite3,
}

impl EngineDb {
    fn new_in_memory() -> Self {
        // `:memory:` is the in-memory VFS — the engine treats it as a fresh database.
        let conn = sqlite3_open(":memory:").expect("open :memory:");
        Self { conn }
    }
}

#[async_trait::async_trait]
impl AsyncDB for EngineDb {
    type Error = DbError;
    type ColumnType = DefaultColumnType;

    async fn run(&mut self, sql: &str) -> Result<DBOutput<Self::ColumnType>, Self::Error> {
        // sqllogictest sometimes sends several statements in one `run` call (e.g. a
        // multi-statement fixture). Execute them one at a time; if the LAST statement
        // is a SELECT, return its rows; if it's a non-SELECT, return StatementComplete.
        let mut last_kind = LastKind::Unknown;

        for stmt_text in split_statements(sql) {
            let trimmed = stmt_text.trim();
            if trimmed.is_empty() {
                continue;
            }
            let (mut stmt, _tail) = match sqlite3_prepare_v2(&mut self.conn, trimmed) {
                Ok(pair) => pair,
                Err(e) => return Err(DbError::Sql(e.to_string())),
            };
            let ncol = stmt.column_count();
            let mut rows: Vec<Vec<String>> = Vec::new();
            let mut types: Vec<DefaultColumnType> = Vec::new();
            // Decide the output kind from the first iteration: if `column_count == 0` and
            // step() returns Done immediately, it's a non-SELECT.
            let mut stepped = false;
            loop {
                match stmt.step() {
                    ResultCode::Row => {
                        stepped = true;
                        let row: Vec<String> = (0..ncol)
                            .map(|i| value_to_string(stmt.column_value(i), &mut types, i))
                            .collect();
                        rows.push(row);
                    }
                    ResultCode::Done => break,
                    _ => return Err(DbError::Sql(stmt.errmsg().to_string())),
                }
            }
            let kind = if stepped {
                LastKind::Rows { types, rows }
            } else {
                // changes() reports the number of rows affected by the last write
                // statement. sqllogictest only checks for equality (or hash) so this is
                // good enough; we don't need the exact semantics of "changes" vs
                // "total_changes" for a single statement.
                let count = self.conn.changes() as u64;
                LastKind::StatementComplete(count)
            };
            last_kind = kind;
        }

        Ok(match last_kind {
            LastKind::Unknown => DBOutput::StatementComplete(0),
            LastKind::StatementComplete(c) => DBOutput::StatementComplete(c),
            LastKind::Rows { types, rows } => DBOutput::Rows { types, rows },
        })
    }

    async fn shutdown(&mut self) {
        // The connection's Drop runs the close path implicitly.
    }

    fn engine_name(&self) -> &str {
        "rustqlite"
    }
}

enum LastKind {
    Unknown,
    StatementComplete(u64),
    Rows {
        types: Vec<DefaultColumnType>,
        rows: Vec<Vec<String>>,
    },
}

/// Format a `Value` as a string and accumulate its `DefaultColumnType` into `types` at index
/// `i`. The first cell of column `i` fixes the type; subsequent cells in the same column
/// must match (sqllogictest's `default_column_validator` treats mismatches as `?`).
fn value_to_string(
    v: Value,
    types: &mut Vec<DefaultColumnType>,
    i: usize,
) -> String {
    let t = match &v {
        Value::Null => DefaultColumnType::Any,
        Value::Int(_) => DefaultColumnType::Integer,
        Value::Real(_) => DefaultColumnType::FloatingPoint,
        Value::Text(_) | Value::Blob(_) => DefaultColumnType::Text,
    };
    if i < types.len() {
        // Keep the first-seen type for the column (any later mismatch is harmless for the
        // default validator).
    } else {
        types.push(t);
    }
    format_value(&v)
}

fn format_value(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Int(i) => i.to_string(),
        Value::Real(r) => rustsqlite_core::util::fp_to_text(*r),
        Value::Text(s) => s.clone(),
        Value::Blob(b) => {
            // SQLite's `quote(x'..')` for blobs. sqllogictest compares with `X'..'`
            // strings, so we emit a quoted hex blob.
            let mut out = String::with_capacity(2 + b.len() * 2 + 1);
            out.push('X');
            out.push('\'');
            for byte in b {
                out.push_str(&format!("{:02X}", byte));
            }
            out.push('\'');
            out
        }
    }
}

/// Split a multi-statement `sql` string on `;` outside of string literals / blob literals /
/// identifiers. The split is naive but sufficient for the upstream `.slt` corpus, which
/// uses `;` exclusively to terminate statements and never as a token inside them.
fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut in_str = false;
    let mut in_blob = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if in_line_comment {
            buf.push(c);
            if c == '\n' {
                in_line_comment = false;
            }
            continue;
        }
        if in_block_comment {
            if c == '*' && chars.peek() == Some(&'/') {
                buf.push(c);
                buf.push(chars.next().unwrap());
                in_block_comment = false;
            } else {
                buf.push(c);
            }
            continue;
        }
        if in_str {
            buf.push(c);
            if c == '\'' {
                // Doubled `''` is an escaped quote — stay in the string.
                if chars.peek() == Some(&'\'') {
                    buf.push(chars.next().unwrap());
                } else {
                    in_str = false;
                }
            }
            continue;
        }
        if in_blob {
            buf.push(c);
            if c == '\'' {
                in_blob = false;
            }
            continue;
        }
        match c {
            '\'' => {
                buf.push(c);
                in_str = true;
            }
            'x' if buf.ends_with('X') || buf.ends_with("x'") || buf.is_empty() => {
                // Blob literal: `X'..hex..'` — the parser feeds the bytes; sqllogictest
                // files use `x'..'` only.
                buf.push(c);
            }
            'x' => buf.push(c),
            '-' if chars.peek() == Some(&'-') => {
                buf.push(c);
                buf.push(chars.next().unwrap());
                in_line_comment = true;
            }
            '/' if chars.peek() == Some(&'*') => {
                buf.push(c);
                buf.push(chars.next().unwrap());
                in_block_comment = true;
            }
            ';' => {
                let trimmed = buf.trim().to_string();
                if !trimmed.is_empty() {
                    out.push(trimmed);
                }
                buf.clear();
            }
            _ => buf.push(c),
        }
    }
    let trimmed = buf.trim().to_string();
    if !trimmed.is_empty() {
        out.push(trimmed);
    }
    out
}

/// Path to the sqllogictest corpus, downloaded by `xtask/fetch-slt.sh`.
/// `CARGO_MANIFEST_DIR` is the path to this test crate (`crates/rustqlite`); we
/// walk up two directories to reach the workspace root, then descend into `target/slt/`.
fn slt_dir() -> PathBuf {
    let mut dir = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    // crates/rustqlite → crates → <workspace>
    dir.pop();
    dir.pop();
    dir.push("target");
    dir.push("slt");
    dir
}

/// One-shot download on first run: invokes `xtask/fetch-slt.sh`. Subsequent runs use the
/// already-downloaded corpus.
fn ensure_corpus() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let mut xtask = std::env::var("CARGO_MANIFEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        xtask.push("xtask");
        xtask.push("fetch-slt.sh");
        let status = std::process::Command::new("bash")
            .arg(&xtask)
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => panic!(
                "fetch-slt.sh exited with {s}; please run `bash {}` to diagnose",
                xtask.display()
            ),
            Err(e) => panic!(
                "failed to invoke fetch-slt.sh at {}: {e}",
                xtask.display()
            ),
        }
    });
}

/// Read the manifest. Lines are paths relative to the `target/slt` root; blank lines and
/// `# comments` are ignored. Missing files are skipped with a warning (the test is
/// skipped, not failed).
fn read_manifest() -> Vec<PathBuf> {
    ensure_corpus();
    let mut manifest = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    manifest.push("tests");
    manifest.push("slt");
    manifest.push("manifest.txt");
    let Ok(text) = std::fs::read_to_string(&manifest) else {
        return Vec::new();
    };
    let root = slt_dir();
    text.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| root.join(l))
        .collect()
}

/// Run the smoke manifest. Fails (with the sqllogictest error report) on the first
/// mismatched test, matching upstream's default reporting.
#[test]
fn slt_smoke() {
    let full = std::env::var("RUSTQLITE_FULL_SLT").is_ok();
    let files: Vec<PathBuf> = read_manifest()
        .into_iter()
        .filter(|p| p.exists() || full)
        .collect();
    if files.is_empty() {
        eprintln!("skipping slt_smoke: no corpus in {}", slt_dir().display());
        return;
    }

    let mut any_failed = false;
    let mut summary = String::new();
    for path in files {
        if !path.exists() {
            summary.push_str(&format!(
                "skip (no corpus): {}\n",
                path.file_name().unwrap().to_string_lossy()
            ));
            continue;
        }
        let conn_maker = || async { Ok::<_, DbError>(EngineDb::new_in_memory()) };
        let mut runner = Runner::new(conn_maker);
        let r = block_on(runner.run_file_async(&path));
        if let Err(e) = r {
            any_failed = true;
            summary.push_str(&format!(
                "FAIL: {} — {}\n",
                path.file_name().unwrap().to_string_lossy(),
                e
            ));
        } else {
            summary.push_str(&format!(
                "ok:   {}\n",
                path.file_name().unwrap().to_string_lossy()
            ));
        }
    }
    if !summary.is_empty() {
        eprintln!("--- slt_smoke ---\n{summary}");
    } else {
        eprintln!(
            "--- slt_smoke ---\n  (no manifest entries; harness is wired but the M4.6 evidence/ \
             files all depend on un-shipped features. See crates/rustqlite/tests/slt/manifest.txt \
             for the rationale.)"
        );
    }
    if any_failed {
        panic!("slt_smoke had failures (see output above)");
    }
}
