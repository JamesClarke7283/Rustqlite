//! Lowering `DROP TABLE [IF EXISTS] [name]` to a VDBE program (mirrors `sqlite3DropTable` in
//! `build.c`).
//!
//! The first M4.6 slice:
//! * Resolves the table from the current catalog (errors if the name is unknown and
//!   `IF EXISTS` was not specified).
//! * Issues `Destroy` on the table's root page, freeing every page the b-tree owned into
//!   the pager freelist.
//! * Walks `sqlite_schema` (cursor 0 on page 1) and `Delete`s the row whose `name` column
//!   matches the dropped table. The schema cookie is bumped and the in-memory schema is
//!   reloaded.
//!
//! `IF EXISTS`, `DROP TABLE IF EXISTS` of a non-existent table is a no-op (still bumps the
//! schema cookie is omitted). `DROP INDEX/VIEW/TRIGGER` are out of scope for this slice.

use rustqlite_parser::DropTableStmt;

use crate::error::{Error, Result};
use crate::schema::Table;
use crate::vdbe::program::Program;
use crate::vdbe::Opcode;

use super::builder::ProgramBuilder;

/// Compile `DROP TABLE <name>` (or `DROP TABLE IF EXISTS <name>`).
pub fn compile_drop_table(
    drop: &DropTableStmt,
    if_exists: bool,
    current_schema_cookie: u32,
    resolved_table: Option<&Table>,
) -> Result<Program> {
    let mut b = ProgramBuilder::new();
    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    let after_init = b.new_label();
    b.resolve(after_init);

    b.emit(Opcode::Transaction, 0, 1, 0);

    // IF EXISTS + missing table → no-op statement (still opens a write txn; matches the
    // upstream behavior of `DROP TABLE IF EXISTS` for an unknown name).
    let table = match resolved_table {
        Some(t) => t.clone(),
        None => {
            if !if_exists {
                return Err(Error::msg(format!("no such table: {}", drop.name)));
            }
            // No work to do; fall through to Halt.
            Table::missing()
        }
    };

    // Skip the actual work when the table is the missing-table placeholder (i.e. `IF EXISTS`
    // matched an absent table — no destroy, no schema-row delete, no cookie bump).
    let do_work = table.name == drop.name && table.rootpage != 0;

    if do_work {
        // 1. Destroy the table's b-tree (free every page it owned).
        b.emit(Opcode::Destroy, table.rootpage as i32, 0, 0);

        // 2. Walk `sqlite_schema` (page 1) and delete the row whose name matches.
        let schema_cursor = 0i32;
        b.emit(
            Opcode::OpenWrite,
            schema_cursor,
            1, // sqlite_schema is always page 1
            0,
        );

        let end_loop = b.new_label();
        let loop_body = b.new_label();
        b.emit_jump(Opcode::Rewind, schema_cursor, end_loop, 0);
        b.resolve(loop_body);

        // For each row: read column 1 (the name), compare to the dropped table's name.
        // On match, Delete the schema row and continue. On mismatch, just continue.
        let name_reg = b.alloc_reg();
        let not_match = b.new_label();
        b.emit(Opcode::Column, schema_cursor, 1, name_reg); // col 1 = name
        let str_reg = b.alloc_reg();
        let str_idx = b.emit(Opcode::String8, 0, str_reg, 0);
        b.set_p4(str_idx, crate::vdbe::program::P4::Text(table.name.clone()));
        // str_reg now holds the dropped table's name. We want the column-name (`r[name_reg]`)
        // to EQUAL the literal. `Eq` jumps on TRUE; we jump to `not_match` (skip the Delete)
        // when they're equal — wait, we want to delete when EQUAL, so we negate.
        b.emit_jump(Opcode::Ne, str_reg, not_match, name_reg);
        // Match: drop the schema row, then continue.
        b.emit(Opcode::Delete, schema_cursor, 0, 0);
        b.resolve(not_match);
        // Bottom of the body: advance to next row, jump back to the body.
        b.emit_jump(Opcode::Next, schema_cursor, loop_body, 0);
        b.resolve(end_loop);

        // 3. Bump the schema cookie.
        b.emit(
            Opcode::SetCookie,
            0,
            1, // cookie 1 = schema cookie
            (current_schema_cookie + 1) as i32,
        );
        b.emit(Opcode::ParseSchema, 0, 0, 0);
    }

    b.emit(Opcode::Halt, 0, 0, 0);
    b.resolve(setup);
    b.emit_jump(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

impl Table {
    /// A placeholder "missing" table used to carry a `name` and `rootpage=0` through the
    /// codegen when the user asked for `DROP TABLE IF EXISTS` of an unknown table.
    fn missing() -> Table {
        Table {
            name: String::new(),
            rootpage: 0,
            columns: Vec::new(),
            rowid_alias: None,
            without_rowid: false,
            pk_columns: Vec::new(),
        }
    }
}
