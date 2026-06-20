//! VDBE execution dispatch (mirrors the giant `switch` in `vdbe.c`).
//!
//! A [`Vdbe`] holds a compiled [`Program`], the register file, and the open cursor table, and
//! steps opcodes with [`step`](Vdbe::step) until it yields a result row (`ResultRow`) or halts.
//! The dispatch is an exhaustive `match`; opcodes outside M3a's read query path return an error
//! rather than panicking, keeping the match total as the opcode set grows.
//!
//! The executor is synchronous to its caller (`sqlite3_step`) but reaches the pages it needs
//! through the async pager via the cursor's `Arc<Pager>`, so `step` is itself `async` and the
//! C-API drives it with `block_on`.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

use crate::btree::{self, IndexCursor, TableCursor};
use crate::error::{Error, Result};
use crate::format::{decode_record, encode_record, TextEncoding};
use crate::func;
use crate::func::aggregate::{Accumulator, AggregateKind};
use crate::pager::Pager;
use crate::types::{Affinity, Collation, Value};

use super::compare::{apply_affinity, mem_compare};
use super::cursor::VdbeCursor;
use super::ephemeral::Ephemeral;
use super::opcode::Opcode;
use super::program::{
    p5_to_aff, Instruction, Program, P4, P5_ISUPDATE, P5_JUMPIFNULL, P5_NCHANGE, P5_NULLEQ,
    P5_STOREP2, P5_UNIQUE,
};
use super::sorter::Sorter;
use super::KeyField;

/// `SQLITE_MAX_LENGTH` — the default cap on the size of a string or BLOB (`sqlite3.c`). A
/// `randomblob(N)` request larger than this is rejected exactly as SQLite does (`SQLITE_TOOBIG`,
/// reported as "string or blob too big").
const SQLITE_MAX_LENGTH: i64 = 1_000_000_000;

/// Per-statement runtime state for the volatile / connection-state scalar functions.
///
/// These functions (`random`, `randomblob`, `changes`, `total_changes`, `last_insert_rowid`)
/// can't live in the pure, deterministic [`crate::func`] registry, so the executor special-cases
/// them and reaches into this context. Keeping them here keeps `func/` unit-testable.
///
/// The PRNG is a splitmix64 (the same construction the fp-rendering fuzz test uses) so it needs
/// no `rand` dependency and works under the crate's `overflow-checks = true` dev profile via
/// `wrapping_*`. It is seeded once per construction from `std::process::id()` mixed with a
/// process-global atomic counter, so successive statements — and successive calls within one
/// statement — produce distinct values, while avoiding `std::time`. The values are not
/// cryptographically strong, which matches SQLite's own non-cryptographic `random()`.
pub struct RuntimeCtx {
    /// splitmix64 state, advanced on each draw.
    rng_state: u64,
    /// `changes()` — rows changed by the most recent write statement (`OP_Insert` bumps it).
    pub changes: i64,
    /// `total_changes()` — rows changed since the connection opened.
    pub total_changes: i64,
    /// `last_insert_rowid()` — rowid of the last successful insert (persists across statements).
    pub last_insert_rowid: i64,
    /// `true` once a real `Insert` (one without `P5_ISUPDATE`) has bumped `last_insert_rowid`
    /// in this statement. The C-API publish path uses it to decide whether to overwrite the
    /// connection's `last_insert_rowid` — a statement that only ran the write side of an
    /// `UPDATE` must not clobber it.
    pub did_insert: bool,
}

impl Default for RuntimeCtx {
    fn default() -> RuntimeCtx {
        RuntimeCtx::new()
    }
}

impl RuntimeCtx {
    /// A fresh context with a distinct PRNG seed.
    pub fn new() -> RuntimeCtx {
        use std::sync::atomic::{AtomicU64, Ordering};
        // A process-global counter guarantees two `RuntimeCtx`es built in the same process (even
        // back-to-back) get different seeds without consulting the clock.
        static SEED_COUNTER: AtomicU64 = AtomicU64::new(0);
        let bump = SEED_COUNTER.fetch_add(1, Ordering::Relaxed);
        // Mix the pid and counter through splitmix64's finalizer so even adjacent seeds diverge.
        let mut seed =
            (u64::from(std::process::id()) << 32) ^ bump.wrapping_mul(0x9e3779b97f4a7c15);
        seed = (seed ^ (seed >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        seed = (seed ^ (seed >> 27)).wrapping_mul(0x94d049bb133111eb);
        RuntimeCtx {
            rng_state: seed ^ (seed >> 31),
            changes: 0,
            total_changes: 0,
            last_insert_rowid: 0,
            did_insert: false,
        }
    }

    /// Advance the splitmix64 PRNG and return the next 64-bit draw.
    fn next_u64(&mut self) -> u64 {
        self.rng_state = self.rng_state.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.rng_state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }

    /// `random()` — a signed 64-bit pseudo-random integer.
    pub fn next_i64(&mut self) -> i64 {
        self.next_u64() as i64
    }

    /// `n` pseudo-random bytes for `randomblob(n)`.
    pub fn random_bytes(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            out.extend_from_slice(&self.next_u64().to_le_bytes());
        }
        out.truncate(n);
        out
    }
}

/// SQLite's `randomblob(N)` length rule: a NULL or `N < 1` argument yields a single byte, and a
/// request larger than `SQLITE_MAX_LENGTH` is rejected (`SQLITE_TOOBIG`). Returns the clamped
/// length, or an error mirroring the oracle's "string or blob too big".
fn randomblob_len(arg: Option<&Value>) -> Result<usize> {
    let n = match arg {
        Some(v) if !v.is_null() => v.as_i64(),
        _ => 1, // missing or NULL → 1 byte (matches the oracle)
    };
    if n > SQLITE_MAX_LENGTH {
        return Err(Error::msg("string or blob too big"));
    }
    Ok(n.max(1) as usize)
}

/// The outcome of one [`Vdbe::step`].
#[derive(Debug, PartialEq, Eq)]
pub enum StepResult {
    /// A result row is available; read it with [`Vdbe::result_count`] / [`Vdbe::result_value`].
    Row,
    /// The statement has finished.
    Done,
}

/// A running VDBE program instance — the execution state behind a `sqlite3_stmt`.
pub struct Vdbe {
    program: Arc<Program>,
    /// The database pager. `None` for a constant `SELECT` (no `FROM`), which never opens a
    /// cursor, so it can run without any open database.
    pager: Option<Arc<Pager>>,
    encoding: TextEncoding,
    pc: usize,
    regs: Vec<Value>,
    cursors: Vec<Option<VdbeCursor>>,
    result_start: usize,
    result_count: usize,
    halted: bool,
    /// Per-`(cursor, rowid)` decoded-record cache so successive `Column` reads of one row decode
    /// the payload once.
    decoded: Option<(usize, i64, Vec<Value>)>,
    /// Runtime state for the volatile / connection-state functions (PRNG + change counters).
    ctx: RuntimeCtx,
    /// The b-tree rootpage each open cursor sits on, keyed by cursor number. `OpenWrite` records
    /// it so the write opcodes (`NewRowid`/`Insert`) can reach the b-tree by root (the write path
    /// goes through `btree::table_insert`/`max_rowid` directly on the pager, not the read cursor).
    cursor_root: HashMap<i32, u32>,
    /// Set to `true` once a write `Transaction` opcode has opened a write transaction, so `Halt`
    /// knows to commit and a step error knows to roll back. Read-only programs leave this `false`.
    write_txn: bool,
    /// Per-accumulator state keyed by the register that holds the aggregate's result (the `p3`
    /// of `AggStep` / `p1` of `AggFinal`). SQLite stores this inside the `Mem` cell itself; we
    /// keep it in a side table so the `Value` type stays storage-class-only. An entry is created
    /// lazily by `AggStep` on its first call for a given register, and consumed by `AggFinal`.
    aggregates: HashMap<usize, Accumulator>,
}

impl Vdbe {
    /// Create an instance ready to run `program`. `pager` is `None` for a constant `SELECT`.
    pub fn new(program: Arc<Program>, pager: Option<Arc<Pager>>) -> Vdbe {
        let encoding = pager
            .as_ref()
            .map_or(TextEncoding::Utf8, |p| p.text_encoding());
        let nreg = program.num_registers.max(1);
        Vdbe {
            program,
            pager,
            encoding,
            pc: 0,
            regs: vec![Value::Null; nreg],
            cursors: Vec::new(),
            result_start: 0,
            result_count: 0,
            halted: false,
            decoded: None,
            ctx: RuntimeCtx::new(),
            cursor_root: HashMap::new(),
            write_txn: false,
            aggregates: HashMap::new(),
        }
    }

    /// A snapshot of the change counters after this program ran (for `sqlite3_changes` /
    /// `sqlite3_last_insert_rowid`). `(changes, total_changes, last_insert_rowid)`.
    pub fn change_counts(&self) -> (i64, i64, i64, bool) {
        (
            self.ctx.changes,
            self.ctx.total_changes,
            self.ctx.last_insert_rowid,
            self.ctx.did_insert,
        )
    }

    /// Reset to the start so the program can be re-run (`sqlite3_reset`).
    ///
    /// The PRNG state in `ctx` is deliberately NOT reset: SQLite's randomness is global, so
    /// re-running a statement keeps advancing the sequence rather than repeating it.
    pub fn reset(&mut self) {
        self.pc = 0;
        for r in &mut self.regs {
            *r = Value::Null;
        }
        self.cursors.clear();
        self.result_start = 0;
        self.result_count = 0;
        self.halted = false;
        self.decoded = None;
        self.cursor_root.clear();
        self.write_txn = false;
        self.aggregates.clear();
    }

    /// Number of columns in the current result row.
    pub fn result_count(&self) -> usize {
        self.result_count
    }

    /// The value of result column `i` in the current row.
    pub fn result_value(&self, i: usize) -> Value {
        self.regs
            .get(self.result_start + i)
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// Run until the next result row or completion.
    ///
    /// On an error inside a write transaction, the transaction is rolled back (discarding the
    /// uncommitted changes and deleting the journal) before the error propagates — mirroring how
    /// `sqlite3VdbeHalt` aborts a statement that errored (`OE_Abort`/`OE_Rollback`).
    pub async fn step(&mut self) -> Result<StepResult> {
        match self.step_inner().await {
            Ok(r) => Ok(r),
            Err(e) => {
                if self.write_txn {
                    if let Some(pager) = &self.pager {
                        let _ = pager.rollback().await;
                    }
                    self.write_txn = false;
                }
                self.halted = true;
                Err(e)
            }
        }
    }

    async fn step_inner(&mut self) -> Result<StepResult> {
        if self.halted {
            return Ok(StepResult::Done);
        }
        let program = Arc::clone(&self.program);
        loop {
            let pc = self.pc;
            let inst: &Instruction = program
                .instructions
                .get(pc)
                .ok_or_else(|| Error::msg("program counter ran off the end of the program"))?;
            let (p1, p2, p3, p5) = (inst.p1, inst.p2, inst.p3, inst.p5);

            match inst.opcode {
                Opcode::Init => self.pc = p2 as usize,
                Opcode::Goto => self.pc = p2 as usize,
                Opcode::Halt => {
                    // A successful Halt commits an open write transaction (the durable commit
                    // point); read-only programs have no transaction to commit. Mirrors the
                    // CommitPhase in `sqlite3VdbeHalt` for a non-erroring statement.
                    if self.write_txn {
                        if let Some(pager) = &self.pager {
                            pager.commit().await?;
                        }
                        self.write_txn = false;
                    }
                    self.halted = true;
                    return Ok(StepResult::Done);
                }
                Opcode::HaltIfNull => {
                    // p3 names a register; p4 carries the constraint message. If the register
                    // is NULL, abort the statement with a NOT NULL constraint error (the
                    // in-flight write transaction is rolled back by `Halt` semantics).
                    if self.regs[p3 as usize].is_null() {
                        let msg = match &inst.p4 {
                            P4::Text(s) => s.clone(),
                            _ => "NOT NULL constraint failed".to_string(),
                        };
                        return Err(Error::new(crate::error::ResultCode::Constraint, msg));
                    }
                    self.pc += 1;
                }
                Opcode::Transaction => {
                    // p2 != 0 opens a WRITE transaction (the rollback journal). A read
                    // transaction is implicit in our engine, so p2 == 0 is a no-op marker.
                    if p2 != 0 {
                        let pager = self
                            .pager
                            .clone()
                            .ok_or_else(|| Error::msg("no database is open"))?;
                        pager.begin_write().await?;
                        self.write_txn = true;
                    }
                    self.pc += 1;
                }

                Opcode::OpenRead => {
                    let pager = self
                        .pager
                        .clone()
                        .ok_or_else(|| Error::msg("no database is open"))?;
                    // An `OpenRead` with `P4::KeyInfo` opens an index b-tree; a bare
                    // `OpenRead` (no KeyInfo) opens a table b-tree — same as M3a.
                    if let P4::KeyInfo(ki) = &inst.p4 {
                        let cursor = IndexCursor::new(pager, p2 as u32, ki.clone());
                        self.set_cursor(p1 as usize, VdbeCursor::Index(cursor));
                    } else {
                        let cursor = TableCursor::new(pager, p2 as u32);
                        self.set_cursor(p1 as usize, VdbeCursor::Table(cursor));
                    }
                    self.cursor_root.insert(p1, p2 as u32);
                    self.pc += 1;
                }
                Opcode::OpenWrite => {
                    // For the first write slice the cursor only needs to remember its rootpage:
                    // the insert itself goes through `btree::table_insert` on the pager, and any
                    // reads-after-write reuse the read-cursor machinery. We still open a table
                    // cursor (so a Rewind/Column after the insert would work) and record the root
                    // for NewRowid/Insert.
                    let pager = self
                        .pager
                        .clone()
                        .ok_or_else(|| Error::msg("no database is open"))?;
                    if let P4::KeyInfo(ki) = &inst.p4 {
                        let cursor = IndexCursor::new(pager, p2 as u32, ki.clone());
                        self.set_cursor(p1 as usize, VdbeCursor::Index(cursor));
                    } else {
                        let cursor = TableCursor::new(pager, p2 as u32);
                        self.set_cursor(p1 as usize, VdbeCursor::Table(cursor));
                    }
                    self.cursor_root.insert(p1, p2 as u32);
                    self.pc += 1;
                }
                Opcode::OpenWriteReg => {
                    // Open a write cursor on the b-tree whose root page is the value of `r[p2]`.
                    // The M5.1 first slice uses this for `CREATE INDEX`'s populate pass: the
                    // index b-tree's root is computed by `CreateBtree` and lands in a register;
                    // this opcode opens a cursor on that value.
                    let pager = self
                        .pager
                        .clone()
                        .ok_or_else(|| Error::msg("no database is open"))?;
                    let root = self.regs[p2 as usize].as_i64() as u32;
                    let cursor = if p3 == 1 {
                        VdbeCursor::Table(TableCursor::new(pager, root))
                    } else {
                        VdbeCursor::Index(IndexCursor::new(pager, root, Vec::new()))
                    };
                    self.set_cursor(p1 as usize, cursor);
                    self.cursor_root.insert(p1, root);
                    self.pc += 1;
                }
                Opcode::Close => {
                    if let Some(slot) = self.cursors.get_mut(p1 as usize) {
                        *slot = None;
                    }
                    self.pc += 1;
                }

                Opcode::Rewind => {
                    // Rewind the cursor and jump to `p2` if it is empty. Works on
                    // table/index cursors and ephemeral/sorter cursors.
                    let cur = self.cursor_mut(p1)?;
                    match cur {
                        VdbeCursor::Table(c) => {
                            c.rewind().await?;
                            let valid = c.is_valid();
                            self.decoded = None;
                            if valid {
                                self.pc += 1;
                            } else {
                                self.pc = p2 as usize;
                            }
                        }
                        VdbeCursor::Index(c) => {
                            c.rewind().await?;
                            let valid = c.is_valid();
                            if valid {
                                self.pc += 1;
                            } else {
                                self.pc = p2 as usize;
                            }
                        }
                        VdbeCursor::Sorter(_) => {
                            return Err(Error::msg("Rewind is not valid on a sorter cursor"))
                        }
                        VdbeCursor::Ephemeral(e) => {
                            if e.rewind() {
                                self.pc += 1;
                            } else {
                                self.pc = p2 as usize;
                            }
                        }
                    }
                }
                Opcode::Next => {
                    // Advance the cursor; jump to `p2` on a valid row, fall through on
                    // exhaustion. Works on both table and index cursors.
                    let cur = self.cursor_mut(p1)?;
                    match cur {
                        VdbeCursor::Table(c) => {
                            c.next().await?;
                            let valid = c.is_valid();
                            self.decoded = None;
                            if valid {
                                self.pc = p2 as usize;
                            } else {
                                self.pc += 1;
                            }
                        }
                        VdbeCursor::Index(c) => {
                            c.next().await?;
                            let valid = c.is_valid();
                            if valid {
                                self.pc = p2 as usize;
                            } else {
                                self.pc += 1;
                            }
                        }
                        VdbeCursor::Sorter(s) => {
                            s.next();
                            if s.is_valid() {
                                self.pc = p2 as usize;
                            } else {
                                self.pc += 1;
                            }
                        }
                        VdbeCursor::Ephemeral(e) => {
                            e.next();
                            if e.is_valid() {
                                self.pc = p2 as usize;
                            } else {
                                self.pc += 1;
                            }
                        }
                    }
                }

                Opcode::Rowid => {
                    let rowid = self.table_cursor(p1)?.rowid()?;
                    self.regs[p2 as usize] = Value::Int(rowid);
                    self.pc += 1;
                }
                Opcode::NotExists => {
                    let target = self.regs[p3 as usize].as_i64();
                    let found = self.table_cursor_mut(p1)?.seek_rowid(target).await?;
                    self.decoded = None;
                    if found {
                        self.pc += 1;
                    } else {
                        self.pc = p2 as usize;
                    }
                }
                Opcode::Column => {
                    let val = self.column(p1 as usize, p2 as usize).await?;
                    self.regs[p3 as usize] = val;
                    self.pc += 1;
                }

                // ---- index seeks (M5.1) ----
                Opcode::SeekGE | Opcode::SeekGT | Opcode::SeekLE | Opcode::SeekLT => {
                    let op = match inst.opcode {
                        Opcode::SeekGE => btree::index_cursor::SeekOp::Ge,
                        Opcode::SeekGT => btree::index_cursor::SeekOp::Gt,
                        Opcode::SeekLE => btree::index_cursor::SeekOp::Le,
                        Opcode::SeekLT => btree::index_cursor::SeekOp::Lt,
                        _ => unreachable!(),
                    };
                    let n = p4_len(&inst.p4);
                    let key: Vec<Value> = self.regs[p3 as usize..p3 as usize + n].to_vec();
                    let cursor = self.index_cursor_mut(p1)?;
                    let found = cursor.seek(op, &key).await?;
                    if !found {
                        self.pc = p2 as usize;
                    } else {
                        self.pc += 1;
                    }
                }
                Opcode::IdxGE | Opcode::IdxGT | Opcode::IdxLE | Opcode::IdxLT => {
                    // Compare the current entry's prefix (first `n` values of the key record)
                    // against the search key; jump to `p2` when the comparison falls in the
                    // "outside the range" direction. Together with the preceding `Seek*` this
                    // implements the indexed comparison operators.
                    //
                    // The post-seek boundary check is *inverted* from the operator name:
                    //   SeekGE+IdxGT  → `WHERE col > X`   → jump when entry `<=` X (we want strict >).
                    //   SeekGE+IdxGE  → `WHERE col >= X`  → jump when entry `<`  X (already handled by SeekGE).
                    //   SeekLE+IdxLT  → `WHERE col < X`   → jump when entry `>=` X.
                    //   SeekLE+IdxLE  → `WHERE col <= X`  → jump when entry `>`  X.
                    //
                    // For an `=` operator (the M5.1 first slice's only shape), we use
                    // SeekGE+IdxGT where the `IdxGT` jumps when entry `>` key (i.e., NOT
                    // matching). Note the inverted semantics: the post-seek opcode is named
                    // for the boundary direction we *don't* want, not the one we do.
                    let key_info = self.index_key_info(p1);
                    let n = p4_len(&inst.p4);
                    let search_key: Vec<Value> = self.regs[p3 as usize..p3 as usize + n].to_vec();
                    let cursor = self.index_cursor(p1)?;
                    let payload = cursor.payload();
                    let values = decode_record(payload, self.encoding)?;
                    let prefix = &values[..values.len().saturating_sub(1).min(n)];
                    let ord = compare_prefix(prefix, &search_key, &key_info);
                    let jump = match inst.opcode {
                        // The "GE"/"GT"/"LE"/"LT" suffixes name the "leave the loop" direction
                        // — the boundary beyond which the entry is no longer in range.
                        // SeekGE+IdxGE means `>=`; the post-seek check fires only on `==`,
                        // which is in range. So IdxGE jumps on `<` (Less).
                        Opcode::IdxGE => matches!(ord, Ordering::Less),
                        // SeekGE+IdxGT means `>`; the post-seek check fires on `==` and `<`.
                        // Inverted: jump when entry is `>` (the next entry in index order, if any,
                        // is the first one strictly greater; we want to stop before it).
                        Opcode::IdxGT => matches!(ord, Ordering::Greater),
                        // SeekLE+IdxLE means `<=`; post-seek check on `>`.
                        Opcode::IdxLE => matches!(ord, Ordering::Greater),
                        // SeekLE+IdxLT means `<`; post-seek check on `==` and `>`.
                        Opcode::IdxLT => matches!(ord, Ordering::Greater),
                        _ => unreachable!(),
                    };
                    if jump {
                        self.pc = p2 as usize;
                    } else {
                        self.pc += 1;
                    }
                }
                Opcode::IdxRowid => {
                    let rowid = self.index_cursor(p1)?.rowid()?;
                    self.regs[p2 as usize] = Value::Int(rowid);
                    self.pc += 1;
                }
                Opcode::IdxInsert => {
                    let pager = self
                        .pager
                        .clone()
                        .ok_or_else(|| Error::msg("no database is open"))?;
                    let root = self.cursor_root_of(p1)?;
                    let record = match &self.regs[p2 as usize] {
                        Value::Blob(b) => b.clone(),
                        _ => return Err(Error::msg("IdxInsert expects a record blob in p2")),
                    };
                    let key_info = self.index_key_info(p1);
                    let unique = p5 & P5_UNIQUE != 0;
                    match btree::index_insert(pager, root, &record, &key_info, unique).await {
                        Ok(()) => {}
                        Err(e) if e.code == crate::error::ResultCode::Constraint && unique => {
                            let msg = match &inst.p4 {
                                P4::Text(s) => s.clone(),
                                _ => "UNIQUE constraint failed".to_string(),
                            };
                            return Err(Error::new(crate::error::ResultCode::Constraint, msg));
                        }
                        Err(other) => return Err(other),
                    }
                    if p5 & P5_NCHANGE != 0 {
                        self.ctx.changes += 1;
                        self.ctx.total_changes += 1;
                    }
                    self.pc += 1;
                }
                Opcode::IdxDelete => {
                    let pager = self
                        .pager
                        .clone()
                        .ok_or_else(|| Error::msg("no database is open"))?;
                    let root = self.cursor_root_of(p1)?;
                    let n = p3 as usize;
                    let key: Vec<Value> = self.regs[p2 as usize..p2 as usize + n].to_vec();
                    let key_record = encode_record(&key);
                    let key_info = self.index_key_info(p1);
                    btree::index_leaf_delete(&pager, root, &key_record, &key_info).await?;
                    if let Some(cur) = self.cursors.get_mut(p1 as usize).and_then(|c| c.as_mut()) {
                        if let VdbeCursor::Index(c) = cur {
                            c.mark_deleted();
                        }
                    }
                    self.pc += 1;
                }

                Opcode::ResultRow => {
                    self.result_start = p1 as usize;
                    self.result_count = p2 as usize;
                    self.pc += 1;
                    return Ok(StepResult::Row);
                }

                // ---- value loads ----
                Opcode::Integer => {
                    self.regs[p2 as usize] = Value::Int(p1 as i64);
                    self.pc += 1;
                }
                Opcode::Int64 => {
                    self.regs[p2 as usize] = Value::Int(as_p4_int(&inst.p4));
                    self.pc += 1;
                }
                Opcode::Real => {
                    self.regs[p2 as usize] = Value::Real(as_p4_real(&inst.p4));
                    self.pc += 1;
                }
                Opcode::String8 => {
                    self.regs[p2 as usize] = Value::Text(as_p4_text(&inst.p4));
                    self.pc += 1;
                }
                Opcode::Null => {
                    if p3 > p2 {
                        for i in p2..=p3 {
                            self.regs[i as usize] = Value::Null;
                        }
                    } else {
                        self.regs[p2 as usize] = Value::Null;
                    }
                    self.pc += 1;
                }
                Opcode::Blob => {
                    self.regs[p2 as usize] = Value::Blob(as_p4_blob(&inst.p4));
                    self.pc += 1;
                }

                // ---- arithmetic: r[p3] = r[p2] OP r[p1] ----
                Opcode::Add
                | Opcode::Subtract
                | Opcode::Multiply
                | Opcode::Divide
                | Opcode::Remainder => {
                    let result = arith(
                        inst.opcode,
                        &self.regs[p2 as usize],
                        &self.regs[p1 as usize],
                    );
                    self.regs[p3 as usize] = result;
                    self.pc += 1;
                }
                Opcode::Concat => {
                    // r[p3] = r[p2] || r[p1]
                    let result = concat(&self.regs[p2 as usize], &self.regs[p1 as usize]);
                    self.regs[p3 as usize] = result;
                    self.pc += 1;
                }

                // ---- bitwise: r[p3] = r[p2] OP r[p1] ----
                Opcode::BitAnd | Opcode::BitOr | Opcode::ShiftLeft | Opcode::ShiftRight => {
                    self.regs[p3 as usize] = bitwise(
                        inst.opcode,
                        &self.regs[p2 as usize],
                        &self.regs[p1 as usize],
                    );
                    self.pc += 1;
                }
                Opcode::BitNot => {
                    self.regs[p2 as usize] = match &self.regs[p1 as usize] {
                        Value::Null => Value::Null,
                        v => Value::Int(!v.as_i64()),
                    };
                    self.pc += 1;
                }

                // ---- comparisons: test r[p3] OP r[p1]; jump to p2, or store the boolean in p2 ----
                Opcode::Eq | Opcode::Ne | Opcode::Lt | Opcode::Le | Opcode::Gt | Opcode::Ge => {
                    let res = self.compare(inst.opcode, p1, p3, p5, &inst.p4);
                    if p5 & P5_STOREP2 != 0 {
                        self.regs[p2 as usize] = match res {
                            None => Value::Null,
                            Some(b) => Value::Int(i64::from(b)),
                        };
                        self.pc += 1;
                    } else {
                        let take = match res {
                            Some(b) => b,
                            None => p5 & P5_JUMPIFNULL != 0,
                        };
                        if take {
                            self.pc = p2 as usize;
                        } else {
                            self.pc += 1;
                        }
                    }
                }

                // ---- logic ----
                Opcode::And => {
                    let r = and3(
                        truth(&self.regs[p1 as usize]),
                        truth(&self.regs[p2 as usize]),
                    );
                    self.regs[p3 as usize] = bool3_to_value(r);
                    self.pc += 1;
                }
                Opcode::Or => {
                    let r = or3(
                        truth(&self.regs[p1 as usize]),
                        truth(&self.regs[p2 as usize]),
                    );
                    self.regs[p3 as usize] = bool3_to_value(r);
                    self.pc += 1;
                }
                Opcode::Not => {
                    self.regs[p2 as usize] = match truth(&self.regs[p1 as usize]) {
                        None => Value::Null,
                        Some(b) => Value::Int(i64::from(!b)),
                    };
                    self.pc += 1;
                }
                Opcode::IsNull => {
                    if self.regs[p1 as usize].is_null() {
                        self.pc = p2 as usize;
                    } else {
                        self.pc += 1;
                    }
                }
                Opcode::NotNull => {
                    if !self.regs[p1 as usize].is_null() {
                        self.pc = p2 as usize;
                    } else {
                        self.pc += 1;
                    }
                }
                Opcode::If => {
                    let jump = match truth(&self.regs[p1 as usize]) {
                        Some(b) => b,
                        None => p3 != 0,
                    };
                    if jump {
                        self.pc = p2 as usize;
                    } else {
                        self.pc += 1;
                    }
                }
                Opcode::IfNot => {
                    let jump = match truth(&self.regs[p1 as usize]) {
                        Some(b) => !b,
                        None => p3 != 0,
                    };
                    if jump {
                        self.pc = p2 as usize;
                    } else {
                        self.pc += 1;
                    }
                }

                // ---- register moves ----
                Opcode::Copy => {
                    for i in 0..=p3 {
                        self.regs[(p2 + i) as usize] = self.regs[(p1 + i) as usize].clone();
                    }
                    self.pc += 1;
                }
                Opcode::SCopy => {
                    self.regs[p2 as usize] = self.regs[p1 as usize].clone();
                    self.pc += 1;
                }
                Opcode::Move => {
                    for i in 0..p3 {
                        self.regs[(p2 + i) as usize] =
                            std::mem::replace(&mut self.regs[(p1 + i) as usize], Value::Null);
                    }
                    self.pc += 1;
                }

                Opcode::Affinity => {
                    let affs = as_p4_text(&inst.p4);
                    for (k, ch) in affs.bytes().enumerate().take(p2 as usize) {
                        let idx = p1 as usize + k;
                        let v = std::mem::replace(&mut self.regs[idx], Value::Null);
                        self.regs[idx] = apply_affinity(v, char_to_aff(ch));
                    }
                    self.pc += 1;
                }
                Opcode::RealAffinity => {
                    if let Value::Int(i) = self.regs[p1 as usize] {
                        self.regs[p1 as usize] = Value::Real(i as f64);
                    }
                    self.pc += 1;
                }

                Opcode::Function => {
                    let name = as_p4_text(&inst.p4);
                    let nargs = p5 as usize;
                    let start = p2 as usize;
                    let args: Vec<Value> = self.regs[start..start + nargs].to_vec();
                    // The volatile / connection-state functions need runtime state, so they are
                    // intercepted here before the pure `func::call_scalar` registry. Names are
                    // case-insensitive (codegen stores the original case in p4).
                    let result = match name.to_ascii_lowercase().as_str() {
                        "random" => Value::Int(self.ctx.next_i64()),
                        "randomblob" => {
                            Value::Blob(self.ctx.random_bytes(randomblob_len(args.first())?))
                        }
                        "changes" => Value::Int(self.ctx.changes),
                        "total_changes" => Value::Int(self.ctx.total_changes),
                        "last_insert_rowid" => Value::Int(self.ctx.last_insert_rowid),
                        "sqlite_version" => Value::Text(crate::SQLITE_VERSION.to_string()),
                        _ => func::call_scalar(&name, &args)?,
                    };
                    self.regs[p3 as usize] = result;
                    self.pc += 1;
                }

                // ---- record building ----
                Opcode::MakeRecord => {
                    let start = p1 as usize;
                    let cnt = p2 as usize;
                    let bytes = encode_record(&self.regs[start..start + cnt]);
                    self.regs[p3 as usize] = Value::Blob(bytes);
                    self.pc += 1;
                }

                // ---- write path ----
                Opcode::CreateBtree => {
                    let pager = self
                        .pager
                        .clone()
                        .ok_or_else(|| Error::msg("no database is open"))?;
                    // p3 selects the b-tree type: 1 = table, 0 = index. M5.1: the index
                    // case allocates a leaf-index page (mirrors `sqlite3BtreeCreateTable` for
                    // `idxType == SQLITE_IDXTYPE_APPDEF`).
                    let root = if p3 == 1 {
                        btree::create_table_btree(&pager).await?
                    } else {
                        btree::create_index_btree(&pager).await?
                    };
                    self.regs[p2 as usize] = Value::Int(i64::from(root));
                    self.pc += 1;
                }
                Opcode::Destroy => {
                    let pager = self
                        .pager
                        .clone()
                        .ok_or_else(|| Error::msg("no database is open"))?;
                    let root = p1 as u32;
                    btree::btree_destroy(&pager, root).await?;
                    self.pc += 1;
                }
                Opcode::Clear => {
                    let pager = self
                        .pager
                        .clone()
                        .ok_or_else(|| Error::msg("no database is open"))?;
                    let root = p1 as u32;
                    btree::btree_clear(&pager, root).await?;
                    // `OP_Clear` bumps both change counters by the number of deleted rows;
                    // the runtime context records those changes. We approximate by counting
                    // rows removed. For now we set changes/total_changes via the same loop
                    // path when needed; this fast path is kept for EXPLAIN parity.
                    self.pc += 1;
                }
                Opcode::NewRowid => {
                    // For an ephemeral cursor (RETURNING buffer), allocate the next integer key
                    // from the cursor itself. For real b-tree cursors, ask the pager.
                    if self
                        .cursors
                        .get(p1 as usize)
                        .and_then(|c| c.as_ref())
                        .is_some_and(VdbeCursor::is_ephemeral)
                    {
                        let slot = self.cursors.get_mut(p1 as usize).unwrap().as_mut().unwrap();
                        let rowid = slot.as_ephemeral_mut().unwrap().next_rowid();
                        self.regs[p2 as usize] = Value::Int(rowid);
                        self.pc += 1;
                    } else {
                        let pager = self
                            .pager
                            .clone()
                            .ok_or_else(|| Error::msg("no database is open"))?;
                        let root = self.cursor_root_of(p1)?;
                        let next = btree::max_rowid(&pager, root).await?.wrapping_add(1);
                        self.regs[p2 as usize] = Value::Int(next);
                        self.pc += 1;
                    }
                }
                Opcode::Insert => {
                    // Ephemeral cursor: insert directly into the in-memory buffer.
                    if self
                        .cursors
                        .get(p1 as usize)
                        .and_then(|c| c.as_ref())
                        .is_some_and(VdbeCursor::is_ephemeral)
                    {
                        let record = match &self.regs[p2 as usize] {
                            Value::Blob(b) => b.clone(),
                            _ => return Err(Error::msg("Insert expects a record blob in p2")),
                        };
                        let slot = self.cursors.get_mut(p1 as usize).unwrap().as_mut().unwrap();
                        let eph = slot.as_ephemeral_mut().unwrap();
                        let rowid = self.regs[p3 as usize].as_i64();
                        eph.insert(rowid, record);
                        self.pc += 1;
                    } else {
                        let pager = self
                            .pager
                            .clone()
                            .ok_or_else(|| Error::msg("no database is open"))?;
                        let root = self.cursor_root_of(p1)?;
                        let record = match &self.regs[p2 as usize] {
                            Value::Blob(b) => b.clone(),
                            _ => return Err(Error::msg("Insert expects a record blob in p2")),
                        };
                        let rowid = self.regs[p3 as usize].as_i64();
                        btree::table_insert(&pager, root, rowid, &record).await?;
                        // `P5_ISUPDATE` means the Insert is the write side of an `UPDATE`: bump
                        // `changes` (one row updated) but do NOT clobber `last_insert_rowid` —
                        // SQLite only updates that for an actual `INSERT`.
                        if p5 & P5_ISUPDATE == 0 {
                            self.ctx.last_insert_rowid = rowid;
                            self.ctx.did_insert = true;
                        }
                        self.ctx.changes += 1;
                        self.ctx.total_changes += 1;
                        self.decoded = None;
                        self.pc += 1;
                    }
                }
                Opcode::Delete => {
                    // Sanity-check the cursor exists; the actual delete goes through
                    // `TableCursor::delete_current`, which addresses the leaf directly.
                    self.cursor_root_of(p1)?;
                    let cur = self.table_cursor_mut(p1)?;
                    let rowid_before = cur.rowid()?;
                    cur.delete_current().await?;
                    // `P5_ISUPDATE` means this `Delete` is the read-side of an `UPDATE` (the
                    // `Insert` that immediately follows is the one that bumps `changes`).
                    // We still publish the rowid for `total_changes` visibility in the test
                    // layer; `last_insert_rowid` is left untouched (matches upstream).
                    if p5 & P5_ISUPDATE == 0 {
                        self.ctx.changes += 1;
                        self.ctx.last_insert_rowid = rowid_before;
                    }
                    self.ctx.total_changes += 1;
                    self.decoded = None;
                    self.pc += 1;
                }
                Opcode::SetCookie => {
                    // p2 selects the cookie; only the schema cookie (1) is emitted today. The
                    // value to write is the operand p3 (the new cookie value computed at codegen).
                    if let Some(pager) = &self.pager {
                        let value = p3 as u32;
                        pager.with_header_mut(|h| h.schema_cookie = value);
                    }
                    self.pc += 1;
                }
                Opcode::ParseSchema => {
                    // Reload the in-memory catalog so later statements see the new object. In our
                    // architecture each prepared statement re-reads `sqlite_schema` at prepare
                    // time (the catalog is not cached on the connection), so the reload here is a
                    // no-op marker; we keep the opcode for faithfulness and EXPLAIN parity.
                    self.pc += 1;
                }

                Opcode::OpenEphemeral => {
                    // Open an in-memory ephemeral table with p2 fields and stash it under cursor p1.
                    let nfield = p2 as usize;
                    self.set_cursor(p1 as usize, VdbeCursor::Ephemeral(Ephemeral::new(nfield, self.encoding)));
                    self.pc += 1;
                }

                // ---- sorter ----
                Opcode::SorterOpen => {
                    let keys = match &inst.p4 {
                        P4::KeyInfo(k) => k.clone(),
                        _ => Vec::new(),
                    };
                    self.set_cursor(
                        p1 as usize,
                        VdbeCursor::Sorter(Sorter::new(keys, self.encoding)),
                    );
                    self.pc += 1;
                }
                Opcode::SorterInsert => {
                    let rec = match &self.regs[p2 as usize] {
                        Value::Blob(b) => b.clone(),
                        _ => return Err(Error::msg("SorterInsert expects a record blob")),
                    };
                    self.sorter_mut(p1)?.insert(rec);
                    self.pc += 1;
                }
                Opcode::SorterSort => {
                    let nonempty = self.sorter_mut(p1)?.sort()?;
                    if nonempty {
                        self.pc += 1;
                    } else {
                        self.pc = p2 as usize;
                    }
                }
                Opcode::SorterData => {
                    self.sorter_mut(p1)?.data()?;
                    self.pc += 1;
                }
                Opcode::SorterNext => {
                    let sorter = self.sorter_mut(p1)?;
                    sorter.next();
                    if sorter.is_valid() {
                        self.pc = p2 as usize;
                    } else {
                        self.pc += 1;
                    }
                }

                // ---- LIMIT / OFFSET ----
                Opcode::DecrJumpZero => {
                    let v = self.regs[p1 as usize].as_i64() - 1;
                    self.regs[p1 as usize] = Value::Int(v);
                    if v == 0 {
                        self.pc = p2 as usize;
                    } else {
                        self.pc += 1;
                    }
                }
                Opcode::IfPos => {
                    let v = self.regs[p1 as usize].as_i64();
                    if v > 0 {
                        self.regs[p1 as usize] = Value::Int(v - p3 as i64);
                        self.pc = p2 as usize;
                    } else {
                        self.pc += 1;
                    }
                }

                // ---- aggregates (M6) ----
                Opcode::AggStep => {
                    // `AggStep p1 p2 p3 p4=FuncDef(kind) p5=nArg`: accumulate one row's
                    // arguments from `r[p2 .. p2+nArg]` into the accumulator at `r[p3]`. `p1`
                    // is reserved (upstream uses it to mark `AggInverse`); we only emit the
                    // step form. The accumulator is created lazily on the first call for a given
                    // register and reused for subsequent calls in the same group.
                    let kind = match &inst.p4 {
                        P4::FuncDef(k) => *k,
                        _ => return Err(Error::msg("AggStep requires a FuncDef p4")),
                    };
                    let n_arg = p5 as usize;
                    let is_count_star = kind == AggregateKind::Count && n_arg == 0;
                    let args: Vec<Value> = if is_count_star {
                        Vec::new()
                    } else {
                        self.regs[p2 as usize..p2 as usize + n_arg].to_vec()
                    };
                    let acc = self
                        .aggregates
                        .entry(p3 as usize)
                        .or_insert_with(|| Accumulator::new(kind));
                    acc.step(&args, is_count_star);
                    self.pc += 1;
                }
                Opcode::AggFinal => {
                    // `AggFinal p1 p2 p3 p4=FuncDef(kind)`: finalize the accumulator at `r[p1]`
                    // and store the result value there. `p2` is the original argument count
                    // (unused by us, like upstream) and `p4` is the function descriptor.
                    let kind = match &inst.p4 {
                        P4::FuncDef(k) => *k,
                        _ => return Err(Error::msg("AggFinal requires a FuncDef p4")),
                    };
                    let result = match self.aggregates.remove(&(p1 as usize)) {
                        Some(acc) => finalize_accumulator(acc, kind),
                        None => empty_aggregate_result(kind),
                    };
                    self.regs[p1 as usize] = result;
                    self.pc += 1;
                }
            }
        }
    }

    // ---- cursor helpers ----

    fn set_cursor(&mut self, idx: usize, cursor: VdbeCursor) {
        if idx >= self.cursors.len() {
            self.cursors.resize_with(idx + 1, || None);
        }
        self.cursors[idx] = Some(cursor);
    }

    /// The b-tree rootpage that cursor `p1` was opened on (recorded by `OpenRead`/`OpenWrite`).
    /// Used by the write opcodes (`NewRowid`/`Insert`), which reach the b-tree by root.
    fn cursor_root_of(&self, p1: i32) -> Result<u32> {
        self.cursor_root
            .get(&p1)
            .copied()
            .ok_or_else(|| Error::msg("cursor has no recorded rootpage"))
    }

    fn table_cursor(&self, p1: i32) -> Result<&TableCursor> {
        self.cursors
            .get(p1 as usize)
            .and_then(|c| c.as_ref())
            .and_then(|c| c.as_table())
            .ok_or_else(|| Error::msg("cursor is not an open table cursor"))
    }

    fn table_cursor_mut(&mut self, p1: i32) -> Result<&mut TableCursor> {
        self.cursors
            .get_mut(p1 as usize)
            .and_then(|c| c.as_mut())
            .and_then(|c| c.as_table_mut())
            .ok_or_else(|| Error::msg("cursor is not an open table cursor"))
    }

    fn index_cursor(&self, p1: i32) -> Result<&IndexCursor> {
        self.cursors
            .get(p1 as usize)
            .and_then(|c| c.as_ref())
            .and_then(|c| c.as_index())
            .ok_or_else(|| Error::msg("cursor is not an open index cursor"))
    }

    fn index_cursor_mut(&mut self, p1: i32) -> Result<&mut IndexCursor> {
        self.cursors
            .get_mut(p1 as usize)
            .and_then(|c| c.as_mut())
            .and_then(|c| c.as_index_mut())
            .ok_or_else(|| Error::msg("cursor is not an open index cursor"))
    }

    /// The per-column `KeyInfo` for the open index cursor at `p1`, if any. Used by the
    /// `IdxInsert`/`IdxDelete` opcodes so the page-level index insertion/deletion compares
    /// keys with the same collation as the cursor used for seek.
    fn index_key_info(&self, p1: i32) -> Vec<KeyField> {
        self.cursors
            .get(p1 as usize)
            .and_then(|c| c.as_ref())
            .and_then(|c| c.as_index())
            .map(|c| c.key_info().to_vec())
            .unwrap_or_default()
    }

    /// Mutably borrow any cursor by index (table, index, sorter, or ephemeral). Used by the
    /// `Rewind` / `Next` opcodes which need to work across all cursor kinds.
    fn cursor_mut(&mut self, p1: i32) -> Result<&mut VdbeCursor> {
        self.cursors
            .get_mut(p1 as usize)
            .and_then(|c| c.as_mut())
            .ok_or_else(|| Error::msg("cursor is not open"))
    }

    fn sorter_mut(&mut self, p1: i32) -> Result<&mut Sorter> {
        self.cursors
            .get_mut(p1 as usize)
            .and_then(|c| c.as_mut())
            .and_then(|c| c.as_sorter_mut())
            .ok_or_else(|| Error::msg("cursor is not an open sorter"))
    }

    /// `Column p1 p2`: the value of column `col` of cursor `idx`'s current row.
    async fn column(&mut self, idx: usize, col: usize) -> Result<Value> {
                // Sorter cursors read from their decoded current record.
                if self
                    .cursors
                    .get(idx)
                    .and_then(|c| c.as_ref())
                    .is_some_and(VdbeCursor::is_sorter)
                {
                    return Ok(self.cursors[idx]
                        .as_ref()
                        .unwrap()
                        .as_sorter()
                        .unwrap()
                        .column(col));
                }

                // Ephemeral cursors read from their decoded current record.
                if self
                    .cursors
                    .get(idx)
                    .and_then(|c| c.as_ref())
                    .is_some_and(VdbeCursor::is_ephemeral)
                {
                    let slot = self.cursors[idx].as_mut().unwrap();
                    slot.as_ephemeral_mut().unwrap().data()?;
                    return Ok(slot.as_ephemeral().unwrap().column(col));
                }

                // Index cursors (used by WITHOUT ROWID tables and by secondary indexes): read
                // the `col`-th value from the current key record. The cursor caches its
                // payload already (IndexCursor::refresh_payload keeps it current), so this is
                // a straight decode.
                if self
                    .cursors
                    .get(idx)
                    .and_then(|c| c.as_ref())
                    .is_some_and(VdbeCursor::is_index)
                {
                    let payload = self.cursors[idx]
                        .as_ref()
                        .unwrap()
                        .as_index()
                        .unwrap()
                        .payload()
                        .to_vec();
                    if payload.is_empty() {
                        return Ok(Value::Null);
                    }
                    let vals = decode_record(&payload, self.encoding)?;
                    return Ok(vals.get(col).cloned().unwrap_or(Value::Null));
                }

        let rowid = self.table_cursor(idx as i32)?.rowid()?;
        let hit = matches!(&self.decoded, Some((ci, rid, _)) if *ci == idx && *rid == rowid);
        if !hit {
            let payload = self.table_cursor(idx as i32)?.payload().await?;
            let vals = decode_record(&payload, self.encoding)?;
            self.decoded = Some((idx, rowid, vals));
        }
        Ok(self
            .decoded
            .as_ref()
            .unwrap()
            .2
            .get(col)
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// The three-valued truth of `r[p3] OP r[p1]`: `None` means NULL (unknown), which only
    /// happens when an operand is NULL and the `NULLEQ` flag is not set.
    fn compare(&self, op: Opcode, p1: i32, p3: i32, p5: u8, p4: &P4) -> Option<bool> {
        let left = &self.regs[p3 as usize]; // r[p3]
        let right = &self.regs[p1 as usize]; // r[p1]
        let nulleq = p5 & P5_NULLEQ != 0;

        if !nulleq && (left.is_null() || right.is_null()) {
            return None; // NULL (unknown)
        }

        // Apply comparison affinity (not for the NULL-equality operators).
        let (l, r) = if nulleq {
            (left.clone(), right.clone())
        } else {
            match p5_to_aff(p5) {
                Some(af) => (
                    apply_affinity(left.clone(), af),
                    apply_affinity(right.clone(), af),
                ),
                None => (left.clone(), right.clone()),
            }
        };
        let coll = collation_of(p4);
        let ord = mem_compare(&l, &r, coll);
        Some(match op {
            Opcode::Eq => ord == Ordering::Equal,
            Opcode::Ne => ord != Ordering::Equal,
            Opcode::Lt => ord == Ordering::Less,
            Opcode::Le => ord != Ordering::Greater,
            Opcode::Gt => ord == Ordering::Greater,
            Opcode::Ge => ord != Ordering::Less,
            _ => unreachable!("compare called with non-comparison opcode"),
        })
    }
}

fn collation_of(p4: &P4) -> Collation {
    match p4 {
        P4::Symbol(name) => Collation::from_name(name).unwrap_or(Collation::Binary),
        _ => Collation::Binary,
    }
}

/// Finalize an accumulator into its result `Value`, mirroring upstream's `xFinal` path. This is
/// the per-aggregate "read out the state" logic that `AggFinal` dispatches to.
fn finalize_accumulator(acc: Accumulator, kind: AggregateKind) -> Value {
    match kind {
        AggregateKind::Count => Value::Int(acc.count),
        AggregateKind::Sum => {
            if acc.count == 0 {
                Value::Null
            } else if acc.has_real {
                Value::Real(acc.sum_r)
            } else {
                Value::Int(acc.sum_i)
            }
        }
        AggregateKind::Total => {
            // `total()` is always REAL and never NULL (0.0 for an empty set).
            if acc.has_real {
                Value::Real(acc.sum_r)
            } else {
                Value::Real(acc.sum_i as f64)
            }
        }
        AggregateKind::Avg => {
            if acc.count == 0 {
                Value::Null
            } else {
                let total = if acc.has_real {
                    acc.sum_r
                } else {
                    acc.sum_i as f64
                };
                Value::Real(total / acc.count as f64)
            }
        }
        AggregateKind::Min | AggregateKind::Max => acc.best.unwrap_or(Value::Null),
        AggregateKind::GroupConcat => {
            acc.concat.map(Value::Text).unwrap_or(Value::Null)
        }
    }
}

/// The result of finalizing an aggregate that never received an `AggStep` call (an empty group).
/// Mirrors the oracle's behavior for `SELECT count(*) FROM t WHERE 0=1` (0) vs `sum` (NULL) vs
/// `total` (0.0) etc.
fn empty_aggregate_result(kind: AggregateKind) -> Value {
    match kind {
        AggregateKind::Count => Value::Int(0),
        AggregateKind::Sum | AggregateKind::Avg | AggregateKind::Min | AggregateKind::Max
        | AggregateKind::GroupConcat => Value::Null,
        AggregateKind::Total => Value::Real(0.0),
    }
}

fn as_p4_int(p4: &P4) -> i64 {
    match p4 {
        P4::Int(i) => *i,
        _ => 0,
    }
}
fn as_p4_real(p4: &P4) -> f64 {
    match p4 {
        P4::Real(r) => *r,
        _ => 0.0,
    }
}
fn as_p4_text(p4: &P4) -> String {
    match p4 {
        P4::Text(s) | P4::Symbol(s) => s.clone(),
        _ => String::new(),
    }
}
fn as_p4_blob(p4: &P4) -> Vec<u8> {
    match p4 {
        P4::Blob(b) => b.clone(),
        _ => Vec::new(),
    }
}

/// The integer in `P4::Int`, used to encode the `nField` operand of `SeekGE`/`IdxGE` etc.
/// Defaults to 0 when the operand is not an integer (the engine uses 0 to mean "no key" for
/// those opcodes).
fn p4_len(p4: &P4) -> usize {
    match p4 {
        P4::Int(n) => (*n).max(0) as usize,
        _ => 0,
    }
}

/// Compare two key prefixes field-by-field using the per-column collation in `key_info`.
/// `prefix` is a slice of `Value` taken from the on-disk key record; `key` is the unpacked
/// register vector. If one vector is shorter than the other, the shorter one is considered less
/// — matching the prefix-vs-full comparison used by `index_insert`.
fn compare_prefix(prefix: &[Value], key: &[Value], key_info: &[KeyField]) -> Ordering {
    let n = prefix.len().min(key.len());
    for i in 0..n {
        let coll = key_info
            .get(i)
            .map(|f| f.collation)
            .unwrap_or(Collation::Binary);
        match mem_compare(&prefix[i], &key[i], coll) {
            Ordering::Equal => {}
            non_eq => return non_eq,
        }
    }
    prefix.len().cmp(&key.len())
}

fn char_to_aff(ch: u8) -> Affinity {
    match ch.to_ascii_uppercase() {
        b'T' => Affinity::Text,
        b'N' => Affinity::Numeric,
        b'I' => Affinity::Integer,
        b'R' => Affinity::Real,
        _ => Affinity::Blob,
    }
}

// ---- three-valued truth ----

/// `None` = NULL (unknown), `Some(bool)` = a definite truth value (numeric value != 0).
fn truth(v: &Value) -> Option<bool> {
    if v.is_null() {
        None
    } else {
        Some(v.as_f64() != 0.0)
    }
}

fn and3(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}
fn or3(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}
fn bool3_to_value(b: Option<bool>) -> Value {
    match b {
        None => Value::Null,
        Some(t) => Value::Int(i64::from(t)),
    }
}

// ---- arithmetic ----

enum Num {
    I(i64),
    R(f64),
}

fn to_num(v: &Value) -> Option<Num> {
    match v {
        Value::Null => None,
        Value::Int(i) => Some(Num::I(*i)),
        Value::Real(r) => Some(Num::R(*r)),
        Value::Text(s) => Some(parse_num(s)),
        // A BLOB is coerced through its bytes-as-text, then the same leading-prefix parse as TEXT
        // (`sqlite3VdbeMemNumerify` runs `sqlite3AtoF`/`Atoi64` over the raw bytes), so e.g.
        // `x'2d35'` (the bytes `"-5"`) arithmetises as `-5`, matching the oracle.
        Value::Blob(b) => Some(parse_num(&String::from_utf8_lossy(b))),
    }
}

/// Coerce a TEXT operand to a number for arithmetic, using SQLite's prefix parsing
/// (`'10garbage'` → 10, `'abc'` → 0).
fn parse_num(s: &str) -> Num {
    match crate::util::numeric_prefix(s).0 {
        Some(Value::Int(i)) => Num::I(i),
        Some(Value::Real(r)) => Num::R(r),
        _ => Num::I(0),
    }
}

fn num_f(n: &Num) -> f64 {
    match n {
        Num::I(i) => *i as f64,
        Num::R(r) => *r,
    }
}

/// `b OP a`, where the opcode encodes `r[p3] = r[p2] OP r[p1]` (so `b = r[p2]`, `a = r[p1]`).
fn arith(op: Opcode, b: &Value, a: &Value) -> Value {
    let (nb, na) = match (to_num(b), to_num(a)) {
        (Some(x), Some(y)) => (x, y),
        _ => return Value::Null, // any NULL operand → NULL
    };

    if let (Num::I(ib), Num::I(ia)) = (&nb, &na) {
        let (ib, ia) = (*ib, *ia);
        match op {
            Opcode::Add => {
                return ib
                    .checked_add(ia)
                    .map(Value::Int)
                    .unwrap_or(Value::Real(ib as f64 + ia as f64))
            }
            Opcode::Subtract => {
                return ib
                    .checked_sub(ia)
                    .map(Value::Int)
                    .unwrap_or(Value::Real(ib as f64 - ia as f64))
            }
            Opcode::Multiply => {
                return ib
                    .checked_mul(ia)
                    .map(Value::Int)
                    .unwrap_or(Value::Real(ib as f64 * ia as f64))
            }
            Opcode::Divide => {
                if ia == 0 {
                    return Value::Null;
                }
                if ia == -1 && ib == i64::MIN {
                    return Value::Real(ib as f64 / ia as f64);
                }
                return Value::Int(ib / ia);
            }
            Opcode::Remainder => {
                if ia == 0 {
                    return Value::Null;
                }
                let ia = if ia == -1 { 1 } else { ia };
                return Value::Int(ib % ia);
            }
            _ => unreachable!(),
        }
    }

    // Floating-point arithmetic.
    let rb = num_f(&nb);
    let ra = num_f(&na);
    let r = match op {
        Opcode::Add => rb + ra,
        Opcode::Subtract => rb - ra,
        Opcode::Multiply => rb * ra,
        Opcode::Divide => {
            if ra == 0.0 {
                return Value::Null;
            }
            rb / ra
        }
        Opcode::Remainder => {
            let ia = ra as i64;
            let ib = rb as i64;
            if ia == 0 {
                return Value::Null;
            }
            let ia = if ia == -1 { 1 } else { ia };
            (ib % ia) as f64
        }
        _ => unreachable!(),
    };
    if r.is_nan() {
        Value::Null
    } else {
        Value::Real(r)
    }
}

/// SQLite bitwise operators: operands are coerced to integers via `sqlite3VdbeIntValue`
/// semantics, any NULL operand yields NULL, and shifts follow the upstream rules (negative
/// counts reverse direction; counts `>= 64` saturate to 0/-1).
fn bitwise(op: Opcode, b: &Value, a: &Value) -> Value {
    if b.is_null() || a.is_null() {
        return Value::Null;
    }
    let mut ia = b.as_i64();
    let mut ib = a.as_i64();
    let mut opcode = op;

    // SQLite treats ShiftRight as ShiftLeft+1 in opcode space and flips op/count when negative.
    // We mirror the same logic literally: negative count reverses direction.
    if ib != 0 && matches!(op, Opcode::ShiftLeft | Opcode::ShiftRight) {
        if ib < 0 {
            // `assert( OP_ShiftRight==OP_ShiftLeft+1 )` in upstream; our enum order matches.
            opcode = if op == Opcode::ShiftLeft {
                Opcode::ShiftRight
            } else {
                Opcode::ShiftLeft
            };
            ib = if ib > -64 { -ib } else { 64 };
        }
        if ib >= 64 {
            ia = if ia >= 0 || opcode == Opcode::ShiftLeft {
                0
            } else {
                -1
            };
        } else {
            let mut ua = ia as u64;
            if opcode == Opcode::ShiftLeft {
                ua <<= ib;
            } else {
                ua >>= ib;
                // Sign-extend on right shift of a negative number.
                if ia < 0 {
                    ua |= u64::MAX << (64 - ib);
                }
            }
            ia = ua as i64;
        }
    }

    match op {
        Opcode::BitAnd => Value::Int(ia & ib),
        Opcode::BitOr => Value::Int(ia | ib),
        Opcode::ShiftLeft | Opcode::ShiftRight => Value::Int(ia),
        _ => unreachable!(),
    }
}

/// `r[p3] = r[p2] || r[p1]` — text concatenation; NULL if either operand is NULL.
fn concat(b: &Value, a: &Value) -> Value {
    match (b.to_text(), a.to_text()) {
        (Some(tb), Some(ta)) => Value::Text(tb + &ta),
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::func::aggregate::AggregateKind;
    use crate::vdbe::program::{Instruction, Program, P4};

    fn inst(opcode: Opcode, p1: i32, p2: i32, p3: i32) -> Instruction {
        Instruction::new(opcode, p1, p2, p3)
    }

    fn inst_with(
        opcode: Opcode,
        p1: i32,
        p2: i32,
        p3: i32,
        p4: P4,
        p5: u8,
    ) -> Instruction {
        Instruction {
            opcode,
            p1,
            p2,
            p3,
            p4,
            p5,
        }
    }

    /// A hand-built `SELECT count(*) FROM (constant 3-row scan)` program that exercises the
    /// `AggStep` → `AggFinal` → `ResultRow` path without needing a pager or a real table. The
    /// "table" is a 3-iteration loop that loads the literal 1 into r2 each row and steps the
    /// accumulator at r3, then finalizes and emits.
    async fn run_aggregate_program(kind: AggregateKind, n_arg: u8, is_count_star: bool) -> Value {
        // r1 = loop counter / scratch, r2 = arg value, r3 = accumulator / result.
        // Layout:
        //   0 Init           -> 10
        //   1 Integer 0       r1      (loop counter)
        //   2 Integer 1       r2      (arg value; reused each row)
        //   3 AggStep  0 r2 r3  FuncDef(kind) p5=n_arg
        //   4 Add      r1 r2 r1      (r1 = r1 + 1) — actually r1 = r2 + r1 = 1 + r1
        //   5 Lt       r1 9 r1       (if r1 < 3 jump back to step; SQLite compares r[p3] OP r[p1])
        //   6 AggFinal r3 0 0  FuncDef(kind)
        //   7 ResultRow r3 1
        //   8 Halt
        //   9 (loop body target — but we use a forward jump model)
        //   10 Transaction
        //   11 Goto -> 1
        //
        // We use a simpler countdown shape: emit 3 explicit AggStep calls.
        let mut prog = Program {
            instructions: Vec::new(),
            num_registers: 4,
        };
        prog.instructions.push(inst(Opcode::Init, 0, 8, 0));
        // Setup: load the literal 1 into r2 (the per-row argument).
        prog.instructions.push(inst(Opcode::Integer, 1, 2, 0));
        // Three rows → three AggStep calls.
        for _ in 0..3 {
            let n_arg_eff = if is_count_star { 0 } else { n_arg };
            prog.instructions.push(inst_with(
                Opcode::AggStep,
                0,
                2,
                3,
                P4::FuncDef(kind),
                n_arg_eff,
            ));
        }
        prog.instructions.push(inst_with(
            Opcode::AggFinal,
            3,
            0,
            0,
            P4::FuncDef(kind),
            0,
        ));
        prog.instructions.push(inst(Opcode::ResultRow, 3, 1, 0));
        prog.instructions.push(inst(Opcode::Halt, 0, 0, 0));
        prog.instructions.push(inst(Opcode::Transaction, 0, 0, 0));
        prog.instructions.push(inst(Opcode::Goto, 0, 1, 0));

        let mut v = Vdbe::new(Arc::new(prog), None);
        let mut rows: Vec<Vec<Value>> = Vec::new();
        while let StepResult::Row = v.step().await.expect("step") {
            let n = v.result_count();
            rows.push((0..n).map(|i| v.result_value(i)).collect());
        }
        assert_eq!(rows.len(), 1, "aggregate query should emit exactly one row");
        rows[0][0].clone()
    }

    #[tokio::test]
    async fn agg_step_count_star_three_rows() {
        let r = run_aggregate_program(AggregateKind::Count, 1, true).await;
        assert_eq!(r, Value::Int(3));
    }

    #[tokio::test]
    async fn agg_step_sum_three_ones() {
        let r = run_aggregate_program(AggregateKind::Sum, 1, false).await;
        assert_eq!(r, Value::Int(3));
    }

    #[tokio::test]
    async fn agg_step_total_three_ones() {
        let r = run_aggregate_program(AggregateKind::Total, 1, false).await;
        assert_eq!(r, Value::Real(3.0));
    }

    #[tokio::test]
    async fn agg_step_min_max() {
        // min/max of three 1s is 1.
        let r = run_aggregate_program(AggregateKind::Min, 1, false).await;
        assert_eq!(r, Value::Int(1));
        let r = run_aggregate_program(AggregateKind::Max, 1, false).await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn agg_final_empty_group_count() {
        // An AggFinal with no preceding AggStep should yield the empty-group result.
        let prog = Program {
            instructions: vec![
                inst(Opcode::Init, 0, 5, 0),
                inst_with(
                    Opcode::AggFinal,
                    1,
                    0,
                    0,
                    P4::FuncDef(AggregateKind::Count),
                    0,
                ),
                inst(Opcode::ResultRow, 1, 1, 0),
                inst(Opcode::Halt, 0, 0, 0),
                inst(Opcode::Transaction, 0, 0, 0),
                inst(Opcode::Goto, 0, 1, 0),
            ],
            num_registers: 2,
        };
        let mut v = Vdbe::new(Arc::new(prog), None);
        let mut rows: Vec<Vec<Value>> = Vec::new();
        while let StepResult::Row = v.step().await.expect("step") {
            let n = v.result_count();
            rows.push((0..n).map(|i| v.result_value(i)).collect());
        }
        assert_eq!(rows, vec![vec![Value::Int(0)]]);
    }

    #[tokio::test]
    async fn agg_final_empty_group_sum_total() {
        let prog = Program {
            instructions: vec![
                inst(Opcode::Init, 0, 5, 0),
                inst_with(
                    Opcode::AggFinal,
                    1,
                    0,
                    0,
                    P4::FuncDef(AggregateKind::Sum),
                    0,
                ),
                inst_with(
                    Opcode::AggFinal,
                    2,
                    0,
                    0,
                    P4::FuncDef(AggregateKind::Total),
                    0,
                ),
                inst(Opcode::ResultRow, 1, 2, 0),
                inst(Opcode::Halt, 0, 0, 0),
                inst(Opcode::Transaction, 0, 0, 0),
                inst(Opcode::Goto, 0, 1, 0),
            ],
            num_registers: 3,
        };
        let mut v = Vdbe::new(Arc::new(prog), None);
        let mut rows: Vec<Vec<Value>> = Vec::new();
        while let StepResult::Row = v.step().await.expect("step") {
            let n = v.result_count();
            rows.push((0..n).map(|i| v.result_value(i)).collect());
        }
        // sum of empty set is NULL, total of empty set is 0.0.
        assert_eq!(rows, vec![vec![Value::Null, Value::Real(0.0)]]);
    }
}
