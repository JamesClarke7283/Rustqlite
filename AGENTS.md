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
| `rustsqlite-core` | `libc` | POSIX `fcntl(F_SETLK)` byte-range locking for `OsTokioVfs` (mirrors `os_unix.c`'s `unixLock`/`posixUnlock`); std does not expose advisory file locking |
| `rustqlite-parser` | `pest`, `pest_derive` | PEG grammar engine; the locked decision for the parser |
| `rustqlite` (CLI) | `clap` (derive) | sqlite3-shell-compatible argument parsing |
| `rustqlite` (CLI) | `rustyline` | line editing + history for interactive mode |

Error types in the core are hand-rolled (no `thiserror`) to keep the dependency surface minimal.

## Research

Check these before web searching (load with Read tool as needed):
- @docs/version-oracle-drift.md - system `sqlite3` version drift vs. the pinned `VERSION` target
- @docs/without-rowid-storage.md - WITHOUT ROWID table on-disk layout and `convertToWithoutRowidTable` shape
- @docs/row-value-expressions.md - SQLite `TK_VECTOR` grammar, row-value comparisons, and `IN (subquery)` forms
- @docs/autovacuum-ptrmap.md - auto-vacuum/ptrmap page math, header meta[] layout, and Rustqlite implementation notes
- @docs/wal-shm-vfs-methods.md - WAL shared-memory VFS methods (`xShmMap`/`xShmLock`/`xShmBarrier`/`xShmUnmap`), lock slot indices, and the in-process `a_lock` array

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
- **M2 — Parser**: ✅ a working subset grammar (`SELECT`/`CREATE TABLE`/`INSERT` + the full expression
  atom/operator set, including `IS NOT` and **JOIN syntax**); full `parse.y` port pending. Known gap: a bare integer literal
  larger than `i64` (e.g. the exact `-9223372036854775808`) is parsed as REAL rather than special-cased.
  All M2 tasks (2.1–2.74) are now done. The **2.73 AST walker**
  (`crates/rustqlite-parser/src/walker.rs`, mirroring `walker.c`) exposes a read-only pre-order
  [`Visitor`] trait with [`WalkControl::Continue`/`Prune`/`Abort`] semantics and free functions
  `walk_expr`/`walk_expr_list`/`walk_select`/`walk_select_expr`/`walk_select_from`/`walk_stmt`
  (plus `walk_window` for window specs). It descends into subqueries, compound arms, CTE bodies,
  trigger bodies, and the `WINDOW` clause; `Prune` lets a visitor skip a node's children without
  stopping the walk (e.g. `contains_aggregate` would prune on `Exists`/`Subquery`). Existing manual
  walks (`contains_aggregate`, `collect_aggregates`, `rewrite_aggregates`, `rewrite_expr`) are
  not yet migrated — the walker is the infrastructure for future passes. The **2.74 name-resolution
  pass** (`crates/rustsqlite-core/src/codegen/resolve.rs`, mirroring `resolve.c`) is a read-only
  validation pre-pass that walks the SELECT's expressions (via `walk_select_expr`) and verifies
  every `Expr::Column` resolves uniquely against a `NameContext` built from the FROM tables. It
  raises `"ambiguous column name: X"` and `"no such column: X[.Y]"` matching the oracle, before
  codegen emits opcodes. The `NameContext` carries a `parent` link for correlated subqueries
  (upstream's `pNext` chain); subqueries with their own FROM are pruned in `visit_select` (their
  column refs are resolved by the subquery codegen paths, which raise "no such column" via
  `compile_column`). The actual cursor/column-index binding still happens at codegen time in
  `compile_column` — the Rust AST is immutable and has no slot for upstream's `pExpr->iTable`/
  `iColumn`, so this pass is validate-only. What's **not** yet done (and upstream does): result-
  column alias resolution for `ORDER BY` (still at codegen time via `resolve_order_term`),
  `NC_*` flag enforcement (`NC_AllowAgg`/`NC_IsCheck`/`NC_PartIdx`/…), compound-arm FROM
  resolution (needs catalog access), and FROM-subquery body resolution (handled by subquery
  codegen). Note: `build_qualified_name`
  preserves quoted-identifier quotes (e.g. `"col"` stays `"col"`, not unquoted to `col`); unquoting
  is deferred to the full parse.y port. `INDEXED`/`MATCH`/`REGEXP` etc. are reserved in our grammar
  (upstream uses `%fallback ID` so they're contextually reserved); this is a minor divergence.
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
  were already in place from the M5.1 follow-up work. **5.2.12–5.2.14 covering/ORDER BY index
  scans** ✅: the planner now picks an index for three benefits — a WHERE equality prefix
  (seek), a covering index (index-only scan, no table lookup), and an ORDER BY prefix (no
  sorter) — which compose on a single index (`SELECT a,b FROM t WHERE a=? ORDER BY b` on
  `INDEX(a,b)` seeks to `a=?` and walks in `b` order). The covering path reads projection /
  WHERE / ORDER BY columns directly from the index cursor via a `Ctx.index_read` column-
  position map (the rowid-alias column maps to the trailing rowid at `nkey_fields`). `EXPLAIN
  QUERY PLAN` emits the oracle-faithful `SCAN/SEARCH t USING [COVERING] INDEX <name>
  [(<col>=? ...)]`. Differential-tested vs the C oracle (`covering_and_orderby_index_scans`,
  `eqp_index_plan_details_match_oracle`). Still M5+: `KeyInfo` per-column collation,
  enforced `UNIQUE`, partial/expression indexes, reverse (DESC) index scans, multi-column
  ORDER BY with mixed ASC/DESC.
- **M5.3 — B-Tree Robustness & WITHOUT ROWID** 🚧: page merging on delete, interior-page
  balancing, `Clear` opcode, freelist reuse/walking all landed. **5.3.6 `WITHOUT ROWID`
  tables** ✅: `CREATE TABLE … (…, PRIMARY KEY(…)) WITHOUT ROWID` opens a blob-keyed
  (index) b-tree keyed by the PK record (PK columns followed by the remaining columns,
  matching upstream's `convertToWithoutRowidTable` covering-index shape). `INSERT` builds
  the storage-order record, enforces implicit `NOT NULL` on PK columns via `OP_HaltIfNull`,
  and `IdxInsert`s it with `P5_UNIQUE` for the PK uniqueness constraint; `SELECT` opens
  the table as an index cursor and the `Column` opcode reads values by storage position.
  Differential-tested vs the C oracle (`without_rowid_*_roundtrip_and_c_oracle` — single
  INTEGER PK, single non-INTEGER PK, and composite PK — plus the CLI reads C-SQLite-written
  WITHOUT ROWID databases and C-SQLite's `PRAGMA integrity_check` passes on
  Rustqlite-written ones). **5.3.7 Auto-vacuum / ptrmap** ✅: `PRAGMA auto_vacuum =
  NONE|FULL|INCREMENTAL` (0/1/2) and `PRAGMA incremental_vacuum(N)`; pointer-map pages
  (`btree/ptrmap.rs`) with the `PTRMAP_*` type codes and `ptrmapPageno`/`is_ptrmap_page` math;
  auto-vacuum-aware root-page allocation (`create_table_btree_autovac` /
  `create_index_btree_autovac` place roots at `meta[4]+1` and update meta[4]); the full
  `autoVacuumCommit` + `incrVacuumStep` + `relocatePage` + `modifyPagePointer` page-move logic
  (`btree/autovac.rs`); `Pager::allocate_page` skips ptrmap/pending-byte pages; `Pager::free_page`
  records FREEPAGE ptrmap entries; b-tree splits write BTREE ptrmap entries for new children.
  Differential-tested vs the C oracle (`auto_vacuum_full_shrinks_file_after_delete_all`,
  `auto_vacuum_incremental_shrinks_file_step_by_step` — C `sqlite3` `PRAGMA integrity_check`
  passes on Rustqlite-written auto-vacuum databases). Still M5.3+: 5.3.8 `PRAGMA
  integrity_check` backend, 5.3.9 `Destroy` freelist reuse; `DELETE`/`UPDATE` on WITHOUT
  ROWID tables (deferred to a follow-up that reuses the storage-order key-record helpers).
  Known gap: overflow-page ptrmap entries (OVERFLOW1/OVERFLOW2) are not yet recorded by the
  sync cell builders, so vacuuming a database where overflow pages need to be relocated
  (not just freed) will not update the overflow chain's parent pointer; see
  @docs/autovacuum-ptrmap.md.
- **M6 — Aggregates, GROUP BY, DISTINCT** ✅ (6.8 done): `AggStep`/`AggFinal` execution,
  `GROUP BY` (sorter + per-group accumulate + `AggFinal`), `HAVING`, the built-in aggregate
  set (`count`/`sum`/`total`/`avg`/`min`/`max`/`group_concat`), `SELECT DISTINCT` (ephemeral
  index dedup), aggregate-without-GROUP-BY, and NULL handling. **6.8 `GROUP BY` + `ORDER BY`**
  ✅: the two-pass shape (aggregate pass writes per-group result rows into an output sorter
  keyed by the ORDER BY expressions; a tail block sorts and walks it with OFFSET/LIMIT). The
  ORDER BY expressions are rewritten like the projection (aggregate calls → `AggRef`, GROUP BY
  exprs → `AggRef`), so `ORDER BY count(*) DESC` works. `EXPLAIN QUERY PLAN` emits the
  oracle-faithful `USE TEMP B-TREE FOR GROUP BY` / `USE TEMP B-TREE FOR ORDER BY` (the latter
  is suppressed when ORDER BY is exactly the GROUP BY keys, matching upstream's `nOBSat`).
  Differential-tested vs the C oracle (`aggregate_queries`, `group_by_order_by_with_varying_counts`).
  Known divergence: the tiebreak order for equal ORDER BY keys is unspecified in SQL; our stable
  sorter preserves GROUP BY insertion order while SQLite's b-tree-backed ORDER BY reverses it
  for DESC — both are correct, test cases with ties use a secondary ORDER BY key for determinism.
- **M7 — Joins** 🚧: **7.1–7.3** ✅ (parser, `OpenEphemeral`, `Found`/`NotFound` already shipped
  in M2/M6). **7.4–7.5 cross / inner joins** ✅: two-table `FROM t1, t2` / `CROSS JOIN` /
  `INNER JOIN ... ON` compile as a nested loop (outer `Rewind`/`Next` over the left table,
  inner `Rewind`/`Next` over the right, ON predicate + WHERE filtered inside). Multi-table
  column resolution via `Ctx::join_tables` — a `table.col` reference resolves to the named
  table (alias or name); a bare `col` searches the FROM tables in order. `SELECT *` expands
  across all tables; `table.*` expands the named table. ORDER BY on a join uses the sorter.
  Differential-tested vs the C oracle (`cross_and_inner_joins`). Still M7+: left/right/full
  outer joins, natural join, `USING`, self-joins, join-order selection, aggregates over joins.
  **7.6–7.7 left outer join + `NullRow`** ✅: `LEFT JOIN ... ON` emits a NULL-filled right-table
  row (via the new `OP_NullRow` opcode) when no inner row matches the ON predicate. A per-outer-
  row match flag tracks whether any inner row matched; after the inner loop, if the flag is 0,
  the right cursor is set to a NULL row and one row is emitted (re-applying the WHERE clause,
  which filters out NULL-filled rows when it tests right-table columns). Differential-tested
  vs the C oracle (LEFT JOIN cases in `cross_and_inner_joins`). **7.8 right join** ✅:
  `RIGHT JOIN` is implemented by swapping the tables and emitting a LEFT JOIN (the original
  right table becomes the outer loop, the original left table becomes the inner). `SELECT *`
  expands in the original FROM order. Still M7+: full outer joins, natural join, `USING`,
  self-joins, join-order selection, aggregates over joins. Known divergence: the row order
  of a RIGHT JOIN differs from the oracle's specialized RIGHT-JOIN path (which scans the
  left table first); both are correct for an unordered result, test cases use ORDER BY for
  determinism. **7.9 full outer join** ✅: `FULL [OUTER] JOIN` is implemented as a LEFT JOIN
  followed by a second pass that scans the (original) right table and, for each right row
  with no left match (ON predicate is never TRUE under strict 3-valued logic — NULL
  comparisons are UNKNOWN, not matches), emits a NULL-filled left row + the right row. The
  second pass uses a per-right-row nested scan over the left cursor with `jump_if_null=false`
  so NULL join keys don't spuriously count as matches. WHERE is re-applied on the NULL-filled
  left row (a WHERE on left-table columns filters it out since NULL comparisons are
  UNKNOWN). LIMIT applies across both passes (the second pass decrements the same limit
  register).   `validate_join` now accepts `Full`/`FullOuter` and rejects only `NATURAL` and
  `USING`. Differential-tested vs the C oracle (FULL JOIN cases in `cross_and_inner_joins`).
  **7.10 natural join + 7.14 USING** ✅: `USING (cols)` and `NATURAL [LEFT|RIGHT|FULL] JOIN`
  are implemented by rewriting the AST before join codegen runs (`codegen::join_using`).
  For each shared column the rewrite synthesizes an `ON l.col = r.col AND …` predicate
  (NATURAL picks the columns common to both tables in left-table declared order) and
  replaces bare references to a USING column (in projection / WHERE / ORDER BY / GROUP BY
  / HAVING) with a synthetic 2-arg coalesce `Expr::Coalesce2 { left, right }` = `IF outer.col
  IS NOT NULL THEN outer.col ELSE inner.col` (in JOIN order, so the preserved side wins for
  LEFT/RIGHT/FULL). `SELECT *` expands in FROM order with the USING cols suppressed from
  the second table; the using col itself appears once, coalesced. Non-using cols that exist
  in both tables are table-qualified to avoid ambiguity. Error-message parity: "cannot join
  using column X - column not present in both tables", "ambiguous column name: X", "a
  NATURAL join may not have an ON or USING clause". `validate_join` is now a no-op for
  USING/NATURAL (the rewrite handles them); it still rejects only unsupported join chains.
  Differential-tested vs the C oracle (`using_and_natural_joins`,
  `using_and_natural_errors`). Still M7+: self-joins, join-order selection, aggregates
  over joins, join chains (multiple ON levels). Known divergence: same RIGHT/FULL JOIN
  row-order note as above; test cases use ORDER BY for determinism. **7.11 self-joins** ✅:
  a table joined with itself via aliases (`FROM t a, t b`) is handled by the existing
  join codegen — the same root page is opened on two distinct cursors (cursor 0 and
  cursor 1), so each alias scans independently. `OpenDup` (M7.12) is NOT needed for
  self-joins on regular tables; that opcode is for sharing an ephemeral cursor (used by
  CTEs / window functions / subqueries in M8/M11), not self-joins. USING and NATURAL
  work on self-joins too (the AST rewrite resolves column references via the alias names).
  Differential-tested vs the C oracle (`self_joins`).

- **M8 — Subqueries & Correlated Scans** 🚧: **8.1–8.4** ✅ (subquery / `EXISTS` /
  `IN (SELECT …)` / scalar-subquery parser support shipped in M2). **8.5 coroutine
  opcodes** ✅: `OP_InitCoroutine`, `OP_EndCoroutine`, and `OP_Yield` are implemented in
  the VDBE (`vdbe::exec`) using a direct-address PC convention (upstream stores `addr - 1`
  because its dispatch loop post-increments `pOp`; we store `addr` directly). `InitCoroutine
  p1 p2 p3` sets `r[p1] = p3` (the coroutine entry) and jumps to `p2` (skipping the
  coroutine body). `Yield p1 p2` swaps the PC with `r[p1]` (saving the next instruction's
  address so the coroutine resumes there); if the destination is an `EndCoroutine`, the
  coroutine has ended and `Yield` jumps to its own `p2` (the "coroutine ended"
  continuation). `EndCoroutine p1` reads the calling `Yield`'s `p2` from the instruction
  at `r[p1] - 1` and jumps there, leaving `r[p1]` set to its own address so subsequent
  `Yield`s re-end. Unit-tested with a 3-row coroutine (`coroutine_init_yield_end_basic`)
  and an empty coroutine (`coroutine_empty`). **8.6 `FROM (subquery)` materialization** ✅:
  `FROM (subquery) AS alias` is compiled by `codegen::subquery::compile_from_subquery`
  (mirrors the `SRT_EphemTab` path in `select.c`). The subquery body is compiled as a
  sub-program, then inlined into the outer program: its `Init` and `Halt`-onward setup
  block are dropped (the outer program keeps its own canonical setup); each `ResultRow`
  is rewritten into `MakeRecord + NewRowid + Insert` into a high-numbered ephemeral
  cursor (cursor 10, clear of any subquery/outer-scan cursor). Because `ResultRow`
  expands to multiple instructions, the inlined addresses do NOT map 1:1 with a constant
  offset — an address map (`sub_addr -> inlined_addr`) is built during inlining and every
  jump's `p2` is patched using it; jumps that targeted the subquery's `Halt` (the
  scan-end label) are redirected to `after_sub` (the outer scan's first opcode). The
  outer SELECT is compiled against a synthesized `Table` whose columns match the
  subquery's output column names (BLOB affinity — no coercion, like SQLite); the outer
  scan reads from the ephemeral via `Rewind`/`Next`/`Column`. Supports: constant
  subquery, subquery over a real table (with `WHERE`), `SELECT *`, projection, outer
  `WHERE`, outer `ORDER BY`, outer `LIMIT`/`OFFSET`, `VALUES` subquery, and a subquery
  with an aggregate. `EXPLAIN QUERY PLAN` emits `SCAN <alias>` for the outer scan (the
  oracle's `CO-ROUTINE <alias>` + `SCAN <alias>` shape for non-flattenable subqueries,
  and the `SCAN t` flattening for simple subqueries, land with M8.12 subquery
  flattening). Differential-tested vs the C oracle (`from_subquery_materialization`).
  Still M8+: scalar subquery / `EXISTS` / `IN (SELECT …)` codegen, `OpenDup`
  (M7.12 BLOCKED), `Program` / `Param` opcodes for correlated subqueries.
  **8.7 scalar subquery in expressions** ✅: `Expr::Subquery` is compiled by
  `codegen::subquery::compile_scalar_subquery` (mirrors `sqlite3CodeSubselect` in `expr.c`
  for the `TK_SELECT` case). The subquery body is compiled as a sub-program via
  `select::compile`, then inlined into the outer program as a subroutine wrapped in
  `OP_Once` (new opcode) + `OP_Gosub`/`OP_Return`: `Once` caches the result across
  encounters so a non-correlated subquery runs only once per statement; the subroutine
  pre-fills `result_reg` with NULL (the no-rows case), then runs the inlined scan; each
  `ResultRow` is rewritten to `SCopy <col0>, result_reg` + `Goto subroutine_end` (the
  `LIMIT 1` equivalent); the body's `Halt` becomes the `Return`. Because the inlined body
  shares the outer program's register/cursor space, every register operand is rebased by
  `reg_offset = next_reg() - 1` and every cursor operand by `cursor_offset = next_cursor()`
  (new `ProgramBuilder::note_cursor`/`next_cursor` API lets the outer scan/sorter/DISTINCT
  codegen record the cursor numbers they open so the inlined subquery offsets past them —
  avoiding the cursor-0 collision between an outer table scan and an inner table scan).
  Multi-column scalar subqueries raise "sub-select returns more than one column (N)"
  matching the oracle. A `SubqueryResolver` trait (implemented by
  `CatalogSubqueryResolver` in `capi::stmt`) gives the expression codegen pager-based
  catalog access so the subquery's `FROM` table is resolved; threaded through `Ctx` as
  `Option<&dyn SubqueryResolver>` (None at every non-SELECT codegen site). Supports:
  constant subquery, subquery over a real table (with `WHERE`/`ORDER BY`/`LIMIT`),
  aggregate subquery (`max`/`min`/`count`/`sum`/`avg`/`total`), subquery in arithmetic /
  concatenation / `WHERE` / multiple subqueries in one query. Differential-tested vs the
  C oracle (`scalar_subquery_in_expressions`). Known limitation: the `Once` wrapping
  assumes the subquery is non-correlated — a correlated subquery (referencing outer
  columns) caches the first row's result and replays it for every outer row (wrong but
  non-crashing); correlation support needs M8.11 `Param` + M8.13 re-materialization,
  plus name resolution (M2.74) to detect `EP_VarSelect`.
  **8.8 `EXISTS (subquery)`** ✅: `Expr::Exists` is compiled by
  `codegen::subquery::compile_exists_subquery` (mirrors `sqlite3CodeSubselect` in
  `expr.c` for the `TK_EXISTS` case). The subquery body is compiled as a sub-program via
  `select::compile`, then inlined into the outer program as a subroutine wrapped in
  `OP_Once` + `OP_Gosub`/`OP_Return` (same shape as 8.7). The `SRT_Exists` destination
  pre-fills `result_reg` with `Integer 0` (the no-rows case), then rewrites each
  `ResultRow` into `Integer 1, result_reg` + `Goto subroutine_end` (the `LIMIT 1`
  equivalent — the first yielded row flips the result to 1 and the subroutine returns).
  The body's `Halt` becomes the `Return`. Register/cursor rebasing and jump-patch loop
  are shared with `compile_scalar_subquery` via `rebase_operands`/`is_absolute_jump`.
  Supports: bare `EXISTS`/`NOT EXISTS` as a scalar, `EXISTS` in `WHERE`, `EXISTS` over a
  real table (with `WHERE`), `EXISTS` with constant subquery, multiple `EXISTS` in one
  query, and `EXISTS` combined with scalar subqueries. Differential-tested vs the C
  oracle (`exists_subquery`). Same non-correlated limitation as 8.7.
  **8.9 `IN (subquery)`** ✅: `Expr::InSubquery` is compiled by
  `codegen::subquery::compile_in_subquery` (mirrors `sqlite3ExprCodeIN` in `expr.c` for
  the `ExprUseXSelect` case, the `IN_INDEX_EPH` path). The subquery body is compiled as a
  sub-program via `select::compile`, then inlined into the outer program as a subroutine
  wrapped in `OP_Once` + `OP_Gosub`/`OP_Return` (same shape as 8.7/8.8). The `SRT_Set`
  destination rewrites each `ResultRow` into `SCopy <col0>, col_reg` + `MakeRecord` +
  `IdxInsert` into an ephemeral index (opened with `P4::KeyInfo` for the record-keyed
  variant). A `rhs_has_null_reg` flag is set to NULL whenever a materialized row's first
  column is NULL (the "RHS contains NULL" flag from `sqlite3SetHasNullFlag`, used by the
  post-probe FALSE-vs-NULL distinction). After the subroutine, the LHS is evaluated and
  the membership test follows in-operator.md's optimized algorithm: Step 2 (LHS NULL →
  Step 6 scan), Step 3 (`Found`/`NotFound` probe), Step 4 (RHS non-NULL → FALSE),
  Step 6 (scan RHS for a NULL comparison → NULL, else FALSE), Step 7 (FALSE). The
  `dest_if_false == dest_if_null` combined case emits a single `NotFound` to dest (Step 3+5
  fused). The value form (`compile_expr`) wraps the jump form with three labels
  (false/null/true) and stores 1/0/NULL into the target; `NOT IN` swaps the TRUE/FALSE
  storage. Register/cursor rebasing and jump-patch loop are shared with
  `compile_scalar_subquery` via `rebase_operands`/`is_absolute_jump`. Supports: constant
  subquery, subquery over a real table (with `WHERE`), `IN`/`NOT IN` in `WHERE` and
  projection, NULL LHS (NULL → NULL when RHS non-empty, FALSE when RHS empty), NULL RHS
  (the FALSE-vs-NULL distinction), empty subquery, and multiple `IN` subqueries in one
  query. Differential-tested vs the C oracle (`in_subquery`). Same non-correlated
  limitation as 8.7/8.8. Known divergence: the parser parses `a = 10 OR a IN (...)` as
  `(a = 10 OR a) IN (...)` (IN binds looser than OR in our grammar — a parser precedence
  bug to fix in the full parse.y port, not in M8.9).
  **8.10 `Program` opcode** ✅ / **8.11 `Param` opcode** ✅: the VDBE now implements
  `OP_Program` and `OP_Param` (mirrors `vdbe.c`'s `OP_Program`/`OP_Param`). `OP_Program
  p1 p2 p3 p4=SubProgram p5=token` invokes a sub-VDBE: the parent's running state
  (program, pc, register file, cursor table, cursor-root map, decoded-record cache,
  aggregate state, `Once`-fired set, `write_txn` flag) is saved into a new `VdbeFrame`
  pushed on `self.frames`; the sub-program from `P4::SubProgram(Arc<Program>)` is
  installed with a fresh register file (sized to `sub_program.num_registers`) and empty
  cursor table; execution begins at the sub-program's first instruction. `p1` is the
  parent-frame register base for `OP_Param` resolution; `p2` is the `OE_Ignore` jump
  target (consulted when the sub-program halts with `p2 == 5`); `p5` is the recursion
  token (non-zero enables the recursive-trigger guard — a sub-program with the same
  token already on the frame stack is skipped, mirroring `pProgram->token` matching in
  `vdbe.c`). `OP_Param p1 p2` copies the parent frame's register at `param_base + p1`
  into the current frame's `r[p2]`. `OP_Halt` with `p1 == SQLITE_OK` and a non-empty
  frame stack pops the frame and resumes the parent at the saved PC; on `OE_Ignore`
  (`p2 == 5`) it jumps to the calling `Program`'s `p2` instead. A new `P4::SubProgram
  (Arc<Program>)` variant carries the sub-program; `render_p4` renders it as
  `program(N,M)` (instruction count, register count) matching upstream's `displayP4`.
  The change-counter deltas in the shared `RuntimeCtx` propagate across the frame
  boundary (the sub-program's writes bump the parent's `changes()`), matching upstream.
  Unit-tested with three hand-built programs: a `Program`+`Param` round-trip (sub reads
  a parent register via `Param`, computes, emits a row, returns; parent resumes and
  emits its own row), an `OE_Ignore` halt (sub halts with `p2 == 5`, parent jumps to
  the calling `Program`'s `p2`), and the recursive-trigger guard (a sub-program calling
  another sub-program with the same `p5` token is skipped). The codegen does not yet
  emit `OP_Program` (no trigger/view sub-programs compiled yet) — these opcodes are the
  runtime infrastructure for M15 views, M16 triggers, and M8.13 correlated-subquery
  re-materialization.

- **M9 — Compound SELECT** ✅: `UNION` / `UNION ALL` / `INTERSECT` / `EXCEPT` via the merge
  algorithm with coroutines (`codegen::compound`, mirrors `multiSelectByMerge` in `select.c`).
  Two shapes: `UNION ALL` without `ORDER BY` uses the simple chain path (left arm then right
  arm, shared LIMIT/OFFSET counters); everything else uses the merge algorithm — each arm is
  compiled as a coroutine (synthesized `ORDER BY 1, 2, … ncol` when the user didn't supply
  one, matching upstream's "invent one first" step), the main loop runs both coroutines in
  parallel, `OP_Compare` + `OP_Jump` route to `AltB`/`AeqB`/`AgtB`/`EofA`/`EofB` handlers
  implementing the operator-specific logic, and duplicate removal for `UNION`/`INTERSECT`/
  `EXCEPT` runs inside `outA`/`outB` subroutines via a `regPrev` block + `OP_Compare`/`OP_Jump`
  skip-if-equal. `EXPLAIN QUERY PLAN` emits the oracle-faithful `COMPOUND QUERY` /
  `LEFT-MOST SUBQUERY` / `<OP> [USING TEMP B-TREE]` tree (no ORDER BY) or `MERGE (<OP>)` /
  `LEFT` / `RIGHT` tree (with ORDER BY). Multi-arm compounds (3+ arms) lower
  left-associatively: the left sub-compound is compiled recursively and materialized into a
  sorter that serves as the outer merge's "A" coroutine. The `Program` struct gained a
  `num_cursors` field so the outer builder can advance `next_cursor` past an inlined arm's
  cursors (both arms' cursors are open simultaneously during the merge). Differential-tested
  vs the C oracle (`compound_select`, `compound_select_column_count_mismatch` — all four
  operators, ORDER BY / LIMIT / OFFSET, multi-arm, cross-table, NULL handling, and
  column-count-mismatch error parity).
- **M10 — CTEs (Common Table Expressions)** ✅: **10.1–10.5** ✅ (parser
  shipped in M2.18). Non-recursive CTEs are implemented by AST rewriting
  (`codegen::cte::rewrite_with_ctes`, mirrors the `searchWith`/`SRT_EphemTab` path in
  `select.c`): a `WITH …` clause on a SELECT is expanded by rewriting each CTE reference
  in the FROM clause (and in the FROM clauses of compound arms) into a
  `TableOrJoin::Subquery` whose body is the CTE's SELECT. The rewritten SELECT has its
  `with_clause` cleared, so this is a one-shot rewrite and downstream codegen sees a plain
  `FROM (subquery) AS alias` shape that the existing `codegen::subquery::compile_from_subquery`
  infrastructure materializes into an ephemeral table and scans (the `SRT_EphemTab` shape
  upstream uses for a CTE referenced once). Multiple CTEs in one `WITH` are processed in
  declared order so a later CTE may reference an earlier one (the prefix is carried as a
  scope and rewritten into the later CTE's body before the later CTE itself is published).
  An explicit CTE column list (`name (cols) AS (…)`) wraps the body's projection so each
  output column carries the declared name as its alias (for a `SELECT *` or `VALUES` body,
  the body is nested inside an outer shell `SELECT <names> FROM (body) AS __cte_inner` so
  the inner `*` expands against its own FROM table at codegen time). A schema-qualified
  reference (`main.cte`) never matches a CTE (matches upstream's `searchWith` early-out).
  **10.3 Recursive CTEs** ✅ are implemented by `codegen::cte::compile_recursive`, which
  mirrors `generateWithRecursiveQuery` in `select.c`: the setup query fills a Queue
  ephemeral; the loop pulls rows from the Queue (`OP_Rewind`/`OP_RowData`/`OP_Delete`),
  appends each to the CTE result ephemeral (`OP_NewRowid`/`OP_Insert`), runs the recursive
  query with the CTE name bound to the single "Current" row via a new `OP_OpenPseudo`
  pseudo-cursor (reading from a register set by `OP_RowData`), and appends the recursive
  results back to the Queue; the loop continues until the Queue is empty. The outer query
  then scans the CTE result ephemeral. Three new opcodes (`OP_OpenPseudo`, `OP_RowData`,
  and the `PseudoCursor` variant) were added; `OP_Delete` now handles ephemeral cursors
  (drain the Queue); `OP_NullRow`/`OP_Rewind`/`OP_Next` handle pseudo-cursors (no-op /
  always-valid / always-exhausted). The setup/recursive/outer sub-programs are inlined
  into one program with register and cursor rebasing (the CTE-name table cursor 0 is
  rewritten to the Current pseudo-cursor for the recursive arm and to the CTE result
  ephemeral for the outer scan). Differential-tested vs the C oracle (`non_recursive_ctes`,
  `recursive_ctes` — counter, multi-column projection, LIMIT/OFFSET, UNION, VALUES setup,
  recursive CTE over a real table). Known limitations: `UNION` (dedup) is not enforced
  (treated as `UNION ALL` — correct for monotonic recursive queries); the recursive arm
  must scan the CTE name as its single FROM entry (no joins in the recursive arm); the
  outer query must scan the CTE name as its single FROM entry (no joins over a recursive
  CTE); a CTE whose body is itself a compound SELECT (coroutine-based) is not yet
  inlinable into the outer materialization; a CTE referenced inside a scalar/`IN`/`EXISTS`
  subquery is not yet rewritten (the SubqueryResolver path doesn't apply the CTE rewrite);
  nested subqueries in FROM block a non-recursive CTE-referencing-CTE whose inner CTE is
  rewritten to a subquery — these land with nested-subquery support.
- **M11 — Window Functions** 🚧: **11.1–11.3** ✅ (parser `OVER`/`FILTER`/named windows, and
  VDBE accumulator state `AggInverse`/`AggValue` opcodes — landed in earlier iterations).
  **11.4–11.6 built-in window function accumulators** ✅: `AggregateKind` now carries the
  window-only kinds `RowNumber`/`Rank`/`DenseRank`/`PercentRank`/`CumeDist`/`Ntile`/
  `FirstValue`/`LastValue`/`NthValue`/`Lead`/`Lag`, resolved case-insensitively at codegen
  time alongside the plain aggregates via `AggregateKind::from_name`. The `Accumulator` struct
  gained `n_value`/`n_step`/`n_total`/`n_param`/`nth_step`/`captured` fields mirroring
  upstream's `CallCount`/`NtileCtx`/`NthValueCtx`/`LastValueCtx` state. The `step`/`inverse`/
  `value_mut` implementations faithfully port `row_numberStepFunc`/`rankStepFunc`/
  `dense_rankStepFunc`/`percent_rankStepFunc`+`InvFunc`/`cume_distStepFunc`+`InvFunc`/
  `ntileStepFunc`+`InvFunc`/`first_valueStepFunc`/`last_valueStepFunc`+`InvFunc`/
  `nth_valueStepFunc` from `window.c`. The executor's `AggValue` arm dispatches window-only
  kinds through a new `value_mut` path (mutating `xValue`, matching upstream — e.g.
  `rankValueFunc` resets `nValue = 0` so the next peer group re-latches); plain aggregates
  keep the non-mutating `value` path. `AggregateKind::default_frame` returns the
  upstream-coerced frame for each built-in (mirrors the `aUp[]` table in
  `sqlite3WindowUpdate`, `window.c:699`). `AggregateKind::window_only` distinguishes the
  window-only built-ins from the aggregate-as-window kinds. Codegen-time validation:
  `check_no_window_only_without_over` walks the projection/WHERE/HAVING/ORDER BY and raises
  the upstream "misuse of window function <name>()" error for a window-only function used
  without an `OVER` clause (matches the oracle). `collect_aggregates` and
  `contains_aggregate` now check `over.is_none()` so a windowed aggregate call
  (`count(*) OVER (...)`) is not double-counted by the plain-aggregate path; the
  `rewrite_aggregates`/`rewrite_aggregates_with_group_keys` walks likewise skip windowed
  calls. **11.7 partition-sort + frame-step codegen driver (first slice)** ✅:
  `codegen::window::compile_window_select` lowers a `SELECT` with `OVER (...)` calls to a
  VDBE program. The shape is: scan the table → sort by PARTITION BY + ORDER BY into a sorter
  (carrying the full table row as payload) → walk the sorted sorter, driving accumulators
  and emitting rows. Two frame shapes are supported in this first slice: **PerRow**
  (`ROWS UNBOUNDED PRECEDING → CURRENT ROW` — the `row_number()` default; each row gets one
  `AggStep` + `AggValue` + `ResultRow`) and **PerPeerGroup** (`RANGE UNBOUNDED PRECEDING →
  CURRENT ROW` — the `rank()`/`dense_rank()`/default-aggregate shape; a peer group is stepped
  together, `AggValue` is read once, and every row in the peer group is emitted with that same
  result via a `Gosub`-driven flush subroutine over a peer-buf ephemeral). Partition changes
  reset the accumulators (the `Null` opcode now clears the `aggregates` HashMap entry so the
  next `AggStep` creates a fresh accumulator). The `Clear` opcode now handles ephemeral
  cursors (resets the in-memory record buffer) so the peer-buf can be cleared between peer
  groups. `first_value`/`nth_value`'s `value_mut` returns the captured value WITHOUT
  clearing (matching the window codegen's per-peer-group `AggValue` pattern; upstream uses
  `noopValueFunc` for `xValue` and emits via `xFinalize`, but our codegen uses `AggValue` per
  peer group, so the non-clearing path is correct). Supports: `PARTITION BY`, `ORDER BY`,
  `FILTER (WHERE expr)`, multiple window calls sharing one `OVER` spec, outer `WHERE`/
  `ORDER BY`/`LIMIT`/`OFFSET`/`DISTINCT`, and the aggregate-as-window functions
  (`count`/`sum`/`total`/`avg`/`min`/`max`/`group_concat`). Differential-tested vs the C
  oracle (`window_functions` — 40+ queries covering all supported shapes).
  **11.8–11.9 sliding window frames (first slice)** ✅: `codegen::window::compile_sliding_frame`
  lowers a `SELECT` with an explicit `OVER (ORDER BY … <frame>)` (any frame spec other than
  the two simple shapes handled by PerRow/PerPeerGroup) to a VDBE program using the
  full-scan-per-row approach (mirrors `sqlite3WindowCodeStep` in `window.c` at a coarser
  granularity): walk the sorted sorter, copying each partition's rows into an ephemeral
  partition cache (rowid 1..=n), then for each current row i re-scan the cache from
  `[start, end]` (computed from the frame bounds), AggStep each row, AggValue → result_reg,
  emit. This is O(n²) per partition but correct and uniform across frame modes; the
  streaming-3-cursor optimization lands with the follow-up. Supports: `ROWS BETWEEN <bound>
  AND <bound>` with `UNBOUNDED PRECEDING`/`CURRENT ROW`/`expr PRECEDING`/`expr FOLLOWING`/
  `UNBOUNDED FOLLOWING` bounds; `PARTITION BY` (partition cache reset between partitions);
  `count`/`sum`/`total`/`avg`/`group_concat` in a sliding frame; outer `WHERE`/`ORDER BY`/
  `LIMIT`/`OFFSET`; `RANGE`/`GROUPS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW`/`UNBOUNDED
  FOLLOWING` (routed to the PerPeerGroup path — same result, lower overhead). New VDBE
  opcodes: `AddImm` (short-form integer add for counters), `SeekRowid` (ephemeral cursor
  seek by rowid), `ResetSorter` (clear ephemeral/sorter without closing), `Last`
  (position cursor at last row), `Prev` (reverse-step cursor), `OpenDup` (open a second
  cursor sharing an ephemeral's storage — runtime infrastructure for the streaming-3-cursor
  follow-up). The `Ephemeral` struct was refactored to hold its shared state in an
  `Rc<RefCell<EphemeralData>>` so `OpenDup` can clone the cursor and share storage; gained
  `seek_rowid`/`len`/`rowid`/`reset`/`dup` helpers. `rebase_operands`/`is_absolute_jump` in
  `codegen::compound` and `codegen::subquery` learned the new opcodes for sub-program
  inlining. Differential-tested vs the C oracle (`window_function_frame_specs` — 25+
  queries; `window_function_frame_spec_unsupported` — verifies graceful error, not crash,
  for `min()`/`max()` in non-default frames). Still M11: **11.10** the `EXCLUDE` clause
  (rejected with a specific error in this slice — needs the streaming-3-cursor
  `AggInverse` shape to remove rows mid-step), full RANGE/GROUPS `<expr>` bounds
  (peer-group logic for `CURRENT ROW`/`<expr>` — this slice treats `CURRENT ROW` as `i`,
  which is correct only when ORDER BY values are distinct), and the streaming-3-cursor
  optimization. Known gaps: `lead`/`lag`/`ntile`/`last_value`/`percent_rank`/`cume_dist`
  are still rejected at codegen time (their default frames need `AggInverse`); multiple
  *different* `OVER` specs in one query are still rejected; `ORDER BY` on the outer query
  that references non-projection columns in the PerPeerGroup flush pass may read NULL (the
  peer-buf only carries projection columns — the follow-up carries the full row).

- **M12 — Transactions & Savepoints** 🚧: **12.1–12.3** ✅ (parser + `OP_AutoCommit`-driven
  `BEGIN`/`COMMIT`/`END`/`ROLLBACK`; the `autocommit` flag is shared between the connection
  and the VDBE so `OP_Halt` defers the commit when inside a `BEGIN`). **12.4–12.5 savepoint
  stack** ✅: `SAVEPOINT name` / `RELEASE [SAVEPOINT] name` / `ROLLBACK [TRANSACTION] TO
  [SAVEPOINT] name` are compiled to the new `OP_Savepoint` opcode (mirrors `OP_Savepoint` in
  `vdbe.c` + `sqlite3Savepoint` in `build.c`). The opcode dispatches on `p1` (0=BEGIN,
  1=RELEASE, 2=ROLLBACK) with `P4::Text(name)`. The pager carries a savepoint stack
  (`Pager::savepoints: Mutex<Vec<PagerSavepoint>>`, mirroring `Pager.aSavepoint` in
  `pager.c`); each entry snapshots the dirty overlay (`HashMap<u32, PageRef>`) and the page
  count at savepoint creation. Because `write_page` replaces the `Arc<Vec<u8>>` in the dirty
  map (never mutating it in place), the snapshot is a cheap shallow clone that preserves the
  savepoint-time page bytes while subsequent writes swap in new `Arc`s. `SAVEPOINT` outside
  a transaction auto-starts one (the "transaction savepoint" — `db->isTransactionSavepoint`
  in `main.c`, shared by `Arc<Mutex<bool>>` between the connection and the VDBE); `RELEASE`
  of that outermost savepoint commits the implicit transaction (turns autocommit on, calls
  `pager.commit()`); `RELEASE` of any other savepoint drops it and nested ones (their changes
  become part of the enclosing transaction); `ROLLBACK TO` restores the dirty overlay to the
  snapshot, truncates the page count back to the savepoint's `n_orig`, and drops nested
  savepoints while keeping the named one (so it can be rolled back to again). A `COMMIT` or
  `ROLLBACK` via `OP_AutoCommit` clears the savepoint stack and resets the transaction-
  savepoint flag (mirrors `sqlite3CloseSavepoints` in `main.c`). Differential-tested vs the
  C oracle (8 new cases in `transactions.rs`: auto-start + release commits, rollback-to
  inside BEGIN discards, auto-start + rollback-to + release, nested savepoints with inner
  rollback, release inner keeps changes for outer rollback, re-rollback to same savepoint,
  unknown-savepoint errors, explicit COMMIT after SAVEPOINT auto-start). **12.6
  `Transaction` opcode** ✅: `BEGIN IMMEDIATE` emits `OP_Transaction 0 1` + `OP_AutoCommit
  0 0` (RESERVED up-front); `BEGIN EXCLUSIVE` emits `OP_Transaction 0 2` + `OP_AutoCommit
  0 0` (EXCLUSIVE up-front); `BEGIN DEFERRED` emits only `OP_AutoCommit 0 0` (lazy
  RESERVED at first write). `Pager::begin_write` takes an `ex_flag: bool` mirroring
  `sqlite3PagerBegin`'s `exFlag`. **12.7 VFS lock escalation** ✅: `OsTokioVfs` performs
  real POSIX `fcntl(F_SETLK)` byte-range locking on the `PENDING_BYTE`/`RESERVED_BYTE`/
  `SHARED_FIRST` ranges (a faithful port of `unixLock`/`posixUnlock` in `os_unix.c`), so
  cross-process contention with the real `sqlite3` binary is correct. A process-global
  `LockState` registry (mirrors `unixInodeInfo`'s `inodeList`) catches same-process
  contention that the per-process OS locks miss. `MemVfs` shares the same `LockState`
  abstraction for in-process multi-connection locking. `VfsFile::check_reserved_lock`
  (mirrors `unixCheckReservedLock`) lets `recover_hot_journal` skip recovery when the
  journal belongs to an active transaction on another connection. `Pager::begin_read` takes
  the SHARED lock lazily (called from `OP_Transaction 0 0` at statement start, not from
  `sqlite3_open` — so `sqlite3_open` on an EXCLUSIVE-locked file succeeds; the first
  statement fails with `SQLITE_BUSY`). `begin_write` ensures a SHARED lock is held before
  escalating to RESERVED/EXCLUSIVE. Differential-tested vs the C oracle (3 new cross-
  connection cases in `transactions.rs`: BEGIN IMMEDIATE blocks BEGIN IMMEDIATE, BEGIN
  EXCLUSIVE blocks BEGIN EXCLUSIVE + SELECT, BEGIN IMMEDIATE allows reads). **12.8
  `OR ROLLBACK`/`OR FAIL`/`OR IGNORE`/`OR REPLACE` conflict resolution** ✅: the codegen
  threads the parsed `or_action` through a new `OeAction` enum (`vdbe::oe`, mirroring the
  `OE_*` macros in `sqliteInt.h`) and sets it on `Program::default_oe` so the executor's
  `step()` knows how to clean up on a constraint violation. `OR IGNORE` and `OR REPLACE` are
  handled at codegen time via a new `OP_NoConflict` opcode (mirrors `OP_NoConflict` in
  `vdbe.c`) that pre-checks each unique index BEFORE the table `Insert`: IGNORE jumps past
  the row on conflict (no table row written); REPLACE fetches the conflicting row's rowid via
  `IdxRowid`, seeks the table cursor to it via `NotExists`, deletes its entries from every
  index via `IdxDelete`, deletes the table row, then falls through to the new `Insert` +
  `IdxInsert`s (which now won't conflict). `OR ABORT` (the default), `OR FAIL`, and
  `OR ROLLBACK` are handled by `step()`'s error path: ABORT rolls back just the statement's
  changes (via an implicit statement savepoint opened by `OP_Transaction` when inside an
  explicit `BEGIN`, or a full rollback under autocommit); FAIL keeps all prior changes
  (including earlier rows from the same statement); ROLLBACK rolls back the entire
  transaction. The statement savepoint (`__rustqlite_stmt_abort`) is released on success by
  `OP_Halt`. `NoConflict`/`Found`/`NotFound` now work on real index b-tree cursors (not
  just ephemeral) by seeking the main cursor and comparing the entry's prefix. UPDATE
  supports `OR ROLLBACK`/`ABORT`/`FAIL` (sets `default_oe`); `OR IGNORE`/`OR REPLACE` on
  UPDATE is rejected at codegen time (needs the same pre-check shape threaded through the
  two-pass sorter — deferred to a follow-up). Differential-tested vs the C oracle (7 new
  cases in `write_roundtrip.rs`: OR IGNORE skips conflicting rows, OR REPLACE deletes + 
  re-inserts, OR REPLACE with secondary index, OR FAIL keeps prior rows, OR ROLLBACK in
  explicit transaction, OR ABORT in explicit transaction, default ABORT in explicit
  transaction). **12.9 `ON CONFLICT` column/table constraints** ✅: the parser captures the
  per-constraint `ON CONFLICT <action>` clause on `PRIMARY KEY`/`UNIQUE`/`NOT NULL`/`CHECK`
  column and table constraints (it was parsed and discarded before; the AST
  `ColumnConstraint`/`TableConstraintBody` variants now carry `on_conflict:
  Option<ConflictAction>`). The schema propagates the per-constraint OE to `Column.notnull_oe`
  (for NOT NULL on PK columns of WITHOUT ROWID tables) and to the WITHOUT ROWID PK's UNIQUE
  constraint. At codegen time, the per-constraint OE overrides the statement-level
  `OR <action>` (mirroring upstream's `overrideError` rule: the per-constraint OE wins when it
  is not `OE_Default`; otherwise the statement's `OR <action>` applies). The executor gained
  an `oe_override` field on `Vdbe` that `HaltIfNull` (via p2) and `IdxInsert` (via p5 bits 4-7)
  set when a per-constraint OE fires; `step()` consumes it for the cleanup, mirroring
  upstream's `p->errorAction = pOp->p2` in `OP_Halt`. The `Halt` opcode now handles `p1 != 0`
  (error halt) — it builds the message from p4 + the p5 constraint-type prefix
  ("NOT NULL/UNIQUE/CHECK/FOREIGN KEY constraint failed: …") and sets `oe_override` from p2,
  matching upstream's `sqlite3HaltConstraint`. For OE_Ignore on NOT NULL, the codegen emits
  `OP_IsNull reg, row_skip` (mirrors upstream's `OP_IsNull iReg, ignoreDest`); for
  OE_Ignore/Replace on the WITHOUT ROWID PK UNIQUE, a `NoConflict` pre-check skips/replaces
  the conflicting row before the `IdxInsert`; for OE_Abort/Fail/Rollback on UNIQUE, the
  pre-check emits a `Halt` BEFORE the table `IdxInsert` so the failing row's partial writes
  are never made and prior rows in the same statement stay clean (mirrors upstream's "OE_Fail
  and OE_Ignore must happen before any changes are made" rule in
  `sqlite3GenerateConstraintChecks`). The rowid path's `emit_conflict_prechecks` now runs for
  all OEs (not just Ignore/Replace), with the ABORT/FAIL/ROLLBACK arm emitting a `Halt` before
  the table `Insert`. Differential-tested vs the C oracle (5 new cases in
  `write_roundtrip.rs`: ON CONFLICT IGNORE/FAIL/ROLLBACK/ABORT on WITHOUT ROWID PK, and ON
  CONFLICT IGNORE NOT NULL skips the row). Known gap: `OR FAIL` under autocommit now commits
  the successful rows (mirrors upstream's `sqlite3VdbeHalt` FAIL-commits semantics); the
  pre-existing `insert_or_fail_keeps_prior_rows` test was updated to verify this. CHECK
  constraint enforcement (the `ON CONFLICT` clause is captured but the CHECK itself is not
  evaluated yet — that's M19.8/M35.1).
- **M13 — WAL (Write-Ahead Logging)** 🚧: **13.1–13.3** ✅ (format codecs for the `-wal`
  header/frame and the `-shm` shared-memory index — see `format/wal.rs` and
  `format/wal_index.rs`). **13.4 WAL mode read path** ✅: `pager::wal::Wal` (mirroring
  `wal.c`) opens the `-wal` sidecar, recovers an in-memory wal-index by scanning every
  frame and verifying the running checksum + salt match (mirrors `walIndexRecover`), and
  answers page lookups via `find_frame` (mirrors `walFindFrame` — walks the hash tables
  newest-block-first, scanning the full hash chain within each block to find the highest
  frame for the page) + `read_frame` (mirrors `sqlite3WalReadFrameFrame` — reads page data
  at the computed WAL offset). The in-memory index is a `Vec<IndexBlock>` of
  `page_mapping: Vec<u32>` + `hash_table: Vec<u16>`, with the asymmetric first-block capacity
  (`HASHTABLE_NPAGE_ONE = 4062`) matching upstream. Recovery stops at the first frame that
  fails checksum or salt validation, and only frames up to and including the last *commit
  frame* (non-zero `commit_size`) are made visible to readers (`mxFrame` = last commit,
  `nPage` = its `commit_size`); the uncommitted tail is dropped, matching upstream's durable
  prefix rule. `Pager::open` detects WAL mode (`header.write_version == 2 ||
  header.read_version == 2`), constructs the `Wal`, recovers its index, and stores it in a
  new `Pager::wal: Option<Wal>` field; `Pager::get_page` consults `Wal::find_frame` before
  reading the database file, falling back to the file when the page is not in the WAL; and
  `Pager::page_count` reports the WAL's `n_page` (the durable size carried by the last
  commit frame) when the WAL is non-empty, since pages added in the WAL but not yet
  checkpointed extend the visible database. `Pager::create_fresh` sets `wal: None` (a fresh
  database starts in rollback-journal mode). The write path (appending frames to the WAL
  instead of journaling DB pages) is M13.5 — not yet implemented, so a WAL-mode database
  written by Rustqlite still goes through the rollback journal and is not crash-safe until
  a checkpoint copies the WAL into the DB file. Differential-tested vs the C oracle (7 new
  cases in `wal_read.rs`: uncheckpointed WAL reads committed rows from the `-wal` sidecar,
  post-checkpoint reads fall back to the DB file, multi-commit WAL exposes all committed
  rows, empty `-wal` (header only) falls back to the DB file, no `-wal` file falls back to
  the DB file, 200-row database reads across multiple b-tree leaf pages from the WAL, and
  `sqlite_schema` reads through the WAL). Known limitation: this is a read-only WAL reader;
  Rustqlite-written WAL-mode databases are not yet crash-safe (M13.5 write path pending).
  **13.5 WAL mode write path** ✅: `Wal::write_frames` (mirrors `walFrames` in `wal.c`) appends
  frames for a set of dirty pages to the `-wal` sidecar, then syncs (the durable commit
  point), then extends the in-memory wal-index with the new page numbers. On the first write
  to a fresh WAL (when `mx_frame == 0`), the WAL header is written first: magic
  `0x377f0683` (big-endian checksum), format `3007000`, the page size, fresh random salts, and
  the header checksum; the running checksum seeds from the header. Each frame's header
  carries the page number, the commit size (non-zero on the last frame of a transaction — the
  commit marker), the salts (copied from the WAL header), and the running checksum over the
  first 8 bytes of the frame header + the page data. After the frames are synced,
  `mx_frame`/`n_page`/`frame_cksum` are advanced to the new last commit frame. `Pager::begin_write`
  in WAL mode skips the rollback journal setup (a no-op in-memory `MemFile` placeholder
  satisfies the `WriteTxn` type) and `Pager::commit` branches on WAL mode: it collects the
  dirty pages in page-number order, calls `Wal::write_frames` with the frames + the new db
  page count, promotes the pages into the clean cache, and ends the transaction (the database
  file is NOT written — pages stay in the WAL until a checkpoint copies them into the DB
  file, M13.6). `Pager::journal_page` is a no-op in WAL mode (no pre-image replay is needed —
  rollback discards the dirty overlay, and uncommitted frames are never written to the WAL).
  `Pager::rollback` in WAL mode just discards the dirty overlay (no frames were written to
  the WAL — frames are only written at commit). The `Wal` file handle is `Arc<dyn VfsFile>`
  (shared via `file_clone`) so `get_page` can read a frame without holding the `Wal` mutex
  across an `await` (which would make the future `!Send`); `frame_data_offset` exposes the
  WAL offset for the same reason. Differential-tested vs the C oracle (3 new cases in
  `wal_read.rs`: Rustqlite writes to a C-SQLite-created WAL-mode DB and the C oracle reads the
  rows back via WAL recovery; Rustqlite writes then reads back in a fresh connection; the C
  oracle's `PRAGMA integrity_check` passes on a Rustqlite-written WAL-mode database). Known
  limitation: `PRAGMA journal_mode = wal` (switching from rollback to WAL mode) is M13.10 —
  the database must already be in WAL mode (e.g. created by C SQLite) for the WAL write path
  to engage; `create_fresh` still starts in rollback-journal mode.
  **13.6–13.7 WAL checkpoint + `OP_Checkpoint`** ✅: `Wal::checkpoint` (mirroring `walCheckpoint`
  + `sqlite3WalCheckpoint` in `wal.c`) copies the committed frames from the `-wal` sidecar
  back into the database file, truncates the DB to the committed `n_page`, syncs it (the
  durable checkpoint commit point), then optionally resets the WAL: `Passive`/`Full` leave
  the WAL in place; `Restart` writes new salts to the WAL header and resets `mx_frame = 0` so
  the next writer starts a fresh log; `Truncate` does the same and truncates the `-wal` file
  to zero bytes (matching `walRestartHdr` + `sqlite3OsTruncate(pWal->pWalFd, 0)`). `Pager::checkpoint`
  drives it, passing the pager's DB file handle so frames land at the right offset. The new
  `OP_Checkpoint p1 p2 p3` opcode (mirrors `OP_Checkpoint` in `vdbe.c`) runs the checkpoint
  in mode `p2` (0=PASSIVE, 1=FULL, 2=RESTART, 3=TRUNCATE — the `SQLITE_CHECKPOINT_*` constants)
  and writes three result registers at `r[p3..p3+3]`: `r[p3]` = busy flag (0/1), `r[p3+1]` =
  `pnLog` (frames in the WAL), `r[p3+2]` = `pnCkpt` (frames backfilled). The `PRAGMA
  wal_checkpoint [ = passive|full|restart|truncate ]` handler (mirrors `PragTyp_WAL_CHECKPOINT`
  in `pragma.c`) runs the checkpoint synchronously and returns one result row of three
  columns (`busy`, `log`, `checkpointed`) — the same shape as the C oracle. A no-op
  `(0, 0, 0)` is returned when the database is not in WAL mode or the WAL is empty. The
  `CheckpointMode` enum + `CheckpointMode::from_name` parse the mode argument
  case-insensitively, matching upstream's `sqlite3StrICmp` ladder; an unknown name falls
  through to PASSIVE (the default). Differential-tested vs the C oracle (5 new cases in
  `wal_read.rs`: TRUNCATE/PASSIVE/RESTART/default-PASSIVE checkpoints copy frames into the DB
  file and the C oracle reads them back + `PRAGMA integrity_check` passes; `wal_checkpoint`
  on a rollback-mode database is a no-op `(0, 0, 0)`; and writes after a TRUNCATE checkpoint
  append fresh frames to the truncated WAL with the new salts — the WAL restart works).
- **M14 — ALTER TABLE** 🚧: **14.1–14.4** ✅ (parser shipped in M2.25–M2.28). **14.5
  `RENAME TO`** ✅: `codegen::alter::compile_alter_rename_table` (mirrors
  `sqlite3AlterRenameTable` in `alter.c`) opens a write cursor on `sqlite_schema` (page 1)
  and, for the table row + every associated index/trigger row whose `tbl_name` matches the
  old name, reads the 5 columns, overwrites the `name`/`tbl_name`/`sql` fields, deletes the
  old row, and inserts the new record at the same rowid (`Insert` does not overwrite in our
  b-tree, so `Delete`+`Insert` is the in-place update shape). The `sql` column is rewritten
  by `rewrite_table_name_in_sql` — a textual splice of the table-name token in the stored
  CREATE TABLE / CREATE INDEX / CREATE TRIGGER text, preserving the original quoting style
  (SQLite uses an AST-aware rewrite via the `sqlite_rename_table` SQL function; this slice
  approximates it with a targeted text substitution at the table-name position). A
  `dequote_ident` helper (mirrors `sqlite3Dequote`) is added to the schema catalog and used
  in `find_table`/`find_index`/`find_index_for_column` so lookups against a stored name like
  `"My Table"` (the parser keeps quote characters in identifier strings) match both
  `"My Table"` and `My Table`. The ALTER TABLE resolver (`resolve_alter_target` in
  `capi::stmt`) dequotes the new name before storing it in the `name`/`tbl_name` columns
  (SQLite stores the dequoted form there), rejects renaming system tables (`sqlite_*`),
  rejects the new name colliding with an existing table or index, and rejects reserved-name
  targets. Differential-tested vs the C oracle (`alter_table_rename_to_*` in
  `write_roundtrip.rs` — basic rename, rename with index, quoted names, data preservation,
  nonexistent-table error, name-collision error). C-SQLite `PRAGMA integrity_check` passes
  on Rustqlite-written renamed databases. Still M14+: 14.6 `ADD COLUMN`, 14.7
  `DROP COLUMN`, 14.8 `RENAME COLUMN`, 14.9 `PRAGMA legacy_alter_table`, 14.10
  `ALTER COLUMN … DROP/SET NOT NULL`. Known gap: the `sql` rewrite is text-based, not
  AST-aware, so FK references inside the CREATE TABLE text (e.g. `REFERENCES old_name`)
  are not rewritten — matches `legacy_alter_table=ON` behavior; the AST-aware rewrite
  lands with the full `parse.y` port.
  **14.6 `ADD COLUMN`** ✅: `codegen::alter::compile_alter_add_column` (mirrors
  `sqlite3AlterFinishAddColumn` in `alter.c`) rewrites the table's `sqlite_schema` row to
  include the new column in the CREATE TABLE text. The `sql` column is rewritten by
  `splice_column_into_create_table` — a paren-depth-aware splice of `, <col_def_text>`
  before the closing `)` of the column list (handles `VARCHAR(10)` nested parens and
  `WITHOUT ROWID` suffixes). The column-definition text is extracted from the user's
  original ALTER TABLE statement by `extract_add_column_text` (finds `ADD [COLUMN]` and
  takes the trimmed rest). The resolver (`resolve_alter_add_column_target` in `capi::stmt`)
  validates the column is legal for ADD COLUMN via `validate_add_column`: rejects
  `PRIMARY KEY` ("Cannot add a PRIMARY KEY column"), `UNIQUE` ("Cannot add a UNIQUE
  column"), and `NOT NULL` without a non-NULL default ("Cannot add a NOT NULL column with
  default value NULL"). Existing rows in the table b-tree are NOT rewritten — they read
  the new column as NULL (the `Column` opcode returns NULL for indices beyond the record's
  length); SQLite applies the DEFAULT on read for existing rows, but our engine does not
  yet model column defaults on read (M35.3), so a non-NULL default diverges for existing
  rows (documented in the test). New INSERTs that don't specify the new column also get
  NULL (the current engine behavior — column DEFAULTs are not modeled at INSERT time for
  unlisted columns). Differential-tested vs the C oracle (`alter_table_add_column_*` in
  `write_roundtrip.rs` — basic add, add with default, multiple adds, add with `COLUMN`
  keyword, NOT NULL without default error, PRIMARY KEY error).
  **14.7 `DROP COLUMN`** ✅: `codegen::alter::compile_alter_drop_column` (mirrors
  `sqlite3AlterDropColumn` in `alter.c`) rewrites the table's `sqlite_schema` row to remove
  the column from the CREATE TABLE text (via `drop_column_from_create_table` — a paren-
  depth-aware segment splitter that finds the column-def segment by name and removes it
  with its leading/trailing comma), AND rewrites every existing row in the table b-tree to
  remove the dropped column's value, using the same two-pass sorter-as-rowset approach as
  UPDATE (first pass captures rowids into a sorter, second pass re-seeks each rowid, reads
  all columns except the dropped one, builds a reduced record, deletes the old row, and
  inserts the new one at the same rowid). The resolver (`resolve_alter_drop_column_target`
  in `capi::stmt`) validates the column can be dropped via `validate_drop_column`: rejects
  dropping a PRIMARY KEY column ("cannot drop PRIMARY KEY column"), rejects dropping the
  only column ("cannot drop column: no other columns exist"), and rejects dropping a
  nonexistent column ("no such column"). Dependent indexes (those referencing the dropped
  column) are NOT automatically dropped in this slice — the user must drop them first or
  the index will reference a missing column (a known gap; upstream drops and rebuilds
  dependent indexes). Differential-tested vs the C oracle
  (`alter_table_drop_column_*` in `write_roundtrip.rs` — drop middle/first/last column,
  multiple rows, nonexistent/PK/only-column errors). C-SQLite `PRAGMA integrity_check`
  passes on Rustqlite-written databases.
  **14.8 `RENAME COLUMN`** ✅: `codegen::alter::compile_alter_rename_column` (mirrors
  `sqlite3AlterRenameColumn` in `alter.c`) rewrites the `sql` column of the table's
  `sqlite_schema` row and every associated index/trigger row whose `sql` references the
  column. The rewrite is done by `rewrite_column_name_in_sql` — a textual whole-word
  replacement of the old column name with the new one, handling quoted identifiers
  (`"..."`, `` `...` ``, `[...]`) and bare identifiers, and skipping string literals
  (SQLite uses an AST-aware rewrite via the `sqlite_rename_column()` SQL function; our
  textual approach may over-replace when the column name collides with another
  identifier, but handles the common cases correctly). The resolver
  (`resolve_alter_rename_column_target`) validates the column exists and the new name
  doesn't collide with an existing column. Differential-tested vs the C oracle
  (`alter_table_rename_column_*` — basic rename, without `COLUMN` keyword, with index,
  nonexistent/collision errors).
- **M15 — Views** 🚧: **15.1–15.2** ✅ (parser shipped in M2.29/M2.30). **15.3 `CREATE
  VIEW`** ✅ / **15.4 `DROP VIEW`** ✅: `codegen::view::compile_create_view` (mirrors
  `sqlite3CreateView` in `build.c`) writes a `sqlite_schema` row with `type='view'`,
  `rootpage=0`, and the verbatim `CREATE VIEW` text; `compile_drop_view` removes the row.
  The `Catalog` gained `find_view` and `find_object` helpers (dequoted, case-insensitive)
  so the resolver can check for name collisions across tables, views, and indexes, and so
  `DROP VIEW` can resolve the view's rowid. `IF NOT EXISTS` against a pre-existing view is
  a no-op; `IF EXISTS` against a missing view is a no-op. Differential-tested vs the C
  oracle (`create_view_*` / `drop_view_*` in `write_roundtrip.rs` — schema row, IF NOT
  EXISTS, collision error, DROP VIEW, IF EXISTS, nonexistent error). Still M15+: 15.5
  view expansion (substituting a view's SELECT body when it appears in FROM), 15.6
  `sqlite_master` alias, 15.7 `INSTEAD OF` triggers (depends on M16).
- **M16 — Triggers** 🚧: **16.1–16.6** ✅ (parser shipped in M2.31/M2.32). **16.7 `CREATE
  TRIGGER`** ✅ / **16.8 `DROP TRIGGER`** ✅: `codegen::trigger::compile_create_trigger`
  (mirrors `sqlite3CreateTrigger` in `build.c`/`trigger.c`) writes a `sqlite_schema` row
  with `type='trigger'`, `tbl_name=<table>`, `rootpage=0`, and the verbatim CREATE TRIGGER
  text; `compile_drop_trigger` removes the row. The resolver validates the target table
  exists, rejects name collisions, and handles `IF NOT EXISTS`/`IF EXISTS`. **16.10
  `OP_Program`** ✅ / **16.11 `OP_Param`** ✅ (already implemented in M8.10/M8.11 — the
  runtime infrastructure for trigger sub-VDBE execution). Still M16+: 16.9 trigger firing
  (detecting triggers on the target table, compiling each trigger body as a sub-VDBE,
  invoking it via `OP_Program` with `OLD`/`NEW` row registers), 16.12 `OLD`/`NEW`
  references, 16.13 `RAISE(IGNORE)`, 16.14 `RAISE(ROLLBACK/ABORT/FAIL)`, 16.15
  `PRAGMA recursive_triggers`, 16.16 dedicated `Trigger`/`DropTrigger` opcodes (the DDL
  path uses direct `sqlite_schema` row manipulation). Differential-tested vs the C oracle
  (`create_trigger_*` / `drop_trigger_*` — schema row, IF NOT EXISTS, collision error,
  nonexistent-table error, DROP TRIGGER, IF EXISTS, nonexistent error).

- **M17 — Foreign Keys** 🚧: **17.1–17.2** ✅ (parser shipped in M2.44 — column-level
  `REFERENCES` and table-level `FOREIGN KEY (cols) REFERENCES` with `ON DELETE|UPDATE
  action` and `DEFERRABLE`). **17.3 `PRAGMA foreign_keys`** ✅ / **17.4
  `PRAGMA foreign_key_list(tbl)`** ✅: `PRAGMA foreign_keys` is a FLAG-pragma (mirrors
  `PragTyp_FLAG` in `pragma.c` with the `SQLITE_ForeignKeys` mask) — read returns 0/1
  (default OFF, matching upstream without `SQLITE_DEFAULT_FOREIGN_KEYS`); set parses the
  value via `sqlite3GetBoolean` (`on`/`yes`/`true`/non-zero number → ON; `off`/`no`/
  `false`/0 → OFF) and updates a per-connection `foreign_keys: Arc<Mutex<bool>>` flag
  on `Sqlite3`. The toggle is silently dropped inside a transaction (upstream masks
  `SQLITE_ForeignKeys` out of the FLAG-pragma mask when `db->autoCommit == 0`); we
  mirror by checking `db.autocommit()`. The parser was extended to accept `TRUE`/`FALSE`
  as `pragma_kw_value` (mapped to `Ident("true")`/`Ident("false")` so `sqlite3GetBoolean`'s
  string match still applies). `PRAGMA foreign_key_list(tbl)` parses the table's stored
  CREATE TABLE text, walks column-level `REFERENCES` and table-level `FOREIGN KEY`
  constraints, and emits one row per (constraint, column) with 8 columns: `id`, `seq`,
  `table`, `from`, `to`, `on_update`, `on_delete`, `match` (always "NONE" — upstream
  accepts but ignores `MATCH`). The constraint order is REVERSE of declaration (upstream's
  `sqlite3AddForeignKey` prepends each FK to `pTab->u.tab.pFKey`'s singly-linked list;
  `PragTyp_FOREIGN_KEY_LIST` walks via `pNextFrom`); we collect in declaration order then
  reverse and assign `id` = 0-based position in the walked list. `to` is NULL when the
  constraint doesn't name a parent column (the parent's PK is referenced). Enforcement
  itself (M17.6+) is deferred — this slice is the read/write flag and the introspection
  pragma. Differential-tested vs the C oracle (`foreign_keys.rs`: default-off + toggle,
  silent-no-op inside a transaction, `foreign_key_list` for column-level + table-level +
  mixed + multi-column + no-FK + missing-table). **17.5 `PRAGMA foreign_key_check`** ✅:
  `PRAGMA foreign_key_check` / `PRAGMA foreign_key_check(table-name)` checks the database (or
  the named table) for FK violations, returning one row per violation with four columns
  `table, rowid, parent, fkid` (mirrors `PragTyp_FOREIGN_KEY_CHECK` in `pragma.c`). The
  backend (`btree/foreign_key_check.rs`) reads the catalog, parses each child table's CREATE
  TABLE to extract `REFERENCES`/`FOREIGN KEY` constraints, and for each FK pre-resolves a
  lookup strategy: `RowidSeek` (single-column FK referencing the parent's `INTEGER PRIMARY KEY`
  rowid alias — probes via `TableCursor::seek_rowid`), `IndexSeek` (a covering index on the
  parent — probes via `IndexCursor::seek(SeekOp::Ge)` + prefix equality), or `TableScan`
  (no usable index — full parent-table scan). A child row with any NULL FK column is skipped
  (NULL foreign keys never violate, matching upstream's `OP_IsNull → addrOk` early-out). A
  dangling parent (the referenced table doesn't exist) reports every non-NULL child row as a
  violation (matching upstream's second loop where `pParent == 0` leaves no `addrOk` jump).
  The `fkid` is the 0-based FK index on the child table. WITHOUT ROWID child tables are
  deferred (their rowid column would be NULL — the M5.3 WITHOUT ROWID write-path follow-up).
  Differential-tested vs the C oracle (`foreign_keys.rs`: no-FK empty, satisfied empty,
  rowid-FK violation, filtered-to-table, NULL child skipped, multi-column FK, dangling
  parent reported, indexed parent, missing-table error, empty DB). Known gap: the
  `TableScan` fallback uses BINARY collation for parent-column comparison rather than the
  parent column's declared collation — correct for the common case (numeric/BINARY-text FKs)
  but may diverge for `COLLATE NOCASE`/`RTRIM` parent columns; the `IndexSeek` path honors
  per-column collation via the index's `KeyInfo`. **17.6 FK enforcement on INSERT** ✅:
  `INSERT INTO child ...` with `PRAGMA foreign_keys = ON` verifies each FK constraint before
  the table `Insert`. The prepare path (`capi::stmt`) calls
  `btree::foreign_key_check::resolve_fk_constraints` to parse the child's CREATE TABLE,
  extract each `REFERENCES`/`FOREIGN KEY` constraint, resolve the parent table + covering
  index (or rowid-alias path), and build a `Vec<FkCheckP4>` threaded into `compile_insert`.
  The codegen emits a new `OP_FkCheck p1 p2 p3 P4=FkCheck` per FK per row (after
  `emit_conflict_prechecks`, before `MakeRecord`): `p1` is the child-key start register,
  `p2` is the violation label, `p3` is the 0-based FK index. The executor (`vdbe::exec`)
  reads the child-key registers, skips when any is NULL (NULL foreign keys never violate),
  and calls `fk_parent_exists` which replays the `FkLookup` strategy (RowidSeek /
  IndexSeek / TableScan / ParentMissing — the same strategies as `foreign_key_check`). When
  the parent is missing, the codegen's violation handler emits a `Halt` with
  `p1=Constraint`, `p2=oe`, `p4="child.col"`, `p5=4` (the "FOREIGN KEY constraint failed"
  prefix, already wired in the `Halt` arm). `OE_Ignore` jumps to `row_skip` instead (the
  row is silently dropped, matching `INSERT OR IGNORE`). When `PRAGMA foreign_keys = OFF`
  (the default), no FK checks are emitted (matching upstream's `db->flags &
  SQLITE_ForeignKeys` guard). Differential-tested vs the C oracle (`foreign_keys.rs`:
  valid insert succeeds, invalid insert fails with FK constraint error, NULL child
  allowed, `OR IGNORE` skips the row, multi-column FK enforced). Known limitation: an FK
  on the rowid-alias column (`x INTEGER PRIMARY KEY REFERENCES parent`) copies the record
  slot (which is NULL for the rowid alias) rather than the rowid register — the fix is to
  thread `rowid_reg` into `emit_fk_checks`; the common `x INTEGER REFERENCES parent(id)`
  case works because `x` is a regular column, not the rowid alias.

- **M18 — INSERT Enhancements** 🚧: **18.3 UPSERT** ✅ (initial slice):
  `ON CONFLICT [(cols)] DO NOTHING` and `ON CONFLICT (cols) DO UPDATE SET ... [WHERE ...]`
  for rowid tables are implemented by `codegen::upsert` (mirrors `upsert.c`'s
  `sqlite3UpsertAnalyzeTarget` + `sqlite3UpsertDoUpdate`). The `OeAction` enum gained an
  `Update` variant (mirrors `OE_Update`). `codegen::upsert::resolve_target` matches a
  conflict target to a unique index (or `MatchedIndex::Rowid` for the INTEGER PRIMARY KEY
  column), raising "ON CONFLICT clause does not match any PRIMARY KEY or UNIQUE
  constraint" when no match is found. For DO NOTHING, a `NoConflict` probe (or
  `NotExists` for the IPK) jumps to `row_skip` on conflict. For DO UPDATE, on conflict
  the codegen fetches the conflicting row's rowid via `IdxRowid`, seeks the table cursor,
  reads the existing row's columns into a register block, applies the WHERE filter
  (false → skip the update), evaluates the SET assignments (with `excluded.col` resolving
  to the new row's record registers and bare `col` resolving to the existing row's
  columns), then Delete+Insert at the same rowid (carrying `P5_ISUPDATE` so `changes`
  bumps once per updated row), and re-syncs index entries (IdxDelete old + IdxInsert new,
  also `P5_ISUPDATE`). The matched index is skipped in the generic
  `emit_conflict_prechecks` so its default OE doesn't double-fire. `ON CONFLICT DO
  NOTHING` without a target uses INSERT OR IGNORE semantics (every unique constraint
  resolves to `OE_Ignore`). `ON CONFLICT DO UPDATE` without a target probes the first
  unique constraint (IPK if present, else the first unique index) and runs the DO UPDATE
  body on conflict; all unique indexes are skipped in the generic prechecks so their
  default OE doesn't double-fire. UPSERT on the rowid-alias column (SET rowid = ...) is
  rejected (rows are not moved via UPSERT in this slice). Differential-tested vs the C
  oracle (`upsert_*` in `write_roundtrip.rs`: DO NOTHING with/without target, DO UPDATE
  with/without target, WHERE filter, bare-column vs `excluded.col` resolution,
  secondary-index maintenance, unmatched target error, INTEGER PRIMARY KEY target,
  multi-row mixed insert+update).

- **M19 — DELETE / UPDATE Enhancements** 🚧: **19.1** `DELETE … ORDER BY … LIMIT …` ✅,
  **19.2** `UPDATE … ORDER BY … LIMIT …` ✅, **19.3** `UPDATE … FROM from_clause` ✅
  (SQLite 3.33+), **19.4** `RETURNING` on DELETE ✅, **19.5** `RETURNING` on UPDATE ✅,
  **19.6** `OR IGNORE`/`OR REPLACE`/`OR ROLLBACK`/`OR ABORT`/`OR FAIL` on UPDATE ✅,
  **19.10** `UNIQUE` on UPDATE ✅. **19.7 `UPDATE` of `INTEGER PRIMARY KEY`** ✅: the
  rowid-alias column (`SET <ipk-col> = <expr>`) is now supported. The codegen detects
  `chng_rowid` (the SET list targets the `rowid_alias` column index) and, in the second
  pass (the sorter-as-rowset update loop), evaluates the SET expression into a dedicated
  `reg_new_rowid` register with INTEGER affinity + the new `OP_MustBeInt` opcode (a faithful
  port of `OP_MustBeInt` in `vdbe.c`: applies NUMERIC affinity, then raises `SQLITE_MISMATCH`
  when the value isn't an integer — a NULL value raises MISMATCH, matching the oracle's
  `UPDATE t SET id = NULL` → "datatype mismatch", unlike `INSERT INTO t VALUES (NULL,…)`
  which auto-assigns; a non-numeric string or a real with a fraction also raises MISMATCH).
  The rowid-alias slot in the stored record is set to NULL (the rowid is carried in the
  register, same as INSERT). A uniqueness pre-check on the new rowid uses `OP_NotExists`:
  when the new rowid equals the old rowid (an `OP_Eq` guard), the check is skipped (a
  self-assign like `UPDATE t SET id = 1 WHERE v='a'` on a row whose id is already 1 is a
  no-op for the rowid — the row doesn't move and there's no conflict); otherwise, a found
  row with the new rowid is a UNIQUE constraint violation on the IPK (`UNIQUE constraint
  failed: <tbl>.<ipk-col>`) handled per the statement-level `OR <action>`: IGNORE jumps to
  `sort_next`, REPLACE deletes the conflicting row + its index entries then falls through,
  ABORT/FAIL/ROLLBACK halt before any writes. The table `Insert` and the NEW index keys use
  `reg_new_rowid` as the rowid (instead of `reg_old_rowid2`), so the row moves within the
  b-tree and the index entries point to the new rowid. `char_to_aff` was fixed to recognize
  the canonical `SQLITE_AFF_*` letters (`A`=BLOB, `B`=TEXT, `C`=NUMERIC, `D`=INTEGER,
  `E`=REAL) — the `Affinity` opcode's INTEGER coercion ('D') was previously a no-op
  (mapped to BLOB), which is why the INSERT path's `apply_affinity(.., Integer)` appeared
  to work despite never coercing (MustBeInt isn't used on the INSERT rowid-alias path;
  `NewRowid` handles NULL there). Differential-tested vs the C oracle
  (`update_rowid_alias_matches_oracle` in `write_roundtrip.rs`: move to a new rowid, id
  arithmetic, self-assign, negative rowid, multi-column SET with the rowid-alias, NULL /
  non-numeric / real-with-fraction / duplicate-rowid error cases — all match the oracle's
  result code and message, and `PRAGMA integrity_check` passes on Rustqlite-written
  databases). Still M19+: 19.8 `CHECK` constraint evaluation, 19.9 `NOT NULL`
  enforcement. The `UPDATE ... FROM` path still rejects rowid-alias SET (staging the new
  rowid through the sorter's set-value columns is a follow-up).
