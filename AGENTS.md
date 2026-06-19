# AGENTS.md — Rustqlite

Rustqlite is a **full, faithful reimplementation of SQLite3 in Rust**. It is not bindings to libsqlite3
(that's `rusqlite`); it is a from-scratch engine whose internal architecture mirrors upstream SQLite.

## Non-negotiable goals
1. **Faithful**: match SQLite's behavior, results, error messages, and quirks. **No extra features** beyond
   what the pinned upstream SQLite version provides.
2. **Architecture parity**: modules map 1:1 to upstream C source (tokenizer/parser, code generator + query
   planner, VDBE register VM, B-tree, pager + WAL, VFS, utilities). See README for the mapping table.
3. **File-format compatibility**: must open, read, and write `.db` files created by C SQLite, byte-for-byte
   per https://www.sqlite.org/fileformat2.html. `PRAGMA integrity_check` on a rustsqlite-written DB must pass
   in C SQLite.
4. **C-API parity**: the public library API mirrors the SQLite C API (`sqlite3_open`, `sqlite3_prepare_v2`,
   `sqlite3_step`, `sqlite3_column_*`, `sqlite3_bind_*`, `sqlite3_finalize`, result codes `SQLITE_*`, …),
   translated to Rust types. Keep names identical where possible.
5. **CLI parity**: the `rustqlite` CLI crate (binary `rustsqlite`) mirrors the `sqlite3` shell — same flags,
   dot-commands, and output modes.

## Compatibility target
- SQLite **3.53.1** (see `VERSION`). `sqlite3_libversion()` reports `"3.53.1"`,
  `sqlite3_libversion_number()` reports `3053001`, and `sqlite3_sourceid()` reports the pinned source id.
- The on-disk **file format is stable across all of SQLite 3.x**, so format compatibility is not tied to the
  exact point release — but behavior/quirks are pinned to the target above.
- Reference oracle on this machine: the system `sqlite3` binary at `/usr/bin/sqlite3`
  (`3.53.2 2026-06-03` at the time of writing). Differential and round-trip tests compare
  against it. Because the project pins behavior to SQLite **3.53.1**, only
  `sqlite_version()` is expected to differ when the oracle drifts; see
  `@docs/version-oracle-drift.md`.

## Workspace
- `crates/rustqlite-parser` — SQL text → AST. **pest** PEG grammar ported from upstream `parse.y`;
  expression precedence via pest `PrattParser`. No engine dependency.
- `crates/rustsqlite-core` — the core engine and the public C-API-mirroring library (imported as
  `rustsqlite_core`). **Async on tokio.**
- `crates/rustqlite` — the shell (binary **`rustsqlite`**). **clap derive**; dot-commands dispatched in
  the REPL, not as clap subcommands.

## Async model
VFS/pager I/O is async on a tokio multi-thread runtime. The `sqlite3_*` functions keep synchronous
signatures and drive the async engine via `block_on` (a process-global runtime, `capi::runtime`).
Concurrency stays sqlite3-compatible (many readers, single writer); tokio adds async I/O, not new SQL
semantics. Because `sqlite3_*` use `Runtime::block_on`, do **not** call them from inside another tokio
runtime (e.g. a `#[tokio::test]`); engine-internal async fns are tested directly with their own runtime.

## Conventions
- Mirror upstream **names and semantics** (opcodes, pragmas, function names, error text). When in doubt,
  consult the upstream C source and sqlite.org docs — they are the spec.
- The VDBE opcode enum is kept **exhaustive** so unimplemented opcodes are compile-time visible.
- Dependencies: small, vetted crates allowed; never one that compromises file-format faithfulness. Justify
  each non-trivial dependency below.

## Versioning
- **Increment the crate version as we go.** Bump `version` in the root `[workspace.package]` (and the
  matching `workspace.dependencies` entries for the internal crates, which must stay in lockstep) whenever
  a change lands — a patch bump (`0.0.1` → `0.0.2`) for an incremental feature/fix, a minor bump for a
  milestone. All three crates inherit the workspace version (`version.workspace = true`), so one edit moves
  the whole tree. This keeps `Cargo.lock` and any published artifact honest about what changed.

## Dependency rationale
| Crate | Dep | Why |
|---|---|---|
| `rustsqlite-core` | `tokio` | async runtime + async file I/O for the VFS/pager layer |
| `rustsqlite-core` | `async-trait` | object-safe (`dyn`) async methods on the `Vfs`/`VfsFile` traits |
| `rustqlite-parser` | `pest`, `pest_derive` | PEG grammar engine; the locked decision for the parser |
| `rustqlite` (CLI) | `clap` (derive) | sqlite3-shell-compatible argument parsing |
| `rustqlite` (CLI) | `rustyline` | line editing + history for interactive mode |

Error types in the core are hand-rolled (no `thiserror`) to keep the dependency surface minimal.

## Research

Check these before web searching (load with Read tool as needed):
- @docs/version-oracle-drift.md - system `sqlite3` version drift vs. the pinned `VERSION` target

## Build / run / test
- Build: `cargo build`
- Shell: `cargo run -p rustqlite -- <file.db>` (the binary is `rustsqlite`)
- Tests: `cargo test`  (unit + differential + file-format round-trip + sqllogictest)
- Running SQLite's own suite against rustsqlite: see `TESTING.md` (run out-of-tree; do not vendor `.test`
  files into this repo).

## Definition of done for a feature
Differential tests vs the system `sqlite3` pass, file-format round-trip passes, relevant sqllogictest pass,
and behavior matches upstream (including quirks). No feature is "done" if it diverges from SQLite.

## Milestone status (see README §roadmap)
- **M0 — Scaffold**: ✅ workspace, three crates, docs, version pin, CI, `sqlite3_libversion*`.
- **M1 — File format (read)**: 🚧 in progress — format codecs (`varint`/`serial_type`/`record`/`header`),
  async VFS (mem + tokio), read-only pager, table-b-tree read cursor with overflow, `sqlite_schema`
  reader. CLI `.tables`/`.schema` read real C-SQLite databases. Remaining: index b-tree read cursor,
  `WITHOUT ROWID`, ptrmap/auto-vacuum awareness.
- **M2 — Parser**: 🚧 a working subset grammar (`SELECT`/`CREATE TABLE`/`INSERT` + the full expression
  atom/operator set, including `IS NOT` and **JOIN syntax**); full `parse.y` port pending. Known gap: a bare integer literal
  larger than `i64` (e.g. the exact `-9223372036854775808`) is parsed as REAL rather than special-cased.
- **M3a — Read query path (single-table SELECT)**: ✅ a faithful register VDBE (executor + opcode set),
  code generator (projection, `WHERE` with 3-valued logic, `ORDER BY` via an in-memory sorter,
  `LIMIT`/`OFFSET`, rowid-alias substitution), value comparison + type affinity, the byte-faithful REAL→text
  formatter (`sqlite3FpDecode` port, fuzz-validated), ~10 scalar functions, and the C-API prepare/step/column
  path. CLI runs `SELECT` in `list`/`csv`/`column` modes. Differential-tested against `sqlite3` 3.53.1.
  Deferred to **M3b**: `EXPLAIN`/`EXPLAIN QUERY PLAN`, the full scalar-function set, the remaining output
  modes, and the `sqllogictest` harness.
- **M3b — Read-path completion**: ✅ `EXPLAIN` (golden-tested bytecode renderer) and `EXPLAIN QUERY PLAN`
  (oracle-matched `detail` wording, shell-faithful tree in the CLI); the full scalar set — string
  (`instr`/`replace`/`trim`/`hex`/`unhex`/`char`/`unicode`/`concat`/`quote`/…), math
  (`sqrt`/`ln`/`log`/trig/`pow`/`mod`/`pi`/…), and misc (`iif`/`min`/`max`/`coalesce`/`nullif`/…);
  `LIKE`/`GLOB` (a faithful `patternCompare` port, ASCII-only case fold, GLOB classes, 3-arg `ESCAPE`);
  volatile/connection functions (`random`/`randomblob`/`changes`/`sqlite_version`/…); and all shell
  output modes (list/csv/column/line/quote/tabs/ascii/html/markdown/box/table/json/insert).
  Moved to **M4**: the `sqllogictest` harness (its `.slt` corpora need the write path — `CREATE`/`INSERT`).
- **M4 — Write path** ✅: mutable pager + rollback journal + crash recovery, b-tree page split +
  root promotion with overflow-page chains, `CREATE TABLE` / `INSERT ... VALUES` /
  `DELETE` / `DROP TABLE`. The `sqllogictest` harness is wired (`crates/rustqlite/tests/slt.rs`
  + `xtask/fetch-slt.sh`) and exercises the engine in-process; the manifest is populated
  as M4.6+ features land.
- **M5.0 — `UPDATE`** ✅: single-table `UPDATE [OR action] tbl SET col = expr [, …] [WHERE expr]`
  via the same two-pass (sorter-as-rowset) shape that upstream's `OP_NotExists` path uses for
  `ONEPASS_OFF` updates. Wired in the new `Opcode::NotExists` + `TableCursor::seek_rowid`,
  the `P5_ISUPDATE` flag that suppresses the double-counting on `Delete`+`Insert`, and the
  connection-level `did_insert` tracker that keeps `last_insert_rowid()` from being clobbered
  by an `UPDATE`. Differential-tested vs the C oracle (`update_writes_match_oracle`),
  file-format round-tripped through C `sqlite3` (`update_roundtrip_and_c_oracle`).
  Still M5+: indexes, joins, aggregates, subqueries, `INSERT ... SELECT`, UPSERT,
  compound SELECT, triggers, views, `UPDATE` of the rowid-alias column, `RETURNING`,
  conflict resolution other than ABORT.
- **M5.1 — single-column indexes** ✅: `CREATE [UNIQUE] INDEX [IF NOT EXISTS] name ON tbl(col)`
  (single-column, `UNIQUE` recorded in the catalog but not enforced at `IdxInsert` time — the
  page-level engine does not yet model uniqueness), `DROP INDEX [IF EXISTS] name`, indexed
  equality `WHERE col = <const>` (uses the new `SeekGE` / `IdxGT` boundary-check opcodes over an
  `IndexCursor`; `IdxGT` jumps when the entry is `>` the boundary, so the equality range
  terminates at the first strictly-greater key), indexed equality + `ORDER BY` (the indexed
  `SELECT` path emits a seek-and-walk, no sorter), and per-row index maintenance from
  `INSERT` / `UPDATE` / `DELETE` (the index `Delete` runs *after* the WHERE check so
  non-matching rows don't drop index entries; the `UPDATE` path snapshots the OLD indexed
  value into a fresh register before the SET, then `IdxDelete` of the old key + table
  `Delete`/`Insert` + `IdxInsert` of the new key). Population is single-leaf only — the
  page-full error propagates; index page splits land in a follow-up. Differential-tested vs
  the C oracle (round-trip + indexed lookup) in `crates/rustsqlite-core/tests/write_roundtrip.rs`
  and the in-process slt harness (`our/index.slt` + `evidence/slt_lang_dropindex.test`).
- **M5.2 — multi-column indexes** ✅: `CREATE [UNIQUE] INDEX … ON tbl(col1, col2, …)`, composite
  index keys (concatenated indexed columns + trailing rowid), per-row `IdxInsert`/`IdxDelete`
  maintenance from `INSERT`/`UPDATE`/`DELETE`, and indexed prefix-equality `SELECT`
  (`WHERE col1 = ? AND col2 = ? …`). Index b-tree page splits and interior-page traversal
  were already in place from the M5.1 follow-up work. Differential-tested vs the C oracle
  (`multi_column_index_select`, `multi_column_index_maintained_on_writes`) and the in-process
  slt harness (`our/multi-column-index.slt`). Still M5+: `KeyInfo` per-column collation,
  enforced `UNIQUE`, partial/expression indexes, `ORDER BY` via index ordering hints.
