# Testing Rustqlite

Rustqlite's correctness bar is **behavioral and byte-level equivalence with C SQLite**. The test strategy is
layered. Per project policy, **SQLite's own `.test` files are NOT vendored into this repository** — instead
this document gives copy-paste instructions plus an in-tree Rust harness to run suites against rustqlite,
out-of-tree.

The reference oracle is the **system `sqlite3` binary** (and/or `libsqlite3`). On this machine that is
`/usr/bin/sqlite3` reporting `3.53.1 2026-05-05`, matching [`VERSION`](VERSION).

## 0. Prerequisites

```sh
# The reference engine used by the differential + round-trip tests:
sqlite3 --version          # should match VERSION (3.53.x)
# If missing:  Debian/Ubuntu: sudo apt-get install -y sqlite3
#              Arch:          sudo pacman -S sqlite
#              macOS:         brew install sqlite
```

The Rust test suites that shell out to `sqlite3` skip themselves (rather than fail) when the binary is
absent, so `cargo test` still works on a machine without it — but coverage is reduced.

## 1. Unit tests (in-tree)

Per-module tests for the codecs and parsers — the file-format-critical layer is tested here first:

```sh
cargo test                                   # whole workspace
cargo test -p rustqlite format::            # varint / serial_type / record / header codecs
cargo test -p rustqlite-parser              # grammar + Pratt precedence golden tests
```

## 2. File-format round-trip (`tests/fileformat/`)

Guarantees byte-compatibility in both directions.

- **C → rustqlite**: create/populate a DB with the system `sqlite3`, open and read it with rustqlite;
  assert identical schema and rows.
- **rustqlite → C** (once the write path lands, M4): write with rustqlite, then in C SQLite run
  `PRAGMA integrity_check;` (must report `ok`) and `SELECT` the rows back.

```sh
cargo test -p rustqlite --test fileformat
```

Manual proof:

```sh
sqlite3 demo.db "create table t(a,b); insert into t values(1,'x'),(2,'y');"
cargo run -p rustqlite-cli -- demo.db ".mode column" "select * from t;"   # (M3+) identical rows
cargo run -p rustqlite-cli -- demo.db ".tables" ".schema"                 # (M1) works today
```

## 3. Differential oracle (`tests/diff/`)

Run identical SQL through rustqlite and the system `sqlite3`; assert identical result rows and error
behavior. The fastest way to catch behavior drift.

```sh
cargo test -p rustqlite --test diff
```

## 4. sqllogictest (`tests/slt/`)

Implement the [`sqllogictest`](https://crates.io/crates/sqllogictest) crate's DB trait for rustqlite and
run `.slt` corpora. Engine-agnostic, large coverage, no TCL needed. (Wired up from M3 once a query path
exists.)

```sh
cargo test -p rustqlite --test slt
```

## 5. Upstream TCL suite (out-of-tree, later phase)

Faithful execution of SQLite's own `.test`/`testrunner.tcl` requires a thin **C-ABI shim** that exports the
real `sqlite3_*` symbols backed by rustqlite (the optional `crates/rustqlite-capi` cdylib, loaded via
`LD_PRELOAD` or linked into `testfixture`). Until that shim exists, sqllogictest + the differential oracle
are the conformance gate.

```sh
# Sketch (later): clone upstream, build testfixture against the rustqlite C-ABI shim.
git clone https://github.com/sqlite/sqlite.git /tmp/sqlite-upstream
git -C /tmp/sqlite-upstream checkout version-3.53.1
# ... build crates/rustqlite-capi as a cdylib exporting sqlite3_* ...
# ... point testfixture/testrunner.tcl at it; run the .test files out-of-tree ...
```

## Definition of done (per feature)

A feature is "done" only when: differential tests vs system `sqlite3` pass, file-format round-trip passes,
relevant sqllogictest pass, and behavior matches upstream **including quirks**.
