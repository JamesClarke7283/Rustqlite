//! VDBE — the register-based bytecode virtual machine (mirrors `vdbe.c`, `vdbeaux.c`,
//! `vdbemem.c`, `vdbesort.c`).
//!
//! A prepared statement compiles to a [`program::Program`] of [`opcode::Opcode`] instructions
//! that the executor steps until it yields a row or finishes. The opcode set and register
//! model exist here as a skeleton; the executor, `EXPLAIN` rendering, cursor table, and sorter
//! are filled in from M3.

pub mod compare;
pub mod cursor;
pub mod ephemeral;
pub mod exec;
pub mod explain;
pub mod mem;
pub mod opcode;
pub mod program;
pub mod sorter;

pub use compare::{apply_affinity, mem_compare};
pub use cursor::VdbeCursor;
pub use exec::{StepResult, Vdbe};
pub use mem::Mem;
pub use opcode::Opcode;
pub use program::{Instruction, KeyField, Program, P4};
pub use sorter::Sorter;
