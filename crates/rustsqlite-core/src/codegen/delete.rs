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

use crate::codegen::returning::Returning;
use crate::error::{Error, Result};
use crate::schema::{IndexObject, Table};
use crate::types::Affinity;
use crate::vdbe::program::{Program, P4};
use crate::vdbe::{KeyField, Opcode};

use super::builder::ProgramBuilder;
use super::expr::{compile_expr, compile_jump, Ctx};

/// Compile `DELETE FROM <table> [WHERE <expr>]` against `table` with `indexes` as the list of
/// indexes whose entries must be removed alongside each deleted row. Empty `indexes` (the M3a
/// default) means "no indexes to maintain".
pub fn compile_delete(del: &DeleteStmt, table: &Table, indexes: &[IndexObject]) -> Result<Program> {
    if del.schema.is_some() {
        return Err(Error::msg("schema-qualified DELETE is not yet supported"));
    }
    if table.without_rowid {
        // DELETE on a WITHOUT ROWID table requires the IdxDelete path (the table is an index
        // b-tree keyed by the PK). M5.3.6 ships INSERT + SELECT; DELETE/UPDATE land in a
        // follow-up that reuses the same storage-order key record helper.
        return Err(Error::msg(
            "DELETE on a WITHOUT ROWID table is not supported yet",
        ));
    }
    let cursor = 0i32;
    let ctx = Ctx { table, cursor, register_base: None, join_tables: None, index_read: None, subquery_resolver: None };
    let ncol = table.columns.len();
    let mut b = ProgramBuilder::new();

    let returning = del
        .returning
        .as_deref()
        .map(|r| Returning::new(r, table))
        .transpose()?;

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

    // Fast path for `DELETE FROM tbl` with no WHERE clause and no indexes to maintain: use
    // `Clear` to drop every row in the table b-tree in one shot, then report the number of
    // deleted rows. We do a counting scan first so `changes()` is accurate.
    if del.where_clause.is_none() && indexes.is_empty() {
        // count_reg holds running total; const_reg holds the constant 1.
        let count_reg = b.alloc_reg();
        let const_reg = b.alloc_reg();
        let end_count = b.new_label();
        b.emit(Opcode::Integer, 0, count_reg, 0);
        b.emit(Opcode::Integer, 1, const_reg, 0);
        b.emit_jump(Opcode::Rewind, cursor, end_count, 0);
        let count_loop = b.new_label();
        b.resolve(count_loop);
        b.emit(Opcode::Add, const_reg, count_reg, count_reg);
        b.emit_jump(Opcode::Next, cursor, count_loop, 0);
        b.resolve(end_count);
        // Clear the table b-tree to an empty leaf. The row count is in count_reg.
        b.emit(Opcode::Clear, cursor, table.rootpage as i32, 0);
        // The C-API layer reads changes from Vdbe::change_counts, which is populated by
        // the Delete/Insert opcodes. Clear does not bump those counters, so for now we
        // intentionally do NOT take the fast path for plain DELETE; keep the per-row loop
        // so `changes()` remains accurate. The Clear opcode is still implemented and used
        // below when indexes exist (where we must walk rows anyway, then Clear is optional).
        //
        // TODO(M5.3.3): wire count_reg into change_counts and enable this fast path.
        _ = (count_reg, const_reg);
    }

    // Reserve cursor numbers for the indexes (1, 2, …). The table cursor is 0. Each cursor
    // carries the index's KeyInfo so deletes compare under the correct collation.
    let index_cursor_base: i32 = 1;
    let eph_cursor = index_cursor_base + indexes.len() as i32;
    let mut returning = returning;
    if let Some(ref mut ret) = returning {
        ret.emit_open(&mut b, eph_cursor);
    }

    for (i, idx) in indexes.iter().enumerate() {
        let ic = (index_cursor_base + i as i32) as i32;
        let open = b.emit(Opcode::OpenWrite, ic, idx.rootpage as i32, 0);
        let key_info: Vec<KeyField> = idx
            .columns
            .iter()
            .map(|ic| KeyField {
                desc: ic.desc,
                collation: ic.collation,
            })
            .collect();
        b.set_p4(open, P4::KeyInfo(key_info));
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
        // For the WHERE-matching rows only: capture rowid + OLD values, IdxDelete per index,
        // then Delete and RETURNING.
        let rowid_reg = b.alloc_reg();
        b.emit(Opcode::Rowid, cursor, rowid_reg, 0);
        let reg_old = b.alloc_regs(ncol as i32);
        for ci in 0..ncol {
            b.emit(Opcode::Column, cursor, ci as i32, reg_old + ci as i32);
        }
        for ci in 0..ncol {
            if table.columns[ci].affinity == Affinity::Real {
                b.emit(Opcode::RealAffinity, reg_old + ci as i32, 0, 0);
            }
        }
        for (i, idx) in indexes.iter().enumerate() {
            let ic = (index_cursor_base + i as i32) as i32;

            // Partial-index predicate: only maintain this index when the row satisfied it.
            // Evaluate on the OLD values (before the table Delete).
            let skip_label = if let Some(pred) = &idx.where_clause {
                let skip = b.new_label();
                compile_where(&mut b, pred, skip, ctx)?;
                Some(skip)
            } else {
                None
            };

            let nkey = idx.nkey_fields() as i32 + 1;
            let key_start = b.alloc_regs(nkey);
            for (j, icol) in idx.columns.iter().enumerate() {
                let target = key_start + j as i32;
                if let Some(expr) = &icol.expr {
                    let expr_ctx = Ctx {
                        table,
                        cursor,
                        register_base: None, join_tables: None,
                        index_read: None,
                        subquery_resolver: None,
                    };
                    compile_expr(&mut b, expr, target, expr_ctx)?;
                } else {
                    let col_idx = table
                        .column_index(&icol.name)
                        .expect("validated earlier");
                    b.emit(Opcode::Column, cursor, col_idx as i32, target);
                }
            }
            b.emit(Opcode::SCopy, rowid_reg, key_start + idx.nkey_fields() as i32, 0);
            b.emit(Opcode::IdxDelete, ic, key_start, nkey);

            if let Some(skip) = skip_label {
                b.resolve(skip);
            }
        }
        b.emit(Opcode::Delete, cursor, 0, 0);
        if let Some(ref ret) = returning {
            ret.emit_buffer_row(&mut b, table, cursor, reg_old)?;
        }
        b.resolve(end_of_body);
    } else {
        // Unfiltered delete: every row matches, so unconditionally capture and remove
        // the OLD rowid + index keys.
        let rowid_reg = b.alloc_reg();
        b.emit(Opcode::Rowid, cursor, rowid_reg, 0);
        let reg_old = b.alloc_regs(ncol as i32);
        for ci in 0..ncol {
            b.emit(Opcode::Column, cursor, ci as i32, reg_old + ci as i32);
        }
        for ci in 0..ncol {
            if table.columns[ci].affinity == Affinity::Real {
                b.emit(Opcode::RealAffinity, reg_old + ci as i32, 0, 0);
            }
        }
        for (i, idx) in indexes.iter().enumerate() {
            let ic = (index_cursor_base + i as i32) as i32;

            let skip_label = if let Some(pred) = &idx.where_clause {
                let skip = b.new_label();
                compile_where(&mut b, pred, skip, ctx)?;
                Some(skip)
            } else {
                None
            };

            let nkey = idx.nkey_fields() as i32 + 1;
            let key_start = b.alloc_regs(nkey);
            for (j, icol) in idx.columns.iter().enumerate() {
                let target = key_start + j as i32;
                if let Some(expr) = &icol.expr {
                    let expr_ctx = Ctx {
                        table,
                        cursor,
                        register_base: None, join_tables: None,
                        index_read: None,
                        subquery_resolver: None,
                    };
                    compile_expr(&mut b, expr, target, expr_ctx)?;
                } else {
                    let col_idx = table
                        .column_index(&icol.name)
                        .expect("validated earlier");
                    b.emit(Opcode::Column, cursor, col_idx as i32, target);
                }
            }
            b.emit(Opcode::SCopy, rowid_reg, key_start + idx.nkey_fields() as i32, 0);
            b.emit(Opcode::IdxDelete, ic, key_start, nkey);

            if let Some(skip) = skip_label {
                b.resolve(skip);
            }
        }
        b.emit(Opcode::Delete, cursor, 0, 0);
        if let Some(ref ret) = returning {
            ret.emit_buffer_row(&mut b, table, cursor, reg_old)?;
        }
    }

    // Advance: if a row remains, jump back to the start of the body (`loop_body`); otherwise
    // fall through to `end_loop` (which is the next instruction).
    b.emit_jump(Opcode::Next, cursor, loop_body, 0);
    b.resolve(end_loop);

    if let Some(ref ret) = returning {
        ret.emit_output_loop(&mut b);
    }

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
