//! Transaction-control statement codegen (M12.3 + M12.4/M12.5): `BEGIN`/`COMMIT`/`END`/
//! `ROLLBACK` (via `OP_AutoCommit`) and `SAVEPOINT`/`RELEASE`/`ROLLBACK TO SAVEPOINT` (via
//! `OP_Savepoint`). Mirrors `build.c`'s `sqlite3BeginTransaction` /
//! `sqlite3CommitTransaction` / `sqlite3RollbackStatement` for the autocommit family, and
//! `sqlite3Savepoint` for the savepoint family.

use rustqlite_parser::TransactionStmt;

use crate::vdbe::program::{Instruction, Program, P4};
use crate::vdbe::{Opcode, Program as VdbeProgram};

/// Compile a transaction-control statement to a tiny one-instruction program that drives the
/// connection's autocommit flag via `OP_AutoCommit` (for BEGIN/COMMIT/END/ROLLBACK) or the
/// pager's savepoint stack via `OP_Savepoint` (for SAVEPOINT/RELEASE/ROLLBACK TO). Both
/// opcodes are terminal — they halt the VDBE themselves, so the program ends with that single
/// instruction and never reaches a `Halt`.
pub fn compile_transaction(stmt: &TransactionStmt) -> crate::error::Result<Program> {
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

        // `SAVEPOINT name`.
        // Emit `OP_Savepoint 0 * * P4=Text(name)` (SAVEPOINT_BEGIN). The runtime turns autocommit
        // OFF if it is on (marking this as the "transaction savepoint") and pushes a savepoint
        // onto the pager's stack. Mirrors `sqlite3Savepoint(pParse, SAVEPOINT_BEGIN, pName)`
        // in `build.c`.
        TransactionStmt::Savepoint(name) => Ok(program_of(Instruction {
            opcode: Opcode::Savepoint,
            p1: 0,
            p2: 0,
            p3: 0,
            p4: P4::Text(name.clone()),
            p5: 0,
        })),

        // `RELEASE [SAVEPOINT] name`.
        // Emit `OP_Savepoint 1 * * P4=Text(name)` (SAVEPOINT_RELEASE). The runtime drops the
        // named savepoint and any nested ones (their changes become part of the enclosing
        // transaction); if the named savepoint is the outermost "transaction savepoint", the
        // runtime commits the implicit transaction instead. Mirrors
        // `sqlite3Savepoint(pParse, SAVEPOINT_RELEASE, pName)`.
        TransactionStmt::Release(name) => Ok(program_of(Instruction {
            opcode: Opcode::Savepoint,
            p1: 1,
            p2: 0,
            p3: 0,
            p4: P4::Text(name.clone()),
            p5: 0,
        })),

        // `ROLLBACK [TRANSACTION [name]] TO [SAVEPOINT] name`.
        // Emit `OP_Savepoint 2 * * P4=Text(name)` (SAVEPOINT_ROLLBACK). The runtime restores
        // the pager's dirty overlay to the savepoint's snapshot and drops any nested
        // savepoints, keeping the named one. Mirrors
        // `sqlite3Savepoint(pParse, SAVEPOINT_ROLLBACK, pName)`.
        TransactionStmt::Rollback {
            to_savepoint: Some(name),
            ..
        } => Ok(program_of(Instruction {
            opcode: Opcode::Savepoint,
            p1: 2,
            p2: 0,
            p3: 0,
            p4: P4::Text(name.clone()),
            p5: 0,
        })),
    }
}

/// Build a one-instruction program (the `OP_AutoCommit`/`OP_Savepoint` opcode is terminal — it
/// halts the VDBE itself, so no trailing `Halt` is needed).
fn program_of(inst: Instruction) -> Program {
    let mut p = VdbeProgram::default();
    p.instructions.push(inst);
    p
}
