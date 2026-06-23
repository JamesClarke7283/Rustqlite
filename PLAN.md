# Rustqlite — Roadmap to SQLite3 Feature Parity

Comprehensive task list tracking all remaining work to reach full feature parity
with SQLite 3.53.1. Completed milestones are collapsed; active/future milestones
list every granular item needed.

---

## M0 — Scaffold ✅

- [x] **0.1** Workspace with three crates (parser, core, CLI)
- [x] **0.2** Version pin (`VERSION` file), CI, AGENTS.md
- [x] **0.3** `sqlite3_libversion*` / `sqlite3_sourceid` C-API stubs

---

## M1 — File Format (Read) ✅

- [x] **1.1** Varint codec (`format/varint.rs`)
- [x] **1.2** Serial-type codec (`format/serial_type.rs`)
- [x] **1.3** Record encode/decode (`format/record.rs`)
- [x] **1.4** 100-byte database header (`format/header.rs`)
- [x] **1.5** Async VFS (`MemVfs` + `OsTokioVfs`)
- [x] **1.6** Read-only pager (page cache, clean-page reads)
- [x] **1.7** Table b-tree read cursor with overflow chains
- [x] **1.8** `sqlite_schema` reader → `Catalog`
- [x] **1.9** CLI `.tables` / `.schema` reading C-SQLite databases

---

## M2 — Parser (Working Subset) 🚧

- [x] **2.1** PEG grammar (pest) for SELECT / CREATE TABLE / INSERT / DELETE / DROP TABLE / UPDATE / CREATE INDEX / DROP INDEX
- [x] **2.2** Full expression precedence (PrattParser: OR → AND → comparison → IS/LIKE/GLOB → concat → add/sub → mul/div/mod → unary)
- [x] **2.3** Literals: NULL, Integer, Real, Text, Blob, Bool, hex integers, bind parameters (?/:name/@name/$name)
- [x] **2.4** EXPLAIN / EXPLAIN QUERY PLAN prefix

- [x] **2.5** Bare-integer-literal edge case: `−9223372036854775808` must be INTEGER, not REAL
- [x] **2.6** Expression: `BETWEEN … AND …` / `NOT BETWEEN`
- [x] **2.7** Expression: `IN (value_list)` / `IN (SELECT …)` / `NOT IN`
- [x] **2.8** Expression: `EXISTS (SELECT …)`
- [x] **2.9** Expression: `CAST(expr AS type)`
- [x] **2.10** Expression: `CASE … WHEN … THEN … ELSE … END`
- [x] **2.11** Expression: `COLLATE` clause on expressions
- [x] **2.12** Expression: Subqueries in expressions (scalar `(SELECT …)`)
- [x] **2.13** Expression: `IS NOT DISTINCT FROM` / `IS DISTINCT FROM`
- [x] **2.14** Bitwise operators: `&`, `|`, `<<`, `>>`, `~`
- [x] **2.15** JSON operators: `->`, `->>`
- [x] **2.16** Compound SELECT: `UNION`, `UNION ALL`, `INTERSECT`, `EXCEPT`
- [x] **2.17** JOIN syntax: `[INNER|CROSS|LEFT|RIGHT|FULL] [NATURAL] JOIN … ON/USING`
- [x] **2.18** CTEs: `WITH [RECURSIVE] name AS (…) SELECT …`
- [x] **2.19** `SELECT … FROM (subquery) AS alias`
- [x] **2.20** `VALUES (expr_list) [, …]` as a select body
- [x] **2.21** `INSERT … SELECT` (read-path INSERT from query)
- [x] **2.22** `INSERT … DEFAULT VALUES`
- [x] **2.23** UPSERT: `ON CONFLICT [(cols)] DO UPDATE SET … / DO NOTHING`
- [x] **2.24** `RETURNING` clause (INSERT / UPDATE / DELETE)
- [x] **2.25** `ALTER TABLE … RENAME TO …`
- [x] **2.26** `ALTER TABLE … ADD [COLUMN] …`
- [x] **2.27** `ALTER TABLE … DROP COLUMN …`
- [x] **2.28** `ALTER TABLE … RENAME COLUMN … TO …`
- [x] **2.29** `CREATE VIEW … AS SELECT …`
- [x] **2.30** `DROP VIEW …`
- [x] **2.31** `CREATE TRIGGER …`
- [x] **2.32** `DROP TRIGGER …`
- [x] **2.33** `PRAGMA name [= value] | (value)`
- [x] **2.34** `BEGIN [DEFERRED|IMMEDIATE|EXCLUSIVE] [TRANSACTION]`
- [x] **2.35** `COMMIT` / `END`
- [x] **2.36** `ROLLBACK [TO SAVEPOINT]`
- [x] **2.37** `SAVEPOINT name` / `RELEASE [SAVEPOINT] name`
- [x] **2.38** `ATTACH [DATABASE] expr AS expr [KEY expr]`
- [x] **2.39** `DETACH [DATABASE] expr`
- [x] **2.40** `VACUUM [INTO expr]`
- [x] **2.41** `ANALYZE [schema.]table_or_index`
- [x] **2.42** `REINDEX [schema.]name`
- [x] **2.43** `CREATE VIRTUAL TABLE … USING module …`
- [x] **2.44** Table constraints: `PRIMARY KEY(cols)`, `UNIQUE(cols)`, `CHECK(expr)`, `FOREIGN KEY(cols) REFERENCES …`
- [x] **2.45** Multi-column `CREATE INDEX … ON tbl(col1, col2, …)`
- [x] **2.46** Partial indexes: `CREATE INDEX … WHERE expr`
- [x] **2.47** Expression indexes: `CREATE INDEX … ON tbl(expr)`
- [x] **2.48** `WITHOUT ROWID` tables
- [x] **2.49** `STRICT` tables
- [x] **2.50** `GENERATED ALWAYS AS (expr) [STORED|VIRTUAL]` columns
- [x] **2.51** `DELETE … ORDER BY … LIMIT …`
- [x] **2.52** `UPDATE … ORDER BY … LIMIT …`
- [x] **2.53** `UPDATE … FROM from_clause …`
- [x] **2.54** `INDEXED BY` / `NOT INDEXED` table hints
- [x] **2.55** `WINDOW … OVER (…) FILTER (WHERE …)` syntax
- [x] **2.56** `NULLS FIRST` / `NULLS LAST` in ORDER BY
- [x] **2.57** Column `DEFAULT expr` (non-constant defaults)
- [x] **2.58** `AUTOINCREMENT` column constraint parsing (runtime tracked separately as M18.7)

---

## M3a — Read Query Path (Single-Table SELECT) ✅

- [x] **3a.1** VDBE register machine with 57 opcodes
- [x] **3a.2** Code generator: projection, WHERE (3-valued logic), ORDER BY sorter, LIMIT/OFFSET
- [x] **3a.3** Value comparison + type affinity
- [x] **3a.4** Byte-faithful REAL→text formatter (`sqlite3FpDecode` port)
- [x] **3a.5** ~10 scalar functions
- [x] **3a.6** C-API prepare/step/column path

---

## M3b — Read-Path Completion ✅

- [x] **3b.1** EXPLAIN bytecode renderer (golden-tested)
- [x] **3b.2** EXPLAIN QUERY PLAN (oracle-matched detail strings)
- [x] **3b.3** Full scalar function set (string, math, misc, LIKE/GLOB)
- [x] **3b.4** All 12 shell output modes

---

## M4 — Write Path ✅

- [x] **4.1** Mutable pager + rollback journal + crash recovery
- [x] **4.2** B-tree page split + root promotion with overflow chains
- [x] **4.3** `CREATE TABLE` / `INSERT … VALUES` / `DELETE` / `DROP TABLE`
- [x] **4.4** sqllogictest harness wired

---

## M5.0 — UPDATE ✅

- [x] **5.0.1** Single-table UPDATE via two-pass sorter-as-rowset
- [x] **5.0.2** `Opcode::NotExists` + `TableCursor::seek_rowid`
- [x] **5.0.3** `P5_ISUPDATE` flag + `did_insert` tracker

---

## M5.1 — Single-Column Indexes ✅

- [x] **5.1.1** `CREATE [UNIQUE] INDEX … ON tbl(col)` / `DROP INDEX`
- [x] **5.1.2** Indexed equality `WHERE col = <const>` (SeekGE / IdxGT)
- [x] **5.1.3** Indexed equality + ORDER BY (seek-and-walk)
- [x] **5.1.4** Per-row index maintenance from INSERT/UPDATE/DELETE
- [x] **5.1.5** `IdxDelete` runs after WHERE check (non-matching rows don't drop entries)

---

## M5.2 — Index Page Splits & Multi-Column Indexes

- [x] **5.2.1** Index b-tree page split (`split_index_leaf` → propagate page-full correctly)
- [x] **5.2.2** Index b-tree interior-page traversal (insert into interior nodes)
- [x] **5.2.3** Multi-column `CREATE INDEX … ON tbl(col1, col2, …)` in parser
- [x] **5.2.4** Multi-column index record format (concatenated columns + rowid suffix)
- [x] **5.2.5** Multi-column index codegen: `IdxInsert` / `IdxDelete` with composite keys
- [x] **5.2.6** Multi-column index seek: prefix comparison for `WHERE col1 = ? AND col2 = ?`
- [x] **5.2.7** `KeyInfo` structure carrying collation sequence per column for sorter/index comparisons
- [x] **5.2.8** Enforce `UNIQUE` constraint at `IdxInsert` time (raise `SQLITE_CONSTRAINT_UNIQUE`)
- [x] **5.2.9** Partial indexes: `CREATE INDEX … WHERE expr` (parser + catalog + codegen filter)
- [x] **5.2.10** Expression indexes: `CREATE INDEX … ON tbl(expr)` (parser + catalog + codegen)
- [x] **5.2.11** `COLLATE` on index columns affects comparison in `IndexCursor`

---

## M5.3 — B-Tree Robustness & WITHOUT ROWID

- [x] **5.3.1** B-tree page merging on delete (when page is too empty, redistribute or merge with sibling)
- [x] **5.3.2** Interior-page balancing for inserts (propagate splits up to root)
- [x] **5.3.3** `Clear` opcode: fast delete of all rows in a b-tree (`DELETE FROM tbl` without WHERE)
- [x] **5.3.4** Freelist reuse: allocate pages from freelist before extending the file
- [x] **5.3.5** Freelist trunk/leaf page walking (read freelist pages for allocation)
- [x] **5.3.6** `WITHOUT ROWID` table b-trees (index-organized tables with primary key as the key)
- [x] **5.3.7** Auto-vacuum / ptrmap pages (`PRAGMA auto_vacuum = INCREMENTAL|FULL`)
- [x] **5.3.8** `PRAGMA integrity_check` backend (b-tree walk, overflow chain verification, freelist check)
- [x] **5.3.9** `Destroy` opcode: remove b-tree + add pages to freelist (already partial; ensure freelist pages are reusable)

---

## M6 — Aggregates, GROUP BY, DISTINCT

- [x] **6.1** VDBE: implement `AggStep` execution (accumulate per-group aggregate state)
- [x] **6.2** VDBE: implement `AggFinal` execution (finalize aggregate, write result register)
- [x] **6.3** Codegen: `GROUP BY` — sorter on group key + `AggStep` per row + `AggFinal` per group
- [x] **6.4** Codegen: `HAVING` — filter after `AggFinal`
- [x] **6.5** Aggregate functions: `count(*)`, `count(expr)`, `sum(expr)`, `total(expr)`, `avg(expr)`, `min(expr)`, `max(expr)`, `group_concat(expr [, sep])`
- [x] **6.6** `SELECT DISTINCT` — ephemeral sorter/b-tree deduplication (`OpenEphemeral` + `Found`/`NotFound`)
- [x] **6.7** Codegen: aggregate without GROUP BY (single-row result for `SELECT count(*) FROM t`)
- [x] **6.8** Codegen: `GROUP BY` + `ORDER BY` — two-pass (aggregate then sort result)
- [x] **6.9** `NULL` handling in aggregates (NULL group keys, NULL exclusion from `sum`/`avg`, `count(*)` vs `count(col)`)

---

## M7 — Joins

- [x] **7.1** Parser: `FROM` clause with `[INNER|CROSS|LEFT|RIGHT|FULL] [NATURAL] JOIN … ON expr / USING (cols)`
- [x] **7.2** VDBE: `OpenEphemeral` opcode — open an ephemeral b-tree for intermediate results
- [x] **7.3** VDBE: `NotFound` / `Found` opcodes — index existence check
- [x] **7.4** Codegen: cross join (cartesian product, nested loop)
- [x] **7.5** Codegen: inner join — nested-loop with `NotExists`/`Found` on inner table
- [x] **7.6** Codegen: left outer join — emit NULL row when inner match fails (`IfNullRow` opcode)
- [x] **7.7** VDBE: `NullRow` opcode — set cursor to all-NULL row for LEFT JOIN miss
- [x] **7.8** Right join (implemented as left join with swapped tables)
- [x] **7.9** Full outer join (left + right with NULL fill)
- [x] **7.10** Natural join: USING columns → deduped projection + coalesced ON condition
- [x] **7.11** Self-join: table aliases, `OpenDup` for same-table join
- [ ] **7.12** VDBE: `OpenDup` opcode — duplicate an existing cursor [BLOCKED: M8 infrastructure — OpenDup duplicates an ephemeral cursor for CTE/subquery/view materialization reuse (select.c:8074, window.c:1400); no current consumer in M7. Lands with M8/M10/M15.]
- [ ] **7.13** Query planner: join order selection (cost estimation based on row counts and available indexes) [BLOCKED: M22 infrastructure — faithful cost estimation needs ANALYZE/sqlite_stat1 row-count statistics; without it, any join-order choice would diverge from SQLite's planner. Lands with M22.]
- [x] **7.14** `USING (cols)` — coalesce matched columns, suppress duplicates in `SELECT *`

---

## M8 — Subqueries & Correlated Scans

- [x] **8.1** Parser: subquery in `FROM` clause `(SELECT …) AS alias`
- [x] **8.2** Parser: scalar subquery in expression `(SELECT …)`
- [x] **8.3** Parser: `EXISTS (SELECT …)`
- [x] **8.4** Parser: `IN (SELECT …)` / `NOT IN (SELECT …)`
- [x] **8.5** VDBE: coroutine opcodes — `InitCoroutine`, `EndCoroutine`, `Yield`
- [x] **8.6** Codegen: `FROM (subquery)` — materialize subquery into ephemeral table, then scan
- [x] **8.7** Codegen: scalar subquery in expressions — `Program` opcode or coroutine
- [x] **8.8** Codegen: `EXISTS (subquery)` — materialize, check if any row exists
- [x] **8.9** Codegen: `IN (subquery)` — ephemeral index or sorter for the subquery result set
- [x] **8.10** VDBE: `Program` opcode — execute a sub-VDBE program (for triggers, views)
- [x] **8.11** VDBE: `Param` opcode — pass outer-query values into correlated subqueries

---

## M9 — Compound SELECT

- [x] **9.1** Parser: `UNION`, `UNION ALL`, `INTERSECT`, `EXCEPT`
- [x] **9.2** Codegen: compound SELECT via ephemeral b-tree (merge-sort or dedup approach)
- [x] **9.3** `UNION ALL` — append all rows from each arm
- [x] **9.4** `UNION` — deduplicate rows across arms
- [x] **9.5** `INTERSECT` — keep only rows appearing in both arms
- [x] **9.6** `EXCEPT` — keep rows from first arm not appearing in second
- [x] **9.7** `ORDER BY` / `LIMIT` on compound result
- [x] **9.8** VDBE: `OpenEphemeral` with `Sorter` flag for compound dedup

---

## M10 — CTEs (Common Table Expressions)

- [x] **10.1** Parser: `WITH [RECURSIVE] name (cols) AS (SELECT …) SELECT …`
- [x] **10.2** Codegen: non-recursive CTE — materialize into ephemeral table, reference by name
- [x] **10.3** Codegen: recursive CTE — initial query → ephemeral table, then iterated recursive query until no new rows
- [x] **10.4** Multiple CTEs in a single `WITH` clause
- [x] **10.5** CTE column name resolution (explicit column list vs inferred from SELECT)

---

## M11 — Window Functions

- [x] **11.1** Parser: `OVER (PARTITION BY … ORDER BY … frame_spec)` and `FILTER (WHERE …)`
- [x] **11.2** Parser: named window definitions (`WINDOW w AS (…)`)
- [x] **11.3** VDBE: window-function accumulator state (`AggInverse`, `AggValue` opcodes)
- [x] **11.4** Built-in window functions: `row_number()`, `rank()`, `dense_rank()`, `percent_rank()`, `cume_dist()`, `ntile(N)`
- [x] **11.5** Built-in window functions: `first_value(expr)`, `last_value(expr)`, `nth_value(expr, N)`
- [x] **11.6** Built-in window functions: `lead(expr [, offset [, default]])`, `lag(expr [, offset [, default]])`
- [x] **11.7** Codegen: sort input by PARTITION BY + ORDER BY, then slide the frame
- [x] **11.8** Frame specification: `ROWS BETWEEN … AND …`, `RANGE BETWEEN … AND …`, `GROUPS BETWEEN … AND …` (first slice: ROWS-mode full-scan algorithm; RANGE/GROUPS with `UNBOUNDED PRECEDING`/`CURRENT ROW`/`UNBOUNDED FOLLOWING` bounds; explicit `expr PRECEDING`/`FOLLOWING` bounds handled in ROWS mode only — full RANGE/GROUPS `expr` bounds and EXCLUDE land in the M11.8 follow-up)
- [x] **11.9** Frame bounds: `UNBOUNDED PRECEDING`, `CURRENT ROW`, `expr PRECEDING/FOLLOWING` (ROWS mode only — full peer-group logic for RANGE/GROUPS `expr` bounds is the M11.8 follow-up)
- [ ] **11.10** `EXCLUDE` clause: `NO OTHERS`, `CURRENT ROW`, `GROUP`, `TIES` [BLOCKED: deferred — the codegen classifies EXCLUDE other than NO OTHERS as unsupported; implementation requires the sliding-frame `AggInverse` shape to remove rows from the frame mid-step, which lands with the streaming-3-cursor follow-up. Rejected with a specific error.]

---

## M12 — Transactions & Savepoints

- [x] **12.1** Parser: `BEGIN [DEFERRED|IMMEDIATE|EXCLUSIVE]`, `COMMIT`/`END`, `ROLLBACK [TO SAVEPOINT]` (shipped in M2.34–M2.37)
- [x] **12.2** Parser: `SAVEPOINT name`, `RELEASE [SAVEPOINT] name` (shipped in M2.36–M2.37)
- [x] **12.3** VDBE: `AutoCommit` opcode — toggle autocommit mode (BEGIN/COMMIT/END/ROLLBACK via `OP_AutoCommit`; the `autocommit` flag is shared between the connection and the VDBE so `OP_Halt` defers the commit when inside a `BEGIN`; `SAVEPOINT`/`RELEASE`/`ROLLBACK TO` are rejected at codegen time — the pager savepoint stack is M12.4/M12.5)
- [x] **12.4** VDBE: `Savepoint` opcode — create/release/rollback-to named savepoints
- [x] **12.5** Pager: nested savepoint support (save/restore page overlays per savepoint level)
- [x] **12.6** `Transaction` opcode: deferred vs immediate vs exclusive locking
- [x] **12.7** VFS: proper shared/exclusive lock escalation for IMMEDIATE/EXCLUSIVE transactions
- [x] **12.8** Conflict resolution: `OR ROLLBACK`, `OR FAIL`, `OR IGNORE`, `OR REPLACE` semantics (parser parses; codegen must enforce)
- [x] **12.9** `ON CONFLICT` column/table constraints (unique, not null, check) triggering constraint abort vs ignore

---

## M13 — WAL (Write-Ahead Logging)

- [x] **13.1** WAL file format: `-wal` header (magic, version, page size, checkpoint sequence, salt, checksum)
- [x] **13.2** WAL frame format: frame header (page number, commit size, salt) + page data + checksums
- [x] **13.3** `-shm` shared memory file (lock-page, WAL index header, hash tables)
- [x] **13.4** Pager: WAL mode read path (check WAL for page before reading DB file)
- [x] **13.5** Pager: WAL mode write path (append frames to WAL instead of journaling DB pages)
- [x] **13.6** WAL checkpoint: `PRAGMA wal_checkpoint` (PASSIVE, FULL, RESTART, TRUNCATE)
- [x] **13.7** VDBE: `Checkpoint` opcode
- [x] **13.8** VDBE: `JournalMode` opcode (switch between delete/wal/memory/off) [implemented via synchronous `Pager::set_journal_mode` from the `PRAGMA journal_mode` codegen, mirroring the existing `wal_checkpoint` pattern — a dedicated `OP_JournalMode` opcode is not emitted; the switch is performed inline at compile time]
- [x] **13.9** VFS: shared-memory `xShmMap`/`xShmLock`/`xShmBarrier`/`xShmUnmap` methods for WAL index
- [x] **13.10** `PRAGMA journal_mode` = wal / delete / memory / off / truncate / persist
- [x] **13.11** Recovery on open: read WAL frames, rebuild WAL index, apply uncommitted frames
- [ ] **13.12** Concurrent readers during WAL writes (MVCC via snapshot in WAL index) [BLOCKED: scope — requires per-reader snapshot state (aReadMark[]/WAL_READ_LOCK protocol), the writer backfill constraint (don't overwrite frames a reader still needs), and integration with the shm methods landed in M13.9. Multi-day feature touching the pager's get_page/begin_read/end_txn paths; deferred to a dedicated session to avoid destabilizing the working WAL read/write paths.]

---

## M14 — ALTER TABLE

- [x] **14.1** Parser: `ALTER TABLE … RENAME TO new_name`
- [x] **14.2** Parser: `ALTER TABLE … ADD [COLUMN] col_def`
- [x] **14.3** Parser: `ALTER TABLE … DROP COLUMN name`
- [x] **14.4** Parser: `ALTER TABLE … RENAME COLUMN old TO new`
- [x] **14.5** Codegen: `RENAME TABLE` — rewrite `sqlite_schema` row + update all FK/view/trigger references
- [x] **14.6** Codegen: `ADD COLUMN` — rewrite `sqlite_schema` CREATE TABLE SQL, default-fill new column in existing rows
- [x] **14.7** Codegen: `DROP COLUMN` — rewrite CREATE TABLE SQL, handle `sqlite_schema` update, rebuild dependent indexes
- [x] **14.8** Codegen: `RENAME COLUMN` — rewrite CREATE TABLE SQL + dependent indexes/views/triggers
- [ ] **14.9** `PRAGMA legacy_alter_table` (old behavior vs new behavior for whether dependent objects are rewritten) [BLOCKED: deferred — requires a connection-level pragma flag and the ALTER TABLE resolver to conditionally skip the `sql` rewrite when the flag is set. The main ALTER TABLE functionality (RENAME TABLE, ADD/DROP/RENAME COLUMN) is complete without it.]
- [ ] **14.10** `ALTER TABLE … ALTER COLUMN … DROP NOT NULL` / `SET NOT NULL` (3.37+) [BLOCKED: deferred — requires rewriting the column's NOT NULL constraint in the CREATE TABLE text and validating existing rows satisfy the new constraint. The parser support is already in place (M2.69/M2.70).]

---

## M15 — Views

- [x] **15.1** Parser: `CREATE [TEMP] VIEW [IF NOT EXISTS] name (cols) AS SELECT …`
- [x] **15.2** Parser: `DROP VIEW [IF EXISTS] name`
- [x] **15.3** Codegen: `CREATE VIEW` — write entry to `sqlite_schema` (type='view')
- [x] **15.4** Codegen: `DROP VIEW` — remove `sqlite_schema` entry + invalidate schema
- [ ] **15.5** View expansion: when a view appears in `FROM`, substitute its SELECT body (coroutine or materialization) [BLOCKED: deferred — requires intercepting FROM-clause resolution to detect view references, parsing the view's stored SELECT text, and substituting it as a subquery (similar to CTE expansion in M10). The catalog now has `find_view` (landed with M15.3/M15.4); the expansion logic is the remaining work.]
- [ ] **15.6** `sqlitemaster` view compatibility (`sqlite_master` vs `sqlite_schema` alias) [BLOCKED: deferred — `sqlite_master` is already accepted as an alias for `sqlite_schema` in the special-case read path; a proper implementation would register it as a view in the in-memory schema.]
- [ ] **15.7** `INSTEAD OF` triggers on views (depends on M18 triggers)

---

## M16 — Triggers

- [x] **16.1** Parser: `CREATE [TEMP] TRIGGER … BEFORE|AFTER|INSTEAD OF INSERT|UPDATE|DELETE ON tbl …`
- [x] **16.2** Parser: `DROP TRIGGER [IF EXISTS] name`
- [x] **16.3** Parser: trigger body (`BEGIN … END` with INSERT/UPDATE/DELETE/SELECT statements)
- [x] **16.4** Parser: `WHEN expr` trigger condition
- [x] **16.5** Parser: `FOR EACH ROW` clause
- [x] **16.6** Parser: `RAISE (IGNORE|ROLLBACK|ABORT|FAIL)` inside trigger body
- [x] **16.7** Codegen: `CREATE TRIGGER` — store trigger definition in `sqlite_schema` (type='trigger')
- [x] **16.8** Codegen: `DROP TRIGGER` — remove entry, invalidate schema
- [ ] **16.9** Trigger firing: before/after INSERT/UPDATE/DELETE, compile trigger body as sub-VDBE (`Program` opcode) [BLOCKED: deferred — requires the codegen to detect triggers on the target table, compile each trigger body as a sub-VDBE, and invoke it via `OP_Program` with `OLD`/`NEW` row registers passed via `OP_Param`. The `OP_Program`/`OP_Param` opcodes are already implemented (M8.10/M8.11); the trigger-firing codegen integration is the remaining work.]
- [x] **16.10** VDBE: `Program` opcode — execute sub-VDBE (trigger program) — already implemented in M8.10
- [x] **16.11** VDBE: `Param` opcode — pass NEW/OLD row references to trigger program — already implemented in M8.11
- [ ] **16.12** `OLD` and `NEW` row references inside trigger body [BLOCKED: deferred — part of trigger firing (M16.9)]
- [ ] **16.13** `RAISE(IGNORE)` — skip the current triggering statement row [BLOCKED: deferred — part of trigger firing (M16.9)]
- [ ] **16.14** `RAISE(ROLLBACK/ABORT/FAIL)` — raise constraint error [BLOCKED: deferred — part of trigger firing (M16.9)]
- [ ] **16.15** Recursive trigger guard (`PRAGMA recursive_triggers`) [BLOCKED: deferred — part of trigger firing (M16.9)]
- [ ] **16.16** VDBE: `Trigger` / `DropTrigger` opcodes [BLOCKED: deferred — the CREATE/DROP TRIGGER DDL is handled via direct sqlite_schema row manipulation (M16.7/M16.8); dedicated opcodes are not needed for the DDL path]

---

## M17 — Foreign Keys

- [x] **17.1** Parser: `REFERENCES parent_tbl(col) [ON DELETE|UPDATE action] [DEFERRABLE|NOT DEFERRABLE]`
- [x] **17.2** Parser: table-level `FOREIGN KEY (cols) REFERENCES parent_tbl(cols) …`
- [x] **17.3** `PRAGMA foreign_keys` — enable/disable FK enforcement
- [x] **17.4** `PRAGMA foreign_key_list(tbl)` — list FK constraints on a table
- [x] **17.5** `PRAGMA foreign_key_check` — verify all FK constraints
- [x] **17.6** FK enforcement on INSERT: child row must reference an existing parent row (or be NULL)
- [ ] **17.7** FK enforcement on DELETE from parent: cascade action (SET NULL, SET DEFAULT, CASCADE, RESTRICT, NO ACTION) [BLOCKED: scope — requires a parent→children reverse-FK resolver, a child-locator opcode (the inverse of FkLookup), a recursive cascade driver for CASCADE/SET NULL/SET DEFAULT, and the FkConstraint extension to carry on_delete. The infrastructure (catalog, extract_fks, plan_fk, FkLookup, OP_FkCheck executor, the per-row hook slot in compile_delete) is reusable, but the net-new codegen + executor work is substantial. Deferred to a dedicated session.]
- [ ] **17.8** FK enforcement on UPDATE of parent key: cascade action [BLOCKED: scope — same as 17.7, plus the UPDATE codegen path. Deferred.]
- [ ] **17.9** VDBE: `FkCheck` / `FkCounter` / `FkIfZero` opcodes [BLOCKED: scope — FkCheck is implemented (M17.6); FkCounter/FkIfZero are for deferred-FK bookkeeping (M17.10) and the cascade counter (M17.7). Deferred with 17.7/17.10.]
- [ ] **17.10** Deferred FK constraints (check at COMMIT time, not at statement time) [BLOCKED: scope — requires FkCounter/FkIfZero (17.9) and the deferred-constraint queue. Deferred with 17.7/17.9.]
- [ ] **17.11** `sqlite_foreign_keys_list` system table / introspection [BLOCKED: not an upstream SQLite feature — `sqlite_foreign_keys_list` does not exist in SQLite 3.53.1; this was a planning error. The FK introspection surface is `PRAGMA foreign_key_list` (M17.4) and `PRAGMA foreign_key_check` (M17.5), both implemented.]

---

## M18 — INSERT Enhancements

- [x] **18.1** `INSERT … SELECT` — materialize the SELECT, then insert rows from the result set
- [x] **18.2** `INSERT … DEFAULT VALUES` — insert a row with all columns set to their default values
- [x] **18.3** UPSERT: `ON CONFLICT [(cols)] DO UPDATE SET … WHERE …` / `DO NOTHING`
- [ ] **18.4** UPSERT: `ON CONFLICT` without column list — uses any unique index [PARTIAL: `ON CONFLICT DO NOTHING` (no target) is implemented as INSERT OR IGNORE semantics. `ON CONFLICT DO UPDATE` (no target) is rejected with an error — the faithful implementation needs to run the update body on the first unique constraint that conflicts, which requires running the per-index probe loop with the DO UPDATE body inline; deferred to a follow-up.]
- [x] **18.5** VDBE: conflict resolution (`OR ROLLBACK`, `OR ABORT`, `OR FAIL`, `OR IGNORE`, `OR REPLACE`) enforcement for INSERT
- [x] **18.6** `OR REPLACE` — delete conflicting row then insert new row
- [ ] **18.7** `AUTOINCREMENT` enforcement: `sqlite_sequence` table for max rowid tracking
- [x] **18.8** `RETURNING` clause on INSERT — yield row values after insert
- [x] **18.9** Multi-row `INSERT … VALUES` optimization (already works; ensure it handles `DEFAULT` keyword in value list)

---

## M19 — DELETE / UPDATE Enhancements

- [ ] **19.1** `DELETE … ORDER BY … LIMIT …`
- [ ] **19.2** `UPDATE … ORDER BY … LIMIT …`
- [ ] **19.3** `UPDATE … FROM from_clause …` (SQLite 3.33+)
- [ ] **19.4** `RETURNING` clause on DELETE
- [ ] **19.5** `RETURNING` clause on UPDATE
- [ ] **19.6** Conflict resolution enforcement for UPDATE (`OR ROLLBACK/ABORT/FAIL/IGNORE/REPLACE`)
- [ ] **19.7** `UPDATE` of `INTEGER PRIMARY KEY` (rowid-alias column) — must delete+reinsert to move the row
- [ ] **19.8** `CHECK` constraint enforcement on INSERT/UPDATE
- [ ] **19.9** `NOT NULL` constraint enforcement on INSERT/UPDATE
- [ ] **19.10** `UNIQUE` constraint enforcement on INSERT/UPDATE (via unique indexes, already partially done)

---

## M20 — PRAGMA

- [ ] **20.1** PRAGMA framework: parse `PRAGMA [schema.]name [= value] | (value)`, dispatch to handler
- [ ] **20.2** `PRAGMA table_info(tbl)` — column info (cid, name, type, notnull, dflt_value, pk)
- [ ] **20.3** `PRAGMA table_xinfo(tbl)` — like `table_info` plus hidden column flag
- [ ] **20.4** `PRAGMA table_list` — list all tables
- [ ] **20.5** `PRAGMA index_list(tbl)` — list indexes on a table
- [ ] **20.6** `PRAGMA index_info(idx)` — columns in an index
- [ ] **20.7** `PRAGMA index_xinfo(idx)` — like `index_info` plus sort order and collation
- [ ] **20.8** `PRAGMA database_list` — list attached databases
- [ ] **20.9** `PRAGMA schema_version` / `PRAGMA user_version` — read/write header fields
- [ ] **20.10** `PRAGMA application_id` — read/write header field
- [ ] **20.11** `PRAGMA page_size` — read; write only before first write
- [ ] **20.12** `PRAGMA page_count` — read total pages
- [ ] **20.13** `PRAGMA freelist_count` — read freelist page count
- [ ] **20.14** `PRAGMA integrity_check` — b-tree walk + overflow chain + freelist verification
- [ ] **20.15** `PRAGMA quick_check` — faster integrity check (skip overflow, freelist)
- [ ] **20.16** `PRAGMA journal_mode` — read/set (delete, wal, memory, off, truncate, persist)
- [ ] **20.17** `PRAGMA synchronous` — read/set (OFF, NORMAL, FULL, EXTRA)
- [ ] **20.18** `PRAGMA cache_size` — read/set page cache size
- [ ] **20.19** `PRAGMA foreign_keys` — read/set FK enforcement
- [ ] **20.20** `PRAGMA encoding` — read/set text encoding (UTF-8 only for now)
- [ ] **20.21** `PRAGMA compile_options` — list compile-time options
- [ ] **20.22** `PRAGMA function_list` — list registered functions
- [ ] **20.23** `PRAGMA collation_list` — list collations
- [ ] **20.24** `PRAGMA collist` — list columns
- [ ] **20.25** `PRAGMA optimize` — trigger ANALYZE
- [ ] **20.26** `PRAGMA wal_checkpoint` — checkpoint WAL
- [ ] **20.27** `PRAGMA busy_timeout` — read/set busy timeout
- [ ] **20.28** `PRAGMA case_sensitive_like` — toggle LIKE case sensitivity
- [ ] **20.29** `PRAGMA recursive_triggers` — read/set trigger recursion
- [ ] **20.30** `PRAGMA secure_delete` — read/set secure deletion (zero-fill deleted data)
- [ ] **20.31** `PRAGMA locking_mode` — read/set (NORMAL, EXCLUSIVE)
- [ ] **20.32** `PRAGMA auto_vacuum` — read/set (NONE, FULL, INCREMENTAL)
- [ ] **20.33** `PRAGMA incremental_vacuum` — free freelist pages
- [ ] **20.34** `PRAGMA defer_foreign_keys` — defer FK checking until COMMIT
- [ ] **20.35** `PRAGMA writable_schema` — allow direct modification of `sqlite_schema`
- [ ] **20.36** `PRAGMA stats` — report b-tree statistics (debug)
- [ ] **20.37** `PRAGMA reverse_unordered_selects` — toggle optimization
- [ ] **20.38** `PRAGMA query_only` — prevent writes

---

## M21 — ATTACH / DETACH

- [ ] **21.1** Parser: `ATTACH [DATABASE] expr AS expr [KEY expr]`
- [ ] **21.2** Parser: `DETACH [DATABASE] expr`
- [ ] **21.3** Multi-database pager: `main`, `temp`, and user-attached schemas in a connection
- [ ] **21.4** Schema-qualified table references: `schema.table` in FROM, INSERT, UPDATE, DELETE
- [ ] **21.5** `PRAGMA database_list` — list all attached databases
- [ ] **21.6** VFS: open additional database files for ATTACH
- [ ] **21.7** VDBE: schema switching for cross-database references
- [ ] **21.8** DETACH: close file, remove schema entry

---

## M22 — VACUUM & ANALYZE & REINDEX

- [ ] **22.1** Parser: `VACUUM [schema] [INTO expr]`
- [ ] **22.2** VACUUM implementation: create new database, copy all data, replace old file (or write to INTO path)
- [ ] **22.3** Parser: `ANALYZE [schema.]table_or_index`
- [ ] **22.4** ANALYZE implementation: scan table/index, write statistics to `sqlite_stat1` (and `sqlite_stat4` if enabled)
- [ ] **22.5** Parser: `REINDEX [schema.]name`
- [ ] **22.6** REINDEX implementation: drop and recreate index, re-populate from table
- [ ] **22.7** `sqlite_stat1` system table: read during query planning for cost estimation
- [ ] **22.8** Use statistics in index selection (row count estimates, selectivity)

---

## M23 — Date/Time Functions

- [ ] **23.1** Time-value parsing: `YYYY-MM-DD`, `HH:MM:SS`, `YYYY-MM-DD HH:MM:SS`, `YYYY-MM-DDTHH:MM:SS`, Julian day, Unix epoch, `now`, modifiers
- [ ] **23.2** Modifier parsing: `+N days`, `-N months`, `start of month`, `start of year`, `weekday N`, `utc`, `localtime`, `unixepoch`, `auto`
- [ ] **23.3** `date(...)` function
- [ ] **23.4** `time(...)` function
- [ ] **23.5** `datetime(...)` function
- [ ] **23.6** `julianday(...)` function
- [ ] **23.7** `strftime(format, ...)` function
- [ ] **23.8** `unixepoch(...)` function
- [ ] **23.9** `timediff(X, Y)` function
- [ ] **23.10** `current_date`, `current_time`, `current_timestamp` keywords

---

## M24 — JSON Functions

- [ ] **24.1** JSON parser (RFC 8259): parse JSON text into internal tree representation
- [ ] **24.2** `json(X)` / `jsonb(X)` — validate and format JSON
- [ ] **24.3** `json_array(...)` — create JSON array from arguments
- [ ] **24.4** `json_object(...)` — create JSON object from key-value pairs
- [ ] **24.5** `json_extract(X, ...)` / `jsonb_extract(X, ...)` — extract value at path
- [ ] **24.6** `json_insert(X, ...)` / `json_replace(X, ...)` / `json_set(X, ...)` — modify JSON
- [ ] **24.7** `json_remove(X, ...)` — remove element at path
- [ ] **24.8** `json_type(X [, Y])` — type of element
- [ ] **24.9** `json_valid(X [, Y])` — validate JSON
- [ ] **24.10** `json_quote(X)` — quote a value as JSON
- [ ] **24.11** `json_array_length(X [, Y])` — length of JSON array
- [ ] **24.12** `json_pretty(X [, Y])` — pretty-print JSON
- [ ] **24.13** `json_patch(X, Y)` — RFC 7396 merge patch
- [ ] **24.14** `json_error_position(X)` — position of first syntax error
- [ ] **24.15** `json_each(X [, Y])` — table-valued function (iterate array/object)
- [ ] **24.16** `json_tree(X [, Y])` — table-valued function (walk JSON tree)
- [ ] **24.17** `->` and `->>` operators (JSON extraction)
- [ ] **24.18** `json_group_array(X)` — aggregate: collect into JSON array
- [ ] **24.19** `json_group_object(X, Y)` — aggregate: collect into JSON object
- [ ] **24.20** VDBE: subtype support (`SetSubtype`, `GetSubtype`, `ClrSubtype`) for JSON values

---

## M25 — Remaining Scalar & Utility Functions

- [ ] **25.1** `printf(format, ...)` / `format(format, ...)` — printf-style string formatting
- [ ] **25.2** `soundex(X)` — SOUNDEX encoding (ifdef SQLITE_SOUNDEX)
- [ ] **25.3** `load_extension(X [, Y])` — stub (return error; extensions not supported)
- [ ] **25.4** `sqlite_compileoption_get(N)` / `sqlite_compileoption_used(X)` — compile option introspection
- [ ] **25.5** `sqlite_source_id()` — return source ID string
- [ ] **25.6** `unistr(X)` — Unicode escape sequence function
- [ ] **25.7** `sqlite_log(E, M)` — log to error log
- [ ] **25.8** Aggregate functions: `string_agg(X, Y)` (alias for `group_concat`)

---

## M26 — Collation Sequences

- [ ] **26.1** `NOCASE` collation — case-insensitive ASCII comparison for TEXT
- [ ] **26.2** `RTRIM` collation — right-trimmed comparison for TEXT (already partially in `mem_compare`)
- [ ] **26.3** User-defined collation registration (`sqlite3_create_collation`)
- [ ] **26.4** `PRAGMA collation_list` — enumerate registered collations
- [ ] **26.5** `COLLATE` clause on expressions, column definitions, index definitions
- [ ] **26.6** Collation precedence: explicit COLLATE > column default > comparison operand > BINARY

---

## M27 — Query Planner / Optimizer

- [ ] **27.1** Cost estimation: approximate row counts (from ANALYZE or heuristics) for table scans vs index lookups
- [ ] **27.2** Multi-table join ordering (exhaustive or greedy search over join plans)
- [ ] **27.3** Index selection for multi-column WHERE clauses (pick best index)
- [ ] **27.4** Index scan for ORDER BY (avoid sorter when index provides ordering)
- [ ] **27.5** Index scan for both WHERE + ORDER BY (prefix of index for WHERE, suffix for ORDER BY)
- [ ] **27.6** `INDEXED BY` / `NOT INDEXED` table hints
- [ ] **27.7** Automatic index creation for correlated subqueries (autoindex)
- [ ] **27.8** Partial index matching (only use index if WHERE clause satisfies index's partial condition)
- [ ] **27.9** Constant propagation (if `WHERE col = const` then replace col with const)
- [ ] **27.10** LIKE optimization: prefix search via index (`LIKE 'abc%'` → `SeekGE + IdxLT`)
- [ ] **27.11** BETWEEN optimization: rewrite as `col >= low AND col <= high`, use index
- [ ] **27.12** OR-to-UNION rewrite (OR optimization)
- [ ] **27.13** `ORDER BY` with `LIMIT` optimization (bounded sorter)
- [ ] **27.14** `MIN()`/`MAX()` optimization: rewrite as `SeekFirst`/`SeekLast` on index
- [ ] **27.15** `COUNT(*)` optimization: use b-tree row count instead of full scan
- [ ] **27.16** VDBE: `Count` opcode (read b-tree row count from header)

---

## M28 — Remaining VDBE Opcodes

- [ ] **28.1** `Gosub` / `Return` — subroutine jump and return
- [ ] **28.2** `InitCoroutine` / `EndCoroutine` / `Yield` — coroutine for subqueries/CTEs
- [ ] **28.3** `OpenEphemeral` — open ephemeral b-tree for intermediate results
- [ ] **28.4** `OpenPseudo` — open pseudo-table cursor (for views, WITHOUT ROWID)
- [ ] **28.5** `OpenDup` — duplicate cursor for self-joins
- [ ] **28.6** `Sort` — legacy sorter sort (replaced by `SorterSort` but needed for compatibility)
- [ ] **28.7** `Prev` / `Last` / `SeekEnd` — reverse scan, seek to last row
- [ ] **28.8** `RowData` — read full row data from cursor
- [ ] **28.9** `RowSetAdd` / `RowSetRead` / `RowSetTest` — row set for one-pass DELETE/UPDATE optimization
- [ ] **28.10** `Sequence` / `SequenceTest` — autoincrement sequence
- [ ] **28.11** `NotFound` / `Found` / `NoConflict` — index existence check
- [ ] **28.12** `IfNullRow` — jump if cursor row is null (for LEFT JOIN)
- [ ] **28.13** `NullRow` — set cursor to null row
- [ ] **28.14** `SeekRowid` — seek by rowid (dedicated opcode)
- [ ] **28.15** `SeekScan` / `SeekHit` / `IfNoHope` / `IfNotOpen` — seek optimizations
- [ ] **28.16** `ElseEq` — equality check for ELSE branch in CASE
- [ ] **28.17** `Compare` / `Permutation` — register array comparison
- [ ] **28.18** `ColumnCopy` — copy column value between cursors
- [ ] **28.19** `Offset` — get column offset
- [ ] **28.20** `ColumnsUsed` — set column-use mask
- [x] **28.21** `BitAnd` / `BitOr` / `ShiftLeft` / `ShiftRight` / `BitNot` — bitwise operations
- [ ] **28.22** `IsTrue` / `IsType` / `ZeroOrNull` — type/boolean checks
- [ ] **28.23** `SoftNull` — set register to soft NULL
- [ ] **28.24** `Cast` / `MustBeInt` — type coercion
- [ ] **28.25** `CollSeq` — set collation sequence for comparison
- [ ] **28.26** `Variable` — load bound parameter value
- [ ] **28.27** `AddImm` — add immediate integer to register
- [ ] **28.28** `MemMax` / `IfPos` / `IfNotZero` / `DecrJumpZero` / `OffsetLimit` — counter operations
- [ ] **28.29** `AutoCommit` — toggle autocommit mode
- [ ] **28.30** `Checkpoint` — WAL checkpoint
- [ ] **28.31** `Savepoint` — create/release/rollback-to savepoint
- [ ] **28.32** `TableLock` — lock a table
- [ ] **28.33** `FkCheck` / `FkCounter` / `FkIfZero` — foreign key checking
- [ ] **28.34** `Clear` — delete all rows in a b-tree
- [ ] **28.35** `ResetSorter` — reset sorter without closing
- [ ] **28.36** `DropTable` / `DropIndex` / `DropTrigger` — drop and invalidate schema
- [ ] **28.37** `LoadAnalysis` — load `sqlite_stat1` data
- [ ] **28.38** `SqlExec` — execute raw SQL (used by VACUUM)
- [ ] **28.39** `IntegrityCk` — integrity check opcode
- [ ] **28.40** `Program` / `Param` — execute sub-VDBE (for triggers)
- [ ] **28.41** `Once` — execute branch only once
- [ ] **28.42** `Jump` — 3-way jump (compare result routing)
- [ ] **28.43** `HaltIfNull` — halt if NULL
- [ ] **28.44** `Trace` / `Init` — trace and initialization
- [ ] **28.45** `CursorLock` / `CursorUnlock` — lock/unlock cursor
- [ ] **28.46** `ReopenIdx` — reopen index cursor (optimization)
- [ ] **28.47** `FilterAdd` / `Filter` — Bloom filter for IN expressions
- [ ] **28.48** `VBegin` / `VCreate` / `VDestroy` / `VOpen` / `VFilter` / `VColumn` / `VNext` / `VRename` / `VUpdate` / `VCheck` / `VInitIn` — virtual table opcodes
- [ ] **28.49** `AggStep1` / `AggInverse` / `AggValue` — aggregate/window opcodes (in addition to existing `AggStep`/`AggFinal`)
- [ ] **28.50** `BeginSubrtn` — begin subroutine (for triggers)
- [ ] **28.51** `ReadCookie` — read database header cookie value
- [ ] **28.52** `TypeCheck` — type check for STRICT tables
- [ ] **28.53** `ReleaseReg` — release registers
- [ ] **28.54** `Expire` — expire prepared statement
- [ ] **28.55** `Abortable` — mark statement as abortable

---

## M29 — C-API Expansion

- [ ] **29.1** `sqlite3_exec()` — convenience exec-with-callback API
- [ ] **29.2** `sqlite3_bind_*()` family: `bind_int`, `bind_int64`, `bind_double`, `bind_text`, `bind_blob`, `bind_null`, `bind_zeroblob`, `bind_value`, `bind_parameter_count`, `bind_parameter_index`, `bind_parameter_name`
- [ ] **29.3** `sqlite3_clear_bindings()` — reset all bound parameters
- [ ] **29.4** `sqlite3_column_*()` type-specific accessors: `column_int`, `column_int64`, `column_double`, `column_text`, `column_blob`, `column_bytes`, `column_type`, `column_decltype`
- [ ] **29.5** `sqlite3_get_table()` / `sqlite3_free_table()` — result-as-2D-array API
- [ ] **29.6** `sqlite3_create_function()` / `sqlite3_create_function_v2()` — user-defined scalar/aggregate functions
- [ ] **29.7** `sqlite3_create_window_function()` — user-defined window functions
- [ ] **29.8** `sqlite3_value_*()` family — value accessors inside function callbacks
- [ ] **29.9** `sqlite3_result_*()` family — result setters inside function callbacks
- [ ] **29.10** `sqlite3_aggregate_context()` — aggregate state allocation
- [ ] **29.11** `sqlite3_create_collation()` / `sqlite3_create_collation_v2()` — user-defined collations
- [ ] **29.12** `sqlite3_collation_needed()` — callback for unknown collation
- [ ] **29.13** `sqlite3_busy_handler()` / `sqlite3_busy_timeout()` — lock contention handling
- [ ] **29.14** `sqlite3_progress_handler()` — periodic callback during long operations
- [ ] **29.15** `sqlite3_commit_hook()` / `sqlite3_rollback_hook()` / `sqlite3_update_hook()` — transaction/change notification
- [ ] **29.16** `sqlite3_set_authorizer()` — authorization callback
- [ ] **29.17** `sqlite3_trace()` / `sqlite3_trace_v2()` / `sqlite3_profile()` — tracing
- [ ] **29.18** `sqlite3_interrupt()` — cancel a running statement
- [ ] **29.19** `sqlite3_extended_result_codes()` — enable extended error codes
- [ ] **29.20** `sqlite3_errmsg16()` / `sqlite3_prepare16()` — UTF-16 variants
- [ ] **29.21** `sqlite3_blob_open()` / `sqlite3_blob_read()` / `sqlite3_blob_write()` / `sqlite3_blob_close()` / `sqlite3_blob_bytes()` / `sqlite3_blob_reopen()` — incremental BLOB I/O
- [ ] **29.22** `sqlite3_backup_init()` / `sqlite3_backup_step()` / `sqlite3_backup_finish()` / `sqlite3_backup_remaining()` / `sqlite3_backup_pagecount()` — online backup
- [ ] **29.23** `sqlite3_serialize()` / `sqlite3_deserialize()` — in-memory database serialization
- [ ] **29.24** `sqlite3_changes64()` / `sqlite3_total_changes64()` — 64-bit change counts
- [ ] **29.25** `sqlite3_set_last_insert_rowid()` — override last insert rowid
- [ ] **29.26** `sqlite3_db_handle()` — get connection from statement
- [ ] **29.27** `sqlite3_db_filename()` / `sqlite3_db_readonly()` / `sqlite3_db_name()` — database file info
- [ ] **29.28** `sqlite3_next_stmt()` — iterate over prepared statements
- [ ] **29.29** `sqlite3_stmt_readonly()` / `sqlite3_stmt_busy()` — statement state queries
- [ ] **29.30** `sqlite3_sql()` / `sqlite3_expanded_sql()` / `sqlite3_normalized_sql()` — SQL text access
- [ ] **29.31** `sqlite3_complete()` / `sqlite3_complete16()` — check if SQL text is complete
- [ ] **29.32** `sqlite3_table_column_metadata()` — column metadata
- [ ] **29.33** `sqlite3_keyword_count()` / `sqlite3_keyword_name()` / `sqlite3_keyword_check()` — keyword introspection
- [ ] **29.34** `sqlite3_str_*()` family — string accumulation/builder
- [ ] **29.35** `sqlite3_mprintf()` / `sqlite3_snprintf()` — formatted string allocation
- [ ] **29.36** `sqlite3_randomness()` — random bytes
- [ ] **29.37** `sqlite3_soft_heap_limit64()` / `sqlite3_hard_heap_limit64()` / `sqlite3_memory_used()` / `sqlite3_memory_highwater()` — memory management
- [ ] **29.38** `sqlite3_config()` / `sqlite3_initialize()` / `sqlite3_shutdown()` — global configuration
- [ ] **29.39** `sqlite3_limit()` / `sqlite3_db_config()` — runtime limits
- [ ] **29.40** `sqlite3_status()` / `sqlite3_status64()` / `sqlite3_db_status()` / `sqlite3_stmt_status()` — status counters
- [ ] **29.41** `sqlite3_preupdate_hook()` / `sqlite3_preupdate_old()` / `sqlite3_preupdate_new()` / `sqlite3_preupdate_count()` / `sqlite3_preupdate_depth()` — pre-update notification
- [ ] **29.42** `sqlite3_unlock_notify()` — notification when lock is released
- [ ] **29.43** `sqlite3_wal_hook()` / `sqlite3_wal_autocheckpoint()` / `sqlite3_wal_checkpoint()` / `sqlite3_wal_checkpoint_v2()` — WAL hooks
- [ ] **29.44** `sqlite3_snapshot_get()` / `sqlite3_snapshot_open()` / `sqlite3_snapshot_free()` / `sqlite3_snapshot_cmp()` — WAL snapshots
- [ ] **29.45** `sqlite3_db_release_memory()` / `sqlite3_db_cacheflush()` — memory management
- [ ] **29.46** `sqlite3_strglob()` / `sqlite3_strlike()` / `sqlite3_stricmp()` / `sqlite3_strnicmp()` — string comparison

---

## M30 — CLI Parity

- [ ] **30.1** `.backup [DB] FILE` — backup database to file
- [ ] **30.2** `.bail on|off` — stop on error
- [ ] **30.3** `.cd DIRECTORY` — change working directory
- [ ] **30.4** `.changes on|off` — show rows changed
- [ ] **30.5** `.echo on|off` — echo commands
- [ ] **30.6** `.import FILE TABLE` — import CSV/TSV into table
- [ ] **30.7** `.load FILE [ENTRY]` — load extension (stub/error)
- [ ] **30.8** `.log FILE|off` — set log file
- [ ] **30.9** `.once FILE` — output next query to file
- [ ] **30.10** `.output FILE|stdout` — set output destination
- [ ] **30.11** `.print STRING` — print literal text
- [ ] **30.12** `.prompt MAIN CONTINUE` — change prompt strings
- [ ] **30.13** `.read FILE` — execute SQL from file
- [ ] **30.14** `.restore [DB] FILE` — restore database from file
- [ ] **30.15** `.save FILE` — save database (alias for `.backup`)
- [ ] **30.16** `.separator STRING` (column) / `.separator STRING STRING` (column + row)
- [ ] **30.17** `.stats on|off` — show performance stats
- [ ] **30.18** `.system CMD` — run system command
- [ ] **30.19** `.timeout MS` — set busy timeout
- [ ] **30.20** `.timer on|off` — show execution time
- [ ] **30.21** `.width NUM NUM ...` — set column widths for `column` mode
- [ ] **30.22** `.nullvalue STRING` (already done)
- [ ] **30.23** `.headers on|off` (already done)
- [ ] **30.24** Multi-statement SQL input (`sqlite3_prepare_v2` loop with `tail`)
- [ ] **30.25** `.read` and `-init FILE` support
- [ ] **30.26** CLI flags: `-bail`, `-readonly`, `-cmd CMD`, `-batch`, `-interactive`

---

## M31 — Virtual Tables

- [ ] **31.1** Parser: `CREATE VIRTUAL TABLE … USING module (args)`
- [ ] **31.2** `sqlite3_module` trait: `xCreate`, `xConnect`, `xBestIndex`, `xDisconnect`, `xDestroy`, `xOpen`, `xClose`, `xFilter`, `xNext`, `xEof`, `xColumn`, `xRowid`, `xUpdate`, `xBegin`, `xSync`, `xCommit`, `xRollback`, `xFindFunction`, `xRename`, `xSavepoint`, `xRelease`, `xRollbackTo`, `xShadowName`, `xIntegrity`
- [ ] **31.3** VDBE virtual table opcodes: `VBegin`, `VCreate`, `VDestroy`, `VOpen`, `VFilter`, `VColumn`, `VNext`, `VRename`, `VUpdate`, `VCheck`, `VInitIn`
- [ ] **31.4** `sqlite3_create_module()` / `sqlite3_create_module_v2()` — register a vtab module
- [ ] **31.5** `sqlite3_declare_vtab()` — declare CREATE TABLE for virtual table schema
- [ ] **31.6** `sqlite3_vtab_config()`, `sqlite3_vtab_on_conflict()`, `sqlite3_vtab_collation()`, `sqlite3_vtab_distinct()`, `sqlite3_vtab_in()`, `sqlite3_vtab_nochange()`, `sqlite3_vtab_rhs_value()` — vtab helper functions
- [ ] **31.7** Built-in virtual tables: `sqlite_schema` (read-only view of `sqlite_master`), `sqlite_dbpage`, `sqlite_stat1`, `sqlite_stat4`

---

## M32 — Pager & VFS Robustness

- [ ] **32.1** Page cache eviction (LRU or approximate LRU, configurable size limit)
- [ ] **32.2** Shared-cache mode (multiple connections sharing a pager)
- [ ] **32.3** Multi-process read concurrency (shared lock for readers, exclusive for writers)
- [ ] **32.4** Proper OS-level file locking (`flock`/`fcntl` advisory locks) on `OsTokioVfs`
- [ ] **32.5** `PRAGMA mmap_size` — memory-mapped I/O for reads
- [ ] **32.6** `PRAGMA cache_size` — configurable page cache size
- [ ] **32.7** `PRAGMA synchronous = EXTRA` — double-sync on WAL checkpoint
- [ ] **32.8** `PRAGMA locking_mode = EXCLUSIVE` — skip shared lock acquisition
- [ ] **32.9** `sqlite3_busy_handler()` / `sqlite3_busy_timeout()` integration with VFS locking
- [ ] **32.10** Dynamic VFS registration (allow user-registered VFS implementations)
- [ ] **32.11** `xAccess` VFS method (file existence/permissions check)
- [ ] **32.12** `xFullPathname` VFS method (resolve relative paths)
- [ ] **32.13** `xDelete` VFS method (delete file)
- [ ] **32.14** `xRandomness` VFS method (OS-provided randomness)
- [ ] **32.15** `xSleep` VFS method
- [ ] **32.16** `xCurrentTime` / `xCurrentTimeInt64` VFS methods
- [ ] **32.17** In-memory journal (`:memory:` databases use memory journal, not file)
- [ ] **32.18** Pager: `PRAGMA journal_mode = persist` (keep journal file, truncate on commit)
- [ ] **32.19** Pager: `PRAGMA journal_mode = truncate` (truncate journal on commit)
- [ ] **32.20** Pager: `PRAGMA journal_mode = off` (no journal, no crash recovery)

---

## M33 — STRICT Tables

- [ ] **33.1** Parser: `STRICT` table option in `CREATE TABLE`
- [ ] **33.2** VDBE: `TypeCheck` opcode — enforce column type affinity strictly (reject wrong-type inserts)
- [ ] **33.3** Codegen: type validation on INSERT for STRICT tables
- [ ] **33.4** `INSERT` into STRICT table with wrong type → `SQLITE_CONSTRAINT_DATATYPE`

---

## M34 — Generated Columns

- [ ] **34.1** Parser: `GENERATED ALWAYS AS (expr) [STORED|VIRTUAL]` / `AS (expr)` column syntax
- [ ] **34.2** Codegen: compute generated column expression during INSERT/UPDATE
- [ ] **34.3** STORED generated columns — store value in the row
- [ ] **34.4** VIRTUAL generated columns — compute on read, do not store
- [ ] **34.5** GENERATED columns in indexes (expression index auto-created)

---

## M35 — `STRICT` SQL Mode & Type Enforcement

- [ ] **35.1** `CHECK` constraint evaluation on INSERT/UPDATE
- [ ] **35.2** `NOT NULL` constraint enforcement (already partially in schema; runtime check needed)
- [ ] **35.3** `DEFAULT` value enforcement (constant defaults already work; expression defaults need evaluation)
- [ ] **35.4** Column type affinity rules: exact match for STRICT, standard affinity for non-STRICT

---

## M36 — BLOB I/O

- [ ] **36.1** `sqlite3_blob_open()` — open incremental BLOB handle
- [ ] **36.2** `sqlite3_blob_read()` — read from BLOB handle
- [ ] **36.3** `sqlite3_blob_write()` — write to BLOB handle
- [ ] **36.4** `sqlite3_blob_close()` — close BLOB handle
- [ ] **36.5** `sqlite3_blob_bytes()` — get BLOB size
- [ ] **36.6** `sqlite3_blob_reopen()` — reposition BLOB handle

---

## M37 — Online Backup

- [ ] **37.1** `sqlite3_backup_init()` — initialize backup between two databases
- [ ] **37.2** `sqlite3_backup_step()` — copy N pages from source to destination
- [ ] **37.3** `sqlite3_backup_finish()` — complete backup
- [ ] **37.4** `sqlite3_backup_remaining()` / `sqlite3_backup_pagecount()` — progress info
- [ ] **37.5** Handle concurrent modifications during backup

---

## M38 — Serialization

- [ ] **38.1** `sqlite3_serialize()` — serialize database to byte buffer
- [ ] **38.2** `sqlite3_deserialize()` — deserialize byte buffer into database connection
- [ ] **38.3** Handle size limits and memory management for serialized DB

---

## M39 — Error Codes & Extended Codes

- [ ] **39.1** Complete set of SQLite result codes: `SQLITE_OK`, `SQLITE_ERROR`, `SQLITE_BUSY`, `SQLITE_LOCKED`, `SQLITE_NOMEM`, `SQLITE_READONLY`, `SQLITE_INTERRUPT`, `SQLITE_IOERR`, `SQLITE_CORRUPT`, `SQLITE_NOTFOUND`, `SQLITE_FULL`, `SQLITE_CANTOPEN`, `SQLITE_PROTOCOL`, `SQLITE_EMPTY`, `SQLITE_SCHEMA`, `SQLITE_TOOBIG`, `SQLITE_CONSTRAINT`, `SQLITE_MISMATCH`, `SQLITE_MISUSE`, `SQLITE_NOLFS`, `SQLITE_AUTH`, `SQLITE_RANGE`, `SQLITE_NOTADB`, `SQLITE_NOTICE`, `SQLITE_WARNING`, etc.
- [ ] **39.2** Extended result codes: `SQLITE_IOERR_READ`, `SQLITE_IOERR_WRITE`, `SQLITE_CONSTRAINT_PRIMARYKEY`, `SQLITE_CONSTRAINT_UNIQUE`, `SQLITE_CONSTRAINT_NOTNULL`, `SQLITE_CONSTRAINT_FOREIGNKEY`, `SQLITE_BUSY_SNAPSHOT`, etc.
- [ ] **39.3** `sqlite3_extended_result_codes()` — toggle extended codes
- [ ] **39.4** `sqlite3_errstr()` — human-readable error string from code

---

## M40 — Testing & Compatibility

- [ ] **40.1** Differential testing framework: run same SQL against C `sqlite3` and Rustqlite, compare results
- [ ] **40.2** File-format round-trip: write DB in Rustqlite, read in C `sqlite3` (and vice versa)
- [ ] **40.3** `PRAGMA integrity_check` passes on Rustqlite-written databases when checked by C `sqlite3`
- [ ] **40.4** sqllogictest harness: expand test manifest beyond M4 subset
- [ ] **40.5** Fuzz testing: AFL/libfuzzer on parser, record format, VDBE execution
- [ ] **40.6** Concurrency testing: multiple readers, single writer
- [ ] **40.7** Crash recovery testing: kill process mid-transaction, verify rollback journal restores
- [ ] **40.8** WAL mode crash recovery testing
- [ ] **40.9** Error message parity: match C `sqlite3` error strings for common errors
- [ ] **40.10** CLI compatibility: run `sqlite3` test scripts through `rustsqlite` binary

---

## Notes

- Tasks marked with `[x]` are complete.
- Tasks marked with `[ ]` are pending (OCLoop will execute these).
- Tasks marked with `[MANUAL]` require human intervention.
- Tasks marked with `[BLOCKED: reason]` cannot proceed until blocker is resolved.
- Milestone ordering reflects dependency: e.g., M7 (joins) depends on M6 (aggregates), M8 (subqueries) depends on M7.
- WAL (M13) can be developed in parallel with query features but is needed for production readiness.
- Virtual tables (M31) and extensions (M29 user-defined functions) can be deferred until core features are complete.
- JSON (M24) and date/time (M23) can be developed in parallel as they are largely self-contained.
---

## Addenda to Existing Milestones

### M2 — Parser (Additional Items)

- [x] **2.59** `CREATE TABLE … AS SELECT …` (CTAS)
- [x] **2.60** Row-value expressions: `(expr, expr, …)` and row-value comparisons
- [x] **2.61** `REGEXP` expression operator (calls user-registered function)
- [x] **2.62** `MATCH` expression operator (for FTS, future)
- [x] **2.63** `FILTER (WHERE expr)` clause on aggregate function calls
- [x] **2.64** `DEFAULT` keyword in INSERT value position (`INSERT INTO t VALUES (1, DEFAULT, 3)`) — verified upstream rejects this as a syntax error; not a SQLite feature
- [x] **2.65** Schema-qualified object names: `schema.table` in all DML/DDL contexts
- [x] **2.66** `ON CONFLICT` clause on column/table constraints (unique, not null, check, foreign key)
- [x] **2.67** `VALUES (expr_list) [, …]` as a standalone statement (not just in INSERT)
- [x] **2.68** Table-valued function syntax in FROM clause: `FROM func(args)`
- [x] **2.69** `ALTER TABLE … ALTER COLUMN … DROP NOT NULL` (3.37+)
- [x] **2.70** `ALTER TABLE … ALTER COLUMN … SET NOT NULL` (3.37+)
- [x] **2.71** `ALTER TABLE … ADD CONSTRAINT [name] CHECK (expr)`
- [x] **2.72** `ALTER TABLE … DROP CONSTRAINT name`

### M5.2 — Index (Additional Items)

- [x] **5.2.12** Covering index / index-only scan: satisfy `SELECT` columns from index without table lookup
- [x] **5.2.13** Index scan for ORDER BY (avoid sorter when index provides ordering)
- [x] **5.2.14** Index scan for both WHERE + ORDER BY (prefix for WHERE, suffix for ORDER BY)

### M5.3 — B-Tree (Additional Items)

- [ ] **5.3.10** Recovery from corrupt databases: malformed page handling, wrong magic numbers, bad cell pointers
- [ ] **5.3.11** Database file header: write all 100-byte header fields on creation (currently only partial fields)

### M6 — Aggregates (Additional Items)

- [ ] **6.10** `FILTER (WHERE expr)` on aggregate calls: `count(*) FILTER (WHERE type='user')`
- [ ] **6.11** `string_agg(X, Y)` — alias for `group_concat`

### M8 — Subqueries (Additional Items)

- [ ] **8.12** Subquery flattening optimization: merge `FROM (SELECT …)` into outer query when safe
- [ ] **8.13** Correlated subquery re-materialization when outer row changes
- [ ] **8.14** Automatic index creation for correlated subqueries (autoindex)

### M12 — Transactions (Additional Items)

- [ ] **12.10** `SQLITE_SCHEMA` error: detect stale prepared statements when schema changes, return `SQLITE_SCHEMA` so caller can re-prepare
- [ ] **12.11** Schema recompilation: automatically re-prepare statements that encounter `SQLITE_SCHEMA`
- [ ] **12.12** `sqlite3_expired()` / statement expiry on schema change

### M13 — WAL (Additional Items)

- [ ] **13.13** Master journal for multi-database atomic commits (ATTACH + WAL)
- [ ] **13.14** `sqlite3_wal_hook()` / `sqlite3_wal_autocheckpoint()` / `sqlite3_wal_checkpoint()` / `sqlite3_wal_checkpoint_v2()`
- [ ] **13.15** WAL snapshots: `sqlite3_snapshot_get()` / `sqlite3_snapshot_open()` / `sqlite3_snapshot_free()` / `sqlite3_snapshot_cmp()`

### M14 — ALTER TABLE (Additional Items)

- [ ] **14.10** `ALTER TABLE … ALTER COLUMN … DROP NOT NULL` / `SET NOT NULL` (3.37+)
- [ ] **14.11** `ALTER TABLE … ADD CONSTRAINT [name] CHECK (expr)` / `DROP CONSTRAINT name`

### M19 — DELETE/UPDATE Enhancements (Additional Items)

- [ ] **19.8** `CHECK` constraint evaluation on INSERT/UPDATE
- [ ] **19.9** `NOT NULL` constraint enforcement on INSERT/UPDATE
- [ ] **19.10** `UNIQUE` constraint enforcement on INSERT/UPDATE (via unique indexes, already partially done)

### M20 — PRAGMA (Additional Items)

- [ ] **20.39** `PRAGMA full_column_names` / `PRAGMA short_column_names` — column naming mode
- [ ] **20.40** `PRAGMA count_changes` — return rows-affected as query result
- [ ] **20.41** `PRAGMA empty_result_callbacks` — callback on empty result sets
- [ ] **20.42** `PRAGMA fullfsync` / `PRAGMA checkpoint_fullfsync` — full fsync control
- [ ] **20.43** `PRAGMA data_version` — read data version (readonly)
- [ ] **20.44** `PRAGMA default_cache_size` — legacy cache size setting
- [ ] **20.45** `PRAGMA mmap_size` — read/set memory-mapped I/O limit
- [ ] **20.46** `PRAGMA temp_store` — read/set (DEFAULT, FILE, MEMORY)
- [ ] **20.47** `PRAGMA max_page_count` — read/set maximum pages
- [ ] **20.48** `PRAGMA shrink_memory` — release unused memory
- [ ] **20.49** `PRAGMA threads` — read/set worker thread count
- [ ] **20.50** `PRAGMA soft_heap_limit` / `PRAGMA hard_heap_limit` — memory limits
- [ ] **20.51** `PRAGMA analysis_limit` — limit ANALYZE sampling
- [ ] **20.52** `PRAGMA module_list` — list registered virtual table modules
- [ ] **20.53** `PRAGMA parser_trace` — debug parser tracing (debug build only)

### M27 — Query Planner (Additional Items)

- [ ] **27.17** Predicate pushdown through joins (move WHERE filters closer to the table scan)
- [ ] **27.18** Bloom filter optimization for `IN` expressions (`FilterAdd` / `Filter` opcodes)
- [ ] **27.19** Subquery flattening: merge `FROM (SELECT …)` into outer query when safe
- [ ] **27.20** Covering index scan: satisfy SELECT columns from index without table lookup
- [ ] **27.21** `IN (value_list)` optimization: sort values and use binary search or ephemeral index
- [ ] **27.22** `EXPLAIN QUERY PLAN` output enhancement: show estimated row counts, index use details

### M28 — VDBE Opcodes (Additional Items)

- [ ] **28.56** `OpenAutoindex` — open an automatic index for subquery materialization
- [ ] **28.57** `DeferredSeek` / `FinishSeek` — deferred index seek optimization
- [ ] **28.58** `RealToHex` — REAL to hex string conversion
- [ ] **28.59** `StringType` — check string type (TEXT vs BLOB)
- [ ] **28.60** `Noop` — no-operation (placeholder/debug)
- [ ] **28.61** `VCreate` / `VDestroy` / `VOpen` / `VFilter` / `VColumn` / `VNext` / `VRename` / `VUpdate` / `VCheck` / `VInitIn` — virtual table opcodes (duplicate of 28.48, ensuring coverage)

### M29 — C-API (Additional Items)

- [ ] **29.47** `sqlite3_close_v2()` — resilient close (unfinalize statements)
- [ ] **29.48** `sqlite3_data_count()` — return number of columns in current row
- [ ] **29.49** `sqlite3_column_database_name()` / `sqlite3_column_table_name()` / `sqlite3_column_origin_name()` — result column provenance
- [ ] **29.50** `sqlite3_uri_parameter()` / `sqlite3_uri_boolean()` / `sqlite3_uri_int64()` / `sqlite3_uri_key()` — URI filename parameters
- [ ] **29.51** `sqlite3_create_filename()` / `sqlite3_free_filename()` — generate database filenames
- [ ] **29.52** `sqlite3_error_offset()` — byte offset into SQL text where error occurred
- [ ] **29.53** `sqlite3_errstr()` — human-readable error string from result code
- [ ] **29.54** `sqlite3_overload_function()` — stub for virtual table function overloading
- [ ] **29.55** `sqlite3_auto_extension()` / `sqlite3_cancel_auto_extension()` / `sqlite3_reset_auto_extension()` — automatic extension loading
- [ ] **29.56** `sqlite3_enable_load_extension()` / `sqlite3_load_extension()` — dynamic extension loading (stub)
- [ ] **29.57** `sqlite3_get_auxdata()` / `sqlite3_set_auxdata()` — function metadata across calls
- [ ] **29.58** `sqlite3_get_clientdata()` / `sqlite3_set_clientdata()` — per-connection client data
- [ ] **29.59** `sqlite3_stmt_scanstatus()` / `sqlite3_stmt_scanstatus_v2()` / `sqlite3_stmt_scanstatus_reset()` — query plan scan status
- [ ] **29.60** `sqlite3_threadsafe()` — return thread-safety mode
- [ ] **29.61** `sqlite3_open_v2()` `SQLITE_OPEN_URI` flag and URI filename parsing
- [ ] **29.62** `sqlite3_vfs_register()` / `sqlite3_vfs_unregister()` / `sqlite3_vfs_find()` — VFS registration
- [ ] **29.63** `sqlite3_vtab_config()` / `sqlite3_vtab_on_conflict()` / `sqlite3_vtab_collation()` / `sqlite3_vtab_distinct()` / `sqlite3_vtab_in()` / `sqlite3_vtab_nochange()` / `sqlite3_vtab_rhs_value()` — virtual table helpers
- [ ] **29.64** `sqlite3_declare_vtab()` — declare virtual table schema
- [ ] **29.65** `sqlite3_create_module()` / `sqlite3_create_module_v2()` / `sqlite3_drop_modules()` — virtual table module registration

### M30 — CLI (Additional Items)

- [ ] **30.27** `.dump` — SQL dump of entire database
- [ ] **30.28** `.eqp on|off` — toggle EXPLAIN QUERY PLAN
- [ ] **30.29** `.fullschema` — schema with statistics
- [ ] **30.30** `.indexes [PATTERN]` — list indexes
- [ ] **30.31** `.limits` — show runtime limits
- [ ] **30.32** `.trace on|off` — trace SQL statements
- [ ] **30.33** `.selftest` — run self-test
- [ ] **30.34** `.info` — show connection info and settings
- [ ] **30.35** `.databases` — list attached databases (expand current stub)

### M31 — Virtual Tables (Additional Items)

- [ ] **31.8** Eponymous-only virtual tables (virtual tables that can be used without explicit CREATE VIRTUAL TABLE)
- [ ] **31.9** `DROP MODULE` — unregister a virtual table module

### M32 — Pager & VFS (Additional Items)

- [ ] **32.21** `xDlOpen` / `xDlError` / `xDlSym` / `xDlClose` VFS methods (dynamic library loading)
- [ ] **32.22** `xGetLastError` VFS method
- [ ] **32.23** `xSectorSize` / `xDeviceCharacteristics` VFS file methods
- [ ] **32.24** `xFileControl` VFS file method (multiplex of FCNTL opcodes)
- [ ] **32.25** `xCheckReservedLock` VFS file method
- [ ] **32.26** `xFetch` / `xUnfetch` VFS file methods (memory-mapped I/O)

### M35 — Constraint Enforcement (Additional Items)

- [ ] **35.5** `UNIQUE` constraint enforcement via unique indexes (raise `SQLITE_CONSTRAINT_UNIQUE` on violation)
- [ ] **35.6** `PRIMARY KEY` constraint enforcement (rowid uniqueness for INTEGER PRIMARY KEY, unique index for composite PKs)
- [ ] **35.7** `ON CONFLICT ROLLBACK` / `ABORT` / `FAIL` / `IGNORE` / `REPLACE` per-column and per-table constraints

### M39 — Error Codes (Additional Items)

- [ ] **39.5** `sqlite3_error_offset()` — byte offset of error in SQL text

---

## M41 — Temporary Objects & Temp Schema

- [ ] **41.1** `CREATE TEMP TABLE …` — create table in the `temp` schema
- [ ] **41.2** `CREATE TEMP VIEW …` — create view in the `temp` schema
- [ ] **41.3** `CREATE TEMP INDEX …` — create index in the `temp` schema
- [ ] **41.4** `CREATE TEMP TRIGGER …` — create trigger in the `temp` schema
- [ ] **41.5** `sqlite_temp_master` — temporary schema catalog table
- [ ] **41.6** `PRAGMA temp_store` — control temp storage (DEFAULT, FILE, MEMORY)
- [ ] **41.7** Ephemeral tables for `IN (SELECT …)`, `ORDER BY` sort, `GROUP BY` hash, compound SELECT dedup
- [ ] **41.8** Temp file management for large ephemeral sorts (spill to disk)

---

## M42 — UTF-16 Encoding Support

- [ ] **42.1** `PRAGMA encoding = 'UTF-16le'` / `'UTF-16be'` — database text encoding (read-only: create as UTF-8; read UTF-16 DBs)
- [ ] **42.2** `sqlite3_column_text16()` / `sqlite3_column_bytes16()` — UTF-16 column accessors
- [ ] **42.3** `sqlite3_bind_text16()` — UTF-16 parameter binding
- [ ] **42.4** `sqlite3_result_text16()` / `sqlite3_result_text16be()` / `sqlite3_result_text16le()` — UTF-16 result values
- [ ] **42.5** BOM detection for UTF-16 databases on open
- [ ] **42.6** `sqlite3_errmsg16()` — UTF-16 error message
- [ ] **42.7** `sqlite3_prepare16()` / `sqlite3_prepare16_v2()` / `sqlite3_prepare16_v3()` — UTF-16 statement preparation

---

## M43 — Session Extension

- [ ] **43.1** `sqlite3session_create()` — create a session object
- [ ] **43.2** `sqlite3session_enable()` / `sqlite3session_diff()` — enable/diff session
- [ ] **43.3** `sqlite3session_delete()` — delete session
- [ ] **43.4** Changeset generation: track row-level changes (INSERT, UPDATE, DELETE)
- [ ] **43.5** `sqlite3changeset_start()` / `sqlite3changeset_next()` — iterate changeset
- [ ] **43.6** `sqlite3changeset_apply()` — apply changeset to another database
- [ ] **43.7** Conflict resolution callbacks during changeset application
- [ ] **43.8** Patchset support (changeset variant with only PK + modified columns)
- [ ] **43.9** `sqlite3session_changeset()` / `sqlite3session_patchset()` — extract changeset data

---

## M44 — Thread Safety & Concurrency

- [ ] **44.1** `sqlite3_config(SQLITE_CONFIG_SINGLETHREAD / MULTITHREAD / SERIALIZED)` — threading mode
- [ ] **44.2** Connection-level mutexes (`sqlite3_db_mutex()`)
- [ ] **44.3** `SQLITE_OPEN_NOMUTEX` / `SQLITE_OPEN_FULLMUTEX` flags
- [ ] **44.4** `sqlite3_threadsafe()` — return threading mode
- [ ] **44.5** `sqlite3_thread_cleanup()` — thread-local cleanup
- [ ] **44.6** `sqlite3_enable_shared_cache()` — shared-cache mode
- [ ] **44.7** Multi-process locking: shared lock for readers, exclusive for writers
- [ ] **44.8** `sqlite3_unlock_notify()` — notification when blocking lock is released

---

## M45 — Memory Management & Configuration

- [ ] **45.1** `sqlite3_config(SQLITE_CONFIG_MALLOC, ...)` — custom allocator
- [ ] **45.2** `sqlite3_config(SQLITE_CONFIG_HEAP, ...)` — heap size limit
- [ ] **45.3** `sqlite3_config(SQLITE_CONFIG_PAGECACHE, ...)` — page cache memory
- [ ] **45.4** `sqlite3_config(SQLITE_CONFIG_LOOKASIDE, ...)` — lookaside allocator
- [ ] **45.5** `sqlite3_config(SQLITE_CONFIG_MEMSTATUS, ...)` — memory statistics toggle
- [ ] **45.6** `sqlite3_malloc()` / `sqlite3_malloc64()` / `sqlite3_realloc()` / `sqlite3_realloc64()` / `sqlite3_free()` — memory allocation
- [ ] **45.7** `sqlite3_msize()` — memory block size
- [ ] **45.8** `sqlite3_memory_used()` / `sqlite3_memory_highwater()` — memory usage tracking
- [ ] **45.9** `sqlite3_release_memory()` / `sqlite3_db_release_memory()` — pressure release

---

## M46 — URI Filenames & Database File Management

- [ ] **46.1** URI filename parsing: `file:db?mode=ro&cache=shared&nolock=1` etc.
- [ ] **46.2** `sqlite3_uri_parameter()` / `sqlite3_uri_boolean()` / `sqlite3_uri_int64()` / `sqlite3_uri_key()` — query parameter access
- [ ] **46.3** `sqlite3_create_filename()` / `sqlite3_free_filename()` — generate database filenames from URI components
- [ ] **46.4** `sqlite3_filename_database()` / `sqlite3_filename_journal()` / `sqlite3_filename_wal()` — derive filenames
- [ ] **46.5** `sqlite3_database_file_object()` — get file object for a database
- [ ] **46.6** `sqlite3_file_control()` — VFS file control operations

---

## M47 — Schema & Catalog Enhancements

- [ ] **47.1** `sqlite_sequence` system table for AUTOINCREMENT high-water mark
- [ ] **47.2** `sqlite_stat1` system table for ANALYZE statistics
- [ ] **47.3** `sqlite_stat4` system table for ANALYZE column statistics
- [ ] **47.4** `sqlite_master` as an alias for `sqlite_schema`
- [ ] **47.5** Schema versioning: detect schema change on `sqlite3_prepare_v2()`, return `SQLITE_SCHEMA`
- [ ] **47.6** Schema invalidation: re-prepare statements when schema version changes
- [ ] **47.7** Multiple schema support: `main`, `temp`, and attached database schemas
- [ ] **47.8** `PRAGMA writable_schema` — allow direct modification of `sqlite_schema` rows

---

## M48 — Extension Loading

- [ ] **48.1** `sqlite3_enable_load_extension()` — enable/disable extension loading
- [ ] **48.2** `sqlite3_load_extension()` — load a shared library extension
- [ ] **48.3** `sqlite3_auto_extension()` / `sqlite3_cancel_auto_extension()` / `sqlite3_reset_auto_extension()` — automatic extension loading
- [ ] **48.4** Extension entry point convention (`sqlite3_extension_init`)
- [ ] **48.5** VFS `xDlOpen` / `xDlError` / `xDlSym` / `xDlClose` methods for dynamic library loading

---

## M49 — Full-Text Search (FTS5)

- [ ] **49.1** FTS5 virtual table module registration
- [ ] **49.2** `CREATE VIRTUAL TABLE … USING fts5(col1, col2, …)` — create FTS5 table
- [ ] **49.3** FTS5 tokenizer: default (Unicode) tokenizer
- [ ] **49.4** FTS5 query syntax: `MATCH`, column filters, NEAR, AND, OR, NOT, phrase queries
- [ ] **49.5** FTS5 auxiliary functions: `bm25()`, `snippet()`, `highlight()`, `fts5()`
- [ ] **49.6** FTS5 content=, contentless, and delete= options
- [ ] **49.7** FTS5 tokenize= option for custom tokenizers
- [ ] **49.8** FTS5 prefix= and tokenize= options in CREATE VIRTUAL TABLE

---

## M50 — R-Tree Extension

- [ ] **50.1** R-Tree virtual table module registration
- [ ] **50.2** `CREATE VIRTUAL TABLE … USING rtree(id, minX, maxX, minY, maxY, …)` — create R-Tree
- [ ] **50.3** R-Tree insert/delete/query operations
- [ ] **50.4** R-Tree range queries and nearest-neighbor search
- [ ] **50.5** R-Tree integrity check

---

## M51 — DBSTAT Virtual Table

- [ ] **51.1** `sqlite_dbstat` virtual table implementation (b-tree page/row statistics)
- [ ] **51.2** `PRAGMA stats` — report b-tree statistics (debug)

---

## M52 — Percentile Extension

- [ ] **52.1** `median(X)` aggregate function
- [ ] **52.2** `percentile(X, Y)` aggregate function
- [ ] **52.3** `percentile_cont(X, Y)` aggregate function
- [ ] **52.4** `percentile_disc(X, Y)` aggregate function

---

## M53 — Conflict Resolution & Constraints (Complete)

- [ ] **53.1** `ON CONFLICT ROLLBACK` — abort transaction on constraint violation
- [ ] **53.2** `ON CONFLICT ABORT` — abort statement, rollback statement-level changes (default)
- [ ] **53.3** `ON CONFLICT FAIL` — abort statement, keep prior changes
- [ ] **53.4** `ON CONFLICT IGNORE` — skip row that violates constraint, continue
- [ ] **53.5** `ON CONFLICT REPLACE` — delete conflicting row, then insert/update
- [ ] **53.6** `UNIQUE` constraint on INSERT/UPDATE: detect violation, apply conflict resolution
- [ ] **53.7** `NOT NULL` constraint on INSERT/UPDATE: detect violation, apply conflict resolution
- [ ] **53.8** `CHECK` constraint on INSERT/UPDATE: evaluate expression, apply conflict resolution on failure
- [ ] **53.9** `FOREIGN KEY` constraint on INSERT/UPDATE/DELETE: detect violation, apply cascade or restrict
- [ ] **53.10** `ON CONFLICT` clause on `CREATE TABLE` column and table constraints

---

## M54 — Expression Functions (Remaining)

- [ ] **54.1** `printf(format, ...)` / `format(format, ...)` — printf-style string formatting
- [ ] **54.2** `soundex(X)` — SOUNDEX encoding (ifdef)
- [ ] **54.3** `load_extension(X [, Y])` — stub (return error)
- [ ] **54.4** `sqlite_compileoption_get(N)` / `sqlite_compileoption_used(X)` — compile option introspection
- [ ] **54.5** `sqlite_source_id()` — return source ID string
- [ ] **54.6** `unistr(X)` — Unicode escape sequence function
- [ ] **54.7** `sqlite_log(E, M)` — log to error log
- [ ] **54.8** `string_agg(X, Y)` — aggregate alias for `group_concat`
- [ ] **54.9** Ordered-set aggregates: `median(X)`, `percentile(X, Y)`, `percentile_cont(X, Y)`, `percentile_disc(X, Y)`

---

## M55 — Collation Sequences (Complete)

- [ ] **55.1** `NOCASE` collation — case-insensitive ASCII comparison for TEXT
- [ ] **55.2** `RTRIM` collation — right-trimmed comparison for TEXT (already partially in `mem_compare`)
- [ ] **55.3** User-defined collation registration (`sqlite3_create_collation`)
- [ ] **55.4** `PRAGMA collation_list` — enumerate registered collations
- [ ] **55.5** `COLLATE` clause on expressions, column definitions, index definitions
- [ ] **55.6** Collation precedence: explicit COLLATE > column default > comparison operand > BINARY

### M31 — Virtual Tables (Additional Items)

- [ ] **31.10** Built-in eponymous virtual table: `generate_series(start, stop, step)` — integer series generator
- [ ] **31.11** Built-in eponymous virtual table: `carray(pointer, count, ctype)` — C array as virtual table (stub; return error without extension loading)

### M47 — Schema & Catalog (Additional Items)

- [ ] **47.9** `sqlite_dbpage` virtual table — read/write individual database pages
- [ ] **47.10** `sqlite_stat4` system table for ANALYZE column-level statistics (in addition to `sqlite_stat1`)

### M40 — Testing (Additional Items)

- [ ] **40.11** Byte-for-byte file format compatibility: databases written by Rustqlite must pass `PRAGMA integrity_check` when read by C SQLite 3.53.1
- [ ] **40.12** Hot journal crash recovery testing: simulate crash by leaving `-journal` file, verify recovery
- [ ] **40.13** Overflow page chain testing: verify large payloads (blobs, long strings) read/write correctly across overflow pages

### M29 — C-API (Additional Items — Final)

- [ ] **29.66** `sqlite3_stmt_explain(pStmt, mode)` — set or clear EXPLAIN mode on a prepared statement
- [ ] **29.67** `sqlite3_autovacuum_pages()` — callback for calculating auto-vacuum page count
- [ ] **29.68** `sqlite3_column_bytes()` — return number of bytes in a column value
- [ ] **29.69** `sqlite3_column_text()` — return column value as UTF-8 string
- [ ] **29.70** `sqlite3_column_blob()` — return column value as BLOB pointer
- [ ] **29.71** `sqlite3_column_double()` — return column value as f64
- [ ] **29.72** `sqlite3_column_int()` — return column value as i32
- [ ] **29.73** `sqlite3_column_int64()` — return column value as i64
- [ ] **29.74** `sqlite3_column_decltype()` — return declared type of a column
- [ ] **29.75** `sqlite3_column_name16()` — UTF-16 variant of column name
- [ ] **29.76** `sqlite3_column_database_name()` — database name for result column
- [ ] **29.77** `sqlite3_column_table_name()` — table name for result column
- [ ] **29.78** `sqlite3_column_origin_name()` — column name for result column
- [ ] **29.79** `sqlite3_data_count()` — return number of columns in result row (differs from column_count for empty results)
- [ ] **29.80** `sqlite3_bind_text64()` — bind text with explicit length and encoding
- [ ] **29.81** `sqlite3_bind_blob64()` — bind blob with explicit length
- [ ] **29.82** `sqlite3_bind_zeroblob64()` — bind zeroblob with i64 length
- [ ] **29.83** `sqlite3_bind_pointer()` — bind a pointer value
- [ ] **29.84** `sqlite3_result_subtype()` / `sqlite3_value_subtype()` — JSON subtype get/set
- [ ] **29.85** `sqlite3_result_pointer()` — set result to a pointer value
- [ ] **29.86** `sqlite3_value_pointer()` — extract pointer from a value
- [ ] **29.87** `sqlite3_value_dup()` / `sqlite3_value_free()` — duplicate and free value objects
- [ ] **29.88** `sqlite3_value_frombind()` — check if value came from a bind parameter
- [ ] **29.89** `sqlite3_value_nochange()` — check if column value is unchanged (for UPDATE SET)
- [ ] **29.90** `sqlite3_value_encoding()` — return text encoding of a value
- [ ] **29.91** `sqlite3_value_numeric_type()` — attempt to coerce value to numeric type
- [ ] **29.92** `sqlite3_result_error_nomem()` / `sqlite3_result_error_toobig()` — specific error results
- [ ] **29.93** `sqlite3_result_error_code()` — set error from an error code
- [ ] **29.94** `sqlite3_context_db_handle()` — get database connection from function context
- [ ] **29.95** `sqlite3_user_data()` — get user data pointer from function registration
- [ ] **29.96** `sqlite3_set_auxdata()` / `sqlite3_get_auxdata()` — function auxiliary data
- [ ] **29.97** `sqlite3_set_clientdata()` / `sqlite3_get_clientdata()` — per-connection client data
- [ ] **29.98** `sqlite3_aggregate_count()` — deprecated: count of aggregate arguments

### M32 — Pager & VFS (Additional Items — Final)

- [ ] **32.27** Page cache (pcache1) LRU eviction with configurable size limit and pcache methods (`sqlite3_pcache_methods2`)
- [ ] **32.28** Reserve bytes per page: `PRAGMA reserved_bytes` / header byte 20 (reserved space for extensions)
- [ ] **32.29** Database file change counter: stamp on every write transaction (header bytes 24-27)
- [ ] **32.30** Schema cookie: increment on every schema change (header bytes 40-43), used for `SQLITE_SCHEMA` detection
- [ ] **32.31** Lock level protocol: full 5-state locking (UNLOCKED → SHARED → RESERVED → PENDING → EXCLUSIVE) across processes
- [ ] **32.32** `:memory:` database: use in-memory journal (not file-based rollback journal)

### M30 — CLI (Additional Items — Final)

- [ ] **30.36** `.import FILE TABLE` — support CSV and TSV import (with mode-specific parsing)
- [ ] **30.37** `.dump` — SQL dump of entire database (all tables, indexes, schema)

### M47 — Schema & Catalog (Additional Items — Final)

- [ ] **47.11** `sqlite_master` as a read-only view alias for `sqlite_schema` (backwards compatibility)
- [ ] **47.12** `PRAGMA data_version` — read data version from header (readonly, for concurrency detection)

### M28 — VDBE Opcodes (Additional Items — Final)

- [ ] **28.62** External sort for large `ORDER BY`/`GROUP BY` (vdbesort.c: spill-to-disk when sorter exceeds memory limit)
- [ ] **28.63** `RowSetAdd` / `RowSetRead` / `RowSetTest` — row-set optimization for one-pass DELETE/UPDATE (already in M28.9, confirming implementation is the vdbesort.c-style sorted row set)

### M32 — Pager & VFS (Additional Items — Final 2)

- [ ] **32.33** Bit vector (`bitvec.c`): bitmap for record validation during b-tree operations
- [ ] **32.34** In-memory VFS (`MemVfs`): already implemented; ensure it supports `:memory:` databases with in-memory journal (no file I/O)

### M2 — Parser (Additional Items — Final)

- [x] **2.73** AST walker infrastructure (`walker.c`-equivalent): tree traversal for expression optimization, name resolution, and constraint checking
- [x] **2.74** Name resolution (`resolve.c`-equivalent): resolve table/column references, bind parameters, function lookups, and validate types
