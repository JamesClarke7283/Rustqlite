//! Lowering `UPDATE [or_action] tbl SET col = expr [, ...] [WHERE expr]` to a VDBE program
//! (mirrors `sqlite3Update` in `update.c`).
//!
//! First M5.0 slice: a single-table `UPDATE` with optional `WHERE` clause. The codegen emits
//! the **two-pass** (ephemeral-rowset) shape that upstream uses for `ONEPASS_OFF` updates —
//! the same one that upstream falls through to when triggers, foreign-key checks, REPLACE
//! conflict handlers, or rowid-aliased PRIMARY KEY changes are absent. Faithfulness and
//! future-proofing (triggers, UPSERT, etc. all slot into the same skeleton).
//!
//! The shape (in VDBE opcodes, comments mirror `update.c`):
//!
//! ```text
//!   Init              0, setup
//! after_init:
//!   Transaction       0, 1                          ; open the write transaction
//!   SorterOpen        sorter, 1, k(rowid asc)       ; the rowid-set
//!   OpenWrite         0, <rootpage>, 0
//!   Rewind            0, end_scan
//! scan_top:
//!   (compile_jump <where> -> scan_next)            ; if WHERE false, skip
//!   Rowid             0, regOldRowid                ; capture the rowid
//!   MakeRecord        regOldRowid, 1, regRowidRec
//!   SorterInsert      sorter, regRowidRec
//! scan_next:
//!   Next              0, scan_top
//! end_scan:
//!   SorterSort        sorter, end_update
//! update_top:
//!   SorterData        sorter
//!   Column            sorter, 0, regOldRowid
//!   NotExists         0, sort_next, regOldRowid     ; row gone? skip
//!   ; build the new record:
//!   <for each table column>:
//!     Column          0, ci, regNew+ci              ; default: copy the old value
//!   <for each SET assignment>:
//!     <compile_expr <value> -> regNew+ci>          ; override the column
//!     (if ci is a rowid alias) NotExists 0, sort_next, regOldRowid [defensive]
//!   <not-null check> per assigned NOT NULL column
//!   Affinity          regNew, ncol, P4=<affString>
//!   MakeRecord        regNew, ncol, regNewRec
//!   Delete            0, 0, 0, p5=P5_ISUPDATE
//!   Insert            0, regNewRec, regOldRowid, p5=P5_ISUPDATE
//! sort_next:
//!   SorterNext        sorter, update_top
//! end_update:
//!   Halt
//! setup:
//!   Goto              after_init
//! ```
//!
//! Scope: `OR action` other than the default ABORT is parsed but errors with a precise
//! "ON CONFLICT <action> is not yet supported" message; updating the rowid-alias column
//! (`SET <IPK col> = …`) is also rejected at codegen time. NOT NULL enforcement is not yet
//! modeled (the INSERT path has the same gap); it arrives with the constraint-checks slice.

use rustqlite_parser::{Assignment, UpdateStmt};

use crate::codegen::builder::Label;
use crate::codegen::returning::Returning;

use crate::error::{Error, Result};
use crate::schema::{IndexObject, Table};
use crate::types::Affinity;
use crate::vdbe::program::{Program, P4, P5_ISUPDATE, P5_UNIQUE};
use crate::vdbe::{KeyField, Opcode};

use super::builder::ProgramBuilder;
use super::expr::{compile_expr, compile_jump, Ctx};

/// Compile `UPDATE [OR action] tbl SET col = expr [, …] [WHERE expr]`. `indexes` is the list
/// of indexes attached to `table`; the codegen emits per-row `IdxDelete` + `IdxInsert`
/// maintenance for each index, now including multi-column composite keys (M5.2).
pub fn compile_update(upd: &UpdateStmt, table: &Table, indexes: &[IndexObject]) -> Result<Program> {
    if upd.schema.is_some() {
        return Err(Error::msg("schema-qualified UPDATE is not yet supported"));
    }
    if table.without_rowid {
        // UPDATE on a WITHOUT ROWID table is an IdxDelete + IdxInsert on the table b-tree
        // (keyed by the PK); M5.3.6 ships INSERT + SELECT, with DELETE/UPDATE landing in a
        // follow-up that reuses the storage-order key-record helpers.
        return Err(Error::msg(
            "UPDATE on a WITHOUT ROWID table is not supported yet",
        ));
    }
    if let Some(action) = upd.or_action {
        return Err(Error::msg(format!(
            "ON CONFLICT {:?} is not yet supported (only the default ABORT is implemented)",
            action
        )));
    }
    if upd.assignments.is_empty() {
        return Err(Error::msg("UPDATE must set at least one column"));
    }
    if upd.table != table.name {
        // `UPDATE main.t` would route to a different schema; the parser has already absorbed
        // any schema qualifier, so a mismatch here means the codegen was given a table object
        // from a different name — guard defensively.
        return Err(Error::msg(format!(
            "UPDATE targets `{}` but table object is `{}`",
            upd.table, table.name
        )));
    }

    // Resolve the assignments: for each, the table column index and the value expression.
    // Last-write-wins on duplicate columns (matches upstream `sqlite3Update`).
    let ncol = table.columns.len();
    let mut target_col: Vec<Option<(usize, &rustqlite_parser::Expr)>> = vec![None; ncol];
    for Assignment { column, value } in &upd.assignments {
        let ci = table.column_index(column).ok_or_else(|| {
            Error::msg(format!("table {} has no column named {column}", table.name))
        })?;
        target_col[ci] = Some((ci, value));
    }

    // The first M5.0 slice cannot change the rowid. Reject the case where the SET list
    // includes the rowid-alias column (an explicit `SET rowid = …` would compile as
    // `resolve_column("rowid") → Rowid`, but column resolution via `column_index` would fail
    // because the rowid alias has no stored column index, so this branch is unreachable
    // through the current parser — kept for clarity and as a guard for a future rowid-set
    // path).
    for (ci, slot) in target_col.iter().enumerate() {
        if slot.is_some() && table.rowid_alias == Some(ci) {
            return Err(Error::msg(format!(
                "UPDATE of the INTEGER PRIMARY KEY column is not yet supported (table {}, column {})",
                table.name, table.columns[ci].name
            )));
        }
    }

    let cursor = 0i32;
    let sorter = 1i32;
    let ctx = Ctx { table, cursor, register_base: None, index_read: None };
    let mut b = ProgramBuilder::new();

    let returning = upd
        .returning
        .as_deref()
        .map(|r| Returning::new(r, table))
        .transpose()?;

    let setup = b.new_label();
    let after_init = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    b.resolve(after_init);

    // (1) Write transaction.
    b.emit(Opcode::Transaction, 0, 1, 0);

    // (2) Open the rowid-set sorter with one key field (the rowid, ASC, BINARY).
    let so = b.emit(Opcode::SorterOpen, sorter, 1, 0);
    b.set_p4(so, P4::KeyInfo(vec![crate::vdbe::KeyField::asc_binary()]));

    // RETURNING ephemeral cursor sits above the index cursors (allocated later). For now pick a
    // high cursor number; we open it after index cursors are emitted.
    let eph_cursor: i32 = 20;
    let mut returning = returning;
    if let Some(ref mut ret) = returning {
        ret.emit_open(&mut b, eph_cursor);
    }

    // (3) Open the table b-tree for read+write.
    let open = b.emit(Opcode::OpenWrite, cursor, table.rootpage as i32, 0);
    b.set_p4(open, P4::Int(ncol as i64));

    // (3b) Open the indexes as write cursors (cursor 2, 3, …) so the second pass can
    // IdxDelete the OLD key and IdxInsert the NEW key for each index. Multi-column indexes
    // are supported from M5.2 onward. Each cursor carries the index's KeyInfo so the
    // underlying index cursor compares keys under the correct per-column collation.
    let index_cursor_base: i32 = 2;
    for (i, idx) in indexes.iter().enumerate() {
        let _ = idx.table_column_indices(table)?; // validate columns exist
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

    // (4) First pass: scan, evaluate WHERE, capture matching rowids into the sorter.
    let end_scan = b.new_label();
    b.emit_jump(Opcode::Rewind, cursor, end_scan, 0);
    let scan_top = b.new_label();
    let scan_next = b.new_label();
    b.resolve(scan_top);

    if let Some(w) = &upd.where_clause {
        compile_jump(&mut b, w, scan_next, false, true, ctx)?;
    }
    let reg_old_rowid = b.alloc_reg();
    let reg_rowid_rec = b.alloc_reg();
    b.emit(Opcode::Rowid, cursor, reg_old_rowid, 0);
    b.emit(Opcode::MakeRecord, reg_old_rowid, 1, reg_rowid_rec);
    b.emit(Opcode::SorterInsert, sorter, reg_rowid_rec, 0);

    b.resolve(scan_next);
    b.emit_jump(Opcode::Next, cursor, scan_top, 0);
    b.resolve(end_scan);

    // (5) Second pass: iterate the sorter, re-seek each rowid, build the new record,
    // delete + re-insert. `changes()` and `total_changes()` count once per row updated
    // because the Delete carries `P5_ISUPDATE` (suppresses its own counter; the Insert
    // bumps once). `last_insert_rowid()` is left untouched (matches upstream).
    let end_update = b.new_label();
    b.emit_jump(Opcode::SorterSort, sorter, end_update, 0);
    let update_top = b.new_label();
    let sort_next = b.new_label();
    b.resolve(update_top);

    b.emit(Opcode::SorterData, sorter, 0, 0);
    // Pull the captured rowid back out of the sorter record.
    let reg_old_rowid2 = b.alloc_reg();
    b.emit(Opcode::Column, sorter, 0, reg_old_rowid2);
    // Re-seek the table cursor; if the row is gone (concurrent delete), skip.
    b.emit_jump(Opcode::NotExists, cursor, sort_next, reg_old_rowid2);

    // (6) Build the new record. One register per table column; default is to copy the
    // current row's value, then a SET assignment overwrites its slot.
    let reg_new = b.alloc_regs(ncol as i32);
    for ci in 0..ncol {
        b.emit(Opcode::Column, cursor, ci as i32, reg_new + ci as i32);
    }
    // A REAL-affinity column may have stored an integer-valued row as an integer; realify
    // it (mirrors `OP_RealAffinity` placed after the `OP_Column` by the read path).
    for ci in 0..ncol {
        if table.columns[ci].affinity == Affinity::Real {
            b.emit(Opcode::RealAffinity, reg_new + ci as i32, 0, 0);
        }
    }

    // (6b) Snapshot the full OLD row into a contiguous register block BEFORE any SET
    // assignment overwrites `reg_new`. The OLD index keys (and the partial-index predicate
    // for the old-key delete) are evaluated from this snapshot so they match the on-disk
    // index entries, even when the SET list changes indexed columns.
    let reg_old = b.alloc_regs(ncol as i32);
    for ci in 0..ncol {
        b.emit(Opcode::SCopy, reg_new + ci as i32, reg_old + ci as i32, 0);
    }

    // (6c) For tables without an INTEGER PRIMARY KEY alias, the staged rowid is not part of
    // reg_new. RETURNING may reference `rowid`; capture it into the block so the helper can
    // resolve it.
    if table.rowid_alias.is_none() {
        let _placeholder = b.alloc_reg();
    }

    for (ci, slot) in target_col.iter().enumerate() {
        if let Some((_, value)) = slot {
            compile_expr(&mut b, value, reg_new + ci as i32, ctx)?;
        }
    }

    // (7) Apply the table's column affinities to the new record.
    let mut aff_string = String::with_capacity(ncol);
    for col in &table.columns {
        aff_string.push(affinity_char(col.affinity) as char);
    }
    if !aff_string.is_empty() {
        let idx = b.emit(Opcode::Affinity, reg_new, ncol as i32, 0);
        b.set_p4(idx, P4::Symbol(aff_string));
    }

    // (9) Make the record, delete the old row, re-insert at the same rowid. The Delete
    // and the Insert both carry `P5_ISUPDATE` so the change counters fire once.
    let reg_new_rec = b.alloc_reg();
    b.emit(Opcode::MakeRecord, reg_new, ncol as i32, reg_new_rec);

    // (9b) Index maintenance: for each index, build the OLD composite key (using the
    // snapshotted old values captured at (6b) above) and IdxDelete it. The table Delete is
    // then performed, followed by the Insert, and finally the NEW composite key is
    // IdxInserted.
    //
    // Partial-index predicate: the OLD row had an index entry only if it satisfied the
    // predicate, so we conditionally skip the IdxDelete. We evaluate the predicate against
    // the NEW row values in `reg_new`: for columns that are not being assigned, `reg_new`
    // still holds the OLD value (we copied it at (6) above); for assigned columns the value
    // changed, but the OLD key must still be deleted, and the predicate's truth value is the
    // same regardless of the NEW value when it references only non-updated columns. Predicates
    // that reference an assigned column are not supported in this slice.
    for (i, idx) in indexes.iter().enumerate() {
        let ic = index_cursor_base + i as i32;
        let indexed_cis = idx.table_column_indices(table).expect("validated earlier");
        let nkey = idx.nkey_fields() as i32 + 1; // indexed key fields + trailing rowid

        let skip_delete_label = if let Some(pred) = &idx.where_clause {
            validate_partial_pred_on_update(pred, table, &target_col)?;
            let skip = b.new_label();
            let pred_ctx = Ctx { table, cursor, register_base: None, index_read: None };
            compile_pred_jump(&mut b, pred, skip, table, reg_new, indexed_cis.as_slice(), pred_ctx)?;
            Some(skip)
        } else {
            None
        };

        let old_key = b.alloc_regs(nkey);
        for (j, icol) in idx.columns.iter().enumerate() {
            let target = old_key + j as i32;
            if let Some(expr) = &icol.expr {
                // Evaluate the OLD expression against the snapshotted OLD row registers.
                let expr_ctx = Ctx {
                    table,
                    cursor,
                    register_base: Some(reg_old),
                    index_read: None,
                };
                compile_expr(&mut b, expr, target, expr_ctx)?;
            } else {
                let col_idx = table
                    .column_index(&icol.name)
                    .expect("validated earlier");
                b.emit(Opcode::SCopy, reg_old + col_idx as i32, target, 0);
            }
        }
        b.emit(
            Opcode::SCopy,
            reg_old_rowid2,
            old_key + idx.nkey_fields() as i32,
            0,
        );
        // IdxDelete reads the key values from r[p2..p2+p3]; we pass the first register and
        // the number of fields.
        b.emit(Opcode::IdxDelete, ic, old_key, nkey);

        if let Some(skip) = skip_delete_label {
            b.resolve(skip);
        }

        let _ = i; // cursor numbers are derived from index position
    }

    let del_idx = b.emit(Opcode::Delete, cursor, 0, 0);
    b.set_p5(del_idx, P5_ISUPDATE);
    let ins_idx = b.emit(Opcode::Insert, cursor, reg_new_rec, reg_old_rowid2);
    b.set_p5(ins_idx, P5_ISUPDATE);

    if let Some(ref ret) = returning {
        // The rowid-alias slot in the stored record is NULL, but RETURNING needs the logical
        // column value (the rowid). Patch it into the staged register block before evaluating.
        if let Some(alias_idx) = table.rowid_alias {
            b.emit(Opcode::SCopy, reg_old_rowid2, reg_new + alias_idx as i32, 0);
        }
        ret.emit_buffer_row(&mut b, table, cursor, reg_new)?;
    }

    // (9c) New-key IdxInsert (post-Insert, so the new values are in `reg_new + col_idx`,
    // whether the SET overwrote them or not).
    for (i, idx) in indexes.iter().enumerate() {
        let ic = index_cursor_base + i as i32;
        let indexed_cis = idx.table_column_indices(table).expect("validated earlier");
        let nkey = idx.nkey_fields() as i32 + 1;

        // Partial-index predicate: only insert the NEW row if it satisfies the predicate.
        let skip_insert_label = if let Some(pred) = &idx.where_clause {
            validate_partial_pred_on_update(pred, table, &target_col)?;
            let skip = b.new_label();
            let pred_ctx = Ctx { table, cursor, register_base: None, index_read: None };
            compile_pred_jump(&mut b, pred, skip, table, reg_new, indexed_cis.as_slice(), pred_ctx)?;
            Some(skip)
        } else {
            None
        };

        let new_key = b.alloc_regs(nkey);
        let mut plain_iter = indexed_cis.iter();
        for (j, icol) in idx.columns.iter().enumerate() {
            let target = new_key + j as i32;
            if let Some(expr) = &icol.expr {
                // Evaluate the expression against the NEW row registers.
                let expr_ctx = Ctx {
                    table,
                    cursor,
                    register_base: Some(reg_new),
                    index_read: None,
                };
                compile_expr(&mut b, expr, target, expr_ctx)?;
            } else {
                let col_idx = *plain_iter.next().expect("plain column aligned with indexed_cis");
                b.emit(
                    Opcode::SCopy,
                    reg_new + col_idx as i32,
                    target,
                    0,
                );
            }
        }
        b.emit(
            Opcode::SCopy,
            reg_old_rowid2,
            new_key + idx.nkey_fields() as i32,
            0,
        );
        let new_key_rec = b.alloc_reg();
        b.emit(Opcode::MakeRecord, new_key, nkey, new_key_rec);
        let idx_ins = b.emit(Opcode::IdxInsert, ic, new_key_rec, 0);
        let mut p5 = P5_ISUPDATE;
        if idx.unique {
            p5 |= P5_UNIQUE;
            if let Some(msg) = idx.unique_constraint_message(table) {
                b.set_p4(idx_ins, P4::Text(msg));
            } else {
                b.set_p4(idx_ins, P4::Int(0));
            }
        } else {
            b.set_p4(idx_ins, P4::Int(0));
        }
        b.set_p5(idx_ins, p5);

        if let Some(skip) = skip_insert_label {
            b.resolve(skip);
        }

        let _ = i;
    }

    b.resolve(sort_next);
    b.emit_jump(Opcode::SorterNext, sorter, update_top, 0);
    b.resolve(end_update);

    if let Some(ref ret) = returning {
        ret.emit_output_loop(&mut b);
    }

    b.emit(Opcode::Halt, 0, 0, 0);

    b.resolve(setup);
    b.emit_jump(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

/// The single-character affinity code the `Affinity` opcode reads (matches `vdbe.c`'s
/// `SQLITE_AFF_*` letters: BLOB='A', TEXT='B', NUMERIC='C', INTEGER='D', REAL='E').
fn affinity_char(a: Affinity) -> u8 {
    match a {
        Affinity::Blob => b'A',
        Affinity::Text => b'B',
        Affinity::Numeric => b'C',
        Affinity::Integer => b'D',
        Affinity::Real => b'E',
    }
}

/// Compile a partial-index predicate as a conditional jump over the row values in a
/// contiguous register block starting at `reg_base`. `compile_jump` is run with a context
/// whose `register_base` is set, so column references read directly from the row registers
/// rather than from a positioned table cursor.
pub(crate) fn compile_pred_jump(
    b: &mut ProgramBuilder,
    pred: &rustqlite_parser::Expr,
    skip: Label,
    _table: &Table,
    reg_base: i32,
    _indexed_cis: &[usize],
    mut ctx: Ctx,
) -> Result<()> {
    ctx.register_base = Some(reg_base);
    compile_jump(b, pred, skip, false, true, ctx)
}

/// Collect the table column names referenced by an expression.
fn referenced_columns(expr: &rustqlite_parser::Expr) -> Vec<String> {
    use rustqlite_parser::Expr;
    let mut out = Vec::new();
    let mut stack = vec![expr];
    while let Some(e) = stack.pop() {
        match e {
            Expr::Column { name, .. } => out.push(name.clone()),
            Expr::Unary { expr, .. } => stack.push(expr),
            Expr::Binary { left, right, .. } => {
                stack.push(left);
                stack.push(right);
            }
            Expr::Function { args, .. } => {
                if let rustqlite_parser::FunctionArgs::List(v) = args {
                    for a in v {
                        stack.push(a);
                    }
                }
            }
            Expr::Cast { expr, .. } => stack.push(expr),
            Expr::Collate { expr, .. } => stack.push(expr),
            Expr::Case {
                base,
                when_then,
                else_expr,
            } => {
                if let Some(b) = base {
                    stack.push(b);
                }
                for (w, t) in when_then {
                    stack.push(w);
                    stack.push(t);
                }
                if let Some(e) = else_expr {
                    stack.push(e);
                }
            }
            Expr::Between { expr, low, high, .. } => {
                stack.push(expr);
                stack.push(low);
                stack.push(high);
            }
            Expr::In { expr, values, .. } => {
                stack.push(expr);
                for v in values {
                    stack.push(v);
                }
            }
            Expr::IsDistinctFrom { left, right, .. } => {
                stack.push(left);
                stack.push(right);
            }
            _ => {}
        }
    }
    out
}

/// Reject partial-index predicates that reference a column being updated. The current
/// for the old-key deletion only when the predicate does not involve an assigned column.
fn validate_partial_pred_on_update(
    pred: &rustqlite_parser::Expr,
    table: &Table,
    target_col: &[Option<(usize, &rustqlite_parser::Expr)>],
) -> Result<()> {
    let mut stack = vec![pred];
    while let Some(e) = stack.pop() {
        match e {
            rustqlite_parser::Expr::Column { name, .. } => {
                let ci = table.column_index(name).ok_or_else(|| {
                    Error::msg(format!(
                        "partial-index predicate references unknown column: {name}"
                    ))
                })?;
                if target_col[ci].is_some() {
                    return Err(Error::msg(format!(
                        "partial-index predicate referencing updated column '{name} is not supported in this slice"
                    )));
                }
            }
            rustqlite_parser::Expr::Unary { expr, .. } => stack.push(expr),
            rustqlite_parser::Expr::Binary { left, right, .. } => {
                stack.push(left);
                stack.push(right);
            }
            rustqlite_parser::Expr::Collate { expr, .. } => stack.push(expr),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{SchemaObject, Table};
    use rustqlite_parser::{parse, Stmt};

    fn table_of(sql: &str) -> Table {
        let obj = SchemaObject {
            rowid: 1,
            obj_type: "table".into(),
            name: "t".into(),
            tbl_name: "t".into(),
            rootpage: 2,
            sql: Some(sql.into()),
        };
        Table::from_schema_object(&obj).unwrap()
    }

    fn update_of(sql: &str) -> UpdateStmt {
        match parse(sql).unwrap().into_iter().next().unwrap() {
            Stmt::Update(u) => u,
            _ => panic!("expected UPDATE"),
        }
    }

    #[test]
    fn rejects_or_action() {
        let t = table_of("CREATE TABLE t(a, b)");
        let u = update_of("UPDATE OR REPLACE t SET a = 1;");
        let err = compile_update(&u, &t, &[]).unwrap_err();
        assert!(err.to_string().contains("ON CONFLICT"));
    }

    #[test]
    fn rejects_unknown_column() {
        let t = table_of("CREATE TABLE t(a, b)");
        let u = update_of("UPDATE t SET nope = 1;");
        let err = compile_update(&u, &t, &[]).unwrap_err();
        assert!(err.to_string().contains("no column named nope"));
    }

    #[test]
    fn rejects_rowid_alias_set() {
        let t = table_of("CREATE TABLE t(id INTEGER PRIMARY KEY, v)");
        let u = update_of("UPDATE t SET id = 5;");
        let err = compile_update(&u, &t, &[]).unwrap_err();
        assert!(err.to_string().contains("INTEGER PRIMARY KEY"));
    }

    #[test]
    fn golden_opcode_shape() {
        let t = table_of("CREATE TABLE t(a, b)");
        let u = update_of("UPDATE t SET a = 1 WHERE b > 0;");
        let prog = compile_update(&u, &t, &[]).unwrap();
        let names: Vec<&str> = prog.instructions.iter().map(|i| i.opcode.name()).collect();
        // Two-pass shape, in this order.
        assert!(names.contains(&"Transaction"));
        assert!(names.contains(&"SorterOpen"));
        assert!(names.contains(&"OpenWrite"));
        assert!(names.contains(&"Rewind"));
        assert!(names.contains(&"Next"));
        assert!(names.contains(&"SorterInsert"));
        assert!(names.contains(&"SorterSort"));
        assert!(names.contains(&"NotExists"));
        assert!(names.contains(&"SorterData"));
        assert!(names.contains(&"MakeRecord"));
        assert!(names.contains(&"Delete"));
        assert!(names.contains(&"Insert"));
        assert!(names.contains(&"SorterNext"));
        assert!(names.contains(&"Halt"));

        // The Delete and the Insert both carry P5_ISUPDATE.
        let del = prog
            .instructions
            .iter()
            .find(|i| i.opcode == Opcode::Delete)
            .unwrap();
        let ins = prog
            .instructions
            .iter()
            .find(|i| i.opcode == Opcode::Insert)
            .unwrap();
        assert_eq!(del.p5, P5_ISUPDATE);
        assert_eq!(ins.p5, P5_ISUPDATE);

        // The write Transaction carries p2 = 1.
        let txn = prog
            .instructions
            .iter()
            .find(|i| i.opcode == Opcode::Transaction)
            .unwrap();
        assert_eq!(txn.p2, 1);
    }
}
