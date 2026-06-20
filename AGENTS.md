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
- @docs/without-rowid-storage.md - WITHOUT ROWID table on-disk layout and `convertToWithoutRowidTable` shape
- @docs/row-value-expressions.md - SQLite `TK_VECTOR` grammar, row-value comparisons, and `IN (subquery)` forms
- @docs/autovacuum-ptrmap.md - auto-vacuum/ptrmap page math, header meta[] layout, and Rustqlite implementation notes

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
  Most M2 tasks (2.1–2.72) are now done; remaining: 2.73 AST walker, 2.74 name resolution. Note:
  `build_qualified_name`
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
