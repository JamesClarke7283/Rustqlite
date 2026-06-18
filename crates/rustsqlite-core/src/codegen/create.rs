//! Lowering `CREATE TABLE` to a VDBE program (mirrors `sqlite3EndTable` in `build.c`).
//!
//! The faithful opcode shape is: open a write transaction, create the new table's b-tree (its
//! root page), build the five-value `sqlite_schema` row record, allocate a rowid on the
//! `sqlite_schema` b-tree (page 1) and `Insert` the row there, bump the schema cookie, reload the
//! schema, then `Halt` (which commits). Upstream emits a richer sequence (it back-patches the
//! rootpage into the record and re-parses via `OP_ParseSchema`); we keep that structure but
//! simplify to what the first write slice needs.

use rustqlite_parser::CreateTable;

use crate::error::{Error, Result};
use crate::schema::bootstrap::table_schema_row;
use crate::types::Value;
use crate::vdbe::program::{Program, P4};
use crate::vdbe::Opcode;

use super::builder::ProgramBuilder;

/// The fixed rootpage of `sqlite_schema` (page 1) — the b-tree every schema row is inserted into.
const SCHEMA_ROOT: i32 = 1;
/// The `SetCookie` selector for the schema cookie (header bytes 40-43).
const COOKIE_SCHEMA: i32 = 1;

/// Compile a `CREATE TABLE` statement. `sql_text` is the user's ORIGINAL statement text, stored
/// verbatim in the new `sqlite_schema` row's `sql` column (SQLite does not canonicalize it).
/// `schema_cookie` is the database's current schema cookie; the program bumps it by one.
pub fn compile_create_table(
    ct: &CreateTable,
    sql_text: &str,
    schema_cookie: u32,
) -> Result<Program> {
    if ct.temporary {
        return Err(Error::msg("TEMP tables are not supported yet"));
    }
    if ct.columns.is_empty() {
        return Err(Error::msg(format!(
            "table {} must have at least one column",
            ct.name
        )));
    }

    let mut b = ProgramBuilder::new();

    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0); // addr 0
    let after_init = b.cur_addr();

    // (1) open the write transaction (p2 = 1).
    b.emit(Opcode::Transaction, 0, 1, 0);

    // (2) create the new table's b-tree; its root page lands in `root_reg`.
    let root_reg = b.alloc_reg();
    b.emit(Opcode::CreateBtree, 0, root_reg, 1); // p3 = 1 → a table b-tree

    // (3) build the five-value sqlite_schema record. The static columns are loaded from the
    // schema-row template (built by bootstrap), with the rootpage taken from `root_reg` (the
    // freshly-created root, not yet known at compile time) rather than a constant.
    let row = table_schema_row(&ct.name, 0, sql_text);
    let ncol = row.len() as i32;
    let rec_start = b.alloc_regs(ncol);
    for (i, v) in row.iter().enumerate() {
        let target = rec_start + i as i32;
        // Column 3 (0-based) is the rootpage — copy the runtime register instead of a literal.
        if i == 3 {
            b.emit(Opcode::SCopy, root_reg, target, 0);
        } else {
            emit_value(&mut b, v, target);
        }
    }
    let record = b.alloc_reg();
    b.emit(Opcode::MakeRecord, rec_start, ncol, record);

    // (4) allocate a rowid on the sqlite_schema b-tree and insert the row there. We open a write
    // cursor on page 1 first so NewRowid/Insert know its rootpage.
    let schema_cursor = 0i32;
    b.emit(Opcode::OpenWrite, schema_cursor, SCHEMA_ROOT, 0);
    let rowid_reg = b.alloc_reg();
    b.emit(Opcode::NewRowid, schema_cursor, rowid_reg, 0);
    b.emit(Opcode::Insert, schema_cursor, record, rowid_reg);

    // (5) bump the schema cookie (DDL advances it by one).
    b.emit(
        Opcode::SetCookie,
        0,
        COOKIE_SCHEMA,
        schema_cookie as i32 + 1,
    );

    // (6) reload the schema so later statements see the new table.
    b.emit(Opcode::ParseSchema, 0, 0, 0);

    // (7) Halt (commits the write transaction).
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

#[cfg(test)]
mod tests {
    use super::*;
    use rustqlite_parser::{parse, Stmt};

    fn create_of(sql: &str) -> CreateTable {
        match parse(sql).unwrap().into_iter().next().unwrap() {
            Stmt::CreateTable(ct) => ct,
            _ => panic!("expected CREATE TABLE"),
        }
    }

    #[test]
    fn create_program_shape() {
        let ct = create_of("CREATE TABLE t(a, b)");
        let prog = compile_create_table(&ct, "CREATE TABLE t(a, b)", 0).unwrap();
        let names: Vec<&str> = prog.instructions.iter().map(|i| i.opcode.name()).collect();
        // The faithful sequence: a write Transaction, CreateBtree, the record build, the schema
        // insert, SetCookie, ParseSchema, Halt.
        assert!(names.contains(&"Transaction"));
        assert!(names.contains(&"CreateBtree"));
        assert!(names.contains(&"MakeRecord"));
        assert!(names.contains(&"NewRowid"));
        assert!(names.contains(&"Insert"));
        assert!(names.contains(&"SetCookie"));
        assert!(names.contains(&"Halt"));

        // The write Transaction must carry p2 = 1.
        let txn = prog
            .instructions
            .iter()
            .find(|i| i.opcode == Opcode::Transaction)
            .unwrap();
        assert_eq!(txn.p2, 1);

        // SetCookie writes cookie+1 (here 1).
        let sc = prog
            .instructions
            .iter()
            .find(|i| i.opcode == Opcode::SetCookie)
            .unwrap();
        assert_eq!(sc.p3, 1);
    }
}
