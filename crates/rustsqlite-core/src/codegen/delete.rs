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
    // ORDER BY without LIMIT is an error (mirrors upstream's
    // "ORDER BY without LIMIT on DELETE").
    if !del.order_by.is_empty() && del.limit.is_none() {
        return Err(Error::msg("ORDER BY without LIMIT on DELETE"));
    }
    // When ORDER BY or LIMIT is present, use the sorter-as-rowset approach: scan
    // matching rowids into a sorter (ordered by ORDER BY), apply LIMIT/OFFSET, then
    // walk the sorted rowids and delete each one by rowid.
    if del.limit.is_some() || !del.order_by.is_empty() {
        return compile_delete_ordered_limited(del, table, indexes);
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

/// Compile `DELETE FROM tbl [WHERE expr] [ORDER BY ...] LIMIT n [OFFSET m]` using the
/// sorter-as-rowset approach (mirrors upstream's `sqlite3LimitWhere` rewrite to
/// `WHERE rowid IN (SELECT rowid FROM tbl WHERE ... ORDER BY ... LIMIT ...)`).
///
/// The shape is: scan the table with the WHERE filter, capture matching rowids + ORDER BY
/// values into a sorter, sort, walk the sorted rowids applying OFFSET/LIMIT, and for each
/// selected rowid seek the table cursor and delete the row + its index entries.
fn compile_delete_ordered_limited(
    del: &DeleteStmt,
    table: &Table,
    indexes: &[IndexObject],
) -> Result<Program> {
    let cursor = 0i32;
    let sorter = 1i32;
    let ctx = Ctx { table, cursor, register_base: None, join_tables: None, index_read: None, subquery_resolver: None };
    let ncol = table.columns.len();
    let mut b = ProgramBuilder::new();

    let returning = del
        .returning
        .as_deref()
        .map(|r| Returning::new(r, table))
        .transpose()?;

    let setup = b.new_label();
    let after_init = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    b.resolve(after_init);

    b.emit(Opcode::Transaction, 0, 1, 0);
    b.emit(Opcode::OpenWrite, cursor, table.rootpage as i32, 0);

    // Index cursors (2, 3, …).
    let index_cursor_base: i32 = 2;
    for (i, idx) in indexes.iter().enumerate() {
        let ic = index_cursor_base + i as i32;
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

    // Sorter layout: [rowid, order_by_values...]. The sorter orders by the ORDER BY
    // columns; the rowid is the payload used to identify the row to delete.
    let n_order = del.order_by.len();
    let _ = n_order;
    let sorter_fields: Vec<KeyField> = del
        .order_by
        .iter()
        .map(|ot| KeyField {
            desc: ot.desc,
            collation: crate::types::Collation::Binary,
        })
        .collect();
    // When there's no ORDER BY (just LIMIT), use a single dummy key field so the sorter
    // preserves insertion order (all BINARY, all ASC).
    let sorter_fields: Vec<KeyField> = if sorter_fields.is_empty() {
        vec![KeyField::asc_binary()]
    } else {
        sorter_fields
    };
    let n_sorter_keys = sorter_fields.len() as i32;
    let so = b.emit(Opcode::SorterOpen, sorter, n_sorter_keys, 0);
    b.set_p4(so, P4::KeyInfo(sorter_fields));

    // --- Pass 1: scan matching rows into the sorter. ---
    let end_scan = b.new_label();
    b.emit_jump(Opcode::Rewind, cursor, end_scan, 0);
    let scan_loop = b.new_label();
    b.resolve(scan_loop);

    // Apply WHERE filter: jump to `skip_row` (just before `Next`) when the
    // predicate is FALSE, so non-matching rows are not added to the sorter.
    let skip_row = b.new_label();
    if let Some(where_expr) = &del.where_clause {
        compile_where(&mut b, where_expr, skip_row, ctx)?;
    }

    // Capture the rowid.
    let rowid_reg = b.alloc_reg();
    b.emit(Opcode::Rowid, cursor, rowid_reg, 0);

    // Build the sorter record: [order_by_values..., rowid].
    // The sorter orders by the first n_order columns; the rowid is the trailing payload.
    let rec_start = b.alloc_regs(n_sorter_keys + 1);
    for (j, ot) in del.order_by.iter().enumerate() {
        let target = rec_start + j as i32;
        compile_expr(&mut b, &ot.expr, target, ctx)?;
    }
    // When there's no ORDER BY, the single dummy key field gets a constant 0 so all
    // rows have the same sort key (insertion order is preserved).
    if del.order_by.is_empty() {
        b.emit(Opcode::Integer, 0, rec_start, 0);
    }
    b.emit(Opcode::SCopy, rowid_reg, rec_start + n_sorter_keys, 0);
    let rec = b.alloc_reg();
    b.emit(Opcode::MakeRecord, rec_start, n_sorter_keys + 1, rec);
    b.emit(Opcode::SorterInsert, sorter, rec, 0);

    b.resolve(skip_row);
    b.emit_jump(Opcode::Next, cursor, scan_loop, 0);
    b.resolve(end_scan);

    // --- Pass 2: sort and walk the rowids, applying LIMIT/OFFSET, then delete. ---
    // Initialize OFFSET and LIMIT counters BEFORE the sort loop so they persist
    // across iterations (the loop jumps back to `delete_top` which is after the
    // initialization).
    let offset_reg = b.alloc_reg();
    if let Some(offset_expr) = &del.offset {
        compile_expr(&mut b, offset_expr, offset_reg, ctx)?;
    } else {
        b.emit(Opcode::Integer, 0, offset_reg, 0);
    }
    let limit_reg = b.alloc_reg();
    if let Some(limit_expr) = &del.limit {
        compile_expr(&mut b, limit_expr, limit_reg, ctx)?;
    } else {
        // No LIMIT: use -1 (effectively unlimited) so IfPos never falls through.
        b.emit(Opcode::Integer, -1, limit_reg, 0);
    }

    let end_delete = b.new_label();
    b.emit_jump(Opcode::SorterSort, sorter, end_delete, 0);
    let delete_top = b.new_label();
    b.resolve(delete_top);
    // Load the current sorter record so `Column` can read from it.
    b.emit(Opcode::SorterData, sorter, 0, 0);

    // OFFSET: skip the first OFFSET rows. IfPos jumps to `skip_offset` when
    // offset > 0 (decrementing the counter); falls through when offset <= 0.
    let has_offset = del.offset.is_some();
    if has_offset {
        let skip_offset = b.new_label();
        let after_offset = b.new_label();
        b.emit_jump(Opcode::IfPos, offset_reg, skip_offset, 1);
        // offset <= 0: fall through to after_offset (start deleting).
        b.emit_jump(Opcode::Goto, 0, after_offset, 0);
        b.resolve(skip_offset);
        // Skip to the next sorter row without deleting.
        b.emit_jump(Opcode::SorterNext, sorter, delete_top, 0);
        // SorterNext fell through (no more rows) → end.
        b.emit_jump(Opcode::Goto, 0, end_delete, 0);
        b.resolve(after_offset);
    }

    // LIMIT: stop after LIMIT rows. IfPos jumps to `do_delete` when limit > 0
    // (decrementing the counter); falls through to `end_delete` when limit <= 0.
    let do_delete = b.new_label();
    b.emit_jump(Opcode::IfPos, limit_reg, do_delete, 1);
    b.emit_jump(Opcode::Goto, 0, end_delete, 0);
    b.resolve(do_delete);

    // Decode the sorter record to get the rowid.
    let decoded_rowid = b.alloc_reg();
    b.emit(Opcode::Column, sorter, n_sorter_keys, decoded_rowid);

    // Seek the table cursor to the rowid.
    let not_found = b.new_label();
    b.emit_jump(Opcode::NotExists, cursor, not_found, decoded_rowid);

    // Read the OLD column values for index maintenance.
    let reg_old = b.alloc_regs(ncol as i32);
    for ci in 0..ncol {
        b.emit(Opcode::Column, cursor, ci as i32, reg_old + ci as i32);
    }
    for ci in 0..ncol {
        if table.columns[ci].affinity == Affinity::Real {
            b.emit(Opcode::RealAffinity, reg_old + ci as i32, 0, 0);
        }
    }

    // IdxDelete per index.
    for (i, idx) in indexes.iter().enumerate() {
        let ic = index_cursor_base + i as i32;
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
        b.emit(Opcode::SCopy, decoded_rowid, key_start + idx.nkey_fields() as i32, 0);
        b.emit(Opcode::IdxDelete, ic, key_start, nkey);
        if let Some(skip) = skip_label {
            b.resolve(skip);
        }
    }

    b.emit(Opcode::Delete, cursor, 0, 0);
    let returning = returning;
    if let Some(ref ret) = returning {
        ret.emit_buffer_row(&mut b, table, cursor, reg_old)?;
    }

    b.resolve(not_found);
    b.emit_jump(Opcode::SorterNext, sorter, delete_top, 0);
    b.resolve(end_delete);

    if let Some(ref ret) = returning {
        ret.emit_output_loop(&mut b);
    }

    b.emit(Opcode::Halt, 0, 0, 0);
    b.resolve(setup);
    b.emit_jump(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}
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
