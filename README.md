# Rustqlite

A **faithful, from-scratch reimplementation of SQLite3 in Rust** — not bindings to libsqlite3. The goal is
an engine whose internal architecture mirrors upstream SQLite, whose public library API mirrors the SQLite
**C API**, whose CLI mirrors the `sqlite3` shell, and whose on-disk format is **byte-compatible**: it opens
and correctly reads/writes `.db` files created by C SQLite.

> Compatibility target: **SQLite 3.53.1** (see [`VERSION`](VERSION)). The on-disk format is stable across
> all of SQLite 3.x.

## Workspace

| Crate | Role |
|---|---|
| [`crates/rustqlite-parser`](crates/rustqlite-parser) | SQL text → AST. A **pest** PEG grammar ported from SQLite's `parse.y`; expression precedence via pest's `PrattParser`. No engine dependency. |
| [`crates/rustqlite`](crates/rustqlite) | The core engine and the public **C-API-mirroring** library. Async on **tokio**. |
| [`crates/rustqlite-cli`](crates/rustqlite-cli) | The shell (binary `rustqlite`). **clap derive** for flags; dot-commands dispatched in the REPL. |

Dependency direction: `rustqlite-cli` → `rustqlite` → `rustqlite-parser`.

## Architecture parity (module → upstream C source)

Rustqlite deliberately mirrors SQLite's internal layering so the implementation can be checked against the
upstream source file-by-file.

| Layer | Upstream C | Rust location |
|---|---|---|
| Tokenizer + Parser | `tokenize.c`, `parse.y` | `rustqlite-parser` (`sqlite.pest`, `ast.rs`, `expr.rs`/Pratt, `lib.rs`) |
| Interface / C-API | `main.c`, `vdbeapi.c`, `prepare.c`, `legacy.c` | `rustqlite::capi` |
| Code generator + planner | `build.c`, `select.c`, `where*.c`, … | `rustqlite::codegen` |
| VDBE (bytecode VM) | `vdbe.c`, `vdbeaux.c`, … | `rustqlite::vdbe` |
| B-tree | `btree.c` | `rustqlite::btree` |
| Pager + WAL | `pager.c`, `pcache.c`, `wal.c` | `rustqlite::pager` |
| Record/format codecs | serial types, file format | `rustqlite::format` |
| VFS / OS | `os_unix.c`, `os.c` | `rustqlite::vfs` |
| Type system & affinity | `vdbemem.c`, `analyze.c` | `rustqlite::types` |
| Built-in functions | `func.c`, `date.c`, `printf.c` | `rustqlite::func` |
| PRAGMA | `pragma.c` | `rustqlite::pragma` |
| Schema / catalog | `build.c`, `prepare.c` | `rustqlite::schema` |
| Utilities | `util.c`, `hash.c`, `utf.c` | `rustqlite::util` |
| Shell | `shell.c.in` | `rustqlite-cli` |

## Async model

The VFS and pager I/O are **async on tokio**. The `sqlite3_*` C-API functions keep their **synchronous
signatures** and drive the async engine to completion via a process-global runtime (`block_on`), so the
public surface stays C-API-faithful while I/O is async underneath. Concurrency stays sqlite3-compatible
(many readers, single writer); tokio adds async I/O and parallel connections, not new SQL semantics.

## Quick start

```sh
cargo build

# Create a database with the reference engine, then read it with rustqlite:
sqlite3 demo.db "create table t(a, b); insert into t values (1, 'x'), (2, 'y');"
cargo run -p rustqlite-cli -- demo.db ".tables"
cargo run -p rustqlite-cli -- demo.db ".schema"

# Library version (mirrors the C API):
cargo run -p rustqlite-cli -- -version
```

## Roadmap (milestones)

Built bottom-up so each layer is verified against real SQLite before the next.

- **M0 — Scaffold** ✅ — workspace, crates, docs, CI, `sqlite3_libversion*`.
- **M1 — File format (read)** 🚧 — format codecs + async VFS + read-only pager + table-b-tree read cursor;
  open a real C-SQLite `.db` and read `sqlite_schema`.
- **M2 — Parser** — full `parse.y` → pest grammar + AST + Pratt expressions.
- **M3 — Read query path** — codegen + VDBE for `SELECT`, affinity, scalar funcs, `EXPLAIN`.
- **M4 — Write path** — pager write + rollback journal + crash recovery; DML/DDL; b-tree balance.
- **M5 — Indexes & planner basics** · **M6 — Transactions & richer SQL** · **M7 — Advanced SQL**
  · **M8 — WAL & durability** · **M9 — Conformance hardening**.

See [`AGENTS.md`](AGENTS.md) for contributor guidance and [`TESTING.md`](TESTING.md) for how to run
SQLite's own suite against rustqlite (out-of-tree; the `.test` files are **not** vendored).

## License

Apache-2.0. See [`LICENSE.md`](LICENSE.md).
