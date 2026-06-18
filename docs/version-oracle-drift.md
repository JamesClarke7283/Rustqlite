# Version oracle drift

## Finding

The differential test suite (`crates/rustsqlite-core/tests/diff.rs`) compares rustsqlite
output against the system `sqlite3` binary at `/usr/bin/sqlite3`. The project pins its
SQLite compatibility target to `3.53.1` (see `VERSION` and `AGENTS.md`).

As of June 2026, the system oracle on this machine reports `3.53.2`. This caused the
`volatile_functions_shape` test to fail on `SELECT sqlite_version();` because the oracle
returned `3.53.2` while rustsqlite correctly returns the pinned `3.53.1`.

## Fix

`volatile_functions_shape` now asserts `sqlite_version()` against the value in `VERSION`
rather than the system oracle, while still oracle-comparing `typeof(sqlite_version())` and
all other volatile-function shape checks.

## Implication

When the system oracle version drifts from the pinned target, `sqlite_version()` is the only
differential query expected to diverge. All other behavioral comparisons remain valid against
the newer oracle as long as the newer version is backward-compatible with the pinned target.
