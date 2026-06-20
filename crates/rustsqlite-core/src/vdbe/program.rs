//! VDBE program representation (mirrors the `Vdbe`/`VdbeOp` structures in `vdbeaux.c`).
//!
//! A compiled statement is a flat array of [`Instruction`]s plus a register count. Each
//! instruction has the classic SQLite shape: an opcode and operands `p1..p3` (i32), a typed
//! `p4`, and a `p5` flag byte. The executor (`exec.rs`) walks this with a program counter.

use std::sync::Arc;

use crate::func::aggregate::AggregateKind;
use crate::types::Collation;

use super::opcode::Opcode;

/// One ORDER BY key's sort direction and collation, carried by a `SorterOpen` instruction's
/// [`P4::KeyInfo`] (mirrors upstream's `KeyInfo`).
#[derive(Clone, Debug, PartialEq)]
pub struct KeyField {
    /// `true` for DESC (the comparison for this key is reversed).
    pub desc: bool,
    /// Collation used to compare TEXT values for this key. The `KeyInfo` structure now
    /// carries this per-key so both sorter and index-cursor comparisons honor it.
    pub collation: Collation,
}

impl KeyField {
    /// A convenience constructor matching the historical default: ASC, BINARY.
    pub fn asc_binary() -> KeyField {
        KeyField {
            desc: false,
            collation: Collation::Binary,
        }
    }
}

/// The typed P4 operand of an instruction.
#[derive(Clone, Debug, PartialEq)]
pub enum P4 {
    None,
    Int(i64),
    Real(f64),
    Text(String),
    /// A BLOB literal operand (used by the `Blob` load opcode).
    Blob(Vec<u8>),
    /// Collation name, function name, or similar symbolic operand.
    Symbol(String),
    /// Sort-key descriptors for a `SorterOpen` (one per ORDER BY term).
    KeyInfo(Vec<KeyField>),
    /// A built-in aggregate descriptor for `AggStep`/`AggFinal`. Carries the aggregate kind
    /// (resolved case-insensitively at codegen time) so the executor can dispatch to the right
    /// step/finalize path without re-parsing the function name. Mirrors upstream's `P4_FUNCDEF`.
    FuncDef(AggregateKind),
    /// A sub-VDBE program for `OP_Program` (triggers, future views). Carries an `Arc<Program>`
    /// so it can be cheaply shared between the parent's instruction stream and the frame the
    /// executor installs when it enters the sub-program. Mirrors upstream's `P4_SUBPROGRAM`.
    /// The sub-program's own `num_registers` determines the size of the fresh register file the
    /// executor allocates for the frame; its `instructions` are executed in place of the
    /// parent's until a `Halt` pops the frame (or a `Return` from a `Gosub`-shaped sub-program
    /// returns to the parent).
    SubProgram(Arc<Program>),
}

/// A single VDBE instruction.
#[derive(Clone, Debug, PartialEq)]
pub struct Instruction {
    pub opcode: Opcode,
    pub p1: i32,
    pub p2: i32,
    pub p3: i32,
    pub p4: P4,
    pub p5: u8,
}

impl Instruction {
    /// Build an instruction with the common `p1/p2/p3` operands and no `p4`/`p5`.
    pub fn new(opcode: Opcode, p1: i32, p2: i32, p3: i32) -> Instruction {
        Instruction {
            opcode,
            p1,
            p2,
            p3,
            p4: P4::None,
            p5: 0,
        }
    }
}

/// A compiled VDBE program.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Program {
    pub instructions: Vec<Instruction>,
    /// Number of registers the program needs.
    pub num_registers: usize,
    /// Number of cursor slots the program needs (the highest cursor number + 1). Used by the
    /// codegen to advance the outer builder's `next_cursor` after inlining a sub-program so a
    /// subsequent inlined sub-program's cursors land in a free range.
    pub num_cursors: usize,
}

impl Program {
    /// A no-op program (the VDBE executes `Halt` immediately and reports `Done`). Used for
    /// `CREATE INDEX IF NOT EXISTS` against a pre-existing index and similar no-op DDL.
    pub fn empty() -> Program {
        let mut p = Program::default();
        p.instructions.push(Instruction::new(Opcode::Halt, 0, 0, 0));
        p
    }
}

// ---- `p5` flag bits for the comparison opcodes (Eq/Ne/Lt/Le/Gt/Ge) ----
//
// The low nibble carries the comparison affinity to apply to both operands before comparing;
// the high bits are boolean flags. These mirror the roles of SQLite's `SQLITE_AFF_*`,
// `SQLITE_JUMPIFNULL`, and `SQLITE_NULLEQ` packed into `p5`, but use a Rustqlite-local layout.

/// Mask selecting the comparison affinity stored in the low bits of a comparison `p5`.
pub const P5_AFF_MASK: u8 = 0x07;
/// Affinity code `none` (no coercion) — the default for literal-vs-literal comparisons.
pub const P5_AFF_NONE: u8 = 0;
pub const P5_AFF_BLOB: u8 = 1;
pub const P5_AFF_TEXT: u8 = 2;
pub const P5_AFF_NUMERIC: u8 = 3;
pub const P5_AFF_INTEGER: u8 = 4;
pub const P5_AFF_REAL: u8 = 5;

/// If set, the comparison takes its jump when either operand is NULL (used to make a WHERE
/// test that is NULL behave as false: the row is skipped). Mirrors `SQLITE_JUMPIFNULL`.
pub const P5_JUMPIFNULL: u8 = 0x10;
/// If set, the comparison stores its boolean result (0/1/NULL) into `p2` instead of jumping —
/// the value form of a comparison (e.g. `SELECT a > 1`). Mirrors `SQLITE_STOREP2`.
pub const P5_STOREP2: u8 = 0x20;
/// If set, NULL compares equal to NULL and unequal to everything else, and the result is never
/// NULL (used for the `IS` / `IS NOT` operators). Mirrors `SQLITE_NULLEQ`.
pub const P5_NULLEQ: u8 = 0x80;

/// Flag bit for the `Delete`/`Insert` opcodes: the row count change is part of an `UPDATE` and
/// must not double-count (the `Delete` is a "logical" delete; the `Insert` is the single +1 to
/// `changes()`). Mirrors `OPFLAG_ISUPDATE` from `vdbe.c`. The `Insert` additionally suppresses
/// its `last_insert_rowid()` write so an `UPDATE` does not clobber the connection's last-insert
/// rowid (matches upstream: only `INSERT` updates `last_insert_rowid()`).
pub const P5_ISUPDATE: u8 = 0x04;

/// Flag bit for `IdxInsert`: bump `db->nChange` (i.e. `changes()`) when the insert lands.
/// Mirrors `OPFLAG_NCHANGE` from `vdbe.c`. The M5.1 path uses it on every index maintenance
/// `IdxInsert` so a non-`UPDATE` write correctly reflects the extra row.
pub const P5_NCHANGE: u8 = 0x01;

/// Flag bit for `IdxInsert`: the record in `r[p2]` is already encoded (the BLOB bytes of the
/// key record, not a list of values to `MakeRecord` from). The M5.1 codegen always pre-builds
/// the record with `MakeRecord` and then immediately `IdxInsert`s it, so this is always set.
/// Mirrors `OPFLAG_PREFORMAT`.
pub const P5_PREFORMAT: u8 = 0x02;

/// Flag bit for `IdxInsert`: this insert is for a `UNIQUE` index; the b-tree layer must
/// raise `SQLITE_CONSTRAINT_UNIQUE` if an entry with the same indexed-column prefix already
/// exists (and none of the key columns are NULL). Mirrors `OPFLAG_UNIQUE` from `vdbe.c`.
pub const P5_UNIQUE: u8 = 0x08;

/// Encode an [`crate::types::Affinity`] (or `None`) into the comparison `p5` affinity bits.
pub fn aff_to_p5(aff: Option<crate::types::Affinity>) -> u8 {
    use crate::types::Affinity::*;
    match aff {
        None => P5_AFF_NONE,
        Some(Blob) => P5_AFF_BLOB,
        Some(Text) => P5_AFF_TEXT,
        Some(Numeric) => P5_AFF_NUMERIC,
        Some(Integer) => P5_AFF_INTEGER,
        Some(Real) => P5_AFF_REAL,
    }
}

/// Decode the comparison `p5` affinity bits back into an [`crate::types::Affinity`] (`None`
/// meaning "apply no affinity").
pub fn p5_to_aff(p5: u8) -> Option<crate::types::Affinity> {
    use crate::types::Affinity::*;
    match p5 & P5_AFF_MASK {
        P5_AFF_BLOB => Some(Blob),
        P5_AFF_TEXT => Some(Text),
        P5_AFF_NUMERIC => Some(Numeric),
        P5_AFF_INTEGER => Some(Integer),
        P5_AFF_REAL => Some(Real),
        _ => None,
    }
}
