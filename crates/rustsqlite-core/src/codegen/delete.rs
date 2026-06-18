//! Lowering `DELETE FROM [tbl] [WHERE expr]` to a VDBE program (mirrors `sqlite3Delete` in
//! `delete.c`).
//!
//! First M4.6 slice: a single-table `DELETE`, with or without a `WHERE` clause. The
//! opcodes that drive the cursor (Rewind, Next, Rowid) and the new `Delete` opcode (see
//! [`crate::vdbe::Opcode`]) together remove each row that matches the predicate (or every
//! row when no predicate is supplied). `ORDER BY` / `LIMIT` / multi-table `DELETE t1, t2 FROM …`
//! are deferred.
//!
//! M5.1: when the prepare path passes a non-empty `indexes` list, the program also emits one
//! `OpenWrite` + `IdxDelete` per index per row, keeping the indexes in sync. The OLD key
//! record is built from the row's column values (read via `Column` opcodes) followed by the
//! rowid. M5.2 generalizes this to multi-column composite keys.

use rustqlite_parser::{DeleteStmt, Expr};

use crate::error::{Error, Result};
use crate::schema::{IndexObject, Table};
use crate::vdbe::program::Program;
use crate::vdbe::Opcode;

use super::builder::ProgramBuilder;
use super::expr::{compile_jump, Ctx};

/// Compile `DELETE FROM <table> [WHERE <expr>]` against `table` with `indexes` as the list of
/// indexes whose entries must be removed alongside each deleted row. Empty `indexes` (the M3a
/// default) means "no indexes to maintain".
pub fn compile_delete(del: &DeleteStmt, table: &Table, indexes: &[IndexObject]) -> Result<Program> {
    if del.schema.is_some() {
        return Err(Error::msg("schema-qualified DELETE is not yet supported"));
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

    // Reserve cursor numbers for the indexes (1, 2, …). The table cursor is 0.
    let index_cursor_base: i32 = 1;
    for (i, idx) in indexes.iter().enumerate() {
        let ic = (index_cursor_base + i as i32) as i32;
        b.emit(Opcode::OpenWrite, ic, idx.rootpage as i32, 0);
    }

    // Top of the loop. `Rewind` jumps to `end_loop` when the table is empty. The body of
    // the loop reads its row, evaluates the WHERE (if any), and either deletes or skips.
    // `Next` advances and, on a valid row, jumps back to the top of the body.
    let end_loop = b.new_label();
    b.emit_jump(Opcode::Rewind, cursor, end_loop, 0);
    let loop_body = b.new_label();
    b.resolve(loop_body);

    // The body proper. The WHERE check is FIRST so that non-matching rows skip both the
    // index maintenance and the table delete. Index maintenance (IdxDelete per index) runs
    // before the table Delete so we can read the OLD column values; the `Rowid` is captured
    // before the table delete for the same reason.
    if let Some(where_expr) = &del.where_clause {
        let end_of_body = b.new_label();
        compile_where(&mut b, where_expr, end_of_body, ctx)?;
        // For the WHERE-matching rows only: capture rowid, IdxDelete per index, then Delete.
        let rowid_reg = b.alloc_reg();
        b.emit(Opcode::Rowid, cursor, rowid_reg, 0);
        for (i, idx) in indexes.iter().enumerate() {
            let ic = (index_cursor_base + i as i32) as i32;
            let indexed_cis = idx.table_column_indices(table)?;
            let nkey = indexed_cis.len() as i32 + 1;
            let key_start = b.alloc_regs(nkey);
            for (j, col_idx) in indexed_cis.iter().enumerate() {
                b.emit(Opcode::Column, cursor, *col_idx as i32, key_start + j as i32);
            }
            b.emit(Opcode::SCopy, rowid_reg, key_start + indexed_cis.len() as i32, 0);
            b.emit(Opcode::IdxDelete, ic, key_start, nkey);
        }
        b.emit(Opcode::Delete, cursor, 0, 0);
        b.resolve(end_of_body);
    } else {
        // Unfiltered delete: every row matches, so unconditionally capture and remove
        // the OLD rowid + index keys.
        let rowid_reg = b.alloc_reg();
        b.emit(Opcode::Rowid, cursor, rowid_reg, 0);
        for (i, idx) in indexes.iter().enumerate() {
            let ic = (index_cursor_base + i as i32) as i32;
            let indexed_cis = idx.table_column_indices(table)?;
            let nkey = indexed_cis.len() as i32 + 1;
            let key_start = b.alloc_regs(nkey);
            for (j, col_idx) in indexed_cis.iter().enumerate() {
                b.emit(Opcode::Column, cursor, *col_idx as i32, key_start + j as i32);
            }
            b.emit(Opcode::SCopy, rowid_reg, key_start + indexed_cis.len() as i32, 0);
            b.emit(Opcode::IdxDelete, ic, key_start, nkey);
        }
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
        let Stmt::CreateTable(ct) = ast else {
            panic!("expected CREATE TABLE")
        };
        Table::from_schema_object(&SchemaObject {
            rowid: 1,
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
        let prog = compile_delete(&d, &t, &[]).unwrap();
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
        let prog = compile_delete(&d, &t, &[]).unwrap();
        let cmp = prog
            .instructions
            .iter()
            .find(|i| matches!(i.opcode, Opcode::Gt | Opcode::Ge | Opcode::Lt | Opcode::Le))
            .expect("expected a comparison opcode for the WHERE");
        assert!(cmp.p2 > 0, "comparison must jump to a non-zero label");
    }
}
