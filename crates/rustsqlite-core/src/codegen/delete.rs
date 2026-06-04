//! Lowering `DELETE FROM [tbl] [WHERE expr]` to a VDBE program (mirrors `sqlite3Delete` in
//! `delete.c`).
//!
//! First M4.6 slice: a single-table `DELETE`, with or without a `WHERE` clause. The
//! opcodes that drive the cursor (Rewind, Next, Rowid) and the new `Delete` opcode (see
//! [`crate::vdbe::Opcode`]) together remove each row that matches the predicate (or every
//! row when no predicate is supplied). `ORDER BY` / `LIMIT` / multi-table `DELETE t1, t2 FROM …`
//! are deferred.
//!
//! The layout below is the standard `sqlite3VdbeAddOp*` sequence from upstream:
//!
//! ```text
//!   Init        0, end                       ; jump past the setup
//!   Transaction 0, 1                         ; open the write transaction
//!   OpenWrite   0, <rootpage>, 0             ; open the table b-tree
//!   Rewind      0, end_loop                  ; empty table → skip
//! loop_top:
//!   Next        0, end_loop                  ; advance to next row, fall through if valid
//!   (compile_jump <where> → end_of_body)     ; row matches predicate → delete (only when WHERE is set)
//!   Delete      0                             ; remove the row at the current cursor
//! end_of_body:
//!   Goto        loop_top
//! end_loop:
//!   Halt
//! end:
//! ```

use rustqlite_parser::{DeleteStmt, Expr};

use crate::error::{Error, Result};
use crate::schema::Table;
use crate::vdbe::program::Program;
use crate::vdbe::Opcode;

use super::builder::ProgramBuilder;
use super::expr::{compile_jump, Ctx};

/// Compile `DELETE FROM <table> [WHERE <expr>]`.
pub fn compile_delete(del: &DeleteStmt, table: &Table) -> Result<Program> {
    if del.schema.is_some() {
        return Err(Error::msg(
            "schema-qualified DELETE is not yet supported",
        ));
    }
    let cursor = 0i32;
    let ctx = Ctx { table, cursor };
    let mut b = ProgramBuilder::new();

    // Standard VDBE preamble: `Init 0, setup` at addr 0 jumps to the trailing `Goto after_init`
    // (the `setup` body), which jumps back to the first real work instruction. This is the
    // pattern used by CREATE TABLE / INSERT so the first opcode can re-execute on a `Reset`.
    let setup = b.new_label();
    let after_init = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    b.resolve(after_init);

    // Open a write transaction and the table cursor.
    b.emit(Opcode::Transaction, 0, 1, 0);
    b.emit(Opcode::OpenWrite, cursor, table.rootpage as i32, 0);

    // Top of the loop. `Rewind` jumps to `end_loop` when the table is empty. The body of
    // the loop reads its row, evaluates the WHERE (if any), and either deletes or skips.
    // `Next` advances and, on a valid row, jumps back to the top of the body.
    let end_loop = b.new_label();
    b.emit_jump(Opcode::Rewind, cursor, end_loop, 0);
    let loop_body = b.new_label();
    b.resolve(loop_body);

    // The body proper. When the WHERE is set, we jump to `end_of_body` on a false predicate.
    if let Some(where_expr) = &del.where_clause {
        let end_of_body = b.new_label();
        compile_where(&mut b, where_expr, end_of_body, ctx)?;
        b.emit(Opcode::Delete, cursor, 0, 0);
        b.resolve(end_of_body);
    } else {
        b.emit(Opcode::Delete, cursor, 0, 0);
    }

    // Advance: if a row remains, jump back to the start of the body (`loop_body`); otherwise
    // fall through to `end_loop` (which is the next instruction).
    b.emit_jump(Opcode::Next, cursor, loop_body, 0);
    b.resolve(end_loop);

    b.emit(Opcode::Halt, 0, 0, 0);
    b.resolve(setup);
    b.emit_jump(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}



/// Compile the WHERE clause: jump to `end_of_body` (which sits after the Delete, just
/// before the next `Next`) when the predicate is FALSE. NULL rows also skip the Delete,
/// matching `SELECT`'s WHERE-null-skip-row convention.
fn compile_where(
    b: &mut ProgramBuilder,
    expr: &Expr,
    end_of_body: super::builder::Label,
    ctx: Ctx,
) -> Result<()> {
    compile_jump(b, expr, end_of_body, false, true, ctx)
}

#[cfg(test)]
mod tests {
    use rustqlite_parser::{parse, Stmt};

    use super::*;
    use crate::schema::{SchemaObject, Table};

    fn table_of(sql: &str) -> Table {
        let ast = parse(sql).unwrap().into_iter().next().unwrap();
        let Stmt::CreateTable(ct) = ast else { panic!("expected CREATE TABLE") };
        Table::from_schema_object(&SchemaObject {
            obj_type: "table".into(),
            name: ct.name.clone(),
            tbl_name: ct.name.clone(),
            rootpage: 100,
            sql: Some(sql.to_string()),
        })
        .unwrap()
    }

    fn delete_of(sql: &str) -> DeleteStmt {
        match parse(sql).unwrap().into_iter().next().unwrap() {
            Stmt::Delete(d) => d,
            _ => panic!("expected DELETE"),
        }
    }

    #[test]
    fn unfiltered_delete_walks_table() {
        let t = table_of("CREATE TABLE t(a, b)");
        let d = delete_of("DELETE FROM t;");
        let prog = compile_delete(&d, &t).unwrap();
        let names: Vec<&str> = prog.instructions.iter().map(|i| i.opcode.name()).collect();
        assert!(names.contains(&"OpenWrite"));
        assert!(names.contains(&"Rewind"));
        assert!(names.contains(&"Next"));
        assert!(names.contains(&"Delete"));
        assert!(names.contains(&"Halt"));
        assert!(names.contains(&"Transaction"));
    }

    #[test]
    fn where_filter_emits_jump() {
        let t = table_of("CREATE TABLE t(a, b)");
        let d = delete_of("DELETE FROM t WHERE a > 1;");
        let prog = compile_delete(&d, &t).unwrap();
        let cmp = prog
            .instructions
            .iter()
            .find(|i| matches!(i.opcode, Opcode::Gt | Opcode::Ge | Opcode::Lt | Opcode::Le))
            .expect("expected a comparison opcode for the WHERE");
        assert!(cmp.p2 > 0, "comparison must jump to a non-zero label");
    }
}
