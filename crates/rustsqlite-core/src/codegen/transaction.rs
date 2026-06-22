//! Transaction-control statement codegen (M12.3): `BEGIN`/`COMMIT`/`END`/`ROLLBACK`
//! (mirrors the `OP_AutoCommit` emission in `build.c`'s `sqlite3BeginTransaction` and
//! `sqlite3CommitTransaction` / `sqlite3RollbackStatement`).
//!
//! `SAVEPOINT`/`RELEASE`/`ROLLBACK TO SAVEPOINT` are parsed (M2.36–M2.37) but their codegen
//! needs the pager savepoint stack (M12.4–M12.5) — this first slice rejects them.

use rustqlite_parser::TransactionStmt;

use crate::error::{Error, Result};
use crate::vdbe::program::Instruction;
use crate::vdbe::{Opcode, Program};

/// Compile a transaction-control statement to a tiny program that drives the connection's
/// autocommit flag via `OP_AutoCommit`. The runtime commit/rollback is performed inside
/// `OP_AutoCommit` (mirrors upstream where `OP_AutoCommit` calls `sqlite3VdbeHalt` itself,
/// so the program ends with the `OP_AutoCommit` and never reaches a `Halt`).
pub fn compile_transaction(stmt: &TransactionStmt) -> Result<Program> {
    match stmt {
        // `BEGIN [DEFERRED|IMMEDIATE|EXCLUSIVE] [TRANSACTION [name]]`.
        // Emit `OP_AutoCommit 0 0` (turn autocommit OFF). The runtime errors with
        // "cannot start a transaction within a transaction" if autocommit is already off.
        // The `transaction_type` (DEFERRED/IMMEDIATE/EXCLUSIVE) and `name` are accepted by the
        // parser but the M12 first slice treats every BEGIN as DEFERRED (the writer lock is
        // acquired lazily at first write, matching the existing `begin_write` behavior).
        // IMMEDIATE/EXCLUSIVE locking (M12.6/M12.7) is the follow-up.
        TransactionStmt::Begin { .. } => Ok(program_of(Instruction::new(Opcode::AutoCommit, 0, 0, 0))),

        // `COMMIT [TRANSACTION [name]]` / `END [TRANSACTION [name]]`.
        // Emit `OP_AutoCommit 1 0` (turn autocommit ON, commit any pending write txn). The
        // runtime errors with "cannot commit - no transaction is active" if autocommit is
        // already on.
        TransactionStmt::Commit { .. } => Ok(program_of(Instruction::new(Opcode::AutoCommit, 1, 0, 0))),

        // `ROLLBACK [TRANSACTION [name]]`.
        // Emit `OP_AutoCommit 1 1` (turn autocommit ON, rollback any pending write txn). The
        // runtime errors with "cannot rollback - no transaction is active" if autocommit is
        // already on.
        TransactionStmt::Rollback {
            to_savepoint: None, ..
        } => Ok(program_of(Instruction::new(Opcode::AutoCommit, 1, 1, 0))),

        // `ROLLBACK [TRANSACTION [name]] TO [SAVEPOINT] name`.
        // `SAVEPOINT name`.
        // `RELEASE [SAVEPOINT] name`.
        // These need the pager savepoint stack (M12.4/M12.5).
        TransactionStmt::Rollback {
            to_savepoint: Some(_), ..
        } => Err(Error::msg(
            "ROLLBACK TO SAVEPOINT is not yet supported (M12.4/M12.5 — pager savepoint stack)",
        )),
        TransactionStmt::Savepoint(_) => Err(Error::msg(
            "SAVEPOINT is not yet supported (M12.4/M12.5 — pager savepoint stack)",
        )),
        TransactionStmt::Release(_) => Err(Error::msg(
            "RELEASE SAVEPOINT is not yet supported (M12.4/M12.5 — pager savepoint stack)",
        )),
    }
}

/// Build a one-instruction program (the `OP_AutoCommit` opcode is terminal — it halts the VDBE
/// itself, so no trailing `Halt` is needed).
fn program_of(inst: Instruction) -> Program {
    let mut p = Program::default();
    p.instructions.push(inst);
    p
}
