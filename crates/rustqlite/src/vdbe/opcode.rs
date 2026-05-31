//! VDBE opcodes (mirrors the opcode set generated from `vdbe.c` into `opcodes.h`).
//!
//! Opcode names and semantics mirror upstream exactly so that `EXPLAIN` output and behavior
//! match. This is an INCREMENTAL subset — the full ~190-opcode set is filled in alongside the
//! code generator (M3+). The execution dispatch in `exec.rs` will `match` exhaustively over
//! this enum so that an unhandled opcode is a compile-time error.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opcode {
    // --- control flow ---
    Init,
    Goto,
    Halt,
    // --- transactions / schema ---
    Transaction,
    // --- cursors ---
    OpenRead,
    OpenWrite,
    Close,
    // --- table/index scans ---
    Rewind,
    Next,
    SeekGE,
    // --- row access ---
    Rowid,
    Column,
    ResultRow,
    // --- record building / writes ---
    MakeRecord,
    Insert,
    IdxInsert,
    // --- aggregates ---
    AggStep,
    AggFinal,
}
