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
//! "ON CONFLICT <action> is not yet supported" message. M19.7 adds support for updating the
//! rowid-alias column (`SET <IPK col> = …`): the row is deleted and re-inserted at the new
//! rowid, with INTEGER-affinity coercion (`MustBeInt`) and a uniqueness pre-check on the new
//! rowid. NOT NULL enforcement is not yet modeled (the INSERT path has the same gap); it
//! arrives with the constraint-checks slice.

use rustqlite_parser::{Assignment, Expr, JoinOp, TableOrJoin, UpdateStmt};

use crate::codegen::builder::Label;
use crate::codegen::returning::Returning;

use crate::error::{Error, Result};
use crate::schema::{IndexObject, Table};
use crate::types::Affinity;
use crate::vdbe::oe::OeAction;
use crate::vdbe::program::{Program, P4, P5_ISUPDATE, P5_UNIQUE};
use crate::vdbe::{KeyField, Opcode};

use super::builder::ProgramBuilder;
use super::expr::{compile_expr, compile_jump, Ctx, JoinTable};

/// A resolved table attached to an `UPDATE ... FROM` clause: the table object, the name used
/// to qualify its columns (alias if present, else the table name), and its indexes. The
/// `UPDATE` target table is NOT in this list — it is passed as the `table` argument to
/// [`compile_update`].
#[derive(Clone, Copy)]
pub struct FromTable<'a> {
    pub table: &'a Table,
    pub name: &'a str,
    pub indexes: &'a [IndexObject],
}

/// Compile `UPDATE [OR action] tbl SET col = expr [, …] [WHERE expr] [FROM from_clause]`.
/// `indexes` is the list of indexes attached to `table`; the codegen emits per-row
/// `IdxDelete` + `IdxInsert` maintenance for each index, now including multi-column composite
/// keys (M5.2). `from_tables` carries the resolved tables of the optional `FROM` clause
/// (M19.3); empty means no `FROM` clause and the original two-pass shape is used.
pub fn compile_update(
    upd: &UpdateStmt,
    table: &Table,
    indexes: &[IndexObject],
    from_tables: &[FromTable<'_>],
) -> Result<Program> {
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
    let oe = OeAction::from_parser(upd.or_action);
    // M19.6: OR IGNORE / OR REPLACE on UPDATE is supported via per-row conflict pre-checks
    // (NoConflict probe per unique index) emitted before the OLD-key IdxDelete + table
    // Delete + Insert. IGNORE skips the row on conflict; REPLACE fetches the conflicting
    // row's rowid via IdxRowid, deletes its index entries + table row, then falls through to
    // the normal Delete/Insert of the current row. ABORT/FAIL/ROLLBACK halt before any
    // writes (mirrors `sqlite3GenerateConstraintChecks`).
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

    // An `UPDATE ... FROM` clause present but no resolved FROM tables supplied is a caller
    // bug; a non-empty `from_tables` without a `FROM` clause on the statement is also invalid.
    if !upd.from.is_empty() && from_tables.is_empty() {
        return Err(Error::msg(
            "UPDATE ... FROM present but no FROM tables were resolved by the caller",
        ));
    }
    if upd.from.is_empty() && !from_tables.is_empty() {
        return Err(Error::msg(
            "from_tables supplied but the UPDATE statement has no FROM clause",
        ));
    }

    // Route to the FROM-clause variant when present. M19.3 first slice: a comma/cross/inner
    // join of plain tables (no subqueries, no outer joins — SQLite forbids OUTER JOIN in
    // UPDATE FROM anyway).
    if !from_tables.is_empty() {
        return compile_update_from(upd, table, indexes, from_tables, oe);
    }

    // Resolve the assignments: for each, the table column index and the value expression.
    // Last-write-wins on duplicate columns (matches upstream `sqlite3Update`).
    let ncol = table.columns.len();
    let mut target_col: Vec<Option<(usize, &Expr)>> = vec![None; ncol];
    for Assignment { column, value } in &upd.assignments {
        let ci = table.column_index(column).ok_or_else(|| {
            Error::msg(format!("table {} has no column named {column}", table.name))
        })?;
        target_col[ci] = Some((ci, value));
    }

    // M19.7: detect `UPDATE ... SET <ipk-col> = <expr>` — the rowid-alias column is being
    // changed. Upstream calls this `chngRowid`; the row must be deleted and re-inserted
    // at the new rowid (it may move within the b-tree). The SET expression is evaluated
    // with INTEGER affinity and `MustBeInt` coercion (a non-integer/NULL value raises
    // SQLITE_MISMATCH, matching the oracle — `UPDATE t SET id = NULL` is an error, unlike
    // `INSERT` where NULL auto-assigns). The rowid-alias slot in the stored record is set
    // to NULL (the rowid is carried in a separate register, not in the record). Uniqueness
    // of the new rowid is enforced by a `NotExists` pre-check against the table cursor.
    let chng_rowid = target_col.iter().enumerate().any(|(ci, slot)| {
        slot.is_some() && table.rowid_alias == Some(ci)
    });
    let rowid_alias_idx = table.rowid_alias;

    // ORDER BY without LIMIT is an error (mirrors upstream).
    if !upd.order_by.is_empty() && upd.limit.is_none() {
        return Err(Error::msg("ORDER BY without LIMIT on UPDATE"));
    }

    let cursor = 0i32;
    let sorter = 1i32;
    let ctx = Ctx { table, cursor, register_base: None, join_tables: None, index_read: None, subquery_resolver: None };
    let mut b = ProgramBuilder::new();
    b.set_default_oe(oe as u8);

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

    // (2) Open the rowid-set sorter. When ORDER BY is present, the sorter key is
    // [order_by_values..., rowid]; otherwise it's just [rowid]. The sorter orders
    // by the ORDER BY columns; the rowid is the trailing payload used to re-seek.
    let _n_order = upd.order_by.len() as i32;
    let sorter_fields: Vec<crate::vdbe::KeyField> = if upd.order_by.is_empty() {
        vec![crate::vdbe::KeyField::asc_binary()]
    } else {
        upd.order_by
            .iter()
            .map(|ot| crate::vdbe::KeyField {
                desc: ot.desc,
                collation: crate::types::Collation::Binary,
            })
            .collect()
    };
    let n_sorter_keys = sorter_fields.len() as i32;
    let so = b.emit(Opcode::SorterOpen, sorter, n_sorter_keys, 0);
    b.set_p4(so, P4::KeyInfo(sorter_fields));

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
    b.emit(Opcode::Rowid, cursor, reg_old_rowid, 0);
    // Build the sorter record: [order_by_values..., rowid].
    let rec_start = b.alloc_regs(n_sorter_keys + 1);
    for (j, ot) in upd.order_by.iter().enumerate() {
        compile_expr(&mut b, &ot.expr, rec_start + j as i32, ctx)?;
    }
    if upd.order_by.is_empty() {
        // No ORDER BY: single dummy key field (preserves insertion order).
        b.emit(Opcode::Integer, 0, rec_start, 0);
    }
    b.emit(Opcode::SCopy, reg_old_rowid, rec_start + n_sorter_keys, 0);
    let reg_rowid_rec = b.alloc_reg();
    b.emit(Opcode::MakeRecord, rec_start, n_sorter_keys + 1, reg_rowid_rec);
    b.emit(Opcode::SorterInsert, sorter, reg_rowid_rec, 0);

    b.resolve(scan_next);
    b.emit_jump(Opcode::Next, cursor, scan_top, 0);
    b.resolve(end_scan);

    // (5) Second pass: iterate the sorter, re-seek each rowid, build the new record,
    // delete + re-insert. `changes()` and `total_changes()` count once per row updated
    // because the Delete carries `P5_ISUPDATE` (suppresses its own counter; the Insert
    // bumps once). `last_insert_rowid()` is left untouched (matches upstream).

    // Initialize OFFSET and LIMIT counters BEFORE the sort loop so they persist
    // across iterations.
    let offset_reg = b.alloc_reg();
    if let Some(offset_expr) = &upd.offset {
        compile_expr(&mut b, offset_expr, offset_reg, ctx)?;
    } else {
        b.emit(Opcode::Integer, 0, offset_reg, 0);
    }
    let limit_reg = b.alloc_reg();
    if let Some(limit_expr) = &upd.limit {
        compile_expr(&mut b, limit_expr, limit_reg, ctx)?;
    } else {
        b.emit(Opcode::Integer, -1, limit_reg, 0);
    }

    let end_update = b.new_label();
    b.emit_jump(Opcode::SorterSort, sorter, end_update, 0);
    let update_top = b.new_label();
    let sort_next = b.new_label();
    b.resolve(update_top);

    b.emit(Opcode::SorterData, sorter, 0, 0);

    // OFFSET: skip the first OFFSET rows.
    if upd.offset.is_some() {
        let skip_offset = b.new_label();
        let after_offset = b.new_label();
        b.emit_jump(Opcode::IfPos, offset_reg, skip_offset, 1);
        b.emit_jump(Opcode::Goto, 0, after_offset, 0);
        b.resolve(skip_offset);
        b.emit_jump(Opcode::SorterNext, sorter, update_top, 0);
        b.emit_jump(Opcode::Goto, 0, end_update, 0);
        b.resolve(after_offset);
    }

    // LIMIT: stop after LIMIT rows.
    if upd.limit.is_some() {
        let do_update = b.new_label();
        b.emit_jump(Opcode::IfPos, limit_reg, do_update, 1);
        b.emit_jump(Opcode::Goto, 0, end_update, 0);
        b.resolve(do_update);
    }

    // Pull the captured rowid back out of the sorter record (at column n_sorter_keys).
    let reg_old_rowid2 = b.alloc_reg();
    b.emit(Opcode::Column, sorter, n_sorter_keys, reg_old_rowid2);
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

    // M19.7: when the rowid-alias column is being updated, the new rowid is staged in this
    // register (evaluated from the SET expression with INTEGER affinity + MustBeInt). When
    // the rowid is not being changed, this stays None and the old rowid register is used.
    let mut reg_new_rowid_opt: Option<i32> = None;

    for (ci, slot) in target_col.iter().enumerate() {
        if let Some((_, value)) = slot {
            // When the rowid-alias column is being updated, evaluate the SET expression into
            // a dedicated rowid register (with INTEGER affinity + `MustBeInt` coercion) rather
            // than into the record slot. The record slot is set to NULL below (the rowid is
            // carried in the register, not in the record — same shape as INSERT).
            if chng_rowid && Some(ci) == rowid_alias_idx {
                reg_new_rowid_opt = Some(b.alloc_reg());
                let rrid = reg_new_rowid_opt.unwrap();
                compile_expr(&mut b, value, rrid, ctx)?;
                // INTEGER affinity + MustBeInt: a non-integer-coercible value (including a
                // non-numeric string or a real with a fractional part) raises MISMATCH;
                // a NULL value also raises MISMATCH (MustBeInt leaves NULL but the subsequent
                // NotNull check rejects it — `UPDATE t SET id = NULL` is an error, unlike
                // INSERT where NULL auto-assigns). Mirrors upstream's `OP_MustBeInt` after
                // `sqlite3ExprCode(pParse, pRowidExpr, regNewRowid)`.
                let aff_idx = b.emit(Opcode::Affinity, rrid, 1, 0);
                b.set_p4(aff_idx, P4::Symbol("D".into()));
                b.emit(Opcode::MustBeInt, rrid, 0, 0);
                continue;
            }
            compile_expr(&mut b, value, reg_new + ci as i32, ctx)?;
        }
    }

    // When the rowid is being changed, the record slot for the alias column is NULL (the
    // rowid is in `reg_new_rowid`). Set it after the SET loop so a later affinity pass does
    // not try to coerce it.
    if chng_rowid {
        if let Some(alias_idx) = rowid_alias_idx {
            b.emit(Opcode::Null, 0, reg_new + alias_idx as i32, 0);
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

    // (8b) M19.6: per-row conflict pre-checks for OR IGNORE / OR REPLACE. For each unique
    // index, probe `NoConflict` against the NEW key. On conflict: IGNORE jumps to sort_next
    // (skip the row); REPLACE fetches the conflicting row's rowid via IdxRowid, deletes its
    // index entries + table row, then falls through to the normal Delete/Insert. ABORT/FAIL/
    // ROLLBACK halt before any writes (the OLD-key IdxDelete pass below never runs for the
    // failing row). The pre-checks run BEFORE the OLD-key IdxDelete so a REPLACE that deletes
    // a different row's index entries doesn't race with the current row's own IdxDelete.
    // `skip_indexes` is empty — the UPDATE path has no UPSERT target concept.
    super::insert::emit_conflict_prechecks(
        &mut b,
        indexes,
        table,
        reg_new,
        reg_old_rowid2,
        index_cursor_base,
        cursor,
        oe,
        sort_next,
        &[],
    )?;

    // (8b-rowid) M19.7: when the rowid-alias column is being changed, the new rowid must be
    // unique. The table b-tree's rowid IS the IPK, so a duplicate is a UNIQUE constraint
    // violation on the IPK (`UNIQUE constraint failed: <tbl>.<ipk-col>`). Probe with
    // `NotExists`: if a row with the new rowid already exists (and it's not the current row,
    // which was already seeked above), handle per the OE. This mirrors upstream's IPK
    // uniqueness check via the implicit unique index on `INTEGER PRIMARY KEY`.
    let effective_rowid_reg = reg_new_rowid_opt.unwrap_or(reg_old_rowid2);
    if chng_rowid {
        let new_rowid = reg_new_rowid_opt.unwrap();
        // Skip the uniqueness check when the new rowid equals the old rowid (a self-assign
        // like `UPDATE t SET id = 1 WHERE v = 'a'` on a row whose id is already 1 is a no-op
        // for the rowid — the row doesn't move and there's no conflict). Compare the two
        // registers; if equal, jump past the uniqueness check.
        let skip_unique = b.new_label();
        // `Eq p1=new_rowid p3=reg_old_rowid2` jumps to p2 (skip_unique) when the two are equal.
        // The `Ne`/`Eq` opcodes compare `r[p3] OP r[p1]`; `Eq` jumps when `r[reg_old_rowid2] ==
        // r[new_rowid]`. When the new rowid equals the old, the row doesn't move and there's no
        // uniqueness conflict (the row's own rowid is the only one at that value).
        b.emit_jump(Opcode::Eq, new_rowid, skip_unique, reg_old_rowid2);
        // Seek the table cursor to the new rowid; if found, it's a conflict. The current
        // row (at `reg_old_rowid2`) was excluded by the equality check above, so a found
        // row is a different row.
        let no_conflict_label = b.new_label();
        b.emit_jump(Opcode::NotExists, cursor, no_conflict_label, new_rowid);
        // Fall-through: a row with the new rowid exists → conflict.
        match oe {
            OeAction::Ignore => {
                b.emit_jump(Opcode::Goto, 0, sort_next, 0);
            }
            OeAction::Replace => {
                // Delete the conflicting row (the one at the new rowid). It's already
                // seeked above; delete it and its index entries, then fall through.
                let conflict_row_start = b.alloc_regs(ncol as i32);
                for ci in 0..ncol {
                    b.emit(Opcode::Column, cursor, ci as i32, conflict_row_start + ci as i32);
                }
                for (i, idx) in indexes.iter().enumerate() {
                    let ic = index_cursor_base + i as i32;
                    let indexed_cis = idx.table_column_indices(table).expect("validated earlier");
                    let nkey = idx.nkey_fields() as i32 + 1;
                    let old_key = b.alloc_regs(nkey);
                    let mut plain_iter = indexed_cis.iter();
                    for (j, icol) in idx.columns.iter().enumerate() {
                        let target = old_key + j as i32;
                        if let Some(expr) = &icol.expr {
                            let expr_ctx = Ctx {
                                table,
                                cursor,
                                register_base: Some(conflict_row_start), join_tables: None,
                                index_read: None,
                                subquery_resolver: None,
                            };
                            compile_expr(&mut b, expr, target, expr_ctx)?;
                        } else {
                            let col_idx = *plain_iter.next().expect("plain column aligned");
                            b.emit(Opcode::SCopy, conflict_row_start + col_idx as i32, target, 0);
                        }
                    }
                    b.emit(Opcode::SCopy, new_rowid, old_key + idx.nkey_fields() as i32, 0);
                    b.emit(Opcode::IdxDelete, ic, old_key, nkey);
                }
                b.emit(Opcode::Delete, cursor, 0, 0);
                // Re-seek the cursor back to the current row (the one we're updating).
                b.emit_jump(Opcode::NotExists, cursor, sort_next, reg_old_rowid2);
            }
            _ => {
                // ABORT/FAIL/ROLLBACK: halt with the UNIQUE constraint message before any
                // writes for this row (the OLD-key IdxDelete + Delete below never run).
                let col_name = rowid_alias_idx
                    .map(|i| table.columns[i].name.as_str())
                    .unwrap_or("rowid");
                let msg = format!("{}.{}", table.name, col_name);
                let halt_idx = b.emit(
                    Opcode::Halt,
                    crate::error::ResultCode::Constraint as i32,
                    oe as i32,
                    0,
                );
                b.set_p4(halt_idx, P4::Text(msg));
                b.set_p5(halt_idx, 2); // UNIQUE constraint prefix
            }
        }
        b.resolve(no_conflict_label);
        b.resolve(skip_unique);
        // Re-seek the cursor back to the current row (NotExists above moved it to the new
        // rowid's position, or didn't move it if not found — but to be safe, re-seek).
        b.emit_jump(Opcode::NotExists, cursor, sort_next, reg_old_rowid2);
    }

    // (8c) After the REPLACE pre-check may have moved the table cursor to the conflicting
    // (now-deleted) row, re-seek it back to the current row so the OLD-key IdxDelete pass
    // and the table Delete below operate on the right row. A no-op when no REPLACE fired
    // (the cursor is already on the current row, and NotExists is a seek anyway). For
    // OE_None / Ignore / Abort / Fail / Rollback this is also a no-op (no cursor movement).
    b.emit_jump(Opcode::NotExists, cursor, sort_next, reg_old_rowid2);

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
            let pred_ctx = Ctx { table, cursor, register_base: None, join_tables: None, index_read: None, subquery_resolver: None };
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
                    register_base: Some(reg_old), join_tables: None,
                    index_read: None,
                    subquery_resolver: None,
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
    let ins_idx = b.emit(Opcode::Insert, cursor, reg_new_rec, effective_rowid_reg);
    b.set_p5(ins_idx, P5_ISUPDATE);

    if let Some(ref ret) = returning {
        // The rowid-alias slot in the stored record is NULL, but RETURNING needs the logical
        // column value (the rowid). Patch it into the staged register block before evaluating.
        if let Some(alias_idx) = table.rowid_alias {
            b.emit(Opcode::SCopy, effective_rowid_reg, reg_new + alias_idx as i32, 0);
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
            let pred_ctx = Ctx { table, cursor, register_base: None, join_tables: None, index_read: None, subquery_resolver: None };
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
                    register_base: Some(reg_new), join_tables: None,
                    index_read: None,
                    subquery_resolver: None,
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
            effective_rowid_reg,
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

/// Compile `UPDATE [OR action] tbl SET col = expr [, …] [WHERE expr] FROM from_clause` —
/// the SQLite-3.33+ `UPDATE ... FROM` form. Mirrors `updateFromSelect` in `update.c`:
/// conceptually `SELECT <rowid>, <set-exprs...> FROM tbl, from_tables WHERE <where>` is run
/// into a sorter, then for each sorter row the target table is seeked by rowid and the SET
/// values (read from the sorter columns) are applied.
///
/// This first slice supports the common shape: a comma / CROSS / INNER join of plain tables
/// (no subqueries, no OUTER JOIN — SQLite forbids OUTER JOIN in `UPDATE FROM` anyway). The
/// SET expressions and the WHERE clause may reference columns from any of the joined tables.
/// The target table's rowid is captured per matched row; the SET expressions are evaluated
/// against the joined row and staged in the sorter alongside the rowid, so the second pass
/// does not need to re-evaluate them (mirrors upstream's `nChangeFrom > 0` path where the
/// change expressions are read back from the ephemeral columns).
///
/// `from_tables` is in declared FROM order. The target table is the outer (left) loop; each
/// FROM table is an inner loop. No join-order selection is performed (matches the simplest
/// path; the planner-based reorder lands with M22).
fn compile_update_from(
    upd: &UpdateStmt,
    table: &Table,
    indexes: &[IndexObject],
    from_tables: &[FromTable<'_>],
    oe: OeAction,
) -> Result<Program> {
    // Validate the FROM clause shape: only plain-table cross/inner joins. A subquery in the
    // UPDATE FROM would need the M8.6 subquery materialization infrastructure threaded here;
    // OUTER JOINs are rejected by SQLite itself in this position.
    for entry in &upd.from {
        match entry {
            TableOrJoin::Table(_) => {}
            TableOrJoin::Subquery { .. } => {
                return Err(Error::msg(
                    "subquery in UPDATE ... FROM is not yet supported (only plain tables)",
                ));
            }
            TableOrJoin::Join(j) => {
                match j.op {
                    JoinOp::Cross | JoinOp::Inner | JoinOp::Natural => {}
                    _ => {
                        return Err(Error::msg(
                            "OUTER JOIN in UPDATE ... FROM is not allowed (use a subquery)",
                        ));
                    }
                }
                // The nested-left side must also be plain-table only.
                if !matches!(&*j.left, TableOrJoin::Table(_)) {
                    return Err(Error::msg(
                        "nested subquery/join in UPDATE ... FROM is not yet supported",
                    ));
                }
            }
        }
    }
    if from_tables.len() > 8 {
        return Err(Error::msg("UPDATE ... FROM supports at most 8 joined tables in this slice"));
    }

    // Reject ORDER BY / LIMIT / OFFSET on UPDATE ... FROM for the first slice. SQLite does
    // support these (when compiled with SQLITE_ENABLE_UPDATE_DELETE_LIMIT), but the
    // row-set + nested-loop shape here doesn't carry the rowid through the order cleanly.
    // The plain UPDATE path keeps ORDER BY / LIMIT / OFFSET support.
    if !upd.order_by.is_empty() || upd.limit.is_some() || upd.offset.is_some() {
        return Err(Error::msg(
            "ORDER BY / LIMIT / OFFSET on UPDATE ... FROM is not yet supported",
        ));
    }

    let ncol = table.columns.len();

    // Resolve assignments: column-index -> SET value expression.
    let mut target_col: Vec<Option<(usize, &Expr)>> = vec![None; ncol];
    for Assignment { column, value } in &upd.assignments {
        let ci = table.column_index(column).ok_or_else(|| {
            Error::msg(format!("table {} has no column named {column}", table.name))
        })?;
        target_col[ci] = Some((ci, value));
    }
    // Updating the rowid-alias column is rejected (same guard as the plain UPDATE path).
    for (ci, slot) in target_col.iter().enumerate() {
        if slot.is_some() && table.rowid_alias == Some(ci) {
            return Err(Error::msg(format!(
                "UPDATE of the INTEGER PRIMARY KEY column is not yet supported (table {}, column {})",
                table.name, table.columns[ci].name
            )));
        }
    }

    // The SET expressions staged in the sorter, in column order. The sorter record layout is:
    //   [rowid, set_value_for_lowest_assigned_col, set_value_for_next_assigned_col, ...]
    // We collect the (col_idx, value_expr) pairs in column order so the second pass can read
    // them back at known offsets.
    let mut set_pairs: Vec<(usize, &Expr)> = Vec::new();
    for (ci, slot) in target_col.iter().enumerate() {
        if let Some((_, value)) = slot {
            set_pairs.push((ci, *value));
        }
    }
    let n_set = set_pairs.len() as i32;

    let mut b = ProgramBuilder::new();
    b.set_default_oe(oe as u8);

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

    // (2) Sorter holds [rowid, set_value_1, ..., set_value_n]. Single key (rowid asc).
    let sorter = 1i32;
    let sorter_fields = vec![KeyField::asc_binary()];
    let so = b.emit(Opcode::SorterOpen, sorter, 1, 0);
    b.set_p4(so, P4::KeyInfo(sorter_fields));

    // Cursor layout:
    //   0           -> target table (write)
    //   2..(2+nidx) -> target table indexes (write)
    //   20..        -> FROM tables (read), high numbers to avoid collisions.
    let target_cursor = 0i32;
    let index_cursor_base = 2i32;
    let from_cursor_base = 20i32;

    // RETURNING ephemeral at an even higher number.
    let returning_cursor = 40i32;
    let mut returning = returning;
    if let Some(ref mut ret) = returning {
        ret.emit_open(&mut b, returning_cursor);
    }

    // (3) Open the target table for write.
    let open = b.emit(Opcode::OpenWrite, target_cursor, table.rootpage as i32, 0);
    b.set_p4(open, P4::Int(ncol as i64));

    // (3b) Open the target's indexes for write (IdxDelete/IdxInsert maintenance).
    for (i, idx) in indexes.iter().enumerate() {
        let _ = idx.table_column_indices(table)?;
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

    // (3c) Open each FROM table for read.
    for (i, ft) in from_tables.iter().enumerate() {
        let fc = from_cursor_base + i as i32;
        let open = b.emit(Opcode::OpenRead, fc, ft.table.rootpage as i32, 0);
        if ft.table.without_rowid {
            b.set_p4(open, P4::KeyInfo(ft.table.without_rowid_key_info()));
        } else {
            b.set_p4(open, P4::Int(ft.table.columns.len() as i64));
        }
        b.note_cursor(fc);
    }

    // (4) First pass: nested loop. Target table is the outer (left) loop; each FROM table is
    //     an inner loop. For each row combination that passes the WHERE filter, capture the
    //     target rowid + each SET expression value into the sorter.
    let join_tables: Vec<JoinTable<'_>> = {
        let mut v: Vec<JoinTable<'_>> = Vec::with_capacity(1 + from_tables.len());
        v.push(JoinTable {
            table,
            cursor: target_cursor,
            name: &upd.table,
        });
        for (i, ft) in from_tables.iter().enumerate() {
            v.push(JoinTable {
                table: ft.table,
                cursor: from_cursor_base + i as i32,
                name: ft.name,
            });
        }
        v
    };
    let jt_slice: &[JoinTable<'_>] = &join_tables;
    let scan_ctx = Ctx {
        table,
        cursor: target_cursor,
        register_base: None,
        index_read: None,
        join_tables: Some(jt_slice),
        subquery_resolver: None,
    };

    // ON predicates per join level (only INNER/CROSS have them; comma joins have none).
    // `flatten_cross_join` walks ONLY the FROM clause (not the target table), so its entries
    // map 1:1 to `from_tables` in declared order. Each entry's constraint is the ON predicate
    // (None for a comma join).
    let flat = super::join::flatten_cross_join(&upd.from)
        .ok_or_else(|| Error::msg("UPDATE ... FROM expects a plain table list"))?;
    if flat.len() != from_tables.len() {
        return Err(Error::msg(
            "UPDATE ... FROM FROM-clause shape mismatch (subqueries or nested joins not supported)",
        ));
    }
    let on_preds: Vec<Option<&Expr>> = flat
        .iter()
        .map(|(_, c)| super::join::on_predicate(*c))
        .collect();

    // Outermost loop: target table.
    let end_scan = b.new_label();
    b.emit_jump(Opcode::Rewind, target_cursor, end_scan, 0);
    let target_loop = b.new_label();
    b.resolve(target_loop);

    // Inner loops: each FROM table. Build them as a ladder of Rewind/Next pairs. The
    // innermost body runs the WHERE filter, captures the rowid, evaluates SET expressions,
    // and inserts into the sorter.
    //
    // Nested loop layout for `from_tables = [a, b, c]`:
    //   target_loop:
    //     Rewind a, end_target_iter      ; on_empty = end_target_iter (skip everything)
    //     from_loops[0]:
    //       ON[0]? jump from_nexts[0]
    //       Rewind b, from_nexts[0]      ; on_empty = from_nexts[i-1] = from_nexts[0]
    //       from_loops[1]:
    //         ON[1]? jump from_nexts[1]
    //         Rewind c, from_nexts[1]    ; on_empty = from_nexts[i-1] = from_nexts[1]
    //         from_loops[2]:
    //           ON[2]? jump from_nexts[2]
    //           WHERE? jump from_nexts[2]
    //           <body: capture rowid + SET values, SorterInsert>
    //         from_nexts[2]:
    //           Next c, from_loops[2]
    //       from_nexts[1]:
    //         Next b, from_loops[1]
    //     from_nexts[0]:
    //       Next a, from_loops[0]
    //   end_target_iter:
    //     Next target, target_loop
    //   end_scan:
    //
    // For a single from table [a]: on_empty of Rewind a = end_target_iter.
    // For zero from tables: there are no inner loops; the body runs directly under target_loop,
    //   and the WHERE-false jump goes to end_target_iter (so the target's Next runs).
    let n_from = from_tables.len();
    let from_loops: Vec<Label> = (0..n_from).map(|_| b.new_label()).collect();
    let from_nexts: Vec<Label> = (0..n_from).map(|_| b.new_label()).collect();
    let end_target_iter = b.new_label();

    // The label that the WHERE-false jump targets: the innermost from_next (continue the
    // innermost loop), or end_target_iter when there are no FROM tables.
    let body_skip = if n_from == 0 {
        end_target_iter
    } else {
        from_nexts[n_from - 1]
    };

    // Emit the Rewind + ON predicate for each FROM table in order.
    for i in 0..n_from {
        let fc = from_cursor_base + i as i32;
        let on_empty = if i == 0 {
            end_target_iter
        } else {
            from_nexts[i - 1]
        };
        b.emit_jump(Opcode::Rewind, fc, on_empty, 0);
        b.resolve(from_loops[i]);
        if let Some(on) = on_preds.get(i).copied().flatten() {
            compile_jump(&mut b, on, from_nexts[i], false, true, scan_ctx)?;
        }
    }

    // Innermost body: WHERE filter, then capture rowid + SET values into the sorter.
    if let Some(w) = &upd.where_clause {
        compile_jump(&mut b, w, body_skip, false, true, scan_ctx)?;
    }

    let reg_rowid = b.alloc_reg();
    b.emit(Opcode::Rowid, target_cursor, reg_rowid, 0);

    let rec_start = b.alloc_regs(1 + n_set);
    b.emit(Opcode::SCopy, reg_rowid, rec_start, 0);
    for (k, (_, value)) in set_pairs.iter().enumerate() {
        compile_expr(&mut b, value, rec_start + 1 + k as i32, scan_ctx)?;
    }
    let reg_rec = b.alloc_reg();
    b.emit(Opcode::MakeRecord, rec_start, 1 + n_set, reg_rec);
    b.emit(Opcode::SorterInsert, sorter, reg_rec, 0);

    // Close out the inner loops in reverse order (innermost first).
    for i in (0..n_from).rev() {
        b.resolve(from_nexts[i]);
        let fc = from_cursor_base + i as i32;
        b.emit_jump(Opcode::Next, fc, from_loops[i], 0);
    }
    b.resolve(end_target_iter);
    b.emit_jump(Opcode::Next, target_cursor, target_loop, 0);
    b.resolve(end_scan);

    // (5) Second pass: iterate the sorter, re-seek the target by rowid, build the new record
    //     from the existing row + staged SET values, do delete + insert + index maintenance.
    let end_update = b.new_label();
    b.emit_jump(Opcode::SorterSort, sorter, end_update, 0);
    let update_top = b.new_label();
    let sort_next = b.new_label();
    b.resolve(update_top);

    b.emit(Opcode::SorterData, sorter, 0, 0);

    // Pull the captured rowid back out of the sorter record (column 0).
    let reg_old_rowid = b.alloc_reg();
    b.emit(Opcode::Column, sorter, 0, reg_old_rowid);
    b.emit_jump(Opcode::NotExists, target_cursor, sort_next, reg_old_rowid);

    // Build the new record: copy each table column from the current row, then override the
    // assigned columns with the staged SET values read from the sorter columns 1..1+n_set.
    let reg_new = b.alloc_regs(ncol as i32);
    for ci in 0..ncol {
        b.emit(Opcode::Column, target_cursor, ci as i32, reg_new + ci as i32);
    }
    for ci in 0..ncol {
        if table.columns[ci].affinity == Affinity::Real {
            b.emit(Opcode::RealAffinity, reg_new + ci as i32, 0, 0);
        }
    }

    // Snapshot the OLD row for index key computation (same as the plain UPDATE path).
    let reg_old = b.alloc_regs(ncol as i32);
    for ci in 0..ncol {
        b.emit(Opcode::SCopy, reg_new + ci as i32, reg_old + ci as i32, 0);
    }
    if table.rowid_alias.is_none() {
        let _placeholder = b.alloc_reg();
    }

    // Override assigned columns from the sorter. The SET values sit at sorter columns 1..1+n_set
    // in the same order as `set_pairs`.
    for (k, (ci, _)) in set_pairs.iter().enumerate() {
        b.emit(Opcode::Column, sorter, 1 + k as i32, reg_new + *ci as i32);
    }

    // Apply column affinities.
    let mut aff_string = String::with_capacity(ncol);
    for col in &table.columns {
        aff_string.push(affinity_char(col.affinity) as char);
    }
    if !aff_string.is_empty() {
        let idx = b.emit(Opcode::Affinity, reg_new, ncol as i32, 0);
        b.set_p4(idx, P4::Symbol(aff_string));
    }

    let reg_new_rec = b.alloc_reg();
    b.emit(Opcode::MakeRecord, reg_new, ncol as i32, reg_new_rec);

    // M19.6: per-row conflict pre-checks for OR IGNORE / OR REPLACE (same shape as the plain
    // UPDATE path). Runs BEFORE the OLD-key IdxDelete pass; on REPLACE conflict, the
    // conflicting (different) row is deleted, then the current row's normal Delete/Insert
    // proceeds.
    super::insert::emit_conflict_prechecks(
        &mut b,
        indexes,
        table,
        reg_new,
        reg_old_rowid,
        index_cursor_base,
        target_cursor,
        oe,
        sort_next,
        &[],
    )?;

    // After a REPLACE pre-check may have moved the target cursor to the conflicting
    // (now-deleted) row, re-seek it to the current row. No-op when no REPLACE fired.
    b.emit_jump(Opcode::NotExists, target_cursor, sort_next, reg_old_rowid);

    // Index maintenance: OLD-key IdxDelete, table Delete+Insert, NEW-key IdxInsert. This
    // mirrors the plain UPDATE path; the OLD keys are evaluated against `reg_old` and the
    // NEW keys against `reg_new`. Partial-index predicates are supported the same way.
    for (i, idx) in indexes.iter().enumerate() {
        let ic = index_cursor_base + i as i32;
        let indexed_cis = idx.table_column_indices(table).expect("validated earlier");
        let nkey = idx.nkey_fields() as i32 + 1;

        let skip_delete_label = if let Some(pred) = &idx.where_clause {
            validate_partial_pred_on_update(pred, table, &target_col)?;
            let skip = b.new_label();
            let pred_ctx = Ctx { table, cursor: target_cursor, register_base: None, join_tables: None, index_read: None, subquery_resolver: None };
            compile_pred_jump(&mut b, pred, skip, table, reg_new, indexed_cis.as_slice(), pred_ctx)?;
            Some(skip)
        } else {
            None
        };

        let old_key = b.alloc_regs(nkey);
        for (j, icol) in idx.columns.iter().enumerate() {
            let target = old_key + j as i32;
            if let Some(expr) = &icol.expr {
                let expr_ctx = Ctx {
                    table,
                    cursor: target_cursor,
                    register_base: Some(reg_old), join_tables: None,
                    index_read: None,
                    subquery_resolver: None,
                };
                compile_expr(&mut b, expr, target, expr_ctx)?;
            } else {
                let col_idx = table
                    .column_index(&icol.name)
                    .expect("validated earlier");
                b.emit(Opcode::SCopy, reg_old + col_idx as i32, target, 0);
            }
        }
        b.emit(Opcode::SCopy, reg_old_rowid, old_key + idx.nkey_fields() as i32, 0);
        b.emit(Opcode::IdxDelete, ic, old_key, nkey);

        if let Some(skip) = skip_delete_label {
            b.resolve(skip);
        }
    }

    let del_idx = b.emit(Opcode::Delete, target_cursor, 0, 0);
    b.set_p5(del_idx, P5_ISUPDATE);
    let ins_idx = b.emit(Opcode::Insert, target_cursor, reg_new_rec, reg_old_rowid);
    b.set_p5(ins_idx, P5_ISUPDATE);

    if let Some(ref ret) = returning {
        if let Some(alias_idx) = table.rowid_alias {
            b.emit(Opcode::SCopy, reg_old_rowid, reg_new + alias_idx as i32, 0);
        }
        ret.emit_buffer_row(&mut b, table, target_cursor, reg_new)?;
    }

    for (i, idx) in indexes.iter().enumerate() {
        let ic = index_cursor_base + i as i32;
        let indexed_cis = idx.table_column_indices(table).expect("validated earlier");
        let nkey = idx.nkey_fields() as i32 + 1;

        let skip_insert_label = if let Some(pred) = &idx.where_clause {
            validate_partial_pred_on_update(pred, table, &target_col)?;
            let skip = b.new_label();
            let pred_ctx = Ctx { table, cursor: target_cursor, register_base: None, join_tables: None, index_read: None, subquery_resolver: None };
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
                let expr_ctx = Ctx {
                    table,
                    cursor: target_cursor,
                    register_base: Some(reg_new), join_tables: None,
                    index_read: None,
                    subquery_resolver: None,
                };
                compile_expr(&mut b, expr, target, expr_ctx)?;
            } else {
                let col_idx = *plain_iter.next().expect("plain column aligned with indexed_cis");
                b.emit(Opcode::SCopy, reg_new + col_idx as i32, target, 0);
            }
        }
        b.emit(Opcode::SCopy, reg_old_rowid, new_key + idx.nkey_fields() as i32, 0);
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
    fn accepts_or_ignore_or_replace() {
        // M19.6: OR IGNORE / OR REPLACE on UPDATE are now supported via per-row conflict
        // pre-checks (NoConflict probe per unique index). The codegen accepts them and sets
        // the program's default_oe so step() does the right cleanup on a non-IGNORE/REPLACE
        // constraint violation (e.g. a NOT NULL failure under ABORT/FAIL/ROLLBACK).
        let t = table_of("CREATE TABLE t(a, b)");
        let u = update_of("UPDATE OR REPLACE t SET a = 1;");
        let prog = compile_update(&u, &t, &[], &[]).unwrap();
        assert_eq!(prog.default_oe, OeAction::Replace as u8);
        let u = update_of("UPDATE OR IGNORE t SET a = 1;");
        let prog = compile_update(&u, &t, &[], &[]).unwrap();
        assert_eq!(prog.default_oe, OeAction::Ignore as u8);
    }

    #[test]
    fn accepts_or_rollback_abort_fail() {
        let t = table_of("CREATE TABLE t(a, b)");
        let u = update_of("UPDATE OR ROLLBACK t SET a = 1;");
        let prog = compile_update(&u, &t, &[], &[]).unwrap();
        assert_eq!(prog.default_oe, OeAction::Rollback as u8);
        let u = update_of("UPDATE OR FAIL t SET a = 1;");
        let prog = compile_update(&u, &t, &[], &[]).unwrap();
        assert_eq!(prog.default_oe, OeAction::Fail as u8);
        let u = update_of("UPDATE OR ABORT t SET a = 1;");
        let prog = compile_update(&u, &t, &[], &[]).unwrap();
        assert_eq!(prog.default_oe, OeAction::Abort as u8);
    }

    #[test]
    fn rejects_unknown_column() {
        let t = table_of("CREATE TABLE t(a, b)");
        let u = update_of("UPDATE t SET nope = 1;");
        let err = compile_update(&u, &t, &[], &[]).unwrap_err();
        assert!(err.to_string().contains("no column named nope"));
    }

    #[test]
    fn accepts_rowid_alias_set() {
        // M19.7: UPDATE of the INTEGER PRIMARY KEY column is now supported (the row is
        // deleted and re-inserted at the new rowid). The codegen accepts it and compiles a
        // program that evaluates the SET expression with INTEGER affinity + MustBeInt.
        let t = table_of("CREATE TABLE t(id INTEGER PRIMARY KEY, v)");
        let u = update_of("UPDATE t SET id = 5;");
        let prog = compile_update(&u, &t, &[], &[]).unwrap();
        let names: Vec<&str> = prog.instructions.iter().map(|i| i.opcode.name()).collect();
        assert!(names.contains(&"MustBeInt"), "MustBeInt emitted for IPK SET");
    }

    #[test]
    fn golden_opcode_shape() {
        let t = table_of("CREATE TABLE t(a, b)");
        let u = update_of("UPDATE t SET a = 1 WHERE b > 0;");
        let prog = compile_update(&u, &t, &[], &[]).unwrap();
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
