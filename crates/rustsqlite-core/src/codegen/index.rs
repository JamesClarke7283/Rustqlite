//! Lowering `CREATE [UNIQUE] INDEX [IF NOT EXISTS] name ON tbl(col …)` to a VDBE program
//! (mirrors `sqlite3CreateIndex` + the schema-row install path in `build.c`).
//!
//! Faithful opcode shape:
//! ```text
//!   Init 0, setup
//! after_init:
//!   Transaction 0, 1
//!   CreateBtree 0, root_reg, 0         ; p3 = 0 → an index b-tree
//!   OpenWriteReg idx_cur, 0, root_reg  ; populate cursor (root from register)
//!   OpenRead  table_cur, table_root, 0
//!   Rewind    table_cur, end_populate
//! populate_top:
//!   Column    table_cur, col_idx_i, reg_col_i   ; for each indexed column
//!   Rowid     table_cur, reg_rowid
//!   MakeRecord reg_cols..., n+1, reg_key
//!   IdxInsert idx_cur, reg_key, 0, p4=0, p5=NCHANGE
//! populate_next:
//!   Next      table_cur, populate_top
//! end_populate:
//!   ; (then the sqlite_schema install:)
//!   OpenWrite schema_cur, 1, 0
//!   NewRowid  schema_cur, rowid_reg
//!   MakeRecord <sqlite_schema row> (rootpage = root_reg) into reg
//!   Insert    schema_cur, record, rowid_reg
//!   SetCookie 0, 1, schema_cookie + 1
//!   ParseSchema 0
//!   Halt
//! setup:
//!   Goto after_init
//! ```
//!
//! M5.2 adds multi-column index support: the populate pass emits one `Column` per indexed
//! column, builds a composite key record, and `IdxInsert`s it. The first M5.1 slice:
//! * records `UNIQUE` in the catalog but does **not** enforce uniqueness at `IdxInsert` time
//!   (the page-level engine does not yet model a uniqueness check). The flag is stored in
//!   `sqlite_schema` for fidelity, and a unit test pins the gap,
//! * rejects `IF NOT EXISTS` against a pre-existing index of a different shape, no-ops when
//!   the existing index matches.

use rustqlite_parser::CreateIndex;

use crate::error::{Error, Result};
use crate::schema::{IndexObject, Table};
use crate::types::Value;
use crate::vdbe::program::{Program, P4, P5_NCHANGE, P5_UNIQUE};
use crate::vdbe::{KeyField, Opcode};

use super::builder::ProgramBuilder;

/// The fixed rootpage of `sqlite_schema` (page 1) — the b-tree every schema row is inserted into.
const SCHEMA_ROOT: i32 = 1;
/// The `SetCookie` selector for the schema cookie (header bytes 40-43).
const COOKIE_SCHEMA: i32 = 1;

/// Compile a `CREATE INDEX` statement.
///
/// * `table` — the catalog-resolved table the index is on (so the codegen can verify the
///   indexed column exists),
/// * `sql_text` — the verbatim `CREATE INDEX` source (stored unchanged in the new
///   `sqlite_schema` row),
/// * `schema_cookie` — the value before the DDL runs (the program bumps it by one).
pub fn compile_create_index(
    ci: &CreateIndex,
    table: &Table,
    sql_text: &str,
    schema_cookie: u32,
) -> Result<Program> {
    if ci.schema.is_some() {
        return Err(Error::msg(
            "schema-qualified CREATE INDEX is not yet supported",
        ));
    }
    let dummy = IndexObject::from_create_and_table(ci, 0, table);
    let indexed_cis = dummy.table_column_indices(table)?;

    let mut b = ProgramBuilder::new();

    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0); // addr 0
    let after_init = b.cur_addr();

    // (1) Open the write transaction.
    b.emit(Opcode::Transaction, 0, 1, 0);

    // (2) Create the index b-tree. `CreateBtree` with `p3 = 0` creates an index b-tree; the
    // new root page number lands in `root_reg`.
    let root_reg = b.alloc_reg();
    b.emit(Opcode::CreateBtree, 0, root_reg, 0);

    // (3) Open a write cursor on the just-created index b-tree (root read from `root_reg`).
    let idx_cursor = 1i32;
    let open_idx = b.emit(Opcode::OpenWriteReg, idx_cursor, root_reg, 0);

    // The index cursor used during population needs the same per-column comparison rules so
    // insertions land in the correct leaf position when a non-BINARY collation is in use.
    let populate_key_info: Vec<KeyField> = dummy
        .columns
        .iter()
        .map(|ic| KeyField {
            desc: ic.desc,
            collation: ic.collation,
        })
        .collect();
    b.set_p4(open_idx, P4::KeyInfo(populate_key_info));

    // (4) Scan the table and populate the index.
    let table_cursor = 2i32;
    let open_table = b.emit(Opcode::OpenRead, table_cursor, table.rootpage as i32, 0);
    b.set_p4(open_table, P4::Int(table.columns.len() as i64));

    let end_populate = b.new_label();
    b.emit_jump(Opcode::Rewind, table_cursor, end_populate, 0);

    // The `Next` opcode at the bottom of the loop jumps back to the start of the body. The
    // `populate_top_label` is created BEFORE the body emits and resolved after — so its address
    // is the first body instruction (the `Column`).
    let populate_top_label = b.new_label();
    b.resolve(populate_top_label);
    let nkey = indexed_cis.len() as i32 + 1; // indexed columns + trailing rowid
    let key_start = b.alloc_regs(nkey);
    let rec_reg = b.alloc_reg();
    for (i, col_idx) in indexed_cis.iter().enumerate() {
        b.emit(
            Opcode::Column,
            table_cursor,
            *col_idx as i32,
            key_start + i as i32,
        );
    }
    b.emit(
        Opcode::Rowid,
        table_cursor,
        key_start + indexed_cis.len() as i32,
        0,
    );
    b.emit(Opcode::MakeRecord, key_start, nkey, rec_reg);
    let idx_insert = b.emit(Opcode::IdxInsert, idx_cursor, rec_reg, 0);
    let mut p5 = P5_NCHANGE;
    if dummy.unique {
        p5 |= P5_UNIQUE;
        if let Some(msg) = dummy.unique_constraint_message(table) {
            b.set_p4(idx_insert, P4::Text(msg));
        } else {
            b.set_p4(idx_insert, P4::Int(0));
        }
    } else {
        b.set_p4(idx_insert, P4::Int(0)); // nMem = 0
    }
    b.set_p5(idx_insert, p5);
    b.emit_jump(Opcode::Next, table_cursor, populate_top_label, 0);

    b.resolve(end_populate);

    // (5) Build the five-value `sqlite_schema` row. Column 3 (rootpage) is filled at runtime
    // from the just-allocated `root_reg`; the other four are constants emitted inline.
    let row = vec![
        Value::Text("index".to_string()),
        Value::Text(ci.name.clone()),
        Value::Text(ci.table.clone()),
        Value::Int(0), // overwritten at runtime
        Value::Text(sql_text.to_string()),
    ];
    let ncol = row.len() as i32;
    let rec_start = b.alloc_regs(ncol);
    for (i, v) in row.iter().enumerate() {
        let target = rec_start + i as i32;
        if i == 3 {
            b.emit(Opcode::SCopy, root_reg, target, 0);
        } else {
            emit_value(&mut b, v, target);
        }
    }
    let record = b.alloc_reg();
    b.emit(Opcode::MakeRecord, rec_start, ncol, record);

    // (6) Insert the schema row on page 1.
    let schema_cursor = 0i32;
    b.emit(Opcode::OpenWrite, schema_cursor, SCHEMA_ROOT, 0);
    let rowid_reg = b.alloc_reg();
    b.emit(Opcode::NewRowid, schema_cursor, rowid_reg, 0);
    b.emit(Opcode::Insert, schema_cursor, record, rowid_reg);

    // (7) Bump the schema cookie.
    b.emit(
        Opcode::SetCookie,
        0,
        COOKIE_SCHEMA,
        schema_cookie as i32 + 1,
    );

    // (8) Reload the schema (marker — see the analogous comment in `create.rs`).
    b.emit(Opcode::ParseSchema, 0, 0, 0);

    // (9) Halt commits the transaction.
    b.emit(Opcode::Halt, 0, 0, 0);

    b.resolve(setup);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

/// Emit a load of a constant [`Value`] into register `target`.
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
