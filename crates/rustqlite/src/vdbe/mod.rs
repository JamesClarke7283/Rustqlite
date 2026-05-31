//! VDBE — the register-based bytecode virtual machine (mirrors `vdbe.c`, `vdbeaux.c`,
//! `vdbemem.c`, `vdbesort.c`).
//!
//! A prepared statement compiles to a [`program::Program`] of [`opcode::Opcode`] instructions
//! that the executor steps until it yields a row or finishes. The opcode set and register
//! model exist here as a skeleton; the executor, `EXPLAIN` rendering, cursor table, and sorter
//! are filled in from M3.

pub mod cursor;
pub mod exec;
pub mod explain;
pub mod mem;
pub mod opcode;
pub mod program;
pub mod sorter;

pub use mem::Mem;
pub use opcode::Opcode;
pub use program::{Instruction, Program, P4};
