//! The program builder: register allocation, instruction emission, and forward-jump labels
//! (mirrors the `sqlite3VdbeAddOp*` / `sqlite3VdbeMakeLabel` helpers in `vdbeaux.c`).
//!
//! Registers are 1-based (register 0 is reserved, matching upstream). Forward jumps target a
//! [`Label`] whose address is filled in once known; [`ProgramBuilder::finish`] backpatches all
//! jump operands.

use crate::vdbe::program::{Instruction, Program, P4};
use crate::vdbe::Opcode;

use std::collections::HashMap;

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

    /// The current number of emitted instructions (alias of [`Self::cur_addr`] as a count).
    pub fn insts_len(&self) -> usize {
        self.insts.len()
    }

    /// Register a label fixup for an already-appended instruction at index `idx`, patching
    /// its `p2` operand to `label`'s resolved address at `finish` time. Mirrors what
    /// [`Self::emit_jump`] does for freshly-emitted jumps; this entry point lets a caller
    /// append a jump instruction directly via [`Self::append`] and still defer its target.
    pub fn add_fixup(&mut self, idx: usize, label: Label) {
        self.fixups.push((idx, label));
    }

    /// The resolved address of `label`, or `None` if it has not been resolved yet.
    pub fn label_addr_of(&self, label: Label) -> i32 {
        self.label_addr[label.0].unwrap_or(0)
    }

    /// Iterate over all emitted instructions mutably, so callers can post-process them
    /// (e.g. patch jump targets after a sub-program is inlined).
    pub fn iter_insts_mut(&mut self) -> impl Iterator<Item = &mut Instruction> {
        self.insts.iter_mut()
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

    /// Emit a three-way jump (`OP_Jump`) whose `p1`, `p2`, and `p3` operands are all labels
    /// patched at `finish`. Mirrors the `OP_Jump P1 P2 P3` form that follows an `OP_Compare`.
    /// Registration order matches the apply order (p2 first, p1 second, p3 third), so we push
    /// `l2` first to land in `p2`, then `l1` for `p1`, then `l3` for `p3`.
    pub fn emit_jump3(&mut self, opcode: Opcode, l1: Label, l2: Label, l3: Label) -> usize {
        let idx = self.emit(opcode, 0, 0, 0);
        self.fixups.push((idx, l2));
        self.fixups.push((idx, l1));
        self.fixups.push((idx, l3));
        idx
    }

    /// Append a pre-built instruction directly, bypassing label fixup machinery.
    pub fn append(&mut self, inst: Instruction) {
        self.insts.push(inst);
    }

    /// Finalize into a [`Program`], backpatching every labeled jump. A single instruction may
    /// carry up to three label fixups; the first is applied to `p2` (the common single-jump case),
    /// the second to `p1`, and the third to `p3` — matching the registration order used by
    /// [`emit_jump`](Self::emit_jump) (one label → p2) and [`emit_jump3`](Self::emit_jump3)
    /// (three labels → p1, p2, p3 in that order, so the second registration overwrites the p2
    /// set by the first and the third sets p3).
    pub fn finish(mut self) -> Program {
        let mut counts: HashMap<usize, usize> = HashMap::new();
        for (idx, label) in &self.fixups {
            let target =
                self.label_addr[label.0].expect("every label must be resolved before finish()");
            let n = counts.entry(*idx).or_insert(0);
            match *n {
                0 => self.insts[*idx].p2 = target,
                1 => self.insts[*idx].p1 = target,
                2 => self.insts[*idx].p3 = target,
                _ => panic!("too many label fixups on one instruction"),
            }
            *n += 1;
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
