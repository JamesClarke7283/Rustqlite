//! Lowering `INSERT ... VALUES` to a VDBE program (mirrors `sqlite3Insert` in `insert.c`).
//!
//! The faithful opcode shape per row is: evaluate each column's value into a contiguous register
//! block (applying the table's column affinities), pick the rowid (an explicit `INTEGER PRIMARY
//! KEY` value becomes the rowid; otherwise `NewRowid` allocates max+1), `MakeRecord` the row, and
//! `Insert` it. The whole statement runs inside one write `Transaction`; `Halt` commits.
//!
//! First-slice scope: `VALUES` rows of literal/constant expressions, the rowid alias rule, and an
//! optional explicit column list. `INSERT ... SELECT`, `DEFAULT VALUES`, `UPSERT`, and conflict
//! resolution beyond the default ABORT are out of scope.
//!
//! M5.1: when the prepare path passes a non-empty `indexes` list, the program also emits one
//! `OpenWrite` + `IdxInsert` pair per index per row, keeping the index b-trees in sync with the
//! table. The index-key record is built from the table's record registers (a `Copy` of each
//! indexed column value followed by an `SCopy` of the rowid), then `MakeRecord`-ed. M5.2
//! generalizes this to multi-column indexes.

use rustqlite_parser::{Expr, InsertSource, InsertStmt, SelectStmt, UpsertAction};

use crate::codegen::returning::Returning;
use crate::codegen::select;
use crate::codegen::update::compile_pred_jump;
use crate::codegen::upsert;
use crate::error::{Error, Result};
use crate::schema::{IndexObject, Table};
use crate::types::Affinity;
use crate::vdbe::oe::OeAction;
use crate::vdbe::program::{FkCheckP4, Program, P4, P5_NCHANGE, P5_UNIQUE};
use crate::vdbe::Opcode;

use super::builder::{Label, ProgramBuilder};
use super::expr::{compile_expr, Ctx};

/// Compile an `INSERT INTO <table>` statement. `indexes` is the list of indexes attached to
/// `table` (the prepare path passes this from the catalog; an empty slice means "no indexes",
/// matching the M3a behavior). Index maintenance is emitted per row per index.
///
/// For `INSERT ... SELECT`, `source_table` is the resolved source table (or `None` for a
/// constant / `VALUES` source), and `source_indexes` are its indexes; these are passed to the
/// SELECT compiler so column references resolve and indexed lookups work.
///
/// `fk_checks` carries the resolved FK constraints for M17.6 enforcement. When non-empty and
/// the connection's `foreign_keys` flag is on, the program emits one `FkCheck` per FK per row
/// before the table `Insert`; a missing parent row raises "FOREIGN KEY constraint failed".
/// When empty (no FKs, or FK enforcement off), no FK opcodes are emitted.
pub fn compile_insert(
    ins: &InsertStmt,
    table: &Table,
    indexes: &[IndexObject],
    source_table: Option<&Table>,
    source_indexes: &[IndexObject],
    fk_checks: &[FkCheckP4],
) -> Result<Program> {
    if table.without_rowid {
        return compile_insert_without_rowid(ins, table, indexes);
    }
    match &ins.source {
        InsertSource::Values(rows) => compile_insert_values(ins, table, indexes, rows, fk_checks),
        InsertSource::Select(sel) => {
            compile_insert_select(ins, table, indexes, sel, source_table, source_indexes, fk_checks)
        }
        InsertSource::DefaultValues => {
            compile_insert_default_values(ins, table, indexes, fk_checks)
        }
    }
}

/// Compile `INSERT INTO ... VALUES (...)[, (...)]`.
fn compile_insert_values(
    ins: &InsertStmt,
    table: &Table,
    indexes: &[IndexObject],
    rows: &[Vec<Expr>],
    fk_checks: &[FkCheckP4],
) -> Result<Program> {
    if rows.is_empty() {
        return Err(Error::msg("INSERT must supply at least one VALUES row"));
    }

    // Map each VALUES position to a table column index. With an explicit column list the values
    // fill those columns (unlisted columns get NULL); otherwise the values are positional over all
    // columns. `value_for_col[c]` is the VALUES index that feeds table column `c`, or None.
    let ncol = table.columns.len();
    let value_for_col: Vec<Option<usize>> = if ins.columns.is_empty() {
        (0..ncol).map(Some).collect()
    } else {
        let mut map = vec![None; ncol];
        for (vi, name) in ins.columns.iter().enumerate() {
            let ci = table.column_index(name).ok_or_else(|| {
                Error::msg(format!("table {} has no column named {name}", table.name))
            })?;
            map[ci] = Some(vi);
        }
        map
    };
    let expected = if ins.columns.is_empty() {
        ncol
    } else {
        ins.columns.len()
    };

    validate_indexes(table, indexes)?;

    let cursor = 0i32;
    let ctx = Ctx {
        table,
        cursor,
        register_base: None, join_tables: None,
        index_read: None,
        subquery_resolver: None,
    };
    let oe = OeAction::from_parser(ins.or_action);
    let mut b = ProgramBuilder::new();
    b.set_default_oe(oe as u8);

    let returning = ins
        .returning
        .as_deref()
        .map(|r| Returning::new(r, table))
        .transpose()?;

    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0); // addr 0
    let after_init = b.cur_addr();

    b.emit(Opcode::Transaction, 0, 1, 0); // open the write transaction
    b.emit(Opcode::OpenWrite, cursor, table.rootpage as i32, 0);

    // Reserve cursor numbers for the indexes (1, 2, …). The table cursor is 0. Each index
    // cursor carries the index's KeyInfo so inserts compare under the correct collation.
    let index_cursor_base: i32 = open_index_cursors(&mut b, indexes)?;

    // RETURNING: open an ephemeral table to buffer result rows. Pick a cursor number safely
    // above the table/index cursors.
    let eph_cursor = index_cursor_base + indexes.len() as i32;
    let mut returning = returning;
    if let Some(ref mut ret) = returning {
        ret.emit_open(&mut b, eph_cursor);
    }

    for row in rows {
        if row.len() != expected {
            return Err(Error::msg(format!(
                "table {} has {expected} columns but {} values were supplied",
                table.name,
                row.len()
            )));
        }

        // The record holds one slot per table column. The rowid-alias column stores NULL on disk;
        // its value becomes the rowid instead.
        let rec_start = b.alloc_regs(ncol as i32);
        let rowid_reg = b.alloc_reg();
        // Whether an `INTEGER PRIMARY KEY` value was supplied for this row's rowid register.
        let mut alias_supplied = false;

        for (ci, col) in table.columns.iter().enumerate() {
            let target = rec_start + ci as i32;
            let is_alias = table.rowid_alias == Some(ci);
            match value_for_col[ci] {
                Some(vi) => {
                    let value_expr = &row[vi];
                    if is_alias {
                        // The INTEGER PRIMARY KEY value becomes the rowid (with INTEGER affinity);
                        // the record slot is stored as NULL. A NULL value means "auto-assign",
                        // handled by the conditional NewRowid below.
                        compile_rowid_alias(&mut b, value_expr, rowid_reg, ctx)?;
                        b.emit(Opcode::Null, 0, target, 0);
                        alias_supplied = true;
                    } else {
                        compile_expr(&mut b, value_expr, target, ctx)?;
                        apply_affinity(&mut b, target, col.affinity);
                    }
                }
                None => {
                    // An unlisted column defaults to NULL (column DEFAULTs are not modeled yet).
                    b.emit(Opcode::Null, 0, target, 0);
                }
            }
        }

        // Pick the rowid. With a supplied alias value, NewRowid runs only when that value is NULL
        // (auto-assign); a concrete value is used as-is. Without an alias, always NewRowid.
        if alias_supplied {
            let have_rowid = b.new_label();
            b.emit_jump(Opcode::NotNull, rowid_reg, have_rowid, 0);
            b.emit(Opcode::NewRowid, cursor, rowid_reg, 0);
            b.resolve(have_rowid);
        } else {
            b.emit(Opcode::NewRowid, cursor, rowid_reg, 0);
        }

        // Conflict-resolution pre-checks (all OEs). For IGNORE, on conflict we jump to
        // `row_skip` — BEFORE the table Insert — so the conflicting row is never written. For
        // REPLACE, on conflict we delete the existing row (from the table and every index) and
        // fall through to the new Insert. For ABORT/FAIL/ROLLBACK, on conflict we emit a `Halt`
        // with the constraint message — BEFORE the table Insert — so the failing row's partial
        // writes are never made and prior rows in the same statement stay clean (mirrors
        // upstream's "OE_Fail and OE_Ignore must happen before any changes are made" rule in
        // `sqlite3GenerateConstraintChecks`).
        let row_skip = b.new_label();
        // UPSERT (M18.3): when an `ON CONFLICT ...` clause is present, emit the
        // upsert-driven conflict precheck before the generic per-index prechecks.
        // The upsert precheck handles its matched index (DO NOTHING jumps to
        // `row_skip`; DO UPDATE updates the conflicting row in place and then
        // jumps to `row_skip`). The matched index is then skipped in
        // `emit_conflict_prechecks` so its default OE doesn't double-fire. For
        // `ON CONFLICT DO NOTHING` (no target) the upsert sets the statement-level
        // OE to `Ignore` and lets `emit_conflict_prechecks` handle every unique
        // index uniformly (equivalent to `INSERT OR IGNORE`). For `ON CONFLICT DO
        // UPDATE` (no target) every unique constraint is handled by the upsert
        // precheck, so all unique indexes are skipped in `emit_conflict_prechecks`.
        let mut effective_oe = oe;
        let mut skip_indexes: Vec<usize> = Vec::new();
        let has_upsert = !ins.upsert.is_empty();
        if has_upsert {
            let clause = &ins.upsert[0];
            if clause.target.is_none() {
                // No-target DO NOTHING → INSERT OR IGNORE semantics.
                if matches!(clause.action, UpsertAction::Nothing) {
                    effective_oe = OeAction::Ignore;
                } else {
                    // No-target DO UPDATE: the upsert precheck handles every unique
                    // constraint; skip all unique indexes in the generic prechecks
                    // so their default OE doesn't double-fire.
                    for (i, idx) in indexes.iter().enumerate() {
                        if idx.unique {
                            skip_indexes.push(i);
                        }
                    }
                }
            } else {
                // Targeted upsert: resolve the matched index. The matched index
                // position is computed so `emit_conflict_prechecks` can skip it.
                let target = clause.target.as_ref().unwrap();
                let matched = upsert::resolve_target(
                    &target.columns,
                    target.where_clause.as_ref(),
                    table,
                    indexes,
                )?;
                if let upsert::MatchedIndex::Index(idx) = matched {
                    if let Some(pos) = indexes.iter().position(|i| std::ptr::eq(i, idx)) {
                        skip_indexes.push(pos);
                    }
                }
            }
            // Emit the upsert precheck for both targeted and no-target forms.
            // For no-target DO NOTHING, the precheck is skipped (effective_oe =
            // Ignore handles it via emit_conflict_prechecks). For no-target DO
            // UPDATE, the precheck probes the first unique constraint and runs
            // the update body on conflict.
            if !(clause.target.is_none() && matches!(clause.action, UpsertAction::Nothing)) {
                upsert::emit_upsert_precheck(
                    &mut b,
                    &ins.upsert,
                    table,
                    indexes,
                    rec_start,
                    rowid_reg,
                    cursor,
                    index_cursor_base,
                    row_skip,
                )?;
            }
        }
        emit_conflict_prechecks(
            &mut b,
            indexes,
            table,
            rec_start,
            rowid_reg,
            index_cursor_base,
            cursor,
            effective_oe,
            row_skip,
            &skip_indexes,
        )?;

        // M17.6: FK enforcement. For each FK constraint on this table, emit an `FkCheck` that
        // verifies the child row's FK columns reference an existing parent row. NULL child keys
        // skip the check (NULL foreign keys never violate). A missing parent jumps to the
        // violation handler (a `Halt` with the "FOREIGN KEY constraint failed" prefix). The
        // `foreign_keys` flag gates emission — when FK enforcement is off, no checks are emitted
        // (matching upstream's `db->flags & SQLITE_ForeignKeys` guard in the codegen).
        if !fk_checks.is_empty() {
            emit_fk_checks(&mut b, fk_checks, table, rec_start, oe, row_skip)?;
        }

        let record = b.alloc_reg();
        b.emit(Opcode::MakeRecord, rec_start, ncol as i32, record);
        b.emit(Opcode::Insert, cursor, record, rowid_reg);

        emit_index_inserts(
            &mut b,
            indexes,
            table,
            rec_start,
            rowid_reg,
            index_cursor_base,
        )?;

        if let Some(ref ret) = returning {
            if let Some(alias_idx) = table.rowid_alias {
                b.emit(Opcode::SCopy, rowid_reg, rec_start + alias_idx as i32, 0);
            }
            ret.emit_buffer_row(&mut b, table, cursor, rec_start)?;
        }

        b.resolve(row_skip);
    }

    if let Some(ref ret) = returning {
        ret.emit_output_loop(&mut b);
    }

    b.emit(Opcode::Halt, 0, 0, 0); // commits the write transaction

    b.resolve(setup);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

/// Compile `INSERT INTO <without-rowid-table>` for `VALUES` and `DEFAULT VALUES` sources.
///
/// The WITHOUT ROWID table is an index b-tree keyed by the PK record (PK columns followed by
/// the remaining non-PK columns, matching upstream's `convertToWithoutRowidTable` covering-index
/// shape). The codegen therefore:
///   * opens the table as an *index* write cursor with a `KeyInfo` derived from the PK
///     (DESC flags honored, BINARY collation today);
///   * builds, per row, a single record holding all stored columns in storage order
///     (`[pk_cols..., non-pk cols...]`);
///   * `IdxInsert`s that record with `P5_UNIQUE` set so the PK uniqueness constraint is enforced
///     (`UNIQUE constraint failed: table.pk1, table.pk2, ...` on conflict);
///   * still emits per-row `IdxInsert` for any user-declared secondary indexes (their key
///     records end in the PK columns rather than a rowid, mirroring upstream's
///     `sqlite3CreateIndex` PK-tail rewrite for WITHOUT ROWID tables).
///
/// `INSERT ... SELECT` into a WITHOUT ROWID table is deferred to M8 (it needs coroutine
/// plumbing that the rowid path already uses); the parser accepts it but the codegen errors.
fn compile_insert_without_rowid(
    ins: &InsertStmt,
    table: &Table,
    indexes: &[IndexObject],
) -> Result<Program> {
    let rows: Vec<Vec<Expr>> = match &ins.source {
        InsertSource::Values(rows) => rows.clone(),
        InsertSource::DefaultValues => vec![Vec::new()],
        InsertSource::Select(_) => {
            return Err(Error::msg(
                "INSERT ... SELECT into a WITHOUT ROWID table is not supported yet",
            ))
        }
    };

    let ncol = table.columns.len();
    let _n_pk = table.pk_columns.len();
    let storage_width = table.without_rowid_storage_width();
    // Map each VALUES position to a table column index (same logic as the rowid path).
    let value_for_col: Vec<Option<usize>> = if ins.columns.is_empty() {
        (0..ncol).map(Some).collect()
    } else {
        let mut map = vec![None; ncol];
        for (vi, name) in ins.columns.iter().enumerate() {
            let ci = table.column_index(name).ok_or_else(|| {
                Error::msg(format!("table {} has no column named {name}", table.name))
            })?;
            map[ci] = Some(vi);
        }
        map
    };
    let expected = if ins.columns.is_empty() {
        ncol
    } else {
        ins.columns.len()
    };

    validate_indexes(table, indexes)?;

    let cursor = 0i32;
    let ctx = Ctx {
        table,
        cursor,
        register_base: None, join_tables: None,
        index_read: None,
        subquery_resolver: None,
    };
    let oe = OeAction::from_parser(ins.or_action);
    let mut b = ProgramBuilder::new();
    b.set_default_oe(oe as u8);

    let returning = ins
        .returning
        .as_deref()
        .map(|r| Returning::new(r, table))
        .transpose()?;

    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    let after_init = b.cur_addr();

    b.emit(Opcode::Transaction, 0, 1, 0);

    // Open the WITHOUT ROWID table as an index b-tree write cursor (KeyInfo from the PK).
    let open = b.emit(Opcode::OpenWrite, cursor, table.rootpage as i32, 0);
    b.set_p4(open, P4::KeyInfo(table.without_rowid_key_info()));

    let index_cursor_base: i32 = open_index_cursors(&mut b, indexes)?;
    let eph_cursor = index_cursor_base + indexes.len() as i32;
    let mut returning = returning;
    if let Some(ref mut ret) = returning {
        ret.emit_open(&mut b, eph_cursor);
    }

    let pk_message = {
        let names: Vec<String> = table
            .pk_columns
            .iter()
            .map(|&(c, _)| format!("{}.{}", table.name, table.columns[c].name))
            .collect();
        format!("UNIQUE constraint failed: {}", names.join(", "))
    };
    // The bare column-list for the Halt-based pre-check path (where p5 = P5_ConstraintUnique
    // supplies the "UNIQUE constraint failed: " prefix, mirroring upstream's
    // `sqlite3HaltConstraint`).
    let pk_columns_msg = {
        let names: Vec<String> = table
            .pk_columns
            .iter()
            .map(|&(c, _)| format!("{}.{}", table.name, table.columns[c].name))
            .collect();
        names.join(", ")
    };

    for row in &rows {
        if row.len() != expected {
            return Err(Error::msg(format!(
                "table {} has {expected} columns but {} values were supplied",
                table.name,
                row.len()
            )));
        }

        // First evaluate every table column into its own register (table-column order), so the
        // secondary-index maintenance below can read columns by their table index just like the
        // rowid path does. Then permutation-copy them into storage order for the table key.
        let col_start = b.alloc_regs(ncol as i32);
        for (ci, col) in table.columns.iter().enumerate() {
            let target = col_start + ci as i32;
            match value_for_col[ci] {
                Some(vi) if vi < row.len() => {
                    let value_expr = &row[vi];
                    compile_expr(&mut b, value_expr, target, ctx)?;
                    apply_affinity(&mut b, target, col.affinity);
                }
                _ => {
                    // Unlisted column or DEFAULT VALUES: load the column DEFAULT, else NULL.
                    if let Some(expr) = &col.default {
                        compile_expr(&mut b, expr, target, ctx)?;
                        apply_affinity(&mut b, target, col.affinity);
                    } else {
                        b.emit(Opcode::Null, 0, target, 0);
                    }
                }
            }
        }

        // NOT NULL on PK columns is enforced at codegen time via a per-row HaltIfNull check
        // (mirrors upstream's `OP_HaltIfNull` for NOT NULL columns in WITHOUT ROWID PKs). The
        // per-constraint `ON CONFLICT <action>` clause on the declaring `PRIMARY KEY` constraint
        // (M12.9) is carried in p2: 0 means "use the statement-level `OR <action>` / default
        // ABORT" (the existing behavior); a non-zero OE value overrides it for this constraint.
        // OE_Ignore is handled here by emitting an `IsNull` jump to `row_skip` instead of
        // `HaltIfNull`, mirroring upstream's `OP_IsNull iReg, ignoreDest` path.
        let null_row_skip = b.new_label();
        let mut any_null_skip = false;
        for &(pk_idx, _) in &table.pk_columns {
            let reg = col_start + pk_idx as i32;
            let col_oe = table.columns[pk_idx].notnull_oe;
            let msg = format!(
                "NOT NULL constraint failed: {}.{}",
                table.name, table.columns[pk_idx].name
            );
            match col_oe {
                OeAction::Ignore => {
                    // OE_Ignore: skip the row when NULL (upstream's `OP_IsNull iReg, ignoreDest`).
                    b.emit_jump(Opcode::IsNull, reg, null_row_skip, 0);
                    any_null_skip = true;
                }
                OeAction::Replace => {
                    // OE_Replace on NOT NULL: REPLACE can only help if the column has a non-NULL
                    // DEFAULT; otherwise upstream falls back to OE_Abort (`b2ndPass || pCol->iDflt==0`).
                    // Our WITHOUT ROWID PK columns are typically NOT NULL with no DEFAULT, so we
                    // treat REPLACE as ABORT here (the common case). Mirrors upstream's fallback.
                    let halt = b.emit(Opcode::HaltIfNull, 0, 0, reg);
                    b.set_p4(halt, P4::Text(msg));
                }
                other if other != OeAction::None => {
                    // A per-constraint OE (Fail/Rollback/Abort) overrides the statement-level OR.
                    let halt = b.emit(Opcode::HaltIfNull, 0, other as i32, reg);
                    b.set_p4(halt, P4::Text(msg));
                }
                _ => {
                    // No per-constraint OE: use the statement-level `default_oe` (the existing
                    // behavior — `p2 = 0` means "use `program.default_oe`").
                    let halt = b.emit(Opcode::HaltIfNull, 0, 0, reg);
                    b.set_p4(halt, P4::Text(msg));
                }
            }
        }

        // Permute into storage order: PK cols first (in declared order), then non-PK cols in
        // table column order.
        let key_start = b.alloc_regs(storage_width as i32);
        let mut out_pos = 0i32;
        for &(c, _) in &table.pk_columns {
            b.emit(Opcode::SCopy, col_start + c as i32, key_start + out_pos, 0);
            out_pos += 1;
        }
        for i in 0..table.columns.len() {
            if table.pk_columns.iter().any(|&(c, _)| c == i) {
                continue;
            }
            b.emit(Opcode::SCopy, col_start + i as i32, key_start + out_pos, 0);
            out_pos += 1;
        }

        // The effective OE for the WITHOUT ROWID PK's UNIQUE constraint: the per-constraint `ON
        // CONFLICT <action>` overrides the statement-level `OR <action>` when set (M12.9).
        // Upstream's `overrideError` (the statement-level OR) only applies when the per-
        // constraint OE is `OE_Default`; our `OeAction::None` stands in for `OE_Default`.
        let pk_per_constraint_oe = table
            .pk_columns
            .first()
            .map(|&(c, _)| table.columns[c].notnull_oe)
            .unwrap_or(OeAction::None);
        let pk_oe = if pk_per_constraint_oe != OeAction::None {
            pk_per_constraint_oe
        } else {
            oe
        };

        // Pre-check the PK uniqueness when the effective OE is not `OE_None`, mirroring
        // `emit_conflict_prechecks` for the rowid path. For ABORT/FAIL/ROLLBACK the pre-check
        // raises the constraint error BEFORE the `IdxInsert` (which writes the row), so the
        // failing row's partial writes are never made and prior rows in the same statement stay
        // clean (mirrors upstream's "OE_Fail and OE_Ignore must happen before any changes are
        // made" rule). For Ignore/Replace the pre-check skips/replaces the conflicting row.
        if pk_oe != OeAction::None {
            let no_conflict = b.new_label();
            let nfield = table.pk_columns.len() as i32;
            let nc = b.emit_jump(Opcode::NoConflict, cursor, no_conflict, key_start);
            b.set_p4(nc, P4::Int(nfield as i64));
            match pk_oe {
                OeAction::Ignore => {
                    b.emit_jump(Opcode::Goto, 0, null_row_skip, 0);
                    any_null_skip = true;
                }
                OeAction::Replace => {
                    // Delete the conflicting row (the storage-order key matches the new row's PK
                    // prefix) from the table b-tree. The subsequent `IdxInsert` will then succeed.
                    b.emit(Opcode::IdxDelete, cursor, key_start, storage_width as i32);
                }
                OeAction::Abort | OeAction::Fail | OeAction::Rollback => {
                    // Halt BEFORE the IdxInsert so the failing row's partial writes are never
                    // made. `p2 = OE` so `step()` does the right cleanup; `p5 = 2` for the
                    // "UNIQUE constraint failed: ..." prefix.
                    let halt = b.emit(
                        Opcode::Halt,
                        crate::error::ResultCode::Constraint as i32,
                        pk_oe as i32,
                        0,
                    );
                    b.set_p4(halt, P4::Text(pk_columns_msg.clone()));
                    b.set_p5(halt, 2); // P5_ConstraintUnique
                }
                _ => unreachable!(),
            }
            b.resolve(no_conflict);
        }

        let key_rec = b.alloc_reg();
        b.emit(Opcode::MakeRecord, key_start, storage_width as i32, key_rec);
        let ins_idx = b.emit(Opcode::IdxInsert, cursor, key_rec, 0);
        b.set_p4(ins_idx, P4::Text(pk_message.clone()));
        // Encode the PK constraint's OE in p5 bits 4-7 so the executor can override the
        // statement-level `OR <action>` for this constraint's UNIQUE violation (M12.9). A zero
        // high nibble means "no per-constraint override; use `default_oe`". For Ignore/Replace
        // the pre-check above already handled the conflict; the encoded OE is still useful for
        // any path that bypasses the pre-check.
        if pk_per_constraint_oe != OeAction::None {
            b.set_p5(
                ins_idx,
                P5_NCHANGE | P5_UNIQUE | ((pk_per_constraint_oe as u8 & 0x0F) << 4),
            );
        } else {
            b.set_p5(ins_idx, P5_NCHANGE | P5_UNIQUE);
        }

        // Secondary index maintenance. The user-declared indexes on a WITHOUT ROWID table end
        // their key with the PK columns (not a rowid); `emit_index_inserts_without_rowid` does
        // that substitution. The connection's `last_insert_rowid` is not updated for a WITHOUT
        // ROWID insert (there is no rowid).
        emit_index_inserts_without_rowid(
            &mut b,
            indexes,
            table,
            col_start,
            index_cursor_base,
        )?;
        self::bump_changes(&mut b);

        if let Some(ref ret) = returning {
            ret.emit_buffer_row(&mut b, table, cursor, col_start)?;
        }

        // The OE_Ignore path for NOT NULL jumps here, skipping the rest of this row's insert
        // (mirrors upstream's `ignoreDest`).
        if any_null_skip {
            b.resolve(null_row_skip);
        }
    }

    if let Some(ref ret) = returning {
        ret.emit_output_loop(&mut b);
    }

    b.emit(Opcode::Halt, 0, 0, 0);

    b.resolve(setup);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

/// Bump the connection change counters for a WITHOUT ROWID insert (one row per VALUES row).
fn bump_changes(_b: &mut ProgramBuilder) {
    // The IdxInsert carries P5_NCHANGE so the executor bumps changes itself; no extra opcode
    // is needed. Kept as a named hook for symmetry with the rowid path.
}

/// Append the secondary-index `IdxInsert` sequence for one row of a WITHOUT ROWID table. The
/// key record for each user-declared index is `[indexed columns..., PK columns...]` — the PK
/// columns replace the trailing rowid that the rowid-table path uses. PK columns are read in
/// their declared order from the table-column register block.
fn emit_index_inserts_without_rowid(
    b: &mut ProgramBuilder,
    indexes: &[IndexObject],
    table: &Table,
    col_start: i32,
    index_cursor_base: i32,
) -> Result<()> {
    for (i, idx) in indexes.iter().enumerate() {
        let ic = index_cursor_base + i as i32;
        let indexed_cis = idx.table_column_indices(table)?;
        let nkey = idx.nkey_fields() as i32 + table.pk_columns.len() as i32;

        let skip_label = if let Some(pred) = &idx.where_clause {
            let skip = b.new_label();
            let pred_ctx = Ctx {
                table,
                cursor: 0,
                register_base: None, join_tables: None,
                index_read: None,
                subquery_resolver: None,
            };
            compile_pred_jump(
                b,
                pred,
                skip,
                table,
                col_start,
                indexed_cis.as_slice(),
                pred_ctx,
            )?;
            Some(skip)
        } else {
            None
        };

        let key_start = b.alloc_regs(nkey);
        let mut plain_iter = indexed_cis.iter();
        for (j, icol) in idx.columns.iter().enumerate() {
            let target = key_start + j as i32;
            if let Some(expr) = &icol.expr {
                let expr_ctx = Ctx {
                    table,
                    cursor: 0,
                    register_base: Some(col_start), join_tables: None,
                    index_read: None,
                    subquery_resolver: None,
                };
                compile_expr(b, expr, target, expr_ctx)?;
            } else {
                let col_idx = *plain_iter
                    .next()
                    .expect("plain column aligned with indexed_cis");
                b.emit(Opcode::SCopy, col_start + col_idx as i32, target, 0);
            }
        }
        // Append the PK columns in declared order (replaces the rowid tail).
        for (k, &(c, _)) in table.pk_columns.iter().enumerate() {
            b.emit(
                Opcode::SCopy,
                col_start + c as i32,
                key_start + idx.nkey_fields() as i32 + k as i32,
                0,
            );
        }
        let key_rec = b.alloc_reg();
        b.emit(Opcode::MakeRecord, key_start, nkey, key_rec);
        let ins_idx = b.emit(Opcode::IdxInsert, ic, key_rec, 0);
        let mut p5 = P5_NCHANGE;
        if idx.unique {
            p5 |= P5_UNIQUE;
            // Encode the per-index OE in p5 bits 4-7 so the executor can override the statement
            // level `OR <action>` for this index's UNIQUE violation (M12.9). Today the OE is
            // always `OE_Abort` because `CREATE [UNIQUE] INDEX` doesn't accept `ON CONFLICT`;
            // autoindex-creating PK/UNIQUE constraints would thread their OE here too. A zero
            // high nibble means "no per-constraint override; use `default_oe`".
            if idx.unique_oe != OeAction::Abort {
                p5 |= (idx.unique_oe as u8 & 0x0F) << 4;
            }
            if let Some(msg) = idx.unique_constraint_message(table) {
                b.set_p4(ins_idx, P4::Text(msg));
            } else {
                b.set_p4(ins_idx, P4::Int(0));
            }
        } else {
            b.set_p4(ins_idx, P4::Int(0));
        }
        b.set_p5(ins_idx, p5);

        if let Some(skip) = skip_label {
            b.resolve(skip);
        }
    }
    Ok(())
}

/// Compile `INSERT INTO ... DEFAULT VALUES`.
fn compile_insert_default_values(
    ins: &InsertStmt,
    table: &Table,
    indexes: &[IndexObject],
    fk_checks: &[FkCheckP4],
) -> Result<Program> {
    // An explicit column list is not meaningful for DEFAULT VALUES, but SQLite accepts it as a
    // no-op (it still uses all defaults). We simply ignore `ins.columns`.
    let _ = &ins.columns;

    validate_indexes(table, indexes)?;
    let returning = ins
        .returning
        .as_deref()
        .map(|r| Returning::new(r, table))
        .transpose()?;

    let cursor = 0i32;
    let ctx = Ctx {
        table,
        cursor,
        register_base: None, join_tables: None,
        index_read: None,
        subquery_resolver: None,
    };
    let oe = OeAction::from_parser(ins.or_action);
    let mut b = ProgramBuilder::new();
    b.set_default_oe(oe as u8);

    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0); // addr 0
    let after_init = b.cur_addr();

    b.emit(Opcode::Transaction, 0, 1, 0);
    b.emit(Opcode::OpenWrite, cursor, table.rootpage as i32, 0);

    let index_cursor_base: i32 = open_index_cursors(&mut b, indexes)?;

    let eph_cursor = index_cursor_base + indexes.len() as i32;
    let mut returning = returning;
    if let Some(ref mut ret) = returning {
        ret.emit_open(&mut b, eph_cursor);
    }

    let ncol = table.columns.len();
    let rec_start = b.alloc_regs(ncol as i32);
    let rowid_reg = b.alloc_reg();
    let mut alias_supplied = false;

    for (ci, col) in table.columns.iter().enumerate() {
        let target = rec_start + ci as i32;
        let is_alias = table.rowid_alias == Some(ci);
        if is_alias {
            // The rowid-alias column default becomes the rowid when present and non-NULL.
            // An absent default is treated as NULL, which lets NewRowid auto-assign below.
            if let Some(expr) = &col.default {
                compile_expr(&mut b, expr, rowid_reg, ctx)?;
                apply_affinity(&mut b, rowid_reg, Affinity::Integer);
            } else {
                b.emit(Opcode::Null, 0, rowid_reg, 0);
            }
            b.emit(Opcode::Null, 0, target, 0);
            alias_supplied = true;
        } else if let Some(expr) = &col.default {
            compile_expr(&mut b, expr, target, ctx)?;
            apply_affinity(&mut b, target, col.affinity);
        } else {
            b.emit(Opcode::Null, 0, target, 0);
        }
    }

    // Pick the rowid. When the rowid alias is absent or its default is NULL, auto-assign.
    if alias_supplied {
        let have_rowid = b.new_label();
        b.emit_jump(Opcode::NotNull, rowid_reg, have_rowid, 0);
        b.emit(Opcode::NewRowid, cursor, rowid_reg, 0);
        b.resolve(have_rowid);
    } else {
        b.emit(Opcode::NewRowid, cursor, rowid_reg, 0);
    }

    // M17.6: FK enforcement (only when the connection has FKs on).
    if !fk_checks.is_empty() {
        let row_skip = b.new_label();
        let oe = OeAction::from_parser(ins.or_action);
        emit_fk_checks(&mut b, fk_checks, table, rec_start, oe, row_skip)?;
        // row_skip is only used by OE_Ignore; for the default ABORT path the violation Halt-s
        // so this label is never branched to in the ABORT case. Resolve it to the post-insert
        // point so a non-branching fall-through is correct.
        b.resolve(row_skip);
    }

    let record = b.alloc_reg();
    b.emit(Opcode::MakeRecord, rec_start, ncol as i32, record);
    b.emit(Opcode::Insert, cursor, record, rowid_reg);

    emit_index_inserts(
        &mut b,
        indexes,
        table,
        rec_start,
        rowid_reg,
        index_cursor_base,
    )?;

    if let Some(ref ret) = returning {
        if let Some(alias_idx) = table.rowid_alias {
            b.emit(Opcode::SCopy, rowid_reg, rec_start + alias_idx as i32, 0);
        }
        ret.emit_buffer_row(&mut b, table, cursor, rec_start)?;
        ret.emit_output_loop(&mut b);
    }

    b.emit(Opcode::Halt, 0, 0, 0);

    b.resolve(setup);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

/// Compile `INSERT INTO ... SELECT ...`.
///
/// The generated program uses a single ephemeral sorter (cursor 1) to stage the selected rows:
/// the SELECT body is compiled as a subprogram that inserts its result rows into the sorter, then
/// the main insert loop reads the sorter, applies column mapping / affinity, allocates rowids, and
/// inserts into the target table (and its indexes). This matches upstream's `sqlite3Insert` shape
/// for `ONEPASS_OFF` inserts from a query.
fn compile_insert_select(
    ins: &InsertStmt,
    table: &Table,
    indexes: &[IndexObject],
    sel: &SelectStmt,
    source_table: Option<&Table>,
    source_indexes: &[IndexObject],
    fk_checks: &[FkCheckP4],
) -> Result<Program> {
    // Map each SELECT-result position to a table column index. With an explicit column list the
    // selected columns fill those columns; otherwise they are positional over all columns.
    let ncol = table.columns.len();
    let value_for_col: Vec<Option<usize>> = if ins.columns.is_empty() {
        (0..ncol).map(Some).collect()
    } else {
        let mut map = vec![None; ncol];
        for (vi, name) in ins.columns.iter().enumerate() {
            let ci = table.column_index(name).ok_or_else(|| {
                Error::msg(format!("table {} has no column named {name}", table.name))
            })?;
            map[ci] = Some(vi);
        }
        map
    };
    let nselect_cols = if ins.columns.is_empty() {
        ncol
    } else {
        ins.columns.len()
    };

    // Arity check can only be done for constant VALUES today; for SELECT we trust the runtime
    // match and let MakeRecord/Column deal with short rows. Still, reject obviously wrong constant
    // selects with `VALUES` here for early error reporting.
    if !sel.values.is_empty() {
        let first_row_cols = sel.values[0].len();
        if first_row_cols != nselect_cols {
            return Err(Error::msg(format!(
                "table {} has {nselect_cols} columns but {} values were supplied",
                table.name, first_row_cols
            )));
        }
    }

    validate_indexes(table, indexes)?;

    let cursor = 0i32;
    let sorter = 1i32;
    let oe = OeAction::from_parser(ins.or_action);
    let mut b = ProgramBuilder::new();
    b.set_default_oe(oe as u8);

    let setup = b.new_label();
    let after_init = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    b.resolve(after_init);

    b.emit(Opcode::Transaction, 0, 1, 0);
    b.emit(Opcode::OpenWrite, cursor, table.rootpage as i32, 0);
    let index_cursor_base: i32 = open_index_cursors(&mut b, indexes)?;

    // Sorter layout: leading rowid-alias slot (when present) followed by the source columns in
    // SELECT-result order, then the computed rowid slot. We use a stable KeyInfo so the sorter
    // preserves insertion order when keys compare equal (all BINARY, all ASC).
    let sorter_fields: Vec<crate::vdbe::KeyField> =
        std::iter::repeat_n(crate::vdbe::KeyField::asc_binary(), nselect_cols + 1).collect();
    let so = b.emit(Opcode::SorterOpen, sorter, nselect_cols as i32 + 1, 0);
    b.set_p4(so, P4::KeyInfo(sorter_fields));

    // --- Run the SELECT and store each result row in the sorter. ---
    // We need a separate VDBE program for the SELECT, but this milestone does not yet have
    // InitCoroutine/EndCoroutine/Yield. Instead, inline the SELECT's scan loop by compiling the
    // select body and then changing each ResultRow into a SorterInsert of the result registers.
    //
    // The select compiler emits ResultRow with the result registers in a contiguous block. We
    // rewrite those ResultRow instructions into MakeRecord + SorterInsert so the selected rows
    // accumulate in the sorter.
    let (select_program, _names) = select::compile(sel, source_table, source_indexes, None)?;
    let select_start = b.cur_addr();
    // Append the select instructions wholesale, remapping ResultRow and Halt.
    let select_offset = select_start;
    for (idx, mut inst) in select_program.instructions.into_iter().enumerate() {
        let _ = idx;
        match inst.opcode {
            Opcode::ResultRow => {
                // The result registers start at inst.p1 and span inst.p2 columns. Build a sorter
                // record [rowid-alias-placeholder, result...] and insert it. The placeholder is
                // overwritten per row during the insert loop if the table has an INTEGER PRIMARY
                // KEY column that is mapped.
                let result_start = inst.p1;
                let nres = inst.p2;
                let block = b.alloc_regs(nselect_cols as i32 + 1);
                // rowid placeholder
                b.emit(Opcode::Null, 0, block, 0);
                // copy result columns into the sorter record
                for j in 0..nres {
                    b.emit(Opcode::SCopy, result_start + j, block + 1 + j, 0);
                }
                // Pad missing trailing columns with NULL (e.g. SELECT with fewer columns than target).
                for j in nres..nselect_cols as i32 {
                    b.emit(Opcode::Null, 0, block + 1 + j, 0);
                }
                let rec = b.alloc_reg();
                b.emit(Opcode::MakeRecord, block, nselect_cols as i32 + 1, rec);
                b.emit(Opcode::SorterInsert, sorter, rec, 0);
            }
            Opcode::Halt => {
                // The select's Halt becomes a Goto the insert loop. Preserve the instruction so
                // the label resolver still has a target for any jumps inside the select.
                let insert_loop = b.new_label();
                b.emit_jump(Opcode::Goto, 0, insert_loop, 0);
                b.resolve(insert_loop);
                // We intentionally resolve the insert-loop label immediately after the Goto.
                // This means any later jump to it will land at the next emitted instruction, which
                // is the start of the insert loop. This is safe because the Goto itself is the
                // fall-through exit from the inlined select.
            }
            _ => {
                // Remap absolute jumps that target addresses inside this copied select program.
                // The select compiler emits p2 targets as absolute instruction addresses. We need
                // to offset them by select_offset, but only for forward/backward jumps inside the
                // copied block. We do this by mutating the instruction before appending.
                if is_absolute_jump(&inst) {
                    inst.p2 += select_offset;
                }
                b.append(inst);
            }
        }
    }

    // --- Insert loop: read sorter rows and insert into the table. ---
    let end_insert = b.new_label();
    b.emit_jump(Opcode::SorterSort, sorter, end_insert, 0);
    let insert_top_label = b.new_label();
    let sort_next = b.new_label();
    b.resolve(insert_top_label);
    b.emit(Opcode::SorterData, sorter, 0, 0);

    // Decode the sorter record into a contiguous register block so SCopy can read source columns.
    // Sorter record layout: [placeholder, source-col-0, source-col-1, ...].
    let source_start = b.alloc_regs(nselect_cols as i32 + 1);
    for j in 0..=nselect_cols as i32 {
        b.emit(Opcode::Column, sorter, j, source_start + j);
    }

    let rec_start = b.alloc_regs(ncol as i32);
    let rowid_reg = b.alloc_reg();
    let mut alias_supplied = false;

    for (ci, col) in table.columns.iter().enumerate() {
        let target = rec_start + ci as i32;
        let is_alias = table.rowid_alias == Some(ci);
        match value_for_col[ci] {
            Some(vi) => {
                let source_reg = source_start + 1 + vi as i32;
                if is_alias {
                    // INTEGER PRIMARY KEY: the selected value becomes the rowid; the stored column is NULL.
                    // If the selected value is NULL, we will auto-assign below.
                    b.emit(Opcode::SCopy, source_reg, rowid_reg, 0);
                    apply_affinity(&mut b, rowid_reg, Affinity::Integer);
                    b.emit(Opcode::Null, 0, target, 0);
                    alias_supplied = true;
                } else {
                    b.emit(Opcode::SCopy, source_reg, target, 0);
                    apply_affinity(&mut b, target, col.affinity);
                }
            }
            None => {
                b.emit(Opcode::Null, 0, target, 0);
            }
        }
    }

    if alias_supplied {
        let have_rowid = b.new_label();
        b.emit_jump(Opcode::NotNull, rowid_reg, have_rowid, 0);
        b.emit(Opcode::NewRowid, cursor, rowid_reg, 0);
        b.resolve(have_rowid);
    } else {
        b.emit(Opcode::NewRowid, cursor, rowid_reg, 0);
    }

    // M17.6: FK enforcement.
    if !fk_checks.is_empty() {
        let row_skip = b.new_label();
        let oe = OeAction::from_parser(ins.or_action);
        emit_fk_checks(&mut b, fk_checks, table, rec_start, oe, row_skip)?;
        b.resolve(row_skip);
    }

    let record = b.alloc_reg();
    b.emit(Opcode::MakeRecord, rec_start, ncol as i32, record);
    b.emit(Opcode::Insert, cursor, record, rowid_reg);

    emit_index_inserts(
        &mut b,
        indexes,
        table,
        rec_start,
        rowid_reg,
        index_cursor_base,
    )?;

    b.resolve(sort_next);
    b.emit_jump(Opcode::SorterNext, sorter, insert_top_label, 0);
    b.resolve(end_insert);

    b.emit(Opcode::Halt, 0, 0, 0);

    b.resolve(setup);
    b.emit_jump(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

/// The register number where `SorterData` writes the decoded current record.
fn sorter_data_reg() -> i32 {
    // SorterData in this engine decodes the current record into the sorter cursor itself and
    // `Column` reads from there. We use cursor 1 as the sorter; Column reads from it below via
    // an explicit register base. The actual register is irrelevant because we use SCopy/Null into
    // the target registers directly from the sorter cursor's decoded record? No — SCopy reads from
    // registers. We therefore need the selected source columns in registers. We handle this by
    // decoding the sorter record into a contiguous register block after SorterData: use Column
    // from the sorter cursor into a fresh register block.
    // NOTE: This helper is replaced by explicit decode below.
    0
}

/// Append the index-insert maintenance sequence for one row. `rec_start` holds the table record
/// registers; `rowid_reg` holds the rowid; `index_cursor_base` is the first index write cursor.
fn emit_index_inserts(
    b: &mut ProgramBuilder,
    indexes: &[IndexObject],
    table: &Table,
    rec_start: i32,
    rowid_reg: i32,
    index_cursor_base: i32,
) -> Result<()> {
    for (i, idx) in indexes.iter().enumerate() {
        let ic = index_cursor_base + i as i32;
        let indexed_cis = idx.table_column_indices(table)?;
        let nkey = idx.nkey_fields() as i32 + 1;

        let skip_label = if let Some(pred) = &idx.where_clause {
            let skip = b.new_label();
            let pred_ctx = Ctx {
                table,
                cursor: 0,
                register_base: None, join_tables: None,
                index_read: None,
                subquery_resolver: None,
            };
            compile_pred_jump(
                b,
                pred,
                skip,
                table,
                rec_start,
                indexed_cis.as_slice(),
                pred_ctx,
            )?;
            Some(skip)
        } else {
            None
        };

        let key_start = b.alloc_regs(nkey);
        let mut plain_iter = indexed_cis.iter();
        for (j, icol) in idx.columns.iter().enumerate() {
            let target = key_start + j as i32;
            if let Some(expr) = &icol.expr {
                let expr_ctx = Ctx {
                    table,
                    cursor: 0,
                    register_base: Some(rec_start), join_tables: None,
                    index_read: None,
                    subquery_resolver: None,
                };
                compile_expr(b, expr, target, expr_ctx)?;
            } else {
                let col_idx = *plain_iter
                    .next()
                    .expect("plain column aligned with indexed_cis");
                b.emit(Opcode::SCopy, rec_start + col_idx as i32, target, 0);
            }
        }
        b.emit(
            Opcode::SCopy,
            rowid_reg,
            key_start + idx.nkey_fields() as i32,
            0,
        );
        let key_rec = b.alloc_reg();
        b.emit(Opcode::MakeRecord, key_start, nkey, key_rec);
        let ins_idx = b.emit(Opcode::IdxInsert, ic, key_rec, 0);
        let mut p5 = P5_NCHANGE;
        if idx.unique {
            p5 |= P5_UNIQUE;
            // Encode the per-index OE in p5 bits 4-7 so the executor can override the statement
            // level `OR <action>` for this index's UNIQUE violation (M12.9). Today the OE is
            // always `OE_Abort` because `CREATE [UNIQUE] INDEX` doesn't accept `ON CONFLICT`;
            // autoindex-creating PK/UNIQUE constraints would thread their OE here too. A zero
            // high nibble means "no per-constraint override; use `default_oe`".
            if idx.unique_oe != OeAction::Abort {
                p5 |= (idx.unique_oe as u8 & 0x0F) << 4;
            }
            if let Some(msg) = idx.unique_constraint_message(table) {
                b.set_p4(ins_idx, P4::Text(msg));
            } else {
                b.set_p4(ins_idx, P4::Int(0));
            }
        } else {
            b.set_p4(ins_idx, P4::Int(0));
        }
        b.set_p5(ins_idx, p5);

        if let Some(skip) = skip_label {
            b.resolve(skip);
        }
    }
    Ok(())
}

/// Validate that every index's plain columns are present on the table.
fn validate_indexes(table: &Table, indexes: &[IndexObject]) -> Result<()> {
    for idx in indexes {
        for ic in &idx.columns {
            if ic.is_expression() {
                continue;
            }
            if table.column_index(&ic.name).is_none() {
                return Err(Error::msg(format!(
                    "index {} references unknown column {} on table {}",
                    idx.name, ic.name, table.name
                )));
            }
        }
    }
    Ok(())
}

/// Emit the pre-table-Insert conflict checks for `OE_Ignore` and `OE_Replace` (mirrors the
/// UNIQUE-index section of `sqlite3GenerateConstraintChecks` in `insert.c`). For each unique
/// index, build the new key's prefix registers, emit `NoConflict` to jump to `no_conflict`
/// when no existing entry matches, and on conflict:
///
/// * `OE_Ignore`: jump to `row_skip` (the caller resolves it past the table Insert + index
/// Emit `FkCheck` opcodes for each FK constraint on `table`, after the row's column values are
/// in `rec_start..rec_start+ncol` and before the table `Insert`. The violation handler emits a
/// `Halt` with `p5 = 4` (the "FOREIGN KEY constraint failed" prefix) and the OE-appropriate `p2`.
/// For `OE_Ignore`, the violation jumps to `row_skip` instead (the row is silently skipped,
/// matching upstream's `if( pIdx==0 && ... ) goto ignore_dest` FK-ignore path).
fn emit_fk_checks(
    b: &mut ProgramBuilder,
    fk_checks: &[FkCheckP4],
    table: &Table,
    rec_start: i32,
    oe: OeAction,
    row_skip: Label,
) -> Result<()> {
    for (fk_id, fk) in fk_checks.iter().enumerate() {
        // Allocate a contiguous block of registers for the child key and SCopy each FK column
        // from the row's record registers.
        let n = fk.child_columns.len() as i32;
        let key_start = b.alloc_regs(n);
        for (i, col_name) in fk.child_columns.iter().enumerate() {
            let target = key_start + i as i32;
            let col_idx = table.column_index(col_name).ok_or_else(|| {
                Error::msg(format!(
                    "FK references unknown column {col_name} on table {}",
                    table.name
                ))
            })?;
            b.emit(Opcode::SCopy, rec_start + col_idx as i32, target, 0);
        }
        // Emit the FkCheck. p1 = key_start, p2 = violation label, p3 = fk_id. FkCheck jumps to
        // p2 when the parent is missing; falls through when the parent exists (OK). The
        // fall-through must skip over the violation handler (the Halt), so we emit a `Goto
        // ok_label` after FkCheck, then the violation handler, then resolve ok_label.
        let ok_label = b.new_label();
        let violation = b.new_label();
        let fc = b.emit_jump(Opcode::FkCheck, key_start, violation, fk_id as i32);
        b.set_p4(fc, P4::FkCheck(fk.clone()));
        // Fall-through = parent exists (OK) → skip the violation handler.
        b.emit_jump(Opcode::Goto, 0, ok_label, 0);
        // violation: the parent row is missing.
        b.resolve(violation);
        match oe {
            OeAction::Ignore => {
                b.emit_jump(Opcode::Goto, 0, row_skip, 0);
            }
            OeAction::None => {
                // The p5=4 prefix "FOREIGN KEY constraint failed: " is added by the executor's
                // Halt arm; the message body is just "child.col".
                let msg = format!("{}.{}", fk.child_table, fk.child_columns.join(", "));
                let halt = b.emit(Opcode::Halt, crate::error::ResultCode::Constraint as i32, OeAction::Abort as i32, 0);
                b.set_p4(halt, P4::Text(msg));
                b.set_p5(halt, 4);
            }
            other => {
                let msg = format!("{}.{}", fk.child_table, fk.child_columns.join(", "));
                let halt = b.emit(Opcode::Halt, crate::error::ResultCode::Constraint as i32, other as i32, 0);
                b.set_p4(halt, P4::Text(msg));
                b.set_p5(halt, 4);
            }
        }
        b.resolve(ok_label);
    }
    Ok(())
}

/// Conflict-resolution pre-checks for UNIQUE constraints (mirrors the UNIQUE pre-check block in
/// `sqlite3GenerateConstraintChecks`). For each unique index, emit a `NoConflict` seek; on
/// conflict:
///
/// * `OE_Ignore`: jump to `row_skip` — BEFORE the table Insert — so the conflicting row is never
///   written (the row is silently dropped, matching `INSERT OR IGNORE`).
/// * `OE_Replace`: fetch the conflicting row's rowid via `IdxRowid`, seek the table cursor
///   to it, delete its entries from every index, delete the table row, then fall through
///   (the subsequent table Insert + IdxInserts will now succeed because the conflict is gone).
/// * `OE_Abort`/`OE_Fail`/`OE_Rollback`: emit a `Halt` with the constraint message BEFORE the
///   table Insert so the failing row's partial writes are never made and prior rows in the
///   same statement stay clean (mirrors upstream's "OE_Fail and OE_Ignore must happen before
///   any changes are made" rule in `sqlite3GenerateConstraintChecks`).
fn emit_conflict_prechecks(
    b: &mut ProgramBuilder,
    indexes: &[IndexObject],
    table: &Table,
    rec_start: i32,
    rowid_reg: i32,
    index_cursor_base: i32,
    table_cursor: i32,
    oe: OeAction,
    row_skip: super::builder::Label,
    skip_indexes: &[usize],
) -> Result<()> {
    // For OE_None (no conflict resolution requested) there's nothing to pre-check — the
    // IdxInsert's P5_UNIQUE will raise the error after the table Insert (the legacy behavior,
    // kept for compatibility with code paths that don't set an OE).
    if oe == OeAction::None {
        return Ok(());
    }
    for (i, idx) in indexes.iter().enumerate() {
        if skip_indexes.contains(&i) {
            continue;
        }
        if !idx.unique {
            continue;
        }
        let ic = index_cursor_base + i as i32;
        let indexed_cis = idx.table_column_indices(table)?;
        let nfield = idx.nkey_fields() as i32;
        let nkey = nfield + 1;

        // Build the new key prefix registers (the indexed columns; the trailing rowid is
        // included for IdxRowid/IdxDelete but not for the NoConflict probe's field count).
        let key_start = b.alloc_regs(nkey);
        let mut plain_iter = indexed_cis.iter();
        for (j, icol) in idx.columns.iter().enumerate() {
            let target = key_start + j as i32;
            if let Some(expr) = &icol.expr {
                let expr_ctx = Ctx {
                    table,
                    cursor: 0,
                    register_base: Some(rec_start), join_tables: None,
                    index_read: None,
                    subquery_resolver: None,
                };
                compile_expr(b, expr, target, expr_ctx)?;
            } else {
                let col_idx = *plain_iter
                    .next()
                    .expect("plain column aligned with indexed_cis");
                b.emit(Opcode::SCopy, rec_start + col_idx as i32, target, 0);
            }
        }
        b.emit(Opcode::SCopy, rowid_reg, key_start + nfield, 0);

        let no_conflict = b.new_label();
        let nc = b.emit_jump(Opcode::NoConflict, ic, no_conflict, key_start);
        b.set_p4(nc, P4::Int(nfield as i64));

        // Fall-through: conflict on this index.
        match oe {
            OeAction::Ignore => {
                b.emit_jump(Opcode::Goto, 0, row_skip, 0);
            }
            OeAction::Replace => {
                // Fetch the conflicting row's rowid from this index.
                let conflict_rowid = b.alloc_reg();
                b.emit(Opcode::IdxRowid, ic, conflict_rowid, 0);
                // Seek the table cursor to the conflicting row; if it's gone, skip the delete
                // (the conflict was with a stale index entry that's already being replaced).
                b.emit_jump(Opcode::NotExists, table_cursor, no_conflict, conflict_rowid);
                // Delete this index's entry for the old row. The old key prefix matches the new
                // key prefix (that's why we're here); overwrite the trailing rowid with the
                // conflict rowid so IdxDelete targets the right entry.
                b.emit(Opcode::SCopy, conflict_rowid, key_start + nfield, 0);
                b.emit(Opcode::IdxDelete, ic, key_start, nkey);
                // Delete the other indexes' entries for the old row. Read the old row's columns
                // from the table (the table cursor is now positioned on it).
                let old_row_start = b.alloc_regs(table.columns.len() as i32);
                for ci in 0..table.columns.len() {
                    b.emit(Opcode::Column, table_cursor, ci as i32, old_row_start + ci as i32);
                }
                for (j, other_idx) in indexes.iter().enumerate() {
                    if j == i {
                        continue;
                    }
                    let oic = index_cursor_base + j as i32;
                    let other_cis = other_idx.table_column_indices(table)?;
                    let onkey = other_idx.nkey_fields() as i32 + 1;
                    let old_key = b.alloc_regs(onkey);
                    let mut other_plain = other_cis.iter();
                    for (k, oicol) in other_idx.columns.iter().enumerate() {
                        let target = old_key + k as i32;
                        if let Some(expr) = &oicol.expr {
                            let expr_ctx = Ctx {
                                table,
                                cursor: 0,
                                register_base: Some(old_row_start), join_tables: None,
                                index_read: None,
                                subquery_resolver: None,
                            };
                            compile_expr(b, expr, target, expr_ctx)?;
                        } else {
                            let col_idx = *other_plain
                                .next()
                                .expect("plain column aligned with other_cis");
                            b.emit(Opcode::SCopy, old_row_start + col_idx as i32, target, 0);
                        }
                    }
                    b.emit(Opcode::SCopy, conflict_rowid, old_key + other_idx.nkey_fields() as i32, 0);
                    // Partial index: skip the delete when the old row doesn't satisfy the
                    // predicate (no index entry exists for it).
                    let skip_del = if let Some(pred) = &other_idx.where_clause {
                        let skip = b.new_label();
                        let pred_ctx = Ctx {
                            table,
                            cursor: 0,
                            register_base: None, join_tables: None,
                            index_read: None,
                            subquery_resolver: None,
                        };
                        compile_pred_jump(b, pred, skip, table, old_row_start, other_cis.as_slice(), pred_ctx)?;
                        Some(skip)
                    } else {
                        None
                    };
                    b.emit(Opcode::IdxDelete, oic, old_key, onkey);
                    if let Some(skip) = skip_del {
                        b.resolve(skip);
                    }
                }
                // Delete the table row.
                let del = b.emit(Opcode::Delete, table_cursor, 0, 0);
                b.set_p5(del, P5_NCHANGE);
                // Restore the new key's trailing rowid (we overwrote it with the conflict
                // rowid above) so the post-Insert IdxInsert sees the correct new entry.
                b.emit(Opcode::SCopy, rowid_reg, key_start + nfield, 0);
            }
            OeAction::Abort | OeAction::Fail | OeAction::Rollback => {
                // Halt BEFORE the table Insert so the failing row's partial writes are never
                // made. `p1 = SQLITE_CONSTRAINT`, `p2 = OE` so `step()` does the right cleanup,
                // `p4` carries the column-list message (the executor's `Halt` arm prepends the
                // "UNIQUE constraint failed: " prefix from `p5 = P5_ConstraintUnique` = 2),
                // mirroring upstream's `sqlite3UniqueConstraint` → `sqlite3HaltConstraint`.
                let msg = idx
                    .unique_constraint_message(table)
                    .map(|m| m.strip_prefix("UNIQUE constraint failed: ").unwrap_or(&m).to_string())
                    .unwrap_or_default();
                let halt = b.emit(Opcode::Halt, crate::error::ResultCode::Constraint as i32, oe as i32, 0);
                b.set_p4(halt, P4::Text(msg));
                b.set_p5(halt, 2); // P5_ConstraintUnique
            }
            _ => unreachable!("emit_conflict_prechecks: unexpected OE {oe:?}"),
        }

        b.resolve(no_conflict);
    }
    Ok(())
}

/// Open write cursors for all indexes starting at cursor 1. Returns the base cursor number.
fn open_index_cursors(b: &mut ProgramBuilder, indexes: &[IndexObject]) -> Result<i32> {
    let index_cursor_base: i32 = 1;
    for (i, idx) in indexes.iter().enumerate() {
        let ic = index_cursor_base + i as i32;
        let open = b.emit(Opcode::OpenWrite, ic, idx.rootpage as i32, 0);
        let key_info: Vec<crate::vdbe::KeyField> = idx
            .columns
            .iter()
            .map(|ic| crate::vdbe::KeyField {
                desc: ic.desc,
                collation: ic.collation,
            })
            .collect();
        b.set_p4(open, P4::KeyInfo(key_info));
    }
    Ok(index_cursor_base)
}

/// Whether an instruction uses p2 as an absolute jump target.
fn is_absolute_jump(inst: &crate::vdbe::program::Instruction) -> bool {
    matches!(
        inst.opcode,
        Opcode::Goto
            | Opcode::Init
            | Opcode::If
            | Opcode::IfNot
            | Opcode::IfPos
            | Opcode::DecrJumpZero
    )
}

/// Compile the rowid value for an `INTEGER PRIMARY KEY` column into `rowid_reg`. A NULL value
/// means "auto-assign" — `NewRowid` will pick max+1 — so we leave the register NULL and let the
/// caller fall through to `NewRowid`. A concrete value is loaded as an integer.
fn compile_rowid_alias(
    b: &mut ProgramBuilder,
    expr: &Expr,
    rowid_reg: i32,
    ctx: Ctx,
) -> Result<()> {
    compile_expr(b, expr, rowid_reg, ctx)?;
    // INTEGER affinity coerces a stored value to an integer; a NULL stays NULL and is handled by
    // the NewRowid that follows when the value is the rowid alias.
    apply_affinity(b, rowid_reg, Affinity::Integer);
    Ok(())
}

/// Emit an `Affinity` opcode coercing the single register `reg` to `affinity` (no-op for BLOB,
/// which applies no coercion, matching upstream's omission of an `OP_Affinity` for it).
fn apply_affinity(b: &mut ProgramBuilder, reg: i32, affinity: Affinity) {
    if affinity == Affinity::Blob {
        return;
    }
    let code = affinity_char(affinity);
    let idx = b.emit(Opcode::Affinity, reg, 1, 0);
    b.set_p4(idx, P4::Symbol((code as char).to_string()));
}

/// Public wrapper so the upsert codegen can reuse the affinity coercion.
pub(crate) fn apply_affinity_pub(b: &mut ProgramBuilder, reg: i32, affinity: Affinity) {
    apply_affinity(b, reg, affinity);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{SchemaObject, Table};
    use rustqlite_parser::{parse, Stmt};

    fn table_of(create: &str) -> Table {
        let obj = SchemaObject {
            rowid: 1,
            obj_type: "table".into(),
            name: "t".into(),
            tbl_name: "t".into(),
            rootpage: 2,
            sql: Some(create.into()),
        };
        Table::from_schema_object(&obj).unwrap()
    }

    fn insert_of(sql: &str) -> InsertStmt {
        match parse(sql).unwrap().into_iter().next().unwrap() {
            Stmt::Insert(i) => i,
            _ => panic!("expected INSERT"),
        }
    }

    #[test]
    fn positional_insert_uses_newrowid() {
        let t = table_of("CREATE TABLE t(a, b)");
        let ins = insert_of("INSERT INTO t VALUES (1, 'x'), (2, 'y');");
        let prog = compile_insert(&ins, &t, &[], None, &[], &[]).unwrap();
        let names: Vec<&str> = prog.instructions.iter().map(|i| i.opcode.name()).collect();
        assert!(names.contains(&"OpenWrite"));
        // Two rows → two NewRowid + two Insert (no rowid alias).
        assert_eq!(names.iter().filter(|n| **n == "NewRowid").count(), 2);
        assert_eq!(names.iter().filter(|n| **n == "Insert").count(), 2);
        // The write Transaction carries p2 = 1.
        let txn = prog
            .instructions
            .iter()
            .find(|i| i.opcode == Opcode::Transaction)
            .unwrap();
        assert_eq!(txn.p2, 1);
    }

    #[test]
    fn rowid_alias_guards_newrowid_with_notnull() {
        let t = table_of("CREATE TABLE t(id INTEGER PRIMARY KEY, v)");
        let ins = insert_of("INSERT INTO t VALUES (5, 'x');");
        let prog = compile_insert(&ins, &t, &[], None, &[], &[]).unwrap();
        let names: Vec<&str> = prog.instructions.iter().map(|i| i.opcode.name()).collect();
        // The alias value becomes the rowid; NewRowid is emitted but guarded by NotNull so it only
        // runs when the supplied value is NULL (auto-assign).
        assert!(names.contains(&"NotNull"));
        assert_eq!(names.iter().filter(|n| **n == "NewRowid").count(), 1);
        assert_eq!(names.iter().filter(|n| **n == "Insert").count(), 1);
    }

    #[test]
    fn explicit_column_list_maps_values() {
        let t = table_of("CREATE TABLE t(a, b, c)");
        let ins = insert_of("INSERT INTO t (b, a) VALUES (10, 20);");
        let prog = compile_insert(&ins, &t, &[], None, &[], &[]).unwrap();
        // 3 record slots are allocated per row; the unlisted column c is NULL.
        let null_count = prog
            .instructions
            .iter()
            .filter(|i| i.opcode == Opcode::Null)
            .count();
        assert!(null_count >= 1, "unlisted column should load NULL");
    }

    #[test]
    fn default_values_uses_column_defaults() {
        let t = table_of("CREATE TABLE t(a INT DEFAULT 42, b TEXT DEFAULT 'hi', c)");
        let ins = insert_of("INSERT INTO t DEFAULT VALUES;");
        let prog = compile_insert(&ins, &t, &[], None, &[], &[]).unwrap();
        let names: Vec<&str> = prog.instructions.iter().map(|i| i.opcode.name()).collect();
        assert!(names.contains(&"OpenWrite"));
        assert_eq!(names.iter().filter(|n| **n == "NewRowid").count(), 1);
        assert_eq!(names.iter().filter(|n| **n == "Insert").count(), 1);
        // The default expressions are compiled as literals (Integer, String8).
        assert!(prog
            .instructions
            .iter()
            .any(|i| { i.opcode == Opcode::Integer && i.p1 == 42 }));
        assert!(prog.instructions.iter().any(|i| matches!(
            i.p4,
            crate::vdbe::program::P4::Text(ref s) if s == "hi"
        )));
    }

    #[test]
    fn default_values_rowid_alias_auto_assigns() {
        let t = table_of("CREATE TABLE t(id INTEGER PRIMARY KEY, v INT DEFAULT 7)");
        let ins = insert_of("INSERT INTO t DEFAULT VALUES;");
        let prog = compile_insert(&ins, &t, &[], None, &[], &[]).unwrap();
        let names: Vec<&str> = prog.instructions.iter().map(|i| i.opcode.name()).collect();
        // The rowid alias has no explicit default, so NewRowid is guarded by NotNull.
        assert!(names.contains(&"NotNull"));
        assert_eq!(names.iter().filter(|n| **n == "NewRowid").count(), 1);
        assert_eq!(names.iter().filter(|n| **n == "Insert").count(), 1);
    }
}
