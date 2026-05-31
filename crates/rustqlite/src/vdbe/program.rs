//! VDBE program representation (mirrors the `Vdbe`/`VdbeOp` structures in `vdbeaux.c`).
//!
//! A compiled statement is a flat array of [`Instruction`]s plus a register count. Each
//! instruction has the classic SQLite shape: an opcode and operands `p1..p3` (i32), a typed
//! `p4`, and a `p5` flag byte. The executor (M3) walks this with a program counter.

use super::opcode::Opcode;

/// The typed P4 operand of an instruction.
#[derive(Clone, Debug, PartialEq)]
pub enum P4 {
    None,
    Int(i64),
    Real(f64),
    Text(String),
    /// Collation name, function name, or similar symbolic operand.
    Symbol(String),
}

/// A single VDBE instruction.
#[derive(Clone, Debug)]
pub struct Instruction {
    pub opcode: Opcode,
    pub p1: i32,
    pub p2: i32,
    pub p3: i32,
    pub p4: P4,
    pub p5: u8,
}

/// A compiled VDBE program.
#[derive(Clone, Debug, Default)]
pub struct Program {
    pub instructions: Vec<Instruction>,
    /// Number of registers the program needs.
    pub num_registers: usize,
}
