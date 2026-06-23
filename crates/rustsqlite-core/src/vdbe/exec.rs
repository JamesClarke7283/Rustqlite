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
use super::cursor::{PseudoCursor, VdbeCursor};
use super::ephemeral::Ephemeral;
use super::opcode::Opcode;
use super::program::{
    p5_to_aff, FkLookup, Instruction, Program, P4, P5_ISUPDATE, P5_JUMPIFNULL, P5_NCHANGE, P5_NULLEQ,
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

/// A saved execution frame for `OP_Program` (mirrors `VdbeFrame` in `vdbe.c`).
///
/// When `OP_Program` invokes a sub-VDBE (a trigger program, view body, or other sub-program),
/// the parent's running state is captured into a `VdbeFrame` and pushed on [`Vdbe::frames`].
/// The executor then installs the sub-program with a fresh register file and cursor table and
/// runs it. When the sub-program halts (its `OP_Halt` with `p1 == SQLITE_OK`), the frame is
/// popped and the parent's state is restored so execution resumes at the instruction following
/// the `OP_Program`.
///
/// `param_base` is the calling `OP_Program`'s `p1` operand — the register in the PARENT frame
/// where the sub-program's inputs begin. `OP_Param p1 p2` inside the sub-program copies the
/// parent's register at index `param_base + p1` into the current frame's `r[p2]`.
struct VdbeFrame {
    /// The parent program (restored on pop).
    program: Arc<Program>,
    /// The parent program counter to resume at (the address of the instruction following the
    /// `OP_Program`).
    pc: usize,
    /// The parent's register file.
    regs: Vec<Value>,
    /// The parent's cursor table.
    cursors: Vec<Option<VdbeCursor>>,
    /// The parent's `cursor_root` map (rootpage per open cursor).
    cursor_root: HashMap<i32, u32>,
    /// The parent's decoded-record cache.
    decoded: Option<(usize, i64, Vec<Value>)>,
    /// The parent's per-accumulator state (aggregate sub-programs are rare but supported).
    aggregates: HashMap<usize, Accumulator>,
    /// The parent's `Once`-fired set.
    once_done: std::collections::HashSet<usize>,
    /// The parent's `write_txn` flag.
    write_txn: bool,
    /// The `p1` operand of the calling `OP_Program` — the base register in the parent frame
    /// for `OP_Param` resolution.
    param_base: i32,
    /// The `p5` operand of the calling `OP_Program` — the recursion token. Non-zero for
    /// trigger sub-programs; the recursive-trigger guard matches on this value.
    token: u8,
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
    /// The connection's shared autocommit flag, set by `sqlite3_prepare_v2` so `OP_Halt` and
    /// `OP_AutoCommit` can consult and mutate it. `None` means "always autocommit" (the default
    /// for unit tests that don't care about transactions — they get the legacy behavior where
    /// every `Halt` commits). Mirrors `db->autoCommit` in `main.c`.
    autocommit: Option<Arc<std::sync::Mutex<bool>>>,
    /// The connection's shared `isTransactionSavepoint` flag, set by `sqlite3_prepare_v2` so
    /// `OP_Savepoint` and `OP_AutoCommit` can consult and mutate it. `None` means "no transaction
    /// savepoint tracking" (the unit-test default — SAVEPOINT auto-starting a transaction still
    /// works but RELEASE-of-outermost can't tell whether to commit; the test code paths never
    /// hit that combination). Mirrors `db->isTransactionSavepoint` in `main.c`.
    is_transaction_savepoint: Option<Arc<std::sync::Mutex<bool>>>,
    /// Per-accumulator state keyed by the register that holds the aggregate's result (the `p3`
    /// of `AggStep` / `p1` of `AggFinal`). SQLite stores this inside the `Mem` cell itself; we
    /// keep it in a side table so the `Value` type stays storage-class-only. An entry is created
    /// lazily by `AggStep` on its first call for a given register, and consumed by `AggFinal`.
    aggregates: HashMap<usize, Accumulator>,
    /// The result of the most recent `OP_Compare` (`-1/0/+1`), read by the immediately following
    /// `OP_Jump`. Mirrors the `iCompare` global in `vdbe.c`. `None` means no `Compare` has run
    /// yet (a defensive default; the codegen always emits `Jump` right after `Compare`).
    last_compare: Ordering,
    /// Addresses of `OP_Once` instructions that have already fired in the current run. A repeat
    /// encounter of a listed address jumps to its `p2`; an unlisted address falls through and is
    /// added. Cleared by [`Self::reset`]. Mirrors the `aOp[0].p1`-cookie trick in `vdbe.c`'s
    /// `OP_Once` but using an explicit set keyed by instruction address for clarity.
    once_done: std::collections::HashSet<usize>,
    /// The stack of saved parent frames for `OP_Program`. The top of the stack is the most
    /// recently entered sub-program's parent. Empty for a flat (non-sub-program) statement.
    /// Mirrors `p->pFrame` in `vdbe.c` (kept as a `Vec` so a pop is a simple `pop()`).
    frames: Vec<VdbeFrame>,
    /// The name of the implicit "statement savepoint" opened at the start of a write statement
    /// so an `OE_Abort` constraint violation can roll back just the statement's changes (not
    /// the entire transaction) when the connection is inside an explicit `BEGIN`. `None` when
    /// no statement savepoint is active (autocommit mode, read-only statements, or a statement
    /// that already completed its setup). Mirrors the statement sub-journal in upstream's
    /// `sqlite3VdbeMakeReady`/`sqlite3VdbeHalt`.
    stmt_savepoint: Option<String>,
    /// Per-constraint OE override set by `HaltIfNull`/`IdxInsert` when a per-constraint `ON
    /// CONFLICT <action>` clause fires (M12.9). When `Some`, `step()`'s error path uses this OE
    /// instead of `program.default_oe` for the cleanup, mirroring how upstream's `OP_Halt` sets
    /// `p->errorAction = pOp->p2` before calling `sqlite3VdbeHalt`. Cleared on each `step()`
    /// entry (a fresh statement starts with no override).
    oe_override: Option<u8>,
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
            autocommit: None,
            is_transaction_savepoint: None,
            aggregates: HashMap::new(),
            last_compare: Ordering::Equal,
            once_done: std::collections::HashSet::new(),
            frames: Vec::new(),
            stmt_savepoint: None,
            oe_override: None,
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

    /// Install the connection's shared autocommit flag so `OP_AutoCommit` and `OP_Halt` can
    /// consult and mutate it. Called by `sqlite3_prepare_v2` after constructing the Vdbe.
    /// When never called, the Vdbe runs in legacy "always autocommit" mode (every `Halt` commits
    /// an open write transaction) — this is the mode unit tests use.
    pub fn set_autocommit_handle(&mut self, handle: Arc<std::sync::Mutex<bool>>) {
        self.autocommit = Some(handle);
    }

    /// Install the connection's shared `isTransactionSavepoint` flag so `OP_Savepoint` and
    /// `OP_AutoCommit` can consult and mutate it. Called by `sqlite3_prepare_v2` after
    /// constructing the Vdbe. When never called, the Vdbe treats SAVEPOINT as if it never
    /// auto-starts a transaction savepoint (the unit-test mode).
    pub fn set_is_transaction_savepoint_handle(&mut self, handle: Arc<std::sync::Mutex<bool>>) {
        self.is_transaction_savepoint = Some(handle);
    }

    /// Read the current autocommit flag. Returns `true` when no shared handle is installed
    /// (the legacy unit-test mode) or when the shared flag's value is `true`.
    fn autocommit(&self) -> bool {
        self.autocommit
            .as_ref()
            .map_or(true, |h| *h.lock().unwrap())
    }

    /// Read the current `isTransactionSavepoint` flag. Returns `false` when no shared handle is
    /// installed (the legacy unit-test mode) or when the shared flag's value is `false`.
    fn is_transaction_savepoint(&self) -> bool {
        self.is_transaction_savepoint
            .as_ref()
            .map_or(false, |h| *h.lock().unwrap())
    }

    /// Set the shared `isTransactionSavepoint` flag, if a handle is installed.
    fn set_is_transaction_savepoint(&self, value: bool) {
        if let Some(h) = self.is_transaction_savepoint.as_ref() {
            *h.lock().unwrap() = value;
        }
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
        self.last_compare = Ordering::Equal;
        self.once_done.clear();
        // Drop any residual sub-program state (a reset mid-sub-program should not happen in
        // normal use, but be defensive — the parent program is the canonical `self.program`,
        // so restoring frames would just put back the same program).
        self.frames.clear();
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
    /// On an error inside a write transaction, the cleanup depends on the program's default
    /// conflict-resolution action (`Program::default_oe`, mirroring `p->errorAction`):
    /// `OE_Abort` (the default) rolls back just this statement's changes (via the implicit
    /// statement savepoint when inside an explicit `BEGIN`, or the whole transaction under
    /// autocommit — same effect since the transaction IS the statement); `OE_Rollback` rolls
    /// back the entire transaction; `OE_Fail` leaves all prior changes in place. `OE_Ignore`
    /// and `OE_Replace` are handled at codegen level and never reach this path. Mirrors how
    /// `sqlite3VdbeHalt` picks `SAVEPOINT_ROLLBACK` / `sqlite3RollbackAll` / nothing based on
    /// `p->errorAction`.
    pub async fn step(&mut self) -> Result<StepResult> {
        match self.step_inner().await {
            Ok(r) => Ok(r),
            Err(e) => {
                // A per-constraint `ON CONFLICT <action>` clause (M12.9) sets `oe_override` when
                // it fires; that OE wins over the statement-level `OR <action>` for the cleanup,
                // mirroring upstream's `p->errorAction = pOp->p2` in `OP_Halt`. The override is
                // consumed here (cleared after the cleanup).
                let oe_byte = self.oe_override.take().unwrap_or(self.program.default_oe);
                let oe = crate::vdbe::oe::OeAction::from_u8(oe_byte);
                if self.write_txn {
                    if let Some(pager) = &self.pager {
                        match oe {
                            crate::vdbe::oe::OeAction::Fail => {
                                // Keep all prior changes (including earlier rows from this
                                // statement). Under an explicit BEGIN, drop the implicit
                                // statement savepoint so a later COMMIT/ROLLBACK sees a clean
                                // savepoint stack — the write transaction stays open. Under
                                // autocommit, commit the successful rows now (mirrors upstream:
                                // `sqlite3VdbeHalt` commits when `errorAction == OE_Fail` even
                                // on a non-OK `rc`, because FAIL is a "soft" error).
                                if let Some(name) = self.stmt_savepoint.take() {
                                    let _ = pager.release_savepoint(&name);
                                } else if self.autocommit() {
                                    let _ = pager.commit().await;
                                    self.write_txn = false;
                                }
                            }
                            crate::vdbe::oe::OeAction::Rollback => {
                                // Roll back the entire transaction.
                                let _ = pager.rollback().await;
                                self.write_txn = false;
                                self.stmt_savepoint = None;
                            }
                            // Abort (default) and any other value: roll back this statement's
                            // changes but keep the transaction.
                            _ => {
                                if let Some(name) = self.stmt_savepoint.take() {
                                    // Inside an explicit BEGIN: roll back to the statement
                                    // savepoint (discards just this statement's writes), then
                                    // release it so the enclosing transaction is clean.
                                    let _ = pager.rollback_to_savepoint(&name).await;
                                    let _ = pager.release_savepoint(&name);
                                } else {
                                    // Autocommit mode: the whole transaction is this one
                                    // statement, so a full rollback is the right move.
                                    let _ = pager.rollback().await;
                                    self.write_txn = false;
                                }
                            }
                        }
                    }
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
        loop {
            // Re-clone the current program each iteration so a `Program` opcode (which swaps
            // `self.program` to a sub-program) or a frame-popping `Halt` sees the new program
            // immediately. The `Arc` clone is cheap (one refcount bump).
            let program = Arc::clone(&self.program);
            let pc = self.pc;
            let inst: &Instruction = program
                .instructions
                .get(pc)
                .ok_or_else(|| Error::msg("program counter ran off the end of the program"))?;
            let (p1, p2, p3, p5) = (inst.p1, inst.p2, inst.p3, inst.p5);

            match inst.opcode {
                Opcode::Init => self.pc = p2 as usize,
                Opcode::Goto => self.pc = p2 as usize,
                Opcode::Once => {
                    // First encounter in this run: fall through (after recording the address so
                    // a repeat encounter jumps to `p2`). Mirrors `OP_Once` in `vdbe.c`.
                    if self.once_done.contains(&pc) {
                        self.pc = p2 as usize;
                    } else {
                        self.once_done.insert(pc);
                        self.pc += 1;
                    }
                }
                Opcode::Program => {
                    // `OP_Program p1 p2 p3 p4=SubProgram`: invoke a sub-VDBE. Save the current
                    // state into a frame stored on `self.frames`, install the sub-program with
                    // a fresh register file and cursor table, and begin executing it at its
                    // first instruction. Mirrors `OP_Program` in `vdbe.c`.
                    //
                    // `p1` is the parent-frame register base for `OP_Param`; `p2` is the
                    // ignore-jump target (used when the sub-program halts with `OE_Ignore`,
                    // which we record on the frame and consult at pop time); `p3` is unused
                    // here (upstream stores a `pRt` runtime blob in `r[p3]`; we keep the
                    // parent state on `self.frames` instead, so `p3` is informational only);
                    // `p4` carries the sub-program; `p5` is the recursion-token (non-zero
                    // enables the recursive-trigger guard).
                    // Recursive-trigger guard: if `p5 != 0` and a frame with the same token is
                    // already on the stack, this is a recursive invocation that P5 says to
                    // suppress — skip the sub-program entirely (fall through). Upstream matches
                    // on `pProgram->token`; we use `p5` as the token (the codegen picks a
                    // distinct non-zero value per trigger).
                    if p5 != 0 && self.frames.iter().any(|f| f.token == p5) {
                        self.pc += 1;
                        continue;
                    }
                    // Save the parent state into a new frame.
                    let sub_program = match &inst.p4 {
                        P4::SubProgram(p) => Arc::clone(p),
                        _ => return Err(Error::msg("OP_Program requires a SubProgram p4")),
                    };
                    let parent_program = std::mem::replace(&mut self.program, sub_program.clone());
                    let frame = VdbeFrame {
                        program: parent_program,
                        pc: pc + 1,
                        regs: std::mem::take(&mut self.regs),
                        cursors: std::mem::take(&mut self.cursors),
                        cursor_root: std::mem::take(&mut self.cursor_root),
                        decoded: self.decoded.take(),
                        aggregates: std::mem::take(&mut self.aggregates),
                        once_done: std::mem::take(&mut self.once_done),
                        write_txn: self.write_txn,
                        param_base: p1,
                        token: p5,
                    };
                    self.frames.push(frame);
                    // Install a fresh register file sized for the sub-program and reset the
                    // cursor table / per-frame scratch. The sub-program's `Init` (if present)
                    // runs next and jumps to its setup block; otherwise execution starts at
                    // its first instruction.
                    let nreg = sub_program.num_registers.max(1);
                    self.regs = vec![Value::Null; nreg];
                    self.cursors = Vec::new();
                    self.cursor_root = HashMap::new();
                    self.decoded = None;
                    self.aggregates = HashMap::new();
                    self.once_done = std::collections::HashSet::new();
                    self.pc = 0;
                }
                Opcode::Param => {
                    // `OP_Param p1 p2`: copy a value from the PARENT frame's register file into
                    // the current frame's `r[p2]`. The parent register index is
                    // `param_base + p1`, where `param_base` is the calling `OP_Program`'s `p1`.
                    // Mirrors `OP_Param` in `vdbe.c`.
                    let frame = self.frames.last().ok_or_else(|| {
                        Error::msg("OP_Param outside of a sub-program (no parent frame)")
                    })?;
                    let parent_idx = (frame.param_base + p1) as usize;
                    let val = frame
                        .regs
                        .get(parent_idx)
                        .cloned()
                        .unwrap_or(Value::Null);
                    self.regs[p2 as usize] = val;
                    self.pc += 1;
                }
                Opcode::Gosub => {
                    // Store the address of the *next* instruction in `r[p1]` and jump to `p2`.
                    self.regs[p1 as usize] = Value::Int((pc + 1) as i64);
                    self.pc = p2 as usize;
                }
                Opcode::Return => {
                    // Jump to the address in `r[p1]`. With `p3 == 1` the jump is conditional on
                    // `r[p1]` being an integer (fall through otherwise); with `p3 == 0` it is
                    // strict. Codegen always pairs `Gosub` with the strict form here.
                    match &self.regs[p1 as usize] {
                        Value::Int(addr) => {
                            self.pc = *addr as usize;
                        }
                        _ => {
                            if p3 == 0 {
                                return Err(Error::msg(
                                    "OP_Return: return-address register is not an integer",
                                ));
                            }
                            self.pc += 1;
                        }
                    }
                }
                Opcode::InitCoroutine => {
                    // Set `r[p1] = p3` so the first `Yield` to `r[p1]` jumps to address `p3`
                    // (the coroutine's first instruction). If `p2 != 0`, jump over the
                    // coroutine body to address `p2`. Mirrors `OP_InitCoroutine` in `vdbe.c`,
                    // adjusted for our direct-address PC convention (upstream stores `p3 - 1`
                    // because its dispatch loop post-increments `pOp`).
                    self.regs[p1 as usize] = Value::Int(p3 as i64);
                    if p2 != 0 {
                        self.pc = p2 as usize;
                    } else {
                        self.pc += 1;
                    }
                }
                Opcode::EndCoroutine => {
                    // The `Yield` that called this coroutine is at address `r[p1] - 1` (the
                    // calling `Yield` stored `pc + 1` in `r[p1]`). Read that `Yield`'s `p2` and
                    // jump to it (the "coroutine ended" continuation). Leave `r[p1]` set to
                    // this `EndCoroutine`'s own address so subsequent `Yield`s jump back here
                    // and re-jump to the same `p2` (the coroutine stays ended).
                    match &self.regs[p1 as usize] {
                        Value::Int(caller_yield_pc_plus_1) => {
                            let caller_yield_pc = (*caller_yield_pc_plus_1 - 1) as usize;
                            let caller = program
                                .instructions
                                .get(caller_yield_pc)
                                .ok_or_else(|| Error::msg("OP_EndCoroutine: caller Yield not found"))?;
                            if !matches!(caller.opcode, Opcode::Yield) {
                                return Err(Error::msg(
                                    "OP_EndCoroutine: caller is not a Yield",
                                ));
                            }
                            self.regs[p1 as usize] = Value::Int(pc as i64);
                            self.pc = caller.p2 as usize;
                        }
                        _ => {
                            return Err(Error::msg(
                                "OP_EndCoroutine: register does not hold a coroutine address",
                            ));
                        }
                    }
                }
                Opcode::Yield => {
                    // Swap the program counter with the value in `r[p1]`: jump to the saved
                    // address, and store the address of the next instruction (the resume point
                    // for the next `Yield`) in `r[p1]`. If the destination is an
                    // `EndCoroutine`, the coroutine has ended: jump to this `Yield`'s `p2`
                    // (the "coroutine ended" continuation) instead of re-entering the body.
                    match &self.regs[p1 as usize] {
                        Value::Int(dest) => {
                            let dest_pc = *dest as usize;
                            self.regs[p1 as usize] = Value::Int((pc + 1) as i64);
                            let is_end_coroutine = program
                                .instructions
                                .get(dest_pc)
                                .map_or(false, |i| matches!(i.opcode, Opcode::EndCoroutine));
                            if is_end_coroutine {
                                self.pc = p2 as usize;
                            } else {
                                self.pc = dest_pc;
                            }
                        }
                        _ => {
                            return Err(Error::msg(
                                "OP_Yield: register does not hold a coroutine address",
                            ));
                        }
                    }
                }
                Opcode::Compare => {
                    // Compare `n = p3` registers starting at `r[p1]` against `r[p2]` under the
                    // per-key collation carried by `p4 = KeyInfo`, leaving the ordering in
                    // `last_compare` for the immediately following `OP_Jump`.
                    let n = p3 as usize;
                    let ki = match &inst.p4 {
                        P4::KeyInfo(k) => k.clone(),
                        _ => {
                            return Err(Error::msg("OP_Compare requires a KeyInfo p4"));
                        }
                    };
                    let mut ord = Ordering::Equal;
                    for i in 0..n {
                        let a = &self.regs[p1 as usize + i];
                        let b = &self.regs[p2 as usize + i];
                        let key = &ki[i];
                        let mut o = mem_compare(a, b, key.collation);
                        if key.desc {
                            o = o.reverse();
                        }
                        if o != Ordering::Equal {
                            ord = o;
                            break;
                        }
                    }
                    self.last_compare = ord;
                    self.pc += 1;
                }
                Opcode::Jump => {
                    // Route to p1/p2/p3 based on the last `Compare` result (Less/Equal/Greater).
                    self.pc = match self.last_compare {
                        Ordering::Less => p1 as usize,
                        Ordering::Equal => p2 as usize,
                        Ordering::Greater => p3 as usize,
                    };
                }
                Opcode::Halt => {
                    // `OP_Halt p1 p2 p3 p4 p5`: stop execution. `p1` is the result code (0 =
                    // SQLITE_OK); `p2` is the conflict-resolution action on error (`OE_Abort`
                    // etc.); `p3`/`p4` carry an error message when `p1 != 0`; `p5` is the
                    // constraint type for error formatting.
                    //
                    // When inside a sub-program (there is a parent frame on `self.frames`) and
                    // `p1 == SQLITE_OK`, the sub-program is returning control to the parent.
                    // Pop the frame and resume at the saved PC. Mirrors the `p->pFrame` branch
                    // of `OP_Halt` in `vdbe.c` / `sqlite3VdbeFrameRestore`.
                    if p1 == 0 && !self.frames.is_empty() {
                        let frame = self.frames.pop().unwrap();
                        // Restore the parent's state. The sub-program's `write_txn` flag is
                        // discarded — the parent's is restored (matches upstream: a sub-program
                        // does not independently commit; the parent's transaction owns the
                        // commit). The sub-program's change-counter deltas propagate via the
                        // shared `RuntimeCtx` (`self.ctx`), which is NOT saved on the frame.
                        self.program = frame.program;
                        self.pc = frame.pc;
                        self.regs = frame.regs;
                        self.cursors = frame.cursors;
                        self.cursor_root = frame.cursor_root;
                        self.decoded = frame.decoded;
                        self.aggregates = frame.aggregates;
                        self.once_done = frame.once_done;
                        self.write_txn = frame.write_txn;
                        // `p2 == OE_Ignore` (5) means the sub-program threw an IGNORE
                        // exception — jump to the calling `OP_Program`'s `p2` (the ignore-jump
                        // target) instead of resuming at the next instruction. Upstream does
                        // `pcx = p->aOp[pcx].p2 - 1`; we set `pc` directly to the calling
                        // `OP_Program`'s `p2`. The calling `OP_Program` is at
                        // `frame.pc - 1` (we saved `pc + 1` as the resume point).
                        if p2 == 5 {
                            // The ignore-jump target is the calling Program's p2. Read it from
                            // the now-restored parent program.
                            let caller_pc = self.pc - 1;
                            if let Some(caller) = self.program.instructions.get(caller_pc) {
                                if matches!(caller.opcode, Opcode::Program) {
                                    self.pc = caller.p2 as usize;
                                }
                            }
                        }
                        continue;
                    }
                    if p1 != 0 {
                        // Error halt: build the error message from `p4` (and `p5`'s constraint-
                        // type prefix when set), set the per-constraint OE override (when `p2 !=
                        // 0`), and return the error so `step()` runs the OE-aware cleanup.
                        // Mirrors the `p->errorAction = pOp->p2` + `sqlite3VdbeHalt` path in
                        // `vdbe.c`.
                        let base = match &inst.p4 {
                            P4::Text(s) => s.clone(),
                            _ => String::new(),
                        };
                        let msg = if p5 != 0 && !base.is_empty() {
                            let kind = match p5 {
                                1 => "NOT NULL constraint failed: ",
                                2 => "UNIQUE constraint failed: ",
                                3 => "CHECK constraint failed: ",
                                4 => "FOREIGN KEY constraint failed: ",
                                _ => "",
                            };
                            format!("{kind}{base}")
                        } else {
                            base
                        };
                        if p2 != 0 {
                            self.oe_override = Some(p2 as u8);
                        }
                        return Err(Error::new(crate::error::ResultCode::Constraint, msg));
                    }
                    // A successful top-level Halt commits an open write transaction (the
                    // durable commit point) — but only in autocommit mode. When the connection
                    // is inside an explicit `BEGIN` (autocommit is false), the write transaction
                    // is left open so subsequent statements in the same transaction can continue
                    // against the same rollback journal. The COMMIT/ROLLBACK statement itself
                    // drives the commit via `OP_AutoCommit`. Mirrors the autocommit-aware
                    // CommitPhase in `sqlite3VdbeHalt`.
                    if self.write_txn && self.autocommit() {
                        if let Some(pager) = &self.pager {
                            pager.commit().await?;
                        }
                        self.write_txn = false;
                    } else if let (Some(name), Some(pager)) =
                        (self.stmt_savepoint.take(), self.pager.as_ref())
                    {
                        // Inside an explicit transaction: release the implicit statement
                        // savepoint so its changes merge into the enclosing transaction (a
                        // no-op when the savepoint was never opened, e.g. a read statement).
                        // Mirrors the `SAVEPOINT_RELEASE` tail of `sqlite3VdbeHalt`.
                        let _ = pager.release_savepoint(&name);
                    }
                    self.halted = true;
                    return Ok(StepResult::Done);
                }
                Opcode::HaltIfNull => {
                    // p1 carries the result code (Constraint), p2 carries the per-constraint OE
                    // action (0 = use the program's `default_oe`); p3 names the register to test
                    // for NULL; p4 carries the constraint message. If NULL, abort the statement.
                    // The per-constraint OE (when non-zero) overrides `default_oe` for the cleanup
                    // that `step()` does, mirroring upstream's `p->errorAction = pOp->p2`.
                    if self.regs[p3 as usize].is_null() {
                        if p2 != 0 {
                            self.oe_override = Some(p2 as u8);
                        }
                        let msg = match &inst.p4 {
                            P4::Text(s) => s.clone(),
                            _ => "NOT NULL constraint failed".to_string(),
                        };
                        return Err(Error::new(crate::error::ResultCode::Constraint, msg));
                    }
                    self.pc += 1;
                }
                Opcode::AddImm => {
                    // `AddImm p1 p2`: `r[p1] += p2`. Short-form integer add used by the
                    // window-function sliding-frame counters.
                    let v = self.regs[p1 as usize].as_i64();
                    self.regs[p1 as usize] = Value::Int(v + p2 as i64);
                    self.pc += 1;
                }
                Opcode::SeekRowid => {
                    // `SeekRowid p1 p2 p3`: position cursor p1 at the row whose rowid is r[p3];
                    // jump to p2 if no such row exists. For ephemeral cursors our rowids are
                    // sequential 1..=n, so we map rowid → index.
                    let rowid = self.regs[p3 as usize].as_i64();
                    let slot = self
                        .cursors
                        .get_mut(p1 as usize)
                        .and_then(|c| c.as_mut());
                    let found = match slot {
                        Some(VdbeCursor::Ephemeral(e)) => e.seek_rowid(rowid),
                        _ => {
                            return Err(Error::msg(
                                "SeekRowid on an unsupported cursor type (only ephemeral supported)",
                            ))
                        }
                    };
                    if found {
                        self.pc += 1;
                    } else {
                        self.pc = p2 as usize;
                    }
                }
                Opcode::ResetSorter => {
                    // `ResetSorter p1`: clear all records from sorter/ephemeral cursor p1.
                    let slot = self
                        .cursors
                        .get_mut(p1 as usize)
                        .and_then(|c| c.as_mut());
                    match slot {
                        Some(VdbeCursor::Ephemeral(e)) => e.clear(),
                        Some(VdbeCursor::Sorter(s)) => s.clear(),
                        _ => return Err(Error::msg("ResetSorter on an unsupported cursor type")),
                    }
                    self.pc += 1;
                }
                Opcode::Last => {
                    // `Last p1 p2`: position cursor p1 at its last row; jump to p2 if empty.
                    let slot = self
                        .cursors
                        .get_mut(p1 as usize)
                        .and_then(|c| c.as_mut());
                    let nonempty = match slot {
                        Some(VdbeCursor::Ephemeral(e)) => {
                            // Position at the last record.
                            let n = e.len();
                            if n == 0 {
                                false
                            } else {
                                e.seek_rowid(n as i64)
                            }
                        }
                        _ => return Err(Error::msg("Last on an unsupported cursor type")),
                    };
                    if nonempty {
                        self.pc += 1;
                    } else {
                        self.pc = p2 as usize;
                    }
                }
                Opcode::Prev => {
                    // `Prev p1 p2`: move cursor p1 to the previous row; jump to p2 if a row
                    // remains, fall through if at the beginning. Mirrors `OP_Prev`.
                    let slot_mut = self
                        .cursors
                        .get_mut(p1 as usize)
                        .and_then(|c| c.as_mut());
                    let has_prev = match slot_mut {
                        Some(VdbeCursor::Ephemeral(e)) => {
                            // Our ephemeral rowids are 1..=n (sequential); rowid = pos + 1.
                            // If pos > 0, seek to (pos - 1) + 1 = pos (the previous row's rowid).
                            let cur = e.current_position();
                            if cur > 0 {
                                e.seek_rowid(cur as i64)
                            } else {
                                false
                            }
                        }
                        _ => return Err(Error::msg("Prev on an unsupported cursor type")),
                    };
                    if has_prev {
                        self.pc = p2 as usize;
                    } else {
                        self.pc += 1;
                    }
                }
                Opcode::Transaction => {
                    // `OP_Transaction p1 p2`: begin a transaction on database `p1`. `p2 == 0`
                    // is a read transaction — acquire a SHARED lock (mirrors
                    // `sqlite3PagerSharedLock` called from `sqlite3BtreeBeginTrans`).
                    // `p2 == 1` opens a WRITE transaction acquiring a RESERVED lock (the
                    // `BEGIN IMMEDIATE` path, and the lazy-write path taken by every write
                    // statement under `BEGIN DEFERRED`). `p2 == 2` opens a WRITE transaction
                    // acquiring an EXCLUSIVE lock (the `BEGIN EXCLUSIVE` path, which blocks
                    // even readers on other connections). The lock-level distinction is
                    // faithful to `sqlite3PagerBegin`'s `exFlag` parameter (`exFlag = wrflag >
                    // 1`). Mirrors `OP_Transaction` in `vdbe.c`.
                    let pager = self
                        .pager
                        .clone()
                        .ok_or_else(|| Error::msg("no database is open"))?;
                    if p2 == 0 {
                        // Read transaction: acquire the SHARED lock (idempotent — a no-op if
                        // already held). Mirrors `sqlite3PagerSharedLock`.
                        pager.begin_read().await?;
                    } else {
                        // `p2 >= 2` carries the EXCLUSIVE flag (`exFlag = wrflag > 1` in
                        // `sqlite3PagerBegin`); `p2 == 1` is a plain write (RESERVED) lock.
                        // If we're upgrading from a read transaction (the common
                        // `BEGIN DEFERRED` + write-statement path), `begin_write` handles the
                        // lock escalation; otherwise it opens a fresh write transaction.
                        let already_write = self.write_txn;
                        pager.begin_write(p2 >= 2).await?;
                        self.write_txn = true;
                        // Open an implicit "statement savepoint" so an `OE_Abort` constraint
                        // violation can roll back just this statement's changes when the
                        // connection is inside an explicit `BEGIN` (autocommit off). Under
                        // autocommit, the whole transaction is this one statement, so a full
                        // rollback is correct and we skip the savepoint. Mirrors the statement
                        // sub-journal that upstream opens in `sqlite3VdbeMakeReady` for
                        // multi-write-statement transactions.
                        if !already_write && !self.autocommit() {
                            const STMT_SP: &str = "__rustqlite_stmt_abort";
                            // Clear any stale statement savepoint from a prior statement in
                            // the same transaction (it should have been released on success,
                            // but a prior error might have left it).
                            pager.drop_savepoint_named(STMT_SP);
                            pager.open_savepoint(STMT_SP.to_string());
                            self.stmt_savepoint = Some(STMT_SP.to_string());
                        }
                    }
                    self.pc += 1;
                }
                Opcode::AutoCommit => {
                    // `OP_AutoCommit p1 p2`: toggle the connection's autocommit flag.
                    //   p1 = desired autocommit (1 = on, 0 = off)
                    //   p2 = rollback flag (only valid when p1 == 1; p2 == 1 means ROLLBACK)
                    // Mirrors `OP_AutoCommit` in `vdbe.c` (which calls `sqlite3VdbeHalt` to
                    // commit/rollback the transaction and `sqlite3CloseSavepoints` to wipe the
                    // savepoint stack and `isTransactionSavepoint` flag).
                    let desired = p1 != 0;
                    let rollback = p2 != 0;
                    let current = self.autocommit();
                    if desired == current {
                        // Same-state transition is an error in upstream SQLite.
                        return Err(Error::msg(
                            if !desired {
                                "cannot start a transaction within a transaction"
                            } else if rollback {
                                "cannot rollback - no transaction is active"
                            } else {
                                "cannot commit - no transaction is active"
                            },
                        ));
                    }
                    if let Some(handle) = self.autocommit.as_ref() {
                        *handle.lock().unwrap() = desired;
                    }
                    if desired {
                        // Transitioning off → on: this is COMMIT (p2 == 0) or ROLLBACK (p2 == 1).
                        if let Some(pager) = &self.pager {
                            if pager.in_write_txn() {
                                if rollback {
                                    pager.rollback().await?;
                                } else {
                                    pager.commit().await?;
                                }
                            }
                            // Mirrors `sqlite3CloseSavepoints` — a COMMIT or ROLLBACK wipes the
                            // savepoint stack (already done by `end_txn` inside commit/rollback)
                            // and resets the transaction-savepoint flag. Belt-and-suspenders:
                            // clear again in case `commit`/`rollback` early-returned without
                            // calling `end_txn` (the no-write-txn path).
                            pager.clear_savepoints();
                        }
                        self.set_is_transaction_savepoint(false);
                        self.write_txn = false;
                    }
                    // OP_AutoCommit ends the statement (mirrors upstream's `goto vdbe_return`
                    // after `sqlite3VdbeHalt`). The `Halt` opcode isn't reached.
                    self.halted = true;
                    return Ok(StepResult::Done);
                }

                Opcode::Savepoint => {
                    // `OP_Savepoint p1 * * P4=Text(name)`: open (p1 == 0), release (p1 == 1),
                    // or rollback (p1 == 2) the savepoint named by `P4`. Mirrors `OP_Savepoint`
                    // in `vdbe.c`.
                    //
                    // p1 == 0 (SAVEPOINT_BEGIN): create a new savepoint. If the connection is in
                    //   autocommit mode, this implicitly starts a transaction (the savepoint
                    //   becomes the "transaction savepoint"); otherwise the savepoint is nested
                    //   inside the current transaction. The pager snapshots the current dirty
                    //   overlay and page count so a later ROLLBACK TO can restore them.
                    // p1 == 1 (SAVEPOINT_RELEASE): find the named savepoint. If it is the outermost
                    //   AND is the transaction savepoint, this is a COMMIT — turn autocommit on,
                    //   commit the pager transaction, and clear the savepoint stack. Otherwise
                    //   drop the named savepoint and any nested ones; their changes become part of
                    //   the enclosing transaction.
                    // p1 == 2 (SAVEPOINT_ROLLBACK): find the named savepoint and restore the dirty
                    //   overlay/page count to its snapshot, dropping nested savepoints but
                    //   keeping the named one (so it can be rolled back to again).
                    let name = match &inst.p4 {
                        P4::Text(s) => s.clone(),
                        _ => return Err(Error::msg("OP_Savepoint requires a Text name in p4")),
                    };
                    let op = p1;
                    if op == 0 {
                        // SAVEPOINT_BEGIN.
                        // Upstream's `db->nVdbeWrite>0` guard (cannot open a savepoint if there are
                        // active write statements) is implicitly satisfied because each statement
                        // is fully stepped through `Done` before the next is prepared.
                        let was_autocommit = self.autocommit();
                        if was_autocommit {
                            // Auto-start a transaction: turn autocommit off and mark this
                            // savepoint as the transaction savepoint. Mirrors
                            // `db->autoCommit = 0; db->isTransactionSavepoint = 1;`.
                            if let Some(handle) = self.autocommit.as_ref() {
                                *handle.lock().unwrap() = false;
                            }
                            self.set_is_transaction_savepoint(true);
                        }
                        if let Some(pager) = &self.pager {
                            pager.open_savepoint(name);
                        } else {
                            // No pager yet (the database has no pages). The savepoint still
                            // needs to be tracked so a later ROLLBACK TO / RELEASE works against
                            // a connection that later opens a database; but since the pager is
                            // created lazily on the first write, and a SAVEPOINT outside a
                            // transaction that auto-starts one is followed by a write that opens
                            // the pager, we cannot push the savepoint without a pager. SQLite
                            // refuses SAVEPOINT on an unopenable database with the same shape;
                            // surface a clearer error.
                            return Err(Error::msg(
                                "cannot open savepoint - no database is open",
                            ));
                        }
                    } else if op == 1 {
                        // SAVEPOINT_RELEASE.
                        let pager = self
                            .pager
                            .clone()
                            .ok_or_else(|| Error::msg("no database is open"))?;
                        // Determine whether this release targets the outermost savepoint that
                        // is also the transaction savepoint. The named savepoint is the
                        // outermost iff its index in the pager's stack is 0 (mirrors upstream's
                        // `pSavepoint->pNext==0` check — `pNext` walks from innermost outward,
                        // so the LAST node is the outermost, which is index 0 in our array).
                        let is_outermost = pager.savepoint_index(&name) == Some(0);
                        let is_txn_savepoint = self.is_transaction_savepoint();
                        if is_outermost && is_txn_savepoint {
                            // Commit the implicit transaction (mirrors `sqlite3VdbeHalt` +
                            // `sqlite3CloseSavepoints`).
                            if pager.in_write_txn() {
                                pager.commit().await?;
                            }
                            pager.clear_savepoints();
                            self.set_is_transaction_savepoint(false);
                            if let Some(handle) = self.autocommit.as_ref() {
                                *handle.lock().unwrap() = true;
                            }
                            self.write_txn = false;
                        } else {
                            // Plain release: drop the named savepoint and any nested ones. The
                            // changes since the savepoint become part of the enclosing txn.
                            pager.release_savepoint(&name)?;
                        }
                    } else if op == 2 {
                        // SAVEPOINT_ROLLBACK.
                        let pager = self
                            .pager
                            .clone()
                            .ok_or_else(|| Error::msg("no database is open"))?;
                        pager.rollback_to_savepoint(&name).await?;
                    } else {
                        return Err(Error::msg(format!(
                            "OP_Savepoint: invalid p1 operand {op}"
                        )));
                    }
                    // OP_Savepoint ends the statement (mirrors upstream's `goto vdbe_return`
                    // after the savepoint op completes). No trailing `Halt` is emitted.
                    self.halted = true;
                    return Ok(StepResult::Done);
                }

                Opcode::Checkpoint => {
                    // `OP_Checkpoint p1 p2 p3`: run a WAL checkpoint on database `p1` (always 0
                    // — only `main` is supported) in mode `p2` (0=PASSIVE, 1=FULL, 2=RESTART,
                    // 3=TRUNCATE), and write three result registers at `r[p3..p3+3]`:
                    //   r[p3]   = 0 on success, 1 if busy (we never go busy in this iteration)
                    //   r[p3+1] = number of frames in the WAL (`pnLog`)
                    //   r[p3+2] = number of frames checkpointed (`pnCkpt`)
                    // Mirrors `OP_Checkpoint` in `vdbe.c`. On error (not BUSY), `r[p3+1]` and
                    // `r[p3+2]` are set to -1 (matching upstream's `aRes[1] = aRes[2] = -1`).
                    let pager = self
                        .pager
                        .clone()
                        .ok_or_else(|| Error::msg("no database is open"))?;
                    let mode = match p2 {
                        0 => crate::pager::wal::CheckpointMode::Passive,
                        1 => crate::pager::wal::CheckpointMode::Full,
                        2 => crate::pager::wal::CheckpointMode::Restart,
                        3 => crate::pager::wal::CheckpointMode::Truncate,
                        other => {
                            return Err(Error::msg(format!(
                                "OP_Checkpoint: invalid mode {other}"
                            )));
                        }
                    };
                    // Pre-fill the result registers with the "error" sentinel; on success they
                    // are overwritten below.
                    let base = p3 as usize;
                    self.regs[base] = Value::Int(0);
                    self.regs[base + 1] = Value::Int(-1);
                    self.regs[base + 2] = Value::Int(-1);
                    match pager.checkpoint(mode).await {
                        Ok((n_log, n_ckpt)) => {
                            self.regs[base] = Value::Int(0);
                            self.regs[base + 1] = Value::Int(n_log as i64);
                            self.regs[base + 2] = Value::Int(n_ckpt as i64);
                        }
                        Err(e) => {
                            // Surface the error. Upstream only treats BUSY specially (it
                            // sets `aRes[0] = 1` and continues); any other error aborts.
                            return Err(e);
                        }
                    }
                    self.pc += 1;
                }

                Opcode::FkCheck => {
                    // `OP_FkCheck p1 p2 p3 P4=FkCheck`: verify a single FK constraint for the
                    // child row whose key columns are in `r[p1..p1+n]`. When any key column is
                    // NULL, skip (no violation). When the parent row is missing, jump to `p2`
                    // (the violation handler). Otherwise fall through. `p3` carries the FK's
                    // 0-based index for the error message; the lookup strategy is in `P4`.
                    let fk = match &inst.p4 {
                        P4::FkCheck(fk) => fk.clone(),
                        _ => return Err(Error::msg("OP_FkCheck: missing P4::FkCheck")),
                    };
                    let n = fk.child_columns.len();
                    let start = p1 as usize;
                    let key: Vec<Value> = self.regs[start..start + n].to_vec();
                    // NULL foreign keys never violate (upstream's `OP_IsNull → addrOk`).
                    if key.iter().any(|v| matches!(v, Value::Null)) {
                        self.pc += 1;
                        continue;
                    }
                    let pager = self
                        .pager
                        .clone()
                        .ok_or_else(|| Error::msg("no database is open"))?;
                    let found = fk_parent_exists(&pager, &fk.lookup, &key).await?;
                    if found {
                        self.pc += 1;
                    } else {
                        self.pc = p2 as usize;
                    }
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

                Opcode::NullRow => {
                    // Set the cursor to a synthetic all-NULL row. Used by LEFT JOIN to emit a
                    // NULL-filled right-table row when no inner match is found.
                    match self.cursor_mut(p1)? {
                        VdbeCursor::Table(c) => c.set_null_row(),
                        VdbeCursor::Index(c) => {
                            // Index cursors don't have a null-row state; the LEFT JOIN codegen
                            // only uses NullRow on table cursors. Defensive: clear the payload
                            // so Column reads return NULL.
                            let _ = c;
                            return Err(Error::msg("NullRow on an index cursor is not supported"));
                        }
                        VdbeCursor::Sorter(_) | VdbeCursor::Ephemeral(_) => {
                            return Err(Error::msg("NullRow on a sorter/ephemeral cursor is not supported"));
                        }
                        VdbeCursor::Pseudo(p) => {
                            // Reset the cached decoded record so the next Column re-reads from
                            // the register. Mirrors upstream's column-cache reset.
                            p.current = None;
                        }
                    }
                    self.decoded = None;
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
                        VdbeCursor::Pseudo(_) => {
                            // A pseudo-cursor always has its single row (set by RowData
                            // before the recursive query runs). Rewind is a no-op that
                            // falls through (never jumps to p2).
                            self.pc += 1;
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
                        VdbeCursor::Pseudo(_) => {
                            // A pseudo-cursor has exactly one row. After that row is
                            // processed, Next falls through (no more rows) — never jumps.
                            self.pc += 1;
                        }
                    }
                }

                Opcode::Rowid => {
                    // For table cursors, read the b-tree rowid. For ephemeral cursors, the rowid
                    // is pos+1 (our ephemeral rowids are sequential 1..=n).
                    let slot = self
                        .cursors
                        .get(p1 as usize)
                        .and_then(|c| c.as_ref());
                    let rowid = match slot {
                        Some(VdbeCursor::Table(_)) => {
                            self.table_cursor(p1)?.rowid()?
                        }
                        Some(VdbeCursor::Ephemeral(e)) => e.rowid(),
                        _ => return Err(Error::msg("Rowid on an unsupported cursor type")),
                    };
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
                    // Ephemeral index cursor (DISTINCT dedup): insert the record blob as a
                    // dedup key; the opcode is a no-op if the key is already present (no
                    // uniqueness error — `OP_Found` already filtered duplicates upstream of
                    // this insert in the DISTINCT path, but be defensive).
                    if self
                        .cursors
                        .get(p1 as usize)
                        .and_then(|c| c.as_ref())
                        .is_some_and(VdbeCursor::is_ephemeral)
                    {
                        let record = match &self.regs[p2 as usize] {
                            Value::Blob(b) => b.clone(),
                            _ => return Err(Error::msg("IdxInsert expects a record blob in p2")),
                        };
                        let slot = self.cursors.get_mut(p1 as usize).unwrap().as_mut().unwrap();
                        let eph = slot.as_ephemeral_mut().unwrap();
                        eph.idx_insert(&record)?;
                        self.pc += 1;
                    } else {
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
                    // The per-constraint `ON CONFLICT <action>` clause on the declaring
                    // `PRIMARY KEY`/`UNIQUE` constraint is encoded in p5 bits 4-7 (M12.9). When
                    // non-zero, it overrides the program's `default_oe` for the cleanup that
                    // `step()` runs after the constraint error propagates.
                    let per_constraint_oe = ((p5 >> 4) & 0x0F) as u8;
                    match btree::index_insert(pager, root, &record, &key_info, unique).await {
                        Ok(()) => {}
                        Err(e) if e.code == crate::error::ResultCode::Constraint && unique => {
                            if per_constraint_oe != 0 {
                                self.oe_override = Some(per_constraint_oe);
                            }
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

                Opcode::Found | Opcode::NotFound | Opcode::NoConflict => {
                    // `Found`/`NotFound`: exact membership test (NULL never matches).
                    // `NoConflict`: "no conflicting entry" test (NULL in any search-key
                    // column means no conflict, regardless of cursor content).
                    //
                    // On an ephemeral index cursor these use the in-memory dedup map; on a
                    // real index b-tree cursor they seek to `>=` the key and compare the
                    // current entry's prefix.
                    let n = p4_len(&inst.p4) as usize;
                    let values: Vec<Value> =
                        self.regs[p3 as usize..p3 as usize + n].to_vec();

                    // `NoConflict`: a NULL in any search-key column short-circuits to "no
                    // conflict" (mirrors upstream's `OP_NoConflict` NULL handling and the
                    // `check_unique_constraint` NULL-suppression in `index_insert.rs`).
                    if matches!(inst.opcode, Opcode::NoConflict)
                        && values.iter().any(|v| matches!(v, Value::Null))
                    {
                        self.pc = p2 as usize;
                        continue;
                    }

                    let found = {
                        let slot = self.cursors.get_mut(p1 as usize)
                            .and_then(|c| c.as_mut())
                            .ok_or_else(|| Error::msg("cursor is not open"))?;
                        match slot {
                            VdbeCursor::Ephemeral(eph) => {
                                // Ephemeral: use the in-memory dedup map (no cursor position
                                // to update).
                                eph.find_values(&values)?
                            }
                            VdbeCursor::Index(idx) => {
                                // Seek the MAIN cursor to the first entry `>=` the key and
                                // check whether the current entry's indexed-column prefix
                                // equals the search key. Positioning the main cursor (rather
                                // than a probe clone) lets a following `IdxRowid` read the
                                // conflicting entry's rowid directly — the INSERT/UPDATE
                                // conflict-resolution codegen relies on this. Mirrors
                                // upstream's `OP_NoConflict`/`OP_Found` which leaves `pC`'s
                                // BtCursor positioned on the match.
                                let ki = idx.key_info().to_vec();
                                let positioned = idx
                                    .seek(btree::index_cursor::SeekOp::Ge, &values)
                                    .await?;
                                if !positioned {
                                    false
                                } else {
                                    let payload = idx.payload().to_vec();
                                    let encoding = self.encoding;
                                    let entry = decode_record(&payload, encoding)?;
                                    let prefix_len = entry.len().saturating_sub(1).min(n);
                                    let prefix = &entry[..prefix_len];
                                    compare_prefix(prefix, &values, &ki) == Ordering::Equal
                                }
                            }
                            _ => return Err(Error::msg(
                                "Found/NotFound/NoConflict requires an index cursor",
                            )),
                        }
                    };
                    let jump = match inst.opcode {
                        Opcode::Found => found,
                        Opcode::NotFound => !found,
                        Opcode::NoConflict => !found,
                        _ => unreachable!(),
                    };
                    if jump {
                        self.pc = p2 as usize;
                    } else {
                        self.pc += 1;
                    }
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
                            // Clear any aggregate accumulator stored at this register so a
                            // subsequent `AggStep` creates a fresh accumulator. Used by the
                            // window codegen to reset accumulators on a partition change.
                            self.aggregates.remove(&(i as usize));
                        }
                    } else {
                        self.regs[p2 as usize] = Value::Null;
                        self.aggregates.remove(&(p2 as usize));
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
                    // An ephemeral cursor (window-function peer-buf) uses the in-memory clear
                    // path; a table b-tree cursor uses the pager-backed path.
                    let is_ephemeral = self
                        .cursors
                        .get(p1 as usize)
                        .and_then(|c| c.as_ref())
                        .is_some_and(VdbeCursor::is_ephemeral);
                    if is_ephemeral {
                        let slot = self.cursors[p1 as usize].as_mut().unwrap();
                        slot.as_ephemeral_mut().unwrap().clear();
                        self.pc += 1;
                        continue;
                    }
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
                    // An ephemeral cursor (recursive CTE Queue drain) uses the in-memory delete
                    // path; a table b-tree cursor uses the pager-backed path.
                    let is_ephemeral = self
                        .cursors
                        .get(p1 as usize)
                        .and_then(|c| c.as_ref())
                        .is_some_and(VdbeCursor::is_ephemeral);
                    if is_ephemeral {
                        let slot = self.cursors[p1 as usize].as_mut().unwrap();
                        slot.as_ephemeral_mut().unwrap().delete_current();
                        self.pc += 1;
                        continue;
                    }
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
                    // When P4 is a KeyInfo, upstream opens an index-keyed ephemeral (used for
                    // DISTINCT dedup and IN-set materialization); otherwise a rowid-keyed table
                    // (used by RETURNING buffer).
                    let nfield = p2 as usize;
                    let eph = match &inst.p4 {
                        P4::KeyInfo(_) => Ephemeral::new_index(nfield, self.encoding),
                        _ => Ephemeral::new(nfield, self.encoding),
                    };
                    self.set_cursor(p1 as usize, VdbeCursor::Ephemeral(eph));
                    self.pc += 1;
                }

                Opcode::OpenPseudo => {
                    // Open a pseudo-cursor that reads a single record from register r[p2].
                    // Used by recursive CTEs to expose the "Current" row to the recursive query.
                    let pseudo = PseudoCursor::new(p2, self.encoding);
                    self.set_cursor(p1 as usize, VdbeCursor::Pseudo(pseudo));
                    self.pc += 1;
                }
                Opcode::OpenDup => {
                    // `OpenDup p1 p2`: open a new cursor p1 sharing the underlying storage of
                    // the existing ephemeral cursor p2. Used by the window-function sliding-
                    // frame algorithm to keep multiple cursors (start/current/end) on the same
                    // partition cache. Mirrors `OP_OpenDup` in `vdbe.c`.
                    let src = self
                        .cursors
                        .get(p2 as usize)
                        .and_then(|c| c.as_ref())
                        .and_then(|c| c.as_ephemeral())
                        .ok_or_else(|| Error::msg("OpenDup source is not an ephemeral cursor"))?;
                    let dup = src.dup();
                    self.set_cursor(p1 as usize, VdbeCursor::Ephemeral(dup));
                    self.pc += 1;
                }

                Opcode::RowData => {
                    // Copy the full record blob of cursor p1's current row into r[p2].
                    // For an ephemeral cursor, this is the raw stored record bytes.
                    let slot = self
                        .cursors
                        .get(p1 as usize)
                        .and_then(|c| c.as_ref())
                        .ok_or_else(|| Error::msg("RowData on a closed cursor"))?;
                    let blob = match slot {
                        VdbeCursor::Ephemeral(e) => {
                            let pos = e.current_position();
                            e.record_at(pos).ok_or_else(|| {
                                Error::msg("RowData: ephemeral cursor has no current record")
                            })?
                        }
                        _ => return Err(Error::msg("RowData on an unsupported cursor type")),
                    };
                    self.regs[p2 as usize] = Value::Blob(blob);
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

                // ---- aggregates (M6) / window functions (M11.3) ----
                Opcode::AggStep => {
                    // `AggStep p1=0 p2 p3 p4=FuncDef(kind) p5=nArg`: accumulate one row's
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
                Opcode::AggInverse => {
                    // `AggInverse p1=1 p2 p3 p4=FuncDef(kind) p5=nArg`: remove one row's
                    // arguments from the accumulator at `r[p3]` (the window-frame inverse step).
                    // The accumulator must already exist (a prior `AggStep` for the same row);
                    // upstream asserts this with `pMem->uTemp == 0x1122e0e3`.
                    let kind = match &inst.p4 {
                        P4::FuncDef(k) => *k,
                        _ => return Err(Error::msg("AggInverse requires a FuncDef p4")),
                    };
                    let n_arg = p5 as usize;
                    let is_count_star = kind == AggregateKind::Count && n_arg == 0;
                    let args: Vec<Value> = if is_count_star {
                        Vec::new()
                    } else {
                        self.regs[p2 as usize..p2 as usize + n_arg].to_vec()
                    };
                    match self.aggregates.get_mut(&(p3 as usize)) {
                        Some(acc) => acc.inverse(&args, is_count_star),
                        None => {
                            return Err(Error::msg(
                                "AggInverse without a preceding AggStep on the accumulator",
                            ))
                        }
                    }
                    self.pc += 1;
                }
                Opcode::AggFinal => {
                    // `AggFinal p1 p2 p3=0 p4=FuncDef(kind)`: finalize the accumulator at `r[p1]`
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
                Opcode::AggValue => {
                    // `AggValue p1 p2 p3 p4=FuncDef(kind)`: invoke the aggregate's `xValue` and
                    // store the result in `r[p3]` *without* consuming the accumulator. `p1`/`p2`
                    // are unused (upstream carries the arg count in `p2` for disambiguation
                    // only). Mirrors `OP_AggValue` in `vdbe.c`.
                    //
                    // Window-only built-in functions (row_number/rank/…/lead/lag) have a
                    // mutating `xValue` (e.g. `rankValueFunc` resets `nValue = 0`); plain
                    // aggregates' `xValue` is non-mutating. We dispatch via `kind.window_only()`.
                    let kind = match &inst.p4 {
                        P4::FuncDef(k) => *k,
                        _ => return Err(Error::msg("AggValue requires a FuncDef p4")),
                    };
                    let result = match self.aggregates.get_mut(&(p1 as usize)) {
                        Some(acc) => {
                            if kind.window_only() {
                                acc.value_mut()
                            } else {
                                acc.value()
                            }
                        }
                        None => empty_aggregate_result(kind),
                    };
                    self.regs[p3 as usize] = result;
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

                // Pseudo cursors (recursive CTE "Current" row) read from a register blob.
                if self
                    .cursors
                    .get(idx)
                    .and_then(|c| c.as_ref())
                    .is_some_and(VdbeCursor::is_pseudo)
                {
                    let slot = self.cursors[idx].as_mut().unwrap();
                    let reg = slot.as_pseudo().unwrap().reg;
                    slot.as_pseudo_mut().unwrap().data(&self.regs)?;
                    let _ = reg;
                    return Ok(slot.as_pseudo().unwrap().column(col));
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

        // Table cursor: check the null-row state first (LEFT JOIN miss). When in the
        // all-NULL state, every column reads as NULL without touching the record.
        if let Some(VdbeCursor::Table(c)) = self.cursors.get(idx).and_then(|c| c.as_ref()) {
            if c.is_null_row() {
                return Ok(Value::Null);
            }
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
fn finalize_accumulator(mut acc: Accumulator, kind: AggregateKind) -> Value {
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
        AggregateKind::GroupConcat => acc.concat.map(Value::Text).unwrap_or(Value::Null),
        // Window-only built-ins (M11.4–M11.6): their `xFinalize` aliases `xValue` (upstream
        // `#define percent_rankFinalizeFunc percent_rankValueFunc` etc.), so finalize just reads
        // the current state via the mutating `value_mut` path.
        AggregateKind::RowNumber
        | AggregateKind::Rank
        | AggregateKind::DenseRank
        | AggregateKind::PercentRank
        | AggregateKind::CumeDist
        | AggregateKind::Ntile
        | AggregateKind::FirstValue
        | AggregateKind::LastValue
        | AggregateKind::NthValue
        | AggregateKind::Lead
        | AggregateKind::Lag => acc.value_mut(),
    }
}

/// The result of finalizing an aggregate that never received an `AggStep` call (an empty group).
/// Mirrors the oracle's behavior for `SELECT count(*) FROM t WHERE 0=1` (0) vs `sum` (NULL) vs
/// `total` (0.0) etc. For window-only built-ins, the empty-frame result mirrors the `xValue`
/// callback's behavior on a never-stepped accumulator.
fn empty_aggregate_result(kind: AggregateKind) -> Value {
    match kind {
        AggregateKind::Count => Value::Int(0),
        AggregateKind::Sum | AggregateKind::Avg | AggregateKind::Min | AggregateKind::Max
        | AggregateKind::GroupConcat => Value::Null,
        AggregateKind::Total => Value::Real(0.0),
        // Window-only built-ins on an empty frame: `row_number`/`rank`/`dense_rank`/`ntile`
        // emit 0 (matches `row_numberValueFunc` on a null `p` and `rankValueFunc` on `nValue=0`);
        // `percent_rank`/`cume_dist` emit 0.0; the value-capture functions emit NULL.
        AggregateKind::RowNumber | AggregateKind::Rank | AggregateKind::DenseRank
        | AggregateKind::Ntile => Value::Int(0),
        AggregateKind::PercentRank | AggregateKind::CumeDist => Value::Real(0.0),
        AggregateKind::FirstValue | AggregateKind::LastValue | AggregateKind::NthValue
        | AggregateKind::Lead | AggregateKind::Lag => Value::Null,
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

/// `OP_FkCheck` runtime: look up the parent row matching `child_key` using the resolved
/// `FkLookup` strategy. Returns `true` when a matching parent row exists (FK satisfied),
/// `false` when missing (FK violated). Mirrors the `foreign_key_check.rs` lookup paths.
async fn fk_parent_exists(pager: &Arc<Pager>, lookup: &FkLookup, child_key: &[Value]) -> Result<bool> {
    use crate::btree::cursor::TableCursor;
    use crate::btree::index_cursor::{IndexCursor, SeekOp};
    use crate::format::decode_record;
    match lookup {
        FkLookup::RowidSeek { root } => {
            let rowid = child_key[0].as_i64();
            let mut cur = TableCursor::new(pager.clone(), *root);
            cur.seek_rowid(rowid).await
        }
        FkLookup::IndexSeek { root, key_info } => {
            let mut cur = IndexCursor::new(pager.clone(), *root, key_info.clone());
            let positioned = cur.seek(SeekOp::Ge, child_key).await?;
            if !positioned {
                return Ok(false);
            }
            let payload = cur.payload().to_vec();
            let entry = decode_record(&payload, pager.text_encoding())?;
            let prefix_len = entry.len().saturating_sub(1).min(child_key.len());
            let prefix = &entry[..prefix_len];
            Ok(compare_prefix(prefix, child_key, key_info) == Ordering::Equal)
        }
        FkLookup::TableScan { root, parent_col_indices, parent_rowid_alias } => {
            let rows = crate::btree::scan_table(pager, *root).await?;
            let encoding = pager.text_encoding();
            for (rowid, payload) in rows {
                let pvalues = decode_record(&payload, encoding)?;
                let mut matched = true;
                for (i, &parent_col_idx) in parent_col_indices.iter().enumerate() {
                    let pv = if Some(parent_col_idx) == *parent_rowid_alias {
                        Value::Int(rowid)
                    } else {
                        pvalues.get(parent_col_idx).cloned().unwrap_or(Value::Null)
                    };
                    if mem_compare(&pv, &child_key[i], Collation::Binary) != Ordering::Equal {
                        matched = false;
                        break;
                    }
                }
                if matched {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        FkLookup::ParentMissing => Ok(false),
    }
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
            num_registers: 4, num_cursors: 0,
            ..Default::default()
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
            num_registers: 2, num_cursors: 0,
            ..Default::default()
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
            num_registers: 3, num_cursors: 0,
            ..Default::default()
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

    /// Hand-built program that exercises `AggStep` → `AggValue` → `AggInverse` → `AggValue` →
    /// `AggFinal` over a `sum` accumulator, modeling the sliding-window shape M11.3 enables.
    /// The window slides over values `[10, 20, 30]` with a 2-row frame:
    ///   * step 10 → value = 10
    ///   * step 20 → value = 30
    ///   * inverse 10 → value = 20
    ///   * step 30 → value = 50
    ///   * finalize → 50
    #[tokio::test]
    async fn agg_inverse_and_value_sum_sliding_window() {
        // r2 = arg, r3 = accumulator, r4 = value-out
        let prog = Program {
            instructions: vec![
                inst(Opcode::Init, 0, 17, 0), // 0: Init -> 17 (Transaction)
                // step 10
                inst(Opcode::Integer, 10, 2, 0), // 1
                inst_with(Opcode::AggStep, 0, 2, 3, P4::FuncDef(AggregateKind::Sum), 1), // 2
                inst_with(Opcode::AggValue, 3, 0, 4, P4::FuncDef(AggregateKind::Sum), 0), // 3: r4 = 10
                inst(Opcode::ResultRow, 4, 1, 0), // 4: emit 10
                // step 20
                inst(Opcode::Integer, 20, 2, 0), // 5
                inst_with(Opcode::AggStep, 0, 2, 3, P4::FuncDef(AggregateKind::Sum), 1), // 6
                inst_with(Opcode::AggValue, 3, 0, 4, P4::FuncDef(AggregateKind::Sum), 0), // 7: r4 = 30
                inst(Opcode::ResultRow, 4, 1, 0), // 8: emit 30
                // inverse 10
                inst(Opcode::Integer, 10, 2, 0), // 9
                inst_with(Opcode::AggInverse, 1, 2, 3, P4::FuncDef(AggregateKind::Sum), 1), // 10
                inst_with(Opcode::AggValue, 3, 0, 4, P4::FuncDef(AggregateKind::Sum), 0), // 11: r4 = 20
                inst(Opcode::ResultRow, 4, 1, 0), // 12: emit 20
                // step 30
                inst(Opcode::Integer, 30, 2, 0), // 13
                inst_with(Opcode::AggStep, 0, 2, 3, P4::FuncDef(AggregateKind::Sum), 1), // 14
                inst_with(Opcode::AggFinal, 3, 0, 0, P4::FuncDef(AggregateKind::Sum), 0), // 15: r3 = 50
                inst(Opcode::Halt, 0, 0, 0), // 16
                inst(Opcode::Transaction, 0, 0, 0), // 17
                inst(Opcode::Goto, 0, 1, 0), // 18
            ],
            num_registers: 5, num_cursors: 0,
            ..Default::default()
        };
        let mut v = Vdbe::new(Arc::new(prog), None);
        let mut rows: Vec<Vec<Value>> = Vec::new();
        while let StepResult::Row = v.step().await.expect("step") {
            let n = v.result_count();
            rows.push((0..n).map(|i| v.result_value(i)).collect());
        }
        // Three AggValue rows (10, 30, 20), then AggFinal writes r3 but we don't emit it.
        assert_eq!(rows, vec![vec![Value::Int(10)], vec![Value::Int(30)], vec![Value::Int(20)]]);
    }

    /// `count(*)` sliding window: step 3 rows → value 3; inverse one → value 2; finalize → 2.
    #[tokio::test]
    async fn agg_inverse_count_star_sliding_window() {
        // r3 = accumulator, r4 = value-out
        let prog = Program {
            instructions: vec![
                inst(Opcode::Init, 0, 12, 0), // 0: Init -> 12 (Transaction)
                // step, step, step (count(*) has 0 args)
                inst_with(Opcode::AggStep, 0, 0, 3, P4::FuncDef(AggregateKind::Count), 0), // 1
                inst_with(Opcode::AggStep, 0, 0, 3, P4::FuncDef(AggregateKind::Count), 0), // 2
                inst_with(Opcode::AggStep, 0, 0, 3, P4::FuncDef(AggregateKind::Count), 0), // 3
                inst_with(Opcode::AggValue, 3, 0, 4, P4::FuncDef(AggregateKind::Count), 0), // 4: r4 = 3
                inst(Opcode::ResultRow, 4, 1, 0), // 5: emit 3
                // inverse one
                inst_with(Opcode::AggInverse, 1, 0, 3, P4::FuncDef(AggregateKind::Count), 0), // 6
                inst_with(Opcode::AggValue, 3, 0, 4, P4::FuncDef(AggregateKind::Count), 0), // 7: r4 = 2
                inst(Opcode::ResultRow, 4, 1, 0), // 8: emit 2
                inst_with(Opcode::AggFinal, 3, 0, 0, P4::FuncDef(AggregateKind::Count), 0), // 9: r3 = 2
                inst(Opcode::Halt, 0, 0, 0), // 10
                inst(Opcode::Transaction, 0, 0, 0), // 11
                inst(Opcode::Goto, 0, 1, 0), // 12
            ],
            num_registers: 5, num_cursors: 0,
            ..Default::default()
        };
        let mut v = Vdbe::new(Arc::new(prog), None);
        let mut rows: Vec<Vec<Value>> = Vec::new();
        while let StepResult::Row = v.step().await.expect("step") {
            let n = v.result_count();
            rows.push((0..n).map(|i| v.result_value(i)).collect());
        }
        assert_eq!(rows, vec![vec![Value::Int(3)], vec![Value::Int(2)]]);
    }

    /// `group_concat` sliding window: step "a", "b" → "a,b"; inverse "a" → "b"; step "c" → "b,c".
    #[tokio::test]
    async fn agg_inverse_group_concat_sliding_window() {
        // r2 = arg, r3 = accumulator, r4 = value-out
        let prog = Program {
            instructions: vec![
                inst(Opcode::Init, 0, 15, 0), // 0: Init -> 15 (Transaction)
                // step "a"
                inst_with(Opcode::String8, 0, 2, 0, P4::Text("a".to_string()), 0), // 1
                inst_with(Opcode::AggStep, 0, 2, 3, P4::FuncDef(AggregateKind::GroupConcat), 1), // 2
                // step "b"
                inst_with(Opcode::String8, 0, 2, 0, P4::Text("b".to_string()), 0), // 3
                inst_with(Opcode::AggStep, 0, 2, 3, P4::FuncDef(AggregateKind::GroupConcat), 1), // 4
                inst_with(Opcode::AggValue, 3, 0, 4, P4::FuncDef(AggregateKind::GroupConcat), 0), // 5: r4 = "a,b"
                inst(Opcode::ResultRow, 4, 1, 0), // 6: emit "a,b"
                // inverse "a"
                inst_with(Opcode::String8, 0, 2, 0, P4::Text("a".to_string()), 0), // 7
                inst_with(Opcode::AggInverse, 1, 2, 3, P4::FuncDef(AggregateKind::GroupConcat), 1), // 8
                inst_with(Opcode::AggValue, 3, 0, 4, P4::FuncDef(AggregateKind::GroupConcat), 0), // 9: r4 = "b"
                inst(Opcode::ResultRow, 4, 1, 0), // 10: emit "b"
                // step "c"
                inst_with(Opcode::String8, 0, 2, 0, P4::Text("c".to_string()), 0), // 11
                inst_with(Opcode::AggStep, 0, 2, 3, P4::FuncDef(AggregateKind::GroupConcat), 1), // 12
                inst_with(Opcode::AggFinal, 3, 0, 0, P4::FuncDef(AggregateKind::GroupConcat), 0), // 13: r3 = "b,c"
                inst(Opcode::Halt, 0, 0, 0), // 14
                inst(Opcode::Transaction, 0, 0, 0), // 15
                inst(Opcode::Goto, 0, 1, 0), // 16
            ],
            num_registers: 5, num_cursors: 0,
            ..Default::default()
        };
        let mut v = Vdbe::new(Arc::new(prog), None);
        let mut rows: Vec<Vec<Value>> = Vec::new();
        while let StepResult::Row = v.step().await.expect("step") {
            let n = v.result_count();
            rows.push((0..n).map(|i| v.result_value(i)).collect());
        }
        assert_eq!(
            rows,
            vec![vec![Value::Text("a,b".to_string())], vec![Value::Text("b".to_string())]]
        );
    }

    /// `AggValue` on a fresh (never-stepped) accumulator yields the empty-group result
    /// (matches `AggFinal` on an empty group: `count` → 0, `sum` → NULL).
    #[tokio::test]
    async fn agg_value_empty_accumulator() {
        // r2 = count value, r3 = sum value (via SCopy from r4), r4 = sum value-out.
        let prog = Program {
            instructions: vec![
                inst(Opcode::Init, 0, 7, 0), // 0: Init -> 7 (Transaction)
                inst_with(Opcode::AggValue, 1, 0, 2, P4::FuncDef(AggregateKind::Count), 0), // 1: r2 = 0
                inst_with(Opcode::AggValue, 3, 0, 4, P4::FuncDef(AggregateKind::Sum), 0), // 2: r4 = NULL
                inst(Opcode::SCopy, 4, 3, 0), // 3: r3 = r4 (sum)
                inst(Opcode::ResultRow, 2, 2, 0), // 4: emit r2..r3 (count, sum)
                inst(Opcode::Halt, 0, 0, 0), // 5
                inst(Opcode::Transaction, 0, 0, 0), // 6
                inst(Opcode::Goto, 0, 1, 0), // 7
            ],
            num_registers: 5, num_cursors: 0,
            ..Default::default()
        };
        let mut v = Vdbe::new(Arc::new(prog), None);
        let mut rows: Vec<Vec<Value>> = Vec::new();
        while let StepResult::Row = v.step().await.expect("step") {
            let n = v.result_count();
            rows.push((0..n).map(|i| v.result_value(i)).collect());
        }
        // count(empty) = 0, sum(empty) = NULL.
        assert_eq!(rows, vec![vec![Value::Int(0), Value::Null]]);
    }

    /// `AggInverse` without a preceding `AggStep` on the accumulator raises an error
    /// (mirrors upstream's `assert(pMem->uTemp == 0x1122e0e3)`).
    #[tokio::test]
    async fn agg_inverse_without_step_errors() {
        let prog = Program {
            instructions: vec![
                inst(Opcode::Init, 0, 5, 0), // 0: Init -> 5
                inst_with(Opcode::AggInverse, 1, 2, 3, P4::FuncDef(AggregateKind::Sum), 1), // 1
                inst(Opcode::Halt, 0, 0, 0), // 2
                inst(Opcode::Transaction, 0, 0, 0), // 3
                inst(Opcode::Goto, 0, 1, 0), // 4
            ],
            num_registers: 4, num_cursors: 0,
            ..Default::default()
        };
        let mut v = Vdbe::new(Arc::new(prog), None);
        let res = v.step().await;
        assert!(res.is_err(), "AggInverse without AggStep should error");
    }

    /// Hand-built program exercising `InitCoroutine` / `Yield` / `EndCoroutine` in the
    /// shape used by `FROM (subquery)` materialization: a main loop drives a coroutine
    /// that yields 3 rows then ends. The test verifies the coroutine protocol:
    ///   - `InitCoroutine` sets `r[1]` to the coroutine entry and jumps over the body.
    ///   - Each `Yield` swaps PC with `r[1]`; the coroutine resumes after the previous
    ///     `Yield`, and the main code resumes after its `Yield`.
    ///   - When the coroutine runs off the end (here, via explicit `EndCoroutine`), the
    ///     main `Yield` jumps to its `p2` continuation (the loop exit).
    ///
    /// Program layout (registers: r1 = coroutine reg, r2 = counter, r3 = output value):
    /// ```text
    ///   0  Init           1  9  6     ; r[1] = 6 (coroutine entry); jump to 9
    ///   1  Integer        0  2  0     ; r2 = 0 (loop counter)        [main]
    ///   2  Integer        0  3  0     ; r3 = 0 (output)              [main]
    ///   3  Yield          1  8  0     ; swap PC with r[1]            [main loop top]
    ///   4  Add            2  3  2     ; r2 = r2 + 1 (counter)        [coroutine body]
    ///   5  Yield          1  8  0     ; swap back to main            [coroutine body]
    ///   6  Goto           0  4  0     ; re-enter coroutine body      [coroutine entry]
    ///   ... (unreachable: EndCoroutine at 7, but our coroutine never ends naturally;
    ///        we drive it for 3 rows then bail out via Halt)
    ///   8  Halt           0  0  0     ; coroutine-ended continuation
    ///   9  Transaction    0  0  0     ; setup
    ///  10  Goto           0  1  0     ; enter main
    /// ```
    /// We use a simpler coroutine body that runs a fixed number of iterations and then
    /// jumps to an `EndCoroutine`. The main loop terminates when `Yield` jumps to its
    /// `p2` (after `EndCoroutine`).
    ///
    /// Concrete shape (registers: r1 = coroutine reg, r2 = counter, r3 = limit, r4 = output):
    /// ```text
    ///   0  Init           1  11 3     ; r[1] = 3 (coroutine entry); jump to 11
    ///   1  Integer        1  4  0     ; r4 = 1 (output value)        [main]
    ///   2  Yield          1  9  0     ; swap PC with r[1] (start coro)  [main loop top]
    ///   3  Add            4  4  4     ; r4 = r4 + r4 (double)         [coro body]
    ///   4  Le             4  6  4    ; if r4 <= 8 jump back to 6     [coro body]
    ///     -- (no: we want a counter; let's use r2 as counter, r3 as limit)
    /// ```
    /// Simpler: the coroutine yields exactly 3 rows by tracking a counter.
    #[tokio::test]
    async fn coroutine_init_yield_end_basic() {
        // Registers: r1 = coroutine reg, r4 = output value.
        //
        // The standard SQLite pattern: the coroutine produces a row, then Yields back to
        // main, which emits and loops. The 3rd Yield returns to main (emit 30), then main
        // loops back to Yield which re-enters the coro at EndCoroutine → main's Yield p2.
        //
        // Layout:
        //   0  Init           0 14 0      ; Init -> 14 (setup)
        //   1  InitCoroutine  1  4 6      ; r[1] = 6 (coro entry); jump to 4 (main entry)
        //   2  Yield          1 12 0      ; main loop: swap; on EndCoro -> 12
        //   3  ResultRow      4 1 0       ; emit r4
        //   4  Goto           0  2 0      ; main entry: loop back to Yield
        //   5  Noop           0 0 0       ; (padding so coro entry is at 6)
        //   6  Integer       10 4 0       ; coro entry: r4 = 10
        //   7  Yield          1 0 0       ; yield back to main (returns to 3)
        //   8  Integer       20 4 0       ; r4 = 20
        //   9  Yield          1 0 0       ; yield back to main (returns to 3)
        //  10  Integer       30 4 0       ; r4 = 30
        //  11  Yield          1 0 0       ; yield back to main (returns to 3)
        //  12  EndCoroutine   1 0 0        ; end coro -> main Yield's p2 (=12, which is Halt)
        //  -- Wait, EndCoroutine at 12 and Halt at 12 collides. Let me use a separate slot.
        //
        // Rewritten: coroutine body ends with EndCoroutine as a separate instruction.
        let prog = Program {
            instructions: vec![
                inst(Opcode::Init, 0, 14, 0),         // 0: Init -> 14
                inst(Opcode::InitCoroutine, 1, 4, 6), // 1: r[1]=6; jump to 4
                inst(Opcode::Yield, 1, 13, 0),       // 2: main loop Yield; EndCoro -> 13
                inst(Opcode::ResultRow, 4, 1, 0),     // 3: emit r4
                inst(Opcode::Goto, 0, 2, 0),          // 4: main entry
                inst(Opcode::Goto, 0, 6, 0),          // 5: padding (jump to coro entry)
                // coroutine body (entry at 6):
                inst(Opcode::Integer, 10, 4, 0),      // 6: r4 = 10
                inst(Opcode::Yield, 1, 0, 0),         // 7: yield back to main
                inst(Opcode::Integer, 20, 4, 0),     // 8: r4 = 20
                inst(Opcode::Yield, 1, 0, 0),         // 9: yield back to main
                inst(Opcode::Integer, 30, 4, 0),      // 10: r4 = 30
                inst(Opcode::Yield, 1, 0, 0),         // 11: yield back to main
                inst(Opcode::EndCoroutine, 1, 0, 0), // 12: end coro -> main Yield p2 (=13)
                inst(Opcode::Halt, 0, 0, 0),          // 13: halt
                inst(Opcode::Transaction, 0, 0, 0),   // 14: setup
                inst(Opcode::Goto, 0, 1, 0),          // 15: enter program
            ],
            num_registers: 5, num_cursors: 0,
            ..Default::default()
        };
        let mut v = Vdbe::new(Arc::new(prog), None);
        let mut rows: Vec<Vec<Value>> = Vec::new();
        while let StepResult::Row = v.step().await.expect("step") {
            let n = v.result_count();
            rows.push((0..n).map(|i| v.result_value(i)).collect());
        }
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(10)],
                vec![Value::Int(20)],
                vec![Value::Int(30)],
            ],
            "coroutine should yield 3 rows in order"
        );
    }

    /// A coroutine that immediately ends (0-row subquery): the main loop's first `Yield`
    /// jumps to the coroutine entry which is an `EndCoroutine`. The main's `Yield` `p2`
    /// continuation fires and no `ResultRow` is emitted.
    #[tokio::test]
    async fn coroutine_empty() {
        // Layout:
        //   0  Init           0 10 0      ; -> 10 (setup)
        //   1  InitCoroutine  1  4 5      ; r[1] = 5 (coro entry); jump to 4
        //   2  Yield          1  7 0       ; main loop: swap; on EndCoro -> 7
        //   3  ResultRow      4 1 0       ; emit r4
        //   4  Goto           0  2 0       ; main entry
        //   5  EndCoroutine   1  0 0      ; coro entry: immediately end
        //   6  Halt           0  0 0       ; (unreachable padding)
        //   7  Halt           0  0 0       ; main continuation (after coro end)
        //   8  Halt           0  0 0       ; (padding)
        //   9  Halt           0  0 0       ; (padding)
        //  10  Transaction    0  0 0       ; setup
        //  11  Goto           0  1 0       ; enter program
        let prog = Program {
            instructions: vec![
                inst(Opcode::Init, 0, 10, 0),         // 0
                inst(Opcode::InitCoroutine, 1, 4, 5), // 1
                inst(Opcode::Yield, 1, 7, 0),          // 2: EndCoro -> 7
                inst(Opcode::ResultRow, 4, 1, 0),     // 3
                inst(Opcode::Goto, 0, 2, 0),           // 4
                inst(Opcode::EndCoroutine, 1, 0, 0),  // 5: coro entry = end
                inst(Opcode::Halt, 0, 0, 0),          // 6 (unreachable)
                inst(Opcode::Halt, 0, 0, 0),          // 7: main continuation
                inst(Opcode::Halt, 0, 0, 0),          // 8 (padding)
                inst(Opcode::Halt, 0, 0, 0),          // 9 (padding)
                inst(Opcode::Transaction, 0, 0, 0),  // 10: setup
                inst(Opcode::Goto, 0, 1, 0),          // 11: enter program
            ],
            num_registers: 5, num_cursors: 0,
            ..Default::default()
        };
        let mut v = Vdbe::new(Arc::new(prog), None);
        let mut rows: Vec<Vec<Value>> = Vec::new();
        while let StepResult::Row = v.step().await.expect("step") {
            let n = v.result_count();
            rows.push((0..n).map(|i| v.result_value(i)).collect());
        }
        assert!(rows.is_empty(), "empty coroutine should emit no rows");
    }

    /// `OP_Program` / `OP_Param` round-trip: a parent program loads 42 into r1, invokes a
    /// sub-program whose body reads the parent's r1 via `OP_Param` and emits a result row
    /// computed from it (doubled). When the sub-program halts, control returns to the parent,
    /// which then emits its own row. Verifies the frame save/restore, the `Param` register
    /// resolution (`param_base + p1`), and the `Halt`-pops-frame path.
    #[tokio::test]
    async fn program_param_round_trip() {
        // Sub-program layout (3 instructions, 3 registers):
        //   0 Param  0  1 0   ; r1 = parent[param_base + 0] = parent r1 = 42
        //   1 Add    1  1 2   ; r2 = r1 + r1 = 84
        //   2 ResultRow 2 1 0 ; emit r2
        //   3 Halt   0  0 0   ; return to parent
        let sub = Program {
            instructions: vec![
                inst(Opcode::Param, 0, 1, 0),
                inst(Opcode::Add, 1, 1, 2),
                inst(Opcode::ResultRow, 2, 1, 0),
                inst(Opcode::Halt, 0, 0, 0),
            ],
            num_registers: 3, num_cursors: 0,
            ..Default::default()
        };
        // Parent layout (6 instructions, 4 registers):
        //   0 Init    0  6 0          ; -> 6 (setup)
        //   1 Integer 42 1 0          ; r1 = 42
        //   2 Program 1  0 3 SubProgram(0)  ; call sub with param_base=1, runtime r3
        //   3 ResultRow 1 1 0         ; emit parent r1 (42) after sub returns
        //   4 Halt    0  0 0          ; done
        //   5 Transaction 0 0 0       ; setup (Init jumps here)
        //   6 Goto 0 1 0
        let parent = Program {
            instructions: vec![
                inst(Opcode::Init, 0, 5, 0),
                inst(Opcode::Integer, 42, 1, 0),
                inst_with(
                    Opcode::Program,
                    1,
                    0,
                    3,
                    P4::SubProgram(Arc::new(sub)),
                    0,
                ),
                inst(Opcode::ResultRow, 1, 1, 0),
                inst(Opcode::Halt, 0, 0, 0),
                inst(Opcode::Transaction, 0, 0, 0),
                inst(Opcode::Goto, 0, 1, 0),
            ],
            num_registers: 4, num_cursors: 0,
            ..Default::default()
        };
        let mut v = Vdbe::new(Arc::new(parent), None);
        let mut rows: Vec<Vec<Value>> = Vec::new();
        while let StepResult::Row = v.step().await.expect("step") {
            let n = v.result_count();
            rows.push((0..n).map(|i| v.result_value(i)).collect());
        }
        // First row: from the sub-program (84). Second row: from the parent after return (42).
        assert_eq!(
            rows,
            vec![vec![Value::Int(84)], vec![Value::Int(42)]],
            "sub-program should emit its row then parent resumes"
        );
    }

    /// `OP_Program` with `OE_Ignore` halt: the sub-program halts with `p2 == 5` (OE_Ignore),
    /// so the parent jumps to the calling `OP_Program`'s `p2` instead of resuming at the next
    /// instruction. Verifies the ignore-jump path.
    #[tokio::test]
    async fn program_halt_with_ignore_jumps_to_caller_p2() {
        // Sub-program: immediately halt with OE_Ignore (p2 == 5), emitting no rows.
        let sub = Program {
            instructions: vec![
                inst(Opcode::Halt, 0, 5, 0), // p1=OK, p2=OE_Ignore
            ],
            num_registers: 1, num_cursors: 0,
            ..Default::default()
        };
        // Parent layout:
        //   0 Init    0  6 0
        //   1 Program 1  4 2 SubProgram(0)  ; call sub; ignore-jump target = 4
        //   2 Integer 99 1 0                ; r1 = 99 (skipped on ignore)
        //   3 ResultRow 1 1 0               ; emit 99 (skipped on ignore)
        //   4 Integer 77 1 0                ; ignore target: r1 = 77
        //   5 ResultRow 1 1 0               ; emit 77
        //   6 Halt    0  0 0
        //   7 Transaction 0 0 0
        //   8 Goto 0 1 0
        let parent = Program {
            instructions: vec![
                inst(Opcode::Init, 0, 7, 0),
                inst_with(Opcode::Program, 1, 4, 2, P4::SubProgram(Arc::new(sub)), 0),
                inst(Opcode::Integer, 99, 1, 0),
                inst(Opcode::ResultRow, 1, 1, 0),
                inst(Opcode::Integer, 77, 1, 0),
                inst(Opcode::ResultRow, 1, 1, 0),
                inst(Opcode::Halt, 0, 0, 0),
                inst(Opcode::Transaction, 0, 0, 0),
                inst(Opcode::Goto, 0, 1, 0),
            ],
            num_registers: 3, num_cursors: 0,
            ..Default::default()
        };
        let mut v = Vdbe::new(Arc::new(parent), None);
        let mut rows: Vec<Vec<Value>> = Vec::new();
        while let StepResult::Row = v.step().await.expect("step") {
            let n = v.result_count();
            rows.push((0..n).map(|i| v.result_value(i)).collect());
        }
        // The sub-program's IGNORE halts jumps to caller.p2 = 4, skipping the 99 row.
        assert_eq!(
            rows,
            vec![vec![Value::Int(77)]],
            "OE_Ignore halt should jump to the calling Program's p2"
        );
    }

    /// Recursive-trigger guard: a sub-program invoked with `p5 != 0` while a frame with the
    /// same token is already on the stack is skipped (the recursive call is a no-op). Verifies
    /// the `p5`-token guard in `OP_Program`.
    #[tokio::test]
    async fn program_recursive_guard_skips_duplicate_token() {
        // The parent calls an outer sub-program with p5=7. The outer sub-program emits a row
        // and then attempts to call an inner sub-program with the SAME token (7). The guard
        // sees the outer's frame (token 7) on the stack and skips the inner call, so the inner
        // sub-program's row (999) is never emitted.
        let inner = Program {
            instructions: vec![
                inst(Opcode::Integer, 999, 1, 0),
                inst(Opcode::ResultRow, 1, 1, 0),
                inst(Opcode::Halt, 0, 0, 0),
            ],
            num_registers: 2, num_cursors: 0,
            ..Default::default()
        };
        let inner_arc = Arc::new(inner);
        let outer = Program {
            instructions: vec![
                inst(Opcode::Integer, 1, 1, 0),
                inst(Opcode::ResultRow, 1, 1, 0),
                inst_with(Opcode::Program, 0, 0, 2, P4::SubProgram(inner_arc), 7),
                inst(Opcode::Halt, 0, 0, 0),
            ],
            num_registers: 3, num_cursors: 0,
            ..Default::default()
        };
        let outer_arc = Arc::new(outer);
        let parent = Program {
            instructions: vec![
                inst(Opcode::Init, 0, 4, 0),
                inst_with(Opcode::Program, 0, 0, 1, P4::SubProgram(outer_arc), 7),
                inst(Opcode::Halt, 0, 0, 0),
                inst(Opcode::Transaction, 0, 0, 0),
                inst(Opcode::Goto, 0, 1, 0),
            ],
            num_registers: 2, num_cursors: 0,
            ..Default::default()
        };
        let mut v = Vdbe::new(Arc::new(parent), None);
        let mut rows: Vec<Vec<Value>> = Vec::new();
        while let StepResult::Row = v.step().await.expect("step") {
            let n = v.result_count();
            rows.push((0..n).map(|i| v.result_value(i)).collect());
        }
        // Only the outer sub's row (1) is emitted; the inner call is suppressed by the guard.
        assert_eq!(
            rows,
            vec![vec![Value::Int(1)]],
            "recursive call with same p5 token should be skipped"
        );
    }

    // ---- window-only built-in accumulator VDBE tests (M11.4–M11.6) ----
    //
    // These exercise the executor's `AggStep`/`AggInverse`/`AggValue`/`AggFinal` dispatch for
    // the window-only `AggregateKind` variants via hand-built programs (no pager needed). The
    // full end-to-end window codegen driver lands in M11.7; these verify the accumulator +
    // opcode plumbing is correct in isolation.

    /// `row_number()` over a 3-row frame: step 3 times (no inverse — the default frame only
    /// grows), value after each step emits 1, 2, 3. Verifies the executor dispatches
    /// `RowNumber` through `AggStep` (counter bump) and `AggValue` (via `value_mut`).
    #[tokio::test]
    async fn agg_value_row_number_increments() {
        // r3 = accumulator, r4 = value-out. row_number() takes 0 args.
        let prog = Program {
            instructions: vec![
                inst(Opcode::Init, 0, 11, 0), // 0: Init -> 11 (Transaction)
                inst_with(Opcode::AggStep, 0, 0, 3, P4::FuncDef(AggregateKind::RowNumber), 0), // 1
                inst_with(Opcode::AggValue, 3, 0, 4, P4::FuncDef(AggregateKind::RowNumber), 0), // 2: r4 = 1
                inst(Opcode::ResultRow, 4, 1, 0), // 3: emit 1
                inst_with(Opcode::AggStep, 0, 0, 3, P4::FuncDef(AggregateKind::RowNumber), 0), // 4
                inst_with(Opcode::AggValue, 3, 0, 4, P4::FuncDef(AggregateKind::RowNumber), 0), // 5: r4 = 2
                inst(Opcode::ResultRow, 4, 1, 0), // 6: emit 2
                inst_with(Opcode::AggStep, 0, 0, 3, P4::FuncDef(AggregateKind::RowNumber), 0), // 7
                inst_with(Opcode::AggValue, 3, 0, 4, P4::FuncDef(AggregateKind::RowNumber), 0), // 8: r4 = 3
                inst(Opcode::ResultRow, 4, 1, 0), // 9: emit 3
                inst(Opcode::Halt, 0, 0, 0), // 10
                inst(Opcode::Transaction, 0, 0, 0), // 11
                inst(Opcode::Goto, 0, 1, 0), // 12
            ],
            num_registers: 5, num_cursors: 0,
            ..Default::default()
        };
        let mut v = Vdbe::new(Arc::new(prog), None);
        let mut rows: Vec<Vec<Value>> = Vec::new();
        while let StepResult::Row = v.step().await.expect("step") {
            let n = v.result_count();
            rows.push((0..n).map(|i| v.result_value(i)).collect());
        }
        assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(2)], vec![Value::Int(3)]]);
    }

    /// `rank()` over a 3-row peer group then a 2-row peer group. Step bumps `nStep`; on the
    /// first row of a peer group `nValue` is latched to `nStep`. `AggValue` (via `value_mut`)
    /// emits the latched value and resets `nValue = 0` so the next peer re-latches.
    #[tokio::test]
    async fn agg_value_rank_latches_peer_groups() {
        // r3 = accumulator, r4 = value-out. rank() takes 0 args.
        // Peer group 1 (3 rows): steps 1-3, latches nValue=1 on the first step.
        //   value_mut → 1, nValue reset to 0.
        // Peer group 2 (2 rows): steps 4-5, latches nValue=4 on the first step of the peer.
        //   value_mut → 4.
        let prog = Program {
            instructions: vec![
                inst(Opcode::Init, 0, 11, 0), // 0: Init -> 11 (Transaction)
                // Peer group 1: step 3 times.
                inst_with(Opcode::AggStep, 0, 0, 3, P4::FuncDef(AggregateKind::Rank), 0), // 1
                inst_with(Opcode::AggStep, 0, 0, 3, P4::FuncDef(AggregateKind::Rank), 0), // 2
                inst_with(Opcode::AggStep, 0, 0, 3, P4::FuncDef(AggregateKind::Rank), 0), // 3
                inst_with(Opcode::AggValue, 3, 0, 4, P4::FuncDef(AggregateKind::Rank), 0), // 4: r4 = 1
                inst(Opcode::ResultRow, 4, 1, 0), // 5: emit 1
                // Peer group 2: step 2 times (nStep is now 5; first step of peer latches nValue=4).
                inst_with(Opcode::AggStep, 0, 0, 3, P4::FuncDef(AggregateKind::Rank), 0), // 6
                inst_with(Opcode::AggStep, 0, 0, 3, P4::FuncDef(AggregateKind::Rank), 0), // 7
                inst_with(Opcode::AggValue, 3, 0, 4, P4::FuncDef(AggregateKind::Rank), 0), // 8: r4 = 4
                inst(Opcode::ResultRow, 4, 1, 0), // 9: emit 4
                inst(Opcode::Halt, 0, 0, 0), // 10
                inst(Opcode::Transaction, 0, 0, 0), // 11
                inst(Opcode::Goto, 0, 1, 0), // 12
            ],
            num_registers: 5, num_cursors: 0,
            ..Default::default()
        };
        let mut v = Vdbe::new(Arc::new(prog), None);
        let mut rows: Vec<Vec<Value>> = Vec::new();
        while let StepResult::Row = v.step().await.expect("step") {
            let n = v.result_count();
            rows.push((0..n).map(|i| v.result_value(i)).collect());
        }
        assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(4)]]);
    }

    /// `cume_dist()` over a 4-row partition. Step counts `nTotal` (4); inverse counts `nStep`
    /// (the row index from the start + 1). `AggValue` (via `value_mut`) emits `nStep / nTotal`.
    /// Verifies the executor dispatches `CumeDist` through both `AggStep` and `AggInverse`.
    #[tokio::test]
    async fn agg_value_cume_dist_ratio() {
        // r3 = accumulator, r4 = value-out. cume_dist() takes 0 args.
        let prog = Program {
            instructions: vec![
                inst(Opcode::Init, 0, 12, 0), // 0: Init -> 12 (Transaction)
                // 4 steps: nTotal=4.
                inst_with(Opcode::AggStep, 0, 0, 3, P4::FuncDef(AggregateKind::CumeDist), 0), // 1
                inst_with(Opcode::AggStep, 0, 0, 3, P4::FuncDef(AggregateKind::CumeDist), 0), // 2
                inst_with(Opcode::AggStep, 0, 0, 3, P4::FuncDef(AggregateKind::CumeDist), 0), // 3
                inst_with(Opcode::AggStep, 0, 0, 3, P4::FuncDef(AggregateKind::CumeDist), 0), // 4
                // inverse 1 → nStep=1, value = 1/4 = 0.25
                inst_with(Opcode::AggInverse, 1, 0, 3, P4::FuncDef(AggregateKind::CumeDist), 0), // 5
                inst_with(Opcode::AggValue, 3, 0, 4, P4::FuncDef(AggregateKind::CumeDist), 0), // 6: r4 = 0.25
                inst(Opcode::ResultRow, 4, 1, 0), // 7: emit 0.25
                // inverse 1 → nStep=2, value = 2/4 = 0.5
                inst_with(Opcode::AggInverse, 1, 0, 3, P4::FuncDef(AggregateKind::CumeDist), 0), // 8
                inst_with(Opcode::AggValue, 3, 0, 4, P4::FuncDef(AggregateKind::CumeDist), 0), // 9: r4 = 0.5
                inst(Opcode::ResultRow, 4, 1, 0), // 10: emit 0.5
                inst(Opcode::Halt, 0, 0, 0), // 11
                inst(Opcode::Transaction, 0, 0, 0), // 12
                inst(Opcode::Goto, 0, 1, 0), // 13
            ],
            num_registers: 5, num_cursors: 0,
            ..Default::default()
        };
        let mut v = Vdbe::new(Arc::new(prog), None);
        let mut rows: Vec<Vec<Value>> = Vec::new();
        while let StepResult::Row = v.step().await.expect("step") {
            let n = v.result_count();
            rows.push((0..n).map(|i| v.result_value(i)).collect());
        }
        assert_eq!(rows.len(), 2);
        assert!(matches!(rows[0][0], Value::Real(r) if (r - 0.25).abs() < 1e-12));
        assert!(matches!(rows[1][0], Value::Real(r) if (r - 0.5).abs() < 1e-12));
    }

    /// `ntile(3)` over a 7-row partition. Step captures `nParam=3` on the first call and
    /// counts `nTotal` (7); inverse bumps `iRow`. `AggValue` emits the 1-based bucket index.
    #[tokio::test]
    async fn agg_value_ntile_buckets() {
        // r2 = arg (nParam), r3 = accumulator, r4 = value-out. ntile(N) takes 1 arg.
        let prog = Program {
            instructions: vec![
                inst(Opcode::Init, 0, 18, 0), // 0: Init -> 18 (Transaction)
                // 7 steps with N=3: nParam=3, nTotal=7.
                inst(Opcode::Integer, 3, 2, 0), // 1
                inst_with(Opcode::AggStep, 0, 2, 3, P4::FuncDef(AggregateKind::Ntile), 1), // 2
                inst(Opcode::Integer, 3, 2, 0), // 3
                inst_with(Opcode::AggStep, 0, 2, 3, P4::FuncDef(AggregateKind::Ntile), 1), // 4
                inst(Opcode::Integer, 3, 2, 0), // 5
                inst_with(Opcode::AggStep, 0, 2, 3, P4::FuncDef(AggregateKind::Ntile), 1), // 6
                inst(Opcode::Integer, 3, 2, 0), // 7
                inst_with(Opcode::AggStep, 0, 2, 3, P4::FuncDef(AggregateKind::Ntile), 1), // 8
                inst(Opcode::Integer, 3, 2, 0), // 9
                inst_with(Opcode::AggStep, 0, 2, 3, P4::FuncDef(AggregateKind::Ntile), 1), // 10
                inst(Opcode::Integer, 3, 2, 0), // 11
                inst_with(Opcode::AggStep, 0, 2, 3, P4::FuncDef(AggregateKind::Ntile), 1), // 12
                inst(Opcode::Integer, 3, 2, 0), // 13
                inst_with(Opcode::AggStep, 0, 2, 3, P4::FuncDef(AggregateKind::Ntile), 1), // 14
                // iRow=0: value = 1 + 0/3 = 1
                inst_with(Opcode::AggValue, 3, 0, 4, P4::FuncDef(AggregateKind::Ntile), 0), // 15: r4 = 1
                inst(Opcode::ResultRow, 4, 1, 0), // 16: emit 1
                inst(Opcode::Halt, 0, 0, 0), // 17
                inst(Opcode::Transaction, 0, 0, 0), // 18
                inst(Opcode::Goto, 0, 1, 0), // 19
            ],
            num_registers: 5, num_cursors: 0,
            ..Default::default()
        };
        let mut v = Vdbe::new(Arc::new(prog), None);
        let mut rows: Vec<Vec<Value>> = Vec::new();
        while let StepResult::Row = v.step().await.expect("step") {
            let n = v.result_count();
            rows.push((0..n).map(|i| v.result_value(i)).collect());
        }
        assert_eq!(rows, vec![vec![Value::Int(1)]]);
    }
}
