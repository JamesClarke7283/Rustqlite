//! Lowering `CREATE VIEW` and `DROP VIEW` to VDBE programs (mirrors the view-handling
//! paths in `build.c`).
//!
//! A view is a saved `SELECT` statement stored as a row in `sqlite_schema` with
//! `type='view'` and `rootpage=0`. Querying a view substitutes its SELECT body into the
//! outer query (view expansion); the view itself has no b-tree. M15.3/M15.4 cover the
//! DDL codegen; view expansion (M15.5) is deferred.
//!
//! `CREATE VIEW` mirrors `sqlite3CreateView` in `build.c`: it writes a `sqlite_schema`
//! row with the verbatim `CREATE VIEW` text, bumps the schema cookie, and reloads the
//! schema. `DROP VIEW` mirrors the view-destroy half of `sqlite3DropTable`: it removes
//! the `sqlite_schema` row, bumps the cookie, and reloads.

use rustqlite_parser::{CreateView, DropViewStmt};

use crate::error::{Error, Result};
use crate::schema::bootstrap::view_schema_row;
use crate::types::Value;
use crate::vdbe::program::{Program, P4, P5_ISUPDATE};
use crate::vdbe::Opcode;

use super::builder::ProgramBuilder;

const SCHEMA_ROOT: i32 = 1;
const COOKIE_SCHEMA: i32 = 1;

/// Compile `CREATE [TEMP] VIEW [IF NOT EXISTS] name [(cols)] AS SELECT ...`.
///
/// `sql_text` is the verbatim `CREATE VIEW` source stored in `sqlite_schema.sql`.
/// `schema_cookie` is the current cookie (the program bumps it by one).
pub fn compile_create_view(
    cv: &CreateView,
    sql_text: &str,
    schema_cookie: u32,
) -> Result<Program> {
    if cv.temporary {
        return Err(Error::msg("TEMP views are not supported yet"));
    }
    if cv.schema.is_some() {
        return Err(Error::msg(
            "schema-qualified CREATE VIEW is not yet supported",
        ));
    }
    let mut b = ProgramBuilder::new();

    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    let after_init = b.cur_addr();

    // (1) Open the write transaction.
    b.emit(Opcode::Transaction, 0, 1, 0);

    // (2) Build the five-value sqlite_schema row for the view. rootpage = 0 (a view has
    //     no b-tree). The name is dequoted (SQLite stores the dequoted form in name/tbl_name).
    let name_dequoted = crate::schema::dequote_ident(&cv.name);
    let row = view_schema_row(&name_dequoted, sql_text);
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

/// Compile `DROP VIEW [IF EXISTS] [schema.]name`. Removes the matching `sqlite_schema`
/// row, bumps the schema cookie, and reloads the schema. `schema_rowid` is the rowid of
/// the view's `sqlite_schema` row (resolved by the caller from the catalog).
pub fn compile_drop_view(
    dv: &DropViewStmt,
    schema_cookie: u32,
    schema_rowid: i64,
) -> Result<Program> {
    if dv.schema.is_some() {
        return Err(Error::msg(
            "schema-qualified DROP VIEW is not yet supported",
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

/// A no-op `DROP VIEW [IF EXISTS]` against a missing view.
pub fn compile_drop_view_noop() -> Program {
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