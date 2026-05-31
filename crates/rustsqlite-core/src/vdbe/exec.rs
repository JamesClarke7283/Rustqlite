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
use std::sync::Arc;

use crate::btree::TableCursor;
use crate::error::{Error, Result};
use crate::format::{decode_record, encode_record, TextEncoding};
use crate::func;
use crate::pager::Pager;
use crate::types::{Affinity, Collation, Value};

use super::compare::{apply_affinity, mem_compare};
use super::cursor::VdbeCursor;
use super::opcode::Opcode;
use super::program::{p5_to_aff, Instruction, Program, P4, P5_JUMPIFNULL, P5_NULLEQ, P5_STOREP2};
use super::sorter::Sorter;

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
    /// `changes()` — rows changed by the last write. Always 0 in M3b (no write path yet).
    pub changes: i64,
    /// `total_changes()` — rows changed since the connection opened. Always 0 in M3b.
    pub total_changes: i64,
    /// `last_insert_rowid()` — rowid of the last successful insert. Always 0 in M3b.
    pub last_insert_rowid: i64,
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
    pub async fn step(&mut self) -> Result<StepResult> {
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
                    self.halted = true;
                    return Ok(StepResult::Done);
                }
                Opcode::Transaction => self.pc += 1,

                Opcode::OpenRead => {
                    let pager = self
                        .pager
                        .clone()
                        .ok_or_else(|| Error::msg("no database is open"))?;
                    let cursor = TableCursor::new(pager, p2 as u32);
                    self.set_cursor(p1 as usize, VdbeCursor::Table(cursor));
                    self.pc += 1;
                }
                Opcode::Close => {
                    if let Some(slot) = self.cursors.get_mut(p1 as usize) {
                        *slot = None;
                    }
                    self.pc += 1;
                }

                Opcode::Rewind => {
                    let cur = self.table_cursor_mut(p1)?;
                    cur.rewind().await?;
                    let valid = cur.is_valid();
                    self.decoded = None;
                    if valid {
                        self.pc += 1;
                    } else {
                        self.pc = p2 as usize;
                    }
                }
                Opcode::Next => {
                    let cur = self.table_cursor_mut(p1)?;
                    cur.next().await?;
                    let valid = cur.is_valid();
                    self.decoded = None;
                    if valid {
                        self.pc = p2 as usize;
                    } else {
                        self.pc += 1;
                    }
                }

                Opcode::Rowid => {
                    let rowid = self.table_cursor(p1)?.rowid()?;
                    self.regs[p2 as usize] = Value::Int(rowid);
                    self.pc += 1;
                }
                Opcode::Column => {
                    let val = self.column(p1 as usize, p2 as usize).await?;
                    self.regs[p3 as usize] = val;
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

                // ---- not implemented in M3a (write path / index / aggregates) ----
                other => {
                    return Err(Error::msg(format!(
                        "opcode {} is not implemented in M3a",
                        other.name()
                    )))
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

/// `r[p3] = r[p2] || r[p1]` — text concatenation; NULL if either operand is NULL.
fn concat(b: &Value, a: &Value) -> Value {
    match (b.to_text(), a.to_text()) {
        (Some(tb), Some(ta)) => Value::Text(tb + &ta),
        _ => Value::Null,
    }
}
