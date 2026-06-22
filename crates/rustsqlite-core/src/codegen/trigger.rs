//! Lowering `CREATE TRIGGER` and `DROP TRIGGER` to VDBE programs (mirrors the trigger-
//! handling paths in `build.c` / `trigger.c`).
//!
//! M16.7/M16.8 cover the DDL codegen: a trigger is stored as a row in `sqlite_schema`
//! with `type='trigger'`, `tbl_name=<table>`, `rootpage=0`, and the verbatim CREATE
//! TRIGGER text. Trigger firing (M16.9+) — compiling the trigger body as a sub-VDBE and
//! invoking it via `OP_Program` on INSERT/UPDATE/DELETE — is deferred.

use rustqlite_parser::{CreateTrigger, DropTriggerStmt};

use crate::error::{Error, Result};
use crate::schema::bootstrap::trigger_schema_row;
use crate::types::Value;
use crate::vdbe::program::{Program, P4, P5_ISUPDATE};
use crate::vdbe::Opcode;

use super::builder::ProgramBuilder;

const SCHEMA_ROOT: i32 = 1;
const COOKIE_SCHEMA: i32 = 1;

/// Compile `CREATE [TEMP] TRIGGER [IF NOT EXISTS] name ...`.
///
/// `sql_text` is the verbatim `CREATE TRIGGER` source stored in `sqlite_schema.sql`.
/// `schema_cookie` is the current cookie (the program bumps it by one).
pub fn compile_create_trigger(
    ct: &CreateTrigger,
    sql_text: &str,
    schema_cookie: u32,
) -> Result<Program> {
    if ct.temporary {
        return Err(Error::msg("TEMP triggers are not supported yet"));
    }
    if ct.schema.is_some() || ct.table_schema.is_some() {
        return Err(Error::msg(
            "schema-qualified CREATE TRIGGER is not yet supported",
        ));
    }
    let mut b = ProgramBuilder::new();

    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    let after_init = b.cur_addr();

    // (1) Open the write transaction.
    b.emit(Opcode::Transaction, 0, 1, 0);

    // (2) Build the five-value sqlite_schema row for the trigger. rootpage = 0.
    let name_dequoted = crate::schema::dequote_ident(&ct.name);
    let tbl_name_dequoted = crate::schema::dequote_ident(&ct.table);
    let row = trigger_schema_row(&name_dequoted, &tbl_name_dequoted, sql_text);
    let ncol = row.len() as i32;
    let rec_start = b.alloc_regs(ncol);
    for (i, v) in row.iter().enumerate() {
        let target = rec_start + i as i32;
        emit_value(&mut b, v, target);
    }
    let record = b.alloc_reg();
    b.emit(Opcode::MakeRecord, rec_start, ncol, record);

    // (3) Allocate a rowid on the sqlite_schema b-tree and insert the row.
    let schema_cursor = 0i32;
    b.emit(Opcode::OpenWrite, schema_cursor, SCHEMA_ROOT, 0);
    let rowid_reg = b.alloc_reg();
    b.emit(Opcode::NewRowid, schema_cursor, rowid_reg, 0);
    let ins = b.emit(Opcode::Insert, schema_cursor, record, rowid_reg);
    b.set_p5(ins, P5_ISUPDATE);

    // (4) Bump the schema cookie.
    b.emit(
        Opcode::SetCookie,
        0,
        COOKIE_SCHEMA,
        schema_cookie as i32 + 1,
    );

    // (5) Reload the schema.
    b.emit(Opcode::ParseSchema, 0, 0, 0);

    // (6) Halt commits the transaction.
    b.emit(Opcode::Halt, 0, 0, 0);

    b.resolve(setup);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

/// Compile `DROP TRIGGER [IF EXISTS] [schema.]name`. Removes the matching
/// `sqlite_schema` row, bumps the schema cookie, and reloads the schema.
pub fn compile_drop_trigger(
    dt: &DropTriggerStmt,
    schema_cookie: u32,
    schema_rowid: i64,
) -> Result<Program> {
    if dt.schema.is_some() {
        return Err(Error::msg(
            "schema-qualified DROP TRIGGER is not yet supported",
        ));
    }
    let mut b = ProgramBuilder::new();

    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    let after_init = b.cur_addr();

    // (1) Open the write transaction.
    b.emit(Opcode::Transaction, 0, 1, 0);

    // (2) Open a write cursor on sqlite_schema, seek to the rowid, delete the row.
    let schema_cursor = 0i32;
    b.emit(Opcode::OpenWrite, schema_cursor, SCHEMA_ROOT, 0);
    let rowid_reg = b.alloc_reg();
    let i = b.emit(Opcode::Int64, 0, rowid_reg, 0);
    b.set_p4(i, P4::Int(schema_rowid));
    let end_delete = b.new_label();
    b.emit_jump(Opcode::NotExists, schema_cursor, end_delete, rowid_reg);
    let del_idx = b.emit(Opcode::Delete, schema_cursor, 0, 0);
    b.set_p5(del_idx, P5_ISUPDATE);
    b.resolve(end_delete);

    // (3) Bump the schema cookie.
    b.emit(
        Opcode::SetCookie,
        0,
        COOKIE_SCHEMA,
        schema_cookie as i32 + 1,
    );

    // (4) Reload the schema.
    b.emit(Opcode::ParseSchema, 0, 0, 0);

    // (5) Halt commits the transaction.
    b.emit(Opcode::Halt, 0, 0, 0);

    b.resolve(setup);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

/// A no-op `DROP TRIGGER [IF EXISTS]` against a missing trigger.
pub fn compile_drop_trigger_noop() -> Program {
    let mut b = ProgramBuilder::new();
    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    let after_init = b.cur_addr();
    b.emit(Opcode::Halt, 0, 0, 0);
    b.resolve(setup);
    b.emit(Opcode::Goto, 0, after_init, 0);
    b.finish()
}

fn emit_value(b: &mut ProgramBuilder, v: &Value, target: i32) {
    match v {
        Value::Null => {
            b.emit(Opcode::Null, 0, target, 0);
        }
        Value::Int(n) => match i32::try_from(*n) {
            Ok(n32) => {
                b.emit(Opcode::Integer, n32, target, 0);
            }
            Err(_) => {
                let i = b.emit(Opcode::Int64, 0, target, 0);
                b.set_p4(i, P4::Int(*n));
            }
        },
        Value::Real(r) => {
            let i = b.emit(Opcode::Real, 0, target, 0);
            b.set_p4(i, P4::Real(*r));
        }
        Value::Text(s) => {
            let i = b.emit(Opcode::String8, 0, target, 0);
            b.set_p4(i, P4::Text(s.clone()));
        }
        Value::Blob(bytes) => {
            let i = b.emit(Opcode::Blob, 0, target, 0);
            b.set_p4(i, P4::Blob(bytes.clone()));
        }
    }
}