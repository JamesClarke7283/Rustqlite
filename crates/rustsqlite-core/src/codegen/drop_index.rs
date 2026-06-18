//! Lowering `DROP INDEX [IF EXISTS] [schema.]name` to a VDBE program (mirrors the index-destroy
//! half of `sqlite3DropTable` in `build.c`).
//!
//! Faithful opcode shape:
//! ```text
//!   Init 0, setup
//! after_init:
//!   Transaction 0, 1
//!   Destroy <rootpage>                       ; free the index b-tree's pages
//!   OpenWrite schema_cur, 1, 0
//!   ; the rowid of the matching schema row is loaded into a register at codegen time
//!   NotExists  schema_cur, end_delete, rowid_reg
//!   Delete     schema_cur, 0, 0, p5=ISUPDATE  ; remove the row
//! end_delete:
//!   SetCookie 0, 1, schema_cookie + 1
//!   ParseSchema 0
//!   Halt
//! setup:
//!   Goto after_init
//! ```
//!
//! `IF EXISTS` against a missing index is a no-op (just `Halt`). Schema qualifiers other than
/// `main`/absent are rejected at codegen time.
use rustqlite_parser::DropIndexStmt;

use crate::error::{Error, Result};
use crate::schema::IndexObject;
use crate::vdbe::program::{P4, P5_ISUPDATE};
use crate::vdbe::{Opcode, Program};

use super::builder::ProgramBuilder;

/// The fixed rootpage of `sqlite_schema` (page 1).
const SCHEMA_ROOT: i32 = 1;
/// The `SetCookie` selector for the schema cookie.
const COOKIE_SCHEMA: i32 = 1;

/// Compile a `DROP INDEX` statement.
///
/// * `index` is the catalog-resolved `IndexObject` (the engine rejects the statement at
///   codegen time when the index does not exist and `IF EXISTS` is absent),
/// * `current_schema_cookie` is the value before this DDL runs (the program bumps it by one),
/// * `schema_rowid` is the rowid of the matching `sqlite_schema` row (so we can `Delete` it
///   without scanning the b-tree; the catalog reader hands the rowid back to the prepare path
///   along with the `IndexObject`).
pub fn compile_drop_index(
    drop: &DropIndexStmt,
    index: &IndexObject,
    current_schema_cookie: u32,
    schema_rowid: i64,
) -> Result<Program> {
    if drop.schema.is_some() {
        return Err(Error::msg(
            "schema-qualified DROP INDEX is not yet supported",
        ));
    }
    let mut b = ProgramBuilder::new();

    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    let after_init = b.cur_addr();

    // (1) Open the write transaction.
    b.emit(Opcode::Transaction, 0, 1, 0);

    // (2) Destroy the index b-tree rooted at its rootpage.
    b.emit(Opcode::Destroy, index.rootpage as i32, 0, 0);

    // (3) Open a write cursor on the `sqlite_schema` b-tree, seek to the row, delete it.
    let schema_cursor = 0i32;
    b.emit(Opcode::OpenWrite, schema_cursor, SCHEMA_ROOT, 0);
    let rowid_reg = b.alloc_reg();
    let i = b.emit(Opcode::Int64, 0, rowid_reg, 0);
    b.set_p4(i, P4::Int(schema_rowid));
    // Seek to the row; jump over the delete when the row is absent (defensive — codegen
    // resolves the rowid from the catalog, so it should be present).
    let end_delete = b.new_label();
    b.emit_jump(Opcode::NotExists, schema_cursor, end_delete, rowid_reg);
    // `Delete` on the schema cursor uses the P5_ISUPDATE flag so `changes()` is not bumped
    // (the index's data is gone; we are not counting the schema-row removal as a user-visible
    // change).
    let del_idx = b.emit(Opcode::Delete, schema_cursor, 0, 0);
    b.set_p5(del_idx, P5_ISUPDATE);
    b.resolve(end_delete);

    // (4) Bump the schema cookie.
    b.emit(
        Opcode::SetCookie,
        0,
        COOKIE_SCHEMA,
        current_schema_cookie as i32 + 1,
    );

    // (5) Reload the schema (marker).
    b.emit(Opcode::ParseSchema, 0, 0, 0);

    // (6) Halt commits the transaction.
    b.emit(Opcode::Halt, 0, 0, 0);

    b.resolve(setup);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

/// A no-op `DROP INDEX [IF EXISTS]` against a missing index. The first slice emits no
/// transaction / cookie bump — the index never existed, so the schema is unchanged.
pub fn compile_drop_index_noop() -> Program {
    let mut b = ProgramBuilder::new();
    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    let after_init = b.cur_addr();
    b.emit(Opcode::Halt, 0, 0, 0);
    b.resolve(setup);
    b.emit(Opcode::Goto, 0, after_init, 0);
    b.finish()
}
