//! Write-path inverse-oracle tests: **rustqlite writes, the C `sqlite3` binary validates.**
//!
//! These are the write-path counterpart to the read-path `fileformat.rs` tests. The pager creates
//! a fresh database (and commits a journaled change) entirely through the Rust engine, then the
//! system `sqlite3` binary opens the resulting file and runs `PRAGMA integrity_check` — the
//! headline M4 guarantee that a rustqlite-written file is byte-format-valid to C SQLite. They are
//! plain `#[test]`s (the pager's async is driven by a local runtime) and SKIP when `sqlite3` is
//! absent.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rustsqlite_core::pager::Pager;
use rustsqlite_core::vfs::{OpenFlags, OsTokioVfs, Vfs};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn sqlite3_available() -> bool {
    Command::new("sqlite3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A temp database path that cleans itself (and its sidecar files) up on drop.
struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new(tag: &str) -> TempDb {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!("rustqlite_pw_{}_{tag}_{n}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        TempDb { path }
    }

    fn str(&self) -> &str {
        self.path.to_str().unwrap()
    }

    /// Run SQL through the system `sqlite3` and return its trimmed stdout.
    fn query(&self, sql: &str) -> String {
        let out = Command::new("sqlite3")
            .arg(self.str())
            .arg(sql)
            .output()
            .expect("run sqlite3");
        assert!(
            out.status.success(),
            "sqlite3 failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-journal", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.str()));
        }
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

macro_rules! skip_if_no_sqlite3 {
    () => {
        if !sqlite3_available() {
            eprintln!("skipping: system `sqlite3` binary not found");
            return;
        }
    };
}

#[test]
fn fresh_database_passes_c_integrity_check() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("fresh");

    rt().block_on(async {
        let vfs: Arc<dyn Vfs> = Arc::new(OsTokioVfs::new());
        let file = vfs
            .open(db.str(), OpenFlags::READWRITE_CREATE)
            .await
            .expect("open");
        // Create a brand-new, empty database through the Rust pager and close it.
        let _pager = Pager::create_fresh(vfs.clone(), db.str().to_string(), file, 4096)
            .await
            .expect("create_fresh");
    });

    // The C sqlite3 binary must accept the file as a valid, consistent database.
    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
    // It is genuinely empty (no user objects in sqlite_schema).
    assert_eq!(db.query("SELECT count(*) FROM sqlite_master;"), "0");
    // The page size we wrote round-trips through the C reader.
    assert_eq!(db.query("PRAGMA page_size;"), "4096");
}

#[test]
fn committed_header_change_passes_c_integrity_check() {
    skip_if_no_sqlite3!();
    let db = TempDb::new("commit");

    rt().block_on(async {
        let vfs: Arc<dyn Vfs> = Arc::new(OsTokioVfs::new());
        let file = vfs
            .open(db.str(), OpenFlags::READWRITE_CREATE)
            .await
            .expect("open");
        let pager = Pager::create_fresh(vfs.clone(), db.str().to_string(), file, 4096)
            .await
            .expect("create_fresh");

        // A journaled write transaction that sets the user_version in the header and restamps
        // page 1. This exercises begin_write → journal → commit (and page-1 change-counter stamp).
        pager.begin_write().await.expect("begin");
        pager.with_header_mut(|h| h.user_version = 42);
        // Mark page 1 dirty so the commit re-serializes the header (with the new user_version).
        let p1 = pager.read_page_for_write(1).await.expect("read p1");
        pager.write_page(1, p1).expect("write p1");
        pager.commit().await.expect("commit");
    });

    // C SQLite validates the file and reads back the committed header value.
    assert_eq!(db.query("PRAGMA integrity_check;"), "ok");
    assert_eq!(db.query("PRAGMA user_version;"), "42");
    // The journal sidecar was deleted by the clean commit.
    assert!(
        !std::path::Path::new(&format!("{}-journal", db.str())).exists(),
        "journal should be gone after commit"
    );
}
