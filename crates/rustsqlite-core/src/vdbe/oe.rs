//! Conflict-resolution action codes (mirrors the `OE_*` macros in `sqliteInt.h`).
//!
//! These are the action codes that `INSERT OR <action>` / `UPDATE OR <action>` and
//! `ON CONFLICT <action>` clauses select. They tell the VDBE how to react when a
//! constraint (UNIQUE, NOT NULL, CHECK, …) is violated:
//!
//! * `None`    — no constraint to check (the index/column is not constrained).
//! * `Rollback`— fail the operation and roll back the entire transaction.
//! * `Abort`   — back out the failing statement's changes but keep the transaction
//!   open (the default for all statements without an explicit `OR` clause).
//! * `Fail`    — stop the operation but leave all prior changes (including earlier
//!   rows from the same statement) in place.
//! * `Ignore`  — skip the offending row and continue with the next one.
//! * `Replace` — delete the conflicting row, then re-attempt the insert/update.
//!
//! The numeric values match upstream's `#define OE_*` so that `OP_Halt`'s `p2`
//! operand (which carries the action) is byte-compatible.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum OeAction {
    None = 0,
    Rollback = 1,
    Abort = 2,
    Fail = 3,
    Ignore = 4,
    Replace = 5,
}

impl OeAction {
    /// Convert from a raw `u8` (e.g. decoded from an opcode operand). Returns
    /// `Abort` for any value outside the known range, matching upstream's
    /// `default: onError = OE_Abort` fallback in `sqlite3GenerateConstraintChecks`.
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => OeAction::None,
            1 => OeAction::Rollback,
            2 => OeAction::Abort,
            3 => OeAction::Fail,
            4 => OeAction::Ignore,
            5 => OeAction::Replace,
            _ => OeAction::Abort,
        }
    }

    /// Convert a parser-level `ConflictAction` (which omits the implicit `Abort`
    /// and has no `None`) into the executor-level `OeAction`. `None` maps to the
    /// default `Abort`.
    pub fn from_parser(action: Option<rustqlite_parser::ConflictAction>) -> Self {
        use rustqlite_parser::ConflictAction::*;
        match action {
            None => OeAction::Abort,
            Some(Rollback) => OeAction::Rollback,
            Some(Abort) => OeAction::Abort,
            Some(Fail) => OeAction::Fail,
            Some(Ignore) => OeAction::Ignore,
            Some(Replace) => OeAction::Replace,
        }
    }
}