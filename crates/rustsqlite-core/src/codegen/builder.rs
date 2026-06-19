//! The program builder: register allocation, instruction emission, and forward-jump labels
//! (mirrors the `sqlite3VdbeAddOp*` / `sqlite3VdbeMakeLabel` helpers in `vdbeaux.c`).
//!
//! Registers are 1-based (register 0 is reserved, matching upstream). Forward jumps target a
//! [`Label`] whose address is filled in once known; [`ProgramBuilder::finish`] backpatches all
//! jump operands.

use crate::vdbe::program::{Instruction, Program, P4};
use crate::vdbe::Opcode;

/// A forward-jump target, resolved later with [`ProgramBuilder::resolve`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Label(usize);

/// Builds a [`Program`] instruction by instruction.
pub struct ProgramBuilder {
    insts: Vec<Instruction>,
    next_reg: i32,
    /// Resolved address of each label (filled by `resolve`).
    label_addr: Vec<Option<i32>>,
    /// `(instruction index, label)` pairs whose `p2` must be patched to the label's address.
    fixups: Vec<(usize, Label)>,
}

impl ProgramBuilder {
    pub fn new() -> ProgramBuilder {
        ProgramBuilder {
            insts: Vec::new(),
            next_reg: 1, // register 0 is reserved
            label_addr: Vec::new(),
            fixups: Vec::new(),
        }
    }

    /// Allocate one register.
    pub fn alloc_reg(&mut self) -> i32 {
        let r = self.next_reg;
        self.next_reg += 1;
        r
    }

    /// Allocate `n` contiguous registers, returning the first.
    pub fn alloc_regs(&mut self, n: i32) -> i32 {
        let r = self.next_reg;
        self.next_reg += n.max(0);
        r
    }

    /// The address the next emitted instruction will have.
    pub fn cur_addr(&self) -> i32 {
        self.insts.len() as i32
    }

    /// Emit `opcode p1 p2 p3` and return its address.
    pub fn emit(&mut self, opcode: Opcode, p1: i32, p2: i32, p3: i32) -> usize {
        self.insts.push(Instruction::new(opcode, p1, p2, p3));
        self.insts.len() - 1
    }

    /// Set the `p4` operand of a previously emitted instruction.
    pub fn set_p4(&mut self, idx: usize, p4: P4) {
        self.insts[idx].p4 = p4;
    }

    /// Set the `p5` flag byte of a previously emitted instruction.
    pub fn set_p5(&mut self, idx: usize, p5: u8) {
        self.insts[idx].p5 = p5;
    }

    /// Create an unresolved forward-jump label.
    pub fn new_label(&mut self) -> Label {
        self.label_addr.push(None);
        Label(self.label_addr.len() - 1)
    }

    /// Bind `label` to the current address (the next instruction emitted).
    pub fn resolve(&mut self, label: Label) {
        self.label_addr[label.0] = Some(self.cur_addr());
    }

    /// Emit a jump-like instruction whose `p2` target is a label (patched at `finish`).
    pub fn emit_jump(&mut self, opcode: Opcode, p1: i32, label: Label, p3: i32) -> usize {
        let idx = self.emit(opcode, p1, 0, p3);
        self.fixups.push((idx, label));
        idx
    }

    /// Append a pre-built instruction directly, bypassing label fixup machinery.
    pub fn append(&mut self, inst: Instruction) {
        self.insts.push(inst);
    }

    /// Finalize into a [`Program`], backpatching every labeled jump.
    pub fn finish(mut self) -> Program {
        for (idx, label) in &self.fixups {
            let target =
                self.label_addr[label.0].expect("every label must be resolved before finish()");
            self.insts[*idx].p2 = target;
        }
        Program {
            instructions: self.insts,
            num_registers: self.next_reg as usize,
        }
    }
}

impl Default for ProgramBuilder {
    fn default() -> Self {
        ProgramBuilder::new()
    }
}
