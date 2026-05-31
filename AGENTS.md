# AGENTS.md — Rustqlite

Rustqlite is a **full, faithful reimplementation of SQLite3 in Rust**. It is not bindings to libsqlite3
(that's `rusqlite`); it is a from-scratch engine whose internal architecture mirrors upstream SQLite.

## Non-negotiable goals
1. **Faithful**: match SQLite's behavior, results, error messages, and quirks. **No extra features** beyond
   what the pinned upstream SQLite version provides.
2. **Architecture parity**: modules map 1:1 to upstream C source (tokenizer/parser, code generator + query
   planner, VDBE register VM, B-tree, pager + WAL, VFS, utilities). See README for the mapping table.
3. **File-format compatibility**: must open, read, and write `.db` files created by C SQLite, byte-for-byte
   per https://www.sqlite.org/fileformat2.html. `PRAGMA integrity_check` on a rustqlite-written DB must pass
   in C SQLite.
4. **C-API parity**: the public library API mirrors the SQLite C API (`sqlite3_open`, `sqlite3_prepare_v2`,
   `sqlite3_step`, `sqlite3_column_*`, `sqlite3_bind_*`, `sqlite3_finalize`, result codes `SQLITE_*`, …),
   translated to Rust types. Keep names identical where possible.
5. **CLI parity**: `rustqlite-cli` mirrors the `sqlite3` shell — same flags, dot-commands, and output modes.

## Compatibility target
- SQLite **3.53.1** (see `VERSION`). `sqlite3_libversion()` reports `"3.53.1"`,
  `sqlite3_libversion_number()` reports `3053001`, and `sqlite3_sourceid()` reports the pinned source id.
- The on-disk **file format is stable across all of SQLite 3.x**, so format compatibility is not tied to the
  exact point release — but behavior/quirks are pinned to the target above.
- Reference oracle on this machine: the system `sqlite3` binary at `/usr/bin/sqlite3`
  (`3.53.1 2026-05-05`). Differential and round-trip tests compare against it.

## Workspace
- `crates/rustqlite-parser` — SQL text → AST. **pest** PEG grammar ported from upstream `parse.y`;
  expression precedence via pest `PrattParser`. No engine dependency.
- `crates/rustqlite` — the core engine and the public C-API-mirroring library. **Async on tokio.**
- `crates/rustqlite-cli` — the shell. **clap derive**; dot-commands dispatched in the REPL, not as clap
  subcommands.

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

## Dependency rationale
| Crate | Dep | Why |
|---|---|---|
| `rustqlite` | `tokio` | async runtime + async file I/O for the VFS/pager layer |
| `rustqlite` | `async-trait` | object-safe (`dyn`) async methods on the `Vfs`/`VfsFile` traits |
| `rustqlite-parser` | `pest`, `pest_derive` | PEG grammar engine; the locked decision for the parser |
| `rustqlite-cli` | `clap` (derive) | sqlite3-shell-compatible argument parsing |
| `rustqlite-cli` | `rustyline` | line editing + history for interactive mode |

Error types in the core are hand-rolled (no `thiserror`) to keep the dependency surface minimal.

## Build / run / test
- Build: `cargo build`
- Shell: `cargo run -p rustqlite-cli -- <file.db>`
- Tests: `cargo test`  (unit + differential + file-format round-trip + sqllogictest)
- Running SQLite's own suite against rustqlite: see `TESTING.md` (run out-of-tree; do not vendor `.test`
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
- **M2+ — Parser, query path, write path, …**: not yet started (parser crate has a working subset grammar
  as a starting point; full `parse.y` port pending).
