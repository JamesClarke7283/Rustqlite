//! Transaction-control statement codegen (M12.3 + M12.4/M12.5): `BEGIN`/`COMMIT`/`END`/
//! `ROLLBACK` (via `OP_AutoCommit`) and `SAVEPOINT`/`RELEASE`/`ROLLBACK TO SAVEPOINT` (via
//! `OP_Savepoint`). Mirrors `build.c`'s `sqlite3BeginTransaction` /
//! `sqlite3CommitTransaction` / `sqlite3RollbackStatement` for the autocommit family, and
//! `sqlite3Savepoint` for the savepoint family.

use rustqlite_parser::{TransactionStmt, TransactionType};

use crate::vdbe::program::{Instruction, Program, P4};
use crate::vdbe::{Opcode, Program as VdbeProgram};

/// Compile a transaction-control statement to a tiny program that drives the connection's
/// autocommit flag via `OP_AutoCommit` (for BEGIN/COMMIT/END/ROLLBACK) or the pager's savepoint
/// stack via `OP_Savepoint` (for SAVEPOINT/RELEASE/ROLLBACK TO). For `BEGIN IMMEDIATE`/`BEGIN
/// EXCLUSIVE` an `OP_Transaction` opcode precedes `OP_AutoCommit` so the pager acquires the
/// RESERVED (IMMEDIATE) or EXCLUSIVE (EXCLUSIVE) lock up-front — mirroring `sqlite3BeginTransaction`
/// in `build.c`, which emits `OP_Transaction iDb eTxnType` (eTxnType = 1 for IMMEDIATE, 2 for
/// EXCLUSIVE) before the `OP_AutoCommit` that turns autocommit off. `BEGIN DEFERRED` emits only
/// `OP_AutoCommit` (the lock is acquired lazily at first write, exactly as in upstream). Both
/// `OP_AutoCommit` and `OP_Savepoint` are terminal — they halt the VDBE themselves, so the
/// program ends with that instruction and never reaches a `Halt`.
pub fn compile_transaction(stmt: &TransactionStmt) -> crate::error::Result<Program> {
    match stmt {
        // `BEGIN [DEFERRED|IMMEDIATE|EXCLUSIVE] [TRANSACTION [name]]`.
        // `BEGIN DEFERRED` emits only `OP_AutoCommit 0 0` (turn autocommit OFF); the RESERVED lock
        // is acquired lazily at first write via the `OP_Transaction 0 1` opcode that every write
        // statement already emits. `BEGIN IMMEDIATE` emits `OP_Transaction 0 1` + `OP_AutoCommit 0
        // 0` so the RESERVED lock is taken up-front (errors with `SQLITE_BUSY` if another
        // connection already holds RESERVED/EXCLUSIVE). `BEGIN EXCLUSIVE` emits `OP_Transaction 0
        // 2` + `OP_AutoCommit 0 0` so the EXCLUSIVE lock is taken up-front (blocks even readers
        // on other connections). The runtime errors with "cannot start a transaction within a
        // transaction" if autocommit is already off (the `OP_AutoCommit` arm detects the
        // same-state transition; `OP_Transaction` is idempotent so the lock-acquisition is a
        // no-op when already in a write transaction). Mirrors `sqlite3BeginTransaction` in
        // `build.c`. The `name` is accepted by the parser but ignored (SQLite's named
        // transactions are syntactic sugar only — the name has no runtime effect on a top-level
        // BEGIN/COMMIT/ROLLBACK).
        TransactionStmt::Begin { transaction_type, .. } => {
            Ok(begin_program(*transaction_type))
        }

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

/// Build the program for a `BEGIN [DEFERRED|IMMEDIATE|EXCLUSIVE]` statement. `DEFERRED` emits
/// only `OP_AutoCommit 0 0` (the lock is acquired lazily at first write by the `OP_Transaction 0
/// 1` that every write statement already emits). `IMMEDIATE` emits `OP_Transaction 0 1` first so
/// the RESERVED lock is taken up-front; `EXCLUSIVE` emits `OP_Transaction 0 2` so the EXCLUSIVE
/// lock is taken up-front. In all three cases `OP_AutoCommit 0 0` turns autocommit OFF (BEGIN)
/// and ends the statement. Mirrors `sqlite3BeginTransaction` in `build.c`, which emits
/// `OP_Transaction iDb eTxnType` for non-deferred BEGINs and then unconditionally emits
/// `OP_AutoCommit`.
fn begin_program(transaction_type: TransactionType) -> Program {
    let mut p = VdbeProgram::default();
    match transaction_type {
        TransactionType::Deferred => {}
        TransactionType::Immediate => p
            .instructions
            .push(Instruction::new(Opcode::Transaction, 0, 1, 0)),
        TransactionType::Exclusive => p
            .instructions
            .push(Instruction::new(Opcode::Transaction, 0, 2, 0)),
    }
    p.instructions
        .push(Instruction::new(Opcode::AutoCommit, 0, 0, 0));
    p
}

/// Build a one-instruction program (the `OP_AutoCommit`/`OP_Savepoint` opcode is terminal — it
/// halts the VDBE itself, so no trailing `Halt` is needed).
fn program_of(inst: Instruction) -> Program {
    let mut p = VdbeProgram::default();
    p.instructions.push(inst);
    p
}
