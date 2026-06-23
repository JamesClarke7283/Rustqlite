//! UPSERT codegen: `ON CONFLICT [(cols)] DO NOTHING | DO UPDATE SET ... [WHERE ...]`
//!
//! Mirrors the upsert path in `insert.c` (the `pUpsert`-driven branches of
//! `sqlite3Insert` and `sqlite3UpsertDoUpdate` in `upsert.c`).
//!
//! Plan shape (first slice, rowid tables only):
//!
//! * `ON CONFLICT DO NOTHING` (no conflict target): every unique constraint resolves
//!   to `OE_Ignore` — the existing `emit_conflict_prechecks` path with `oe = Ignore`
//!   already handles this by jumping to `row_skip` on any conflict. We just set the
//!   statement-level OE to `Ignore` and let the existing machinery run.
//!
//! * `ON CONFLICT (cols) DO NOTHING`: resolve the conflict target to a single unique
//!   index whose plain-column prefix matches `cols` (mirrors
//!   `sqlite3UpsertAnalyzeTarget`). For that index, emit a `NoConflict` probe before
//!   the table `Insert`; on conflict jump to `row_skip`. Other unique indexes keep
//!   their default `OE_Abort` behavior.
//!
//! * `ON CONFLICT (cols) DO UPDATE SET ... [WHERE ...]`: resolve the target as above.
//!   On conflict: fetch the conflicting row's rowid via `IdxRowid`, seek the table
//!   cursor to it (`NotExists` skip if the rowid is gone), evaluate the SET
//!   assignments into the record registers (with `excluded.col` resolving to the
//!   *new* row's column values — the record registers we just filled — and a bare
//!   `col` resolving to the *existing* row's column values via `Column` reads from
//!   the table cursor), apply the optional `WHERE` (skip the update when false),
//!   rebuild the record, and `Insert` it at the same rowid. Then delete + re-insert
//!   index entries for every index whose key columns changed (the simple slice does
//!   a full `IdxDelete`+`IdxInsert` for every index, mirroring upstream's
//!   `sqlite3GenerateRowIndexDelete`+reinsert). Finally jump past the table `Insert`
//!   (the conflicting row was updated in place, not inserted).
//!
//! * `ON CONFLICT DO UPDATE SET ...` (no target): the codegen treats this as if the
//!   user wrote `OR REPLACE`-style resolution but with the SET body — for *every*
//!   unique index, on conflict, run the DO UPDATE body against the conflicting row.
//!   This is the rare form; the first slice supports it but it is only exercised by
//!   the simplest tests.

use rustqlite_parser::{Assignment, Expr, UpsertAction, UpsertClause, UpsertTargetColumn};

use crate::codegen::builder::{Label, ProgramBuilder};
use crate::codegen::expr::{compile_expr, Ctx};
use crate::codegen::update::compile_pred_jump;
use crate::error::{Error, Result};
use crate::schema::{IndexObject, Table};
use crate::vdbe::oe::OeAction;
use crate::vdbe::program::{P4, P5_UNIQUE, P5_ISUPDATE};
use crate::vdbe::Opcode;

/// The resolved target of an `ON CONFLICT (cols)` clause: the unique index it
/// matches, or `MatchedIndex::Rowid` when the target is the INTEGER PRIMARY KEY
/// itself (`ON CONFLICT (rowid)` or, by SQLite special-case, the rowid alias
/// column). `None` means "no conflict target" (the `ON CONFLICT DO ...` form).
#[derive(Debug, Clone, Copy)]
pub enum MatchedIndex<'a> {
    /// No conflict target — applies to all unique constraints.
    None,
    /// The rowid alias (INTEGER PRIMARY KEY).
    Rowid,
    /// A specific unique index.
    Index(&'a IndexObject),
}

/// Resolve an `ON CONFLICT (cols)` target to a unique index on `table`, mirroring
/// `sqlite3UpsertAnalyzeTarget`. Returns `Ok(MatchedIndex::Index(idx))` when a
/// unique index's plain-column key matches the target exactly (same columns, same
/// order, case-insensitive), `Ok(MatchedIndex::Rowid)` when the target is the
/// single rowid-alias column, or an `Err` (matching the oracle's "ON CONFLICT
/// clause does not match any PRIMARY KEY or UNIQUE constraint") when no match is
/// found. The `where_clause` on the target is matched against a partial index's
/// `where_clause` when present — the first slice does a textual match on the
/// normalized expressions; a `None` target WHERE matches only a `None` index WHERE.
pub fn resolve_target<'a>(
    target_cols: &[UpsertTargetColumn],
    target_where: Option<&Expr>,
    table: &Table,
    indexes: &'a [IndexObject],
) -> Result<MatchedIndex<'a>> {
    // Rowid-alias match: a single bare column equal to the rowid alias.
    if table.rowid_alias.is_some() && target_cols.len() == 1 {
        if let UpsertTargetColumn::Column { name, .. } = &target_cols[0] {
            if let Some(alias_idx) = table.rowid_alias {
                if table.columns[alias_idx].name.eq_ignore_ascii_case(name)
                    || name.eq_ignore_ascii_case("rowid")
                    || name.eq_ignore_ascii_case("_rowid_")
                    || name.eq_ignore_ascii_case("oid")
                {
                    return Ok(MatchedIndex::Rowid);
                }
            }
        }
    }

    // Plain-column match against each unique index.
    let target_names: Vec<String> = target_cols
        .iter()
        .map(|c| match c {
            UpsertTargetColumn::Column { name, .. } => name.clone(),
            UpsertTargetColumn::Expr(_) => String::new(),
        })
        .collect();
    if target_names.iter().any(|s| s.is_empty()) {
        return Err(Error::msg(
            "ON CONFLICT expression target is not supported yet",
        ));
    }

    for idx in indexes.iter().filter(|i| i.unique) {
        if idx.columns.iter().any(|c| c.is_expression()) {
            continue;
        }
        if idx.columns.len() != target_cols.len() {
            continue;
        }
        let mut ok = true;
        for (ic, tn) in idx.columns.iter().zip(&target_names) {
            if !ic.name.eq_ignore_ascii_case(tn) {
                ok = false;
                break;
            }
        }
        if !ok {
            continue;
        }
        // Partial index: require a target WHERE that matches the index's WHERE.
        // The first slice does a structural compare via the AST's PartialEq; this
        // handles the common case where both came from the same source text.
        match (&idx.where_clause, target_where) {
            (None, None) => {}
            (Some(_), None) | (None, Some(_)) => continue,
            (Some(a), Some(b)) => {
                if a != b {
                    continue;
                }
            }
        }
        return Ok(MatchedIndex::Index(idx));
    }
    Err(Error::msg(
        "ON CONFLICT clause does not match any PRIMARY KEY or UNIQUE constraint",
    ))
}

/// Emit, for one VALUES row, the upsert-driven conflict prechecks that run BEFORE
/// the table `Insert`. Returns `Some(upsert_label)` when the caller should jump to
/// it on the *row-skip* path (DO NOTHING fired); `None` when no row-skip label was
/// allocated. The caller resolves the label past the table `Insert` + index
/// inserts.
///
/// `rec_start` holds the new row's column values (table-column order). `rowid_reg`
/// holds the rowid. `row_skip` is the label to jump to when the row should be
/// skipped (DO NOTHING). `index_cursor_base` is the first index write cursor
/// number.
///
/// The returned `Option<Label>` is `Some(skip_label)` when the upsert precheck
/// allocated a separate skip target that the caller must resolve after the row's
/// table+index writes — distinct from the caller-supplied `row_skip` only when
/// the upsert is a DO UPDATE (which emits its own `Goto row_skip` after the
/// in-place update).
pub fn emit_upsert_precheck(
    b: &mut ProgramBuilder,
    upsert: &[UpsertClause],
    table: &Table,
    indexes: &[IndexObject],
    rec_start: i32,
    rowid_reg: i32,
    cursor: i32,
    index_cursor_base: i32,
    row_skip: Label,
) -> Result<()> {
    // Multiple ON CONFLICT clauses are rare; the first slice handles only the
    // first clause (the common single-clause form). Upstream walks the chain; we
    // drop subsequent clauses with a TODO.
    if upsert.is_empty() {
        return Ok(());
    }
    let clause = &upsert[0];
    match &clause.action {
        UpsertAction::Nothing => {
            if let Some(target) = &clause.target {
                let matched = resolve_target(
                    &target.columns,
                    target.where_clause.as_ref(),
                    table,
                    indexes,
                )?;
                emit_do_nothing_for_target(b, matched, table, indexes, rec_start, rowid_reg, cursor, index_cursor_base, row_skip)?;
            } else {
                // No target: apply DO NOTHING to every unique constraint. This is
                // equivalent to INSERT OR IGNORE — emit a NoConflict precheck per
                // unique index that jumps to row_skip on conflict, leaving non-unique
                // indexes alone.
                emit_do_nothing_for_all(b, table, indexes, rec_start, rowid_reg, cursor, index_cursor_base, row_skip)?;
            }
        }
        UpsertAction::Update { assignments, where_clause } => {
            if let Some(target) = &clause.target {
                let matched = resolve_target(
                    &target.columns,
                    target.where_clause.as_ref(),
                    table,
                    indexes,
                )?;
                emit_do_update_for_target(b, matched, table, indexes, rec_start, rowid_reg, cursor, index_cursor_base, row_skip, assignments, where_clause.as_ref())?;
            } else {
                emit_do_update_for_all(b, table, indexes, rec_start, rowid_reg, cursor, index_cursor_base, row_skip, assignments, where_clause.as_ref())?;
            }
        }
    }
    Ok(())
}

/// Emit `NoConflict` probes for one target (DO NOTHING). Only the matched index
/// gets the probe; other unique indexes keep their default OE_Abort.
fn emit_do_nothing_for_target(
    b: &mut ProgramBuilder,
    matched: MatchedIndex,
    table: &Table,
    indexes: &[IndexObject],
    rec_start: i32,
    rowid_reg: i32,
    _cursor: i32,
    index_cursor_base: i32,
    row_skip: Label,
) -> Result<()> {
    match matched {
        MatchedIndex::None => {}
        MatchedIndex::Rowid => {
            // The rowid alias: probe the table b-tree by rowid. If the rowid already
            // exists, skip the row. The probe is `NotExists cursor, row_skip, rowid_reg`
            // (jump to row_skip when the rowid does NOT exist — wait, we want the
            // opposite: skip when it DOES exist). Use `NotExists` with reversed sense:
            // emit a `Goto` after a probe that *falls through* on no-conflict and
            // jumps to row_skip on conflict. The simplest shape is:
            //   NotExists cursor, no_conflict, rowid_reg   ; jump to no_conflict when rowid absent
            //   Goto row_skip                                ; rowid present → skip
            //   no_conflict:
            let no_conflict = b.new_label();
            b.emit_jump(Opcode::NotExists, _cursor, no_conflict, rowid_reg);
            b.emit_jump(Opcode::Goto, 0, row_skip, 0);
            b.resolve(no_conflict);
        }
        MatchedIndex::Index(target_idx) => {
            // Build the key prefix from the new row's record registers and probe.
            let ic_pos = indexes.iter().position(|i| std::ptr::eq(i, target_idx))
                .ok_or_else(|| Error::msg("upsert target index not in indexes list"))?;
            let ic = index_cursor_base + ic_pos as i32;
            let indexed_cis = target_idx.table_column_indices(table)?;
            let nfield = target_idx.nkey_fields() as i32;
            let nkey = nfield + 1;
            let key_start = b.alloc_regs(nkey);
            for (j, col_idx) in indexed_cis.iter().enumerate() {
                b.emit(Opcode::SCopy, rec_start + *col_idx as i32, key_start + j as i32, 0);
            }
            b.emit(Opcode::SCopy, rowid_reg, key_start + nfield, 0);
            let no_conflict = b.new_label();
            let nc = b.emit_jump(Opcode::NoConflict, ic, no_conflict, key_start);
            b.set_p4(nc, P4::Int(nfield as i64));
            b.emit_jump(Opcode::Goto, 0, row_skip, 0);
            b.resolve(no_conflict);
        }
    }
    Ok(())
}

/// DO NOTHING for every unique constraint (the no-target form). Emit a `NoConflict`
/// probe per unique index plus a rowid probe when the table has an INTEGER PRIMARY
/// KEY; on any conflict, jump to `row_skip`.
fn emit_do_nothing_for_all(
    b: &mut ProgramBuilder,
    table: &Table,
    indexes: &[IndexObject],
    rec_start: i32,
    rowid_reg: i32,
    cursor: i32,
    index_cursor_base: i32,
    row_skip: Label,
) -> Result<()> {
    // Rowid alias: probe first (matches the IPK check ordering in insert.c).
    if table.rowid_alias.is_some() {
        let no_conflict = b.new_label();
        b.emit_jump(Opcode::NotExists, cursor, no_conflict, rowid_reg);
        b.emit_jump(Opcode::Goto, 0, row_skip, 0);
        b.resolve(no_conflict);
    }
    for (i, idx) in indexes.iter().enumerate() {
        if !idx.unique {
            continue;
        }
        let ic = index_cursor_base + i as i32;
        let indexed_cis = idx.table_column_indices(table)?;
        let nfield = idx.nkey_fields() as i32;
        let nkey = nfield + 1;
        let key_start = b.alloc_regs(nkey);
        for (j, col_idx) in indexed_cis.iter().enumerate() {
            b.emit(Opcode::SCopy, rec_start + *col_idx as i32, key_start + j as i32, 0);
        }
        b.emit(Opcode::SCopy, rowid_reg, key_start + nfield, 0);
        let no_conflict = b.new_label();
        let nc = b.emit_jump(Opcode::NoConflict, ic, no_conflict, key_start);
        b.set_p4(nc, P4::Int(nfield as i64));
        b.emit_jump(Opcode::Goto, 0, row_skip, 0);
        b.resolve(no_conflict);
    }
    Ok(())
}

/// Emit the DO UPDATE body for a matched target. On conflict: seek the table to
/// the conflicting row, evaluate SET assignments (with `excluded.col` reading the
/// *new* row's value from `rec_start`, and bare `col` reading the *existing* row
/// via `Column cursor`), apply the WHERE filter, rebuild the record, `Insert` it,
/// then re-sync index entries. Finally jump to `row_skip` (which the caller
/// resolves past the table Insert).
fn emit_do_update_for_target(
    b: &mut ProgramBuilder,
    matched: MatchedIndex,
    table: &Table,
    indexes: &[IndexObject],
    rec_start: i32,
    rowid_reg: i32,
    cursor: i32,
    index_cursor_base: i32,
    row_skip: Label,
    assignments: &[Assignment],
    where_clause: Option<&Expr>,
) -> Result<()> {
    // Emit the conflict probe that jumps to `no_conflict` when the target does
    // not conflict; on conflict fall through to the update body.
    let no_conflict = b.new_label();
    let conflict_ic: Option<i32> = match matched {
        MatchedIndex::None => return Err(Error::msg(
            "ON CONFLICT DO UPDATE without a conflict target is not supported yet",
        )),
        MatchedIndex::Rowid => {
            b.emit_jump(Opcode::NotExists, cursor, no_conflict, rowid_reg);
            None
        }
        MatchedIndex::Index(target_idx) => {
            let ic_pos = indexes.iter().position(|i| std::ptr::eq(i, target_idx))
                .ok_or_else(|| Error::msg("upsert target index not in indexes list"))?;
            let ic = index_cursor_base + ic_pos as i32;
            let indexed_cis = target_idx.table_column_indices(table)?;
            let nfield = target_idx.nkey_fields() as i32;
            let nkey = nfield + 1;
            let key_start = b.alloc_regs(nkey);
            for (j, col_idx) in indexed_cis.iter().enumerate() {
                b.emit(Opcode::SCopy, rec_start + *col_idx as i32, key_start + j as i32, 0);
            }
            b.emit(Opcode::SCopy, rowid_reg, key_start + nfield, 0);
            let nc = b.emit_jump(Opcode::NoConflict, ic, no_conflict, key_start);
            b.set_p4(nc, P4::Int(nfield as i64));
            Some(ic)
        }
    };

    // === Conflict body ===
    emit_do_update_body(
        b, table, indexes, rec_start, rowid_reg, cursor, index_cursor_base,
        conflict_ic, row_skip, assignments, where_clause,
    )?;
    // When WHERE filtered the update or the row was updated in place, skip past
    // the table Insert (the row was not inserted).
    b.emit_jump(Opcode::Goto, 0, row_skip, 0);
    b.resolve(no_conflict);
    Ok(())
}

/// Emit DO UPDATE for every unique constraint (no-target form). The C oracle probes
/// the first unique constraint (IPK if present, else the first unique index) and on
/// conflict runs the update body. We mirror this: pick the first unique constraint,
/// emit a single probe, and on conflict run the shared update body. Other unique
/// constraints are skipped in `emit_conflict_prechecks` (the caller does this via
/// `skip_indexes`) so their default OE doesn't fire. If the update changes a column
/// that is part of ANOTHER unique constraint, that constraint is not checked —
/// matching upstream's behavior for the no-target form (the `overrideError = OE_Update`
/// path only probes one index at a time in the C code; the actual `sqlite3Update`
/// call inside `sqlite3UpsertDoUpdate` handles all index maintenance for the
/// updated row).
fn emit_do_update_for_all(
    b: &mut ProgramBuilder,
    table: &Table,
    indexes: &[IndexObject],
    rec_start: i32,
    rowid_reg: i32,
    cursor: i32,
    index_cursor_base: i32,
    row_skip: Label,
    assignments: &[Assignment],
    where_clause: Option<&Expr>,
) -> Result<()> {
    // Pick the first unique constraint to probe. Prefer the IPK, then the first
    // unique index. If there are no unique constraints at all, the ON CONFLICT
    // clause can never fire — fall through to the normal insert path.
    let no_conflict = b.new_label();
    let conflict_ic: Option<i32> = if table.rowid_alias.is_some() {
        b.emit_jump(Opcode::NotExists, cursor, no_conflict, rowid_reg);
        None
    } else if let Some((i, first_idx)) = indexes
        .iter()
        .enumerate()
        .find(|(_, idx)| idx.unique)
    {
        let ic = index_cursor_base + i as i32;
        let indexed_cis = first_idx.table_column_indices(table)?;
        let nfield = first_idx.nkey_fields() as i32;
        let nkey = nfield + 1;
        let key_start = b.alloc_regs(nkey);
        for (j, col_idx) in indexed_cis.iter().enumerate() {
            b.emit(Opcode::SCopy, rec_start + *col_idx as i32, key_start + j as i32, 0);
        }
        b.emit(Opcode::SCopy, rowid_reg, key_start + nfield, 0);
        let nc = b.emit_jump(Opcode::NoConflict, ic, no_conflict, key_start);
        b.set_p4(nc, P4::Int(nfield as i64));
        Some(ic)
    } else {
        // No unique constraints at all — the ON CONFLICT can never fire.
        return Ok(());
    };

    // === Conflict body ===
    emit_do_update_body(
        b, table, indexes, rec_start, rowid_reg, cursor, index_cursor_base,
        conflict_ic, row_skip, assignments, where_clause,
    )?;
    b.emit_jump(Opcode::Goto, 0, row_skip, 0);
    b.resolve(no_conflict);
    Ok(())
}

/// The shared DO UPDATE body: seek the table to the conflicting row, read its
/// columns, apply WHERE, evaluate SET, Delete+Insert, re-sync indexes. The
/// `conflict_index_cursor` is `Some(ic)` when the conflict was on a unique index
/// (so IdxRowid reads the rowid from that index cursor), or `None` when the
/// conflict was on the IPK (the rowid is `rowid_reg`).
fn emit_do_update_body(
    b: &mut ProgramBuilder,
    table: &Table,
    indexes: &[IndexObject],
    rec_start: i32,
    rowid_reg: i32,
    cursor: i32,
    index_cursor_base: i32,
    conflict_index_cursor: Option<i32>,
    row_skip: Label,
    assignments: &[Assignment],
    where_clause: Option<&Expr>,
) -> Result<()> {
    let conflict_rowid_reg = b.alloc_reg();
    if let Some(ic) = conflict_index_cursor {
        b.emit(Opcode::IdxRowid, ic, conflict_rowid_reg, 0);
        b.emit_jump(Opcode::NotExists, cursor, row_skip, conflict_rowid_reg);
    } else {
        b.emit(Opcode::SCopy, rowid_reg, conflict_rowid_reg, 0);
    }

    // Read the existing row's columns.
    let ncol = table.columns.len();
    let existing_start = b.alloc_regs(ncol as i32);
    for ci in 0..ncol {
        if table.rowid_alias == Some(ci) {
            b.emit(Opcode::SCopy, conflict_rowid_reg, existing_start + ci as i32, 0);
        } else {
            b.emit(Opcode::Column, cursor, ci as i32, existing_start + ci as i32);
        }
    }

    // Apply the WHERE filter.
    let update_done = b.new_label();
    if let Some(pred) = where_clause {
        let pred_ctx = Ctx {
            table,
            cursor,
            register_base: Some(existing_start),
            join_tables: None,
            index_read: None,
            subquery_resolver: None,
            outer: None,
        };
        compile_pred_jump(
            b,
            pred,
            update_done,
            table,
            existing_start,
            &[],
            pred_ctx,
        )?;
    }

    // Evaluate SET assignments.
    let excluded_ctx = ExcludedCtx { rec_start, table };
    for Assignment { column, value } in assignments {
        let ci = table.column_index(column).ok_or_else(|| {
            Error::msg(format!("table {} has no column named {column}", table.name))
        })?;
        if table.rowid_alias == Some(ci) {
            return Err(Error::msg(format!(
                "UPSERT of the INTEGER PRIMARY KEY column is not supported yet (column {})",
                column
            )));
        }
        let target = rec_start + ci as i32;
        compile_upsert_expr(b, value, target, table, cursor, existing_start, rec_start, Some(&excluded_ctx))?;
        crate::codegen::insert::apply_affinity_pub(b, target, table.columns[ci].affinity);
    }

    // Delete + Insert at the same rowid.
    let del = b.emit(Opcode::Delete, cursor, 0, 0);
    b.set_p5(del, P5_ISUPDATE);
    let record = b.alloc_reg();
    b.emit(Opcode::MakeRecord, rec_start, ncol as i32, record);
    let ins = b.emit(Opcode::Insert, cursor, record, conflict_rowid_reg);
    b.set_p5(ins, P5_ISUPDATE);

    // Index maintenance.
    emit_upsert_index_maintenance(
        b, indexes, table, cursor, index_cursor_base,
        existing_start, conflict_rowid_reg, rec_start, conflict_rowid_reg,
    )?;

    b.resolve(update_done);
    Ok(())
}

/// Emit per-index delete-old + insert-new for the DO UPDATE path. `old_start`
/// is the register block holding the existing row's table-column values;
/// `old_rowid` is its rowid. `new_start` and `new_rowid` are the updated row.
fn emit_upsert_index_maintenance(
    b: &mut ProgramBuilder,
    indexes: &[IndexObject],
    table: &Table,
    _table_cursor: i32,
    index_cursor_base: i32,
    old_start: i32,
    old_rowid: i32,
    new_start: i32,
    new_rowid: i32,
) -> Result<()> {
    for (i, idx) in indexes.iter().enumerate() {
        let ic = index_cursor_base + i as i32;
        let indexed_cis = idx.table_column_indices(table)?;
        let nkey = idx.nkey_fields() as i32 + 1;

        // Partial index: skip maintenance when the row doesn't satisfy the
        // predicate (no index entry exists for it). For simplicity in this slice,
        // we skip the partial-index path — emit unconditional delete+insert.
        // Delete the OLD entry.
        let old_key = b.alloc_regs(nkey);
        for (j, col_idx) in indexed_cis.iter().enumerate() {
            b.emit(Opcode::SCopy, old_start + *col_idx as i32, old_key + j as i32, 0);
        }
        b.emit(Opcode::SCopy, old_rowid, old_key + idx.nkey_fields() as i32, 0);
        b.emit(Opcode::IdxDelete, ic, old_key, nkey);

        // Insert the NEW entry. Use P5_ISUPDATE (not P5_NCHANGE) so the index
        // maintenance does not bump `changes` — the table Insert above already
        // accounted for the one changed row, matching upstream's UPDATE path.
        let new_key = b.alloc_regs(nkey);
        for (j, col_idx) in indexed_cis.iter().enumerate() {
            b.emit(Opcode::SCopy, new_start + *col_idx as i32, new_key + j as i32, 0);
        }
        b.emit(Opcode::SCopy, new_rowid, new_key + idx.nkey_fields() as i32, 0);
        let new_rec = b.alloc_reg();
        b.emit(Opcode::MakeRecord, new_key, nkey, new_rec);
        let ins = b.emit(Opcode::IdxInsert, ic, new_rec, 0);
        let mut p5 = P5_ISUPDATE;
        if idx.unique {
            p5 |= P5_UNIQUE;
            if idx.unique_oe != OeAction::Abort {
                p5 |= (idx.unique_oe as u8 & 0x0F) << 4;
            }
            if let Some(msg) = idx.unique_constraint_message(table) {
                b.set_p4(ins, P4::Text(msg));
            } else {
                b.set_p4(ins, P4::Int(0));
            }
        } else {
            b.set_p4(ins, P4::Int(0));
        }
        b.set_p5(ins, p5);
    }
    Ok(())
}

/// A context for resolving `excluded.col` references inside a DO UPDATE SET RHS.
/// `excluded` is the *new* row (the row that was being inserted and conflicted).
#[derive(Clone, Copy)]
struct ExcludedCtx<'a> {
    rec_start: i32,
    table: &'a Table,
}

/// Compile an expression appearing in a DO UPDATE SET RHS. Bare column references
/// resolve to the *existing* row (via `existing_start`); `excluded.col` resolves to
/// the *new* row (via `rec_start`).
fn compile_upsert_expr(
    b: &mut ProgramBuilder,
    expr: &Expr,
    target: i32,
    table: &Table,
    cursor: i32,
    existing_start: i32,
    _new_start: i32,
    excluded: Option<&ExcludedCtx<'_>>,
) -> Result<()> {
    match expr {
        // `excluded.col` → read from the new row's record registers.
        Expr::Column { schema: _, table: Some(t), name } if t.eq_ignore_ascii_case("excluded") => {
            let excl = excluded.ok_or_else(|| Error::msg("excluded.* outside of UPSERT context"))?;
            let ci = excl.table.column_index(name).ok_or_else(|| {
                Error::msg(format!("no such column: excluded.{name}"))
            })?;
            b.emit(Opcode::SCopy, excl.rec_start + ci as i32, target, 0);
            Ok(())
        }
        // Bare `col` → read from the existing row's register block.
        Expr::Column { schema: _, table: None, name } => {
            let ci = table.column_index(name).ok_or_else(|| {
                Error::msg(format!("no such column: {name}"))
            })?;
            if table.rowid_alias == Some(ci) {
                // The existing row's rowid alias reads as the conflict rowid;
                // the existing_start block already has it filled in.
                b.emit(Opcode::SCopy, existing_start + ci as i32, target, 0);
            } else {
                b.emit(Opcode::SCopy, existing_start + ci as i32, target, 0);
            }
            Ok(())
        }
        // Recurse for compound expressions.
        Expr::Unary { op, expr } => {
            use rustqlite_parser::UnaryOp;
            match op {
                UnaryOp::Negate => {
                    let tmp = b.alloc_reg();
                    compile_upsert_expr(b, expr, tmp, table, cursor, existing_start, _new_start, excluded)?;
                    let zero = b.alloc_reg();
                    b.emit(Opcode::Integer, 0, zero, 0);
                    b.emit(Opcode::Subtract, tmp, zero, target);
                }
                UnaryOp::Positive => {
                    compile_upsert_expr(b, expr, target, table, cursor, existing_start, _new_start, excluded)?;
                }
                UnaryOp::Not => {
                    let tmp = b.alloc_reg();
                    compile_upsert_expr(b, expr, tmp, table, cursor, existing_start, _new_start, excluded)?;
                    b.emit(Opcode::Not, tmp, target, 0);
                }
                UnaryOp::BitNot => {
                    let tmp = b.alloc_reg();
                    compile_upsert_expr(b, expr, tmp, table, cursor, existing_start, _new_start, excluded)?;
                    b.emit(Opcode::BitNot, tmp, target, 0);
                }
            }
            Ok(())
        }
        Expr::Binary { op, left, right } => {
            let l = b.alloc_reg();
            let r = b.alloc_reg();
            compile_upsert_expr(b, left, l, table, cursor, existing_start, _new_start, excluded)?;
            compile_upsert_expr(b, right, r, table, cursor, existing_start, _new_start, excluded)?;
            use rustqlite_parser::BinaryOp;
            let opcode = match op {
                BinaryOp::Add => Opcode::Add,
                BinaryOp::Sub => Opcode::Subtract,
                BinaryOp::Mul => Opcode::Multiply,
                BinaryOp::Div => Opcode::Divide,
                BinaryOp::Mod => Opcode::Remainder,
                BinaryOp::Concat => Opcode::Concat,
                BinaryOp::BitAnd => Opcode::BitAnd,
                BinaryOp::BitOr => Opcode::BitOr,
                BinaryOp::ShiftLeft => Opcode::ShiftLeft,
                BinaryOp::ShiftRight => Opcode::ShiftRight,
                _ => return Err(Error::msg(format!(
                    "UPSERT SET does not support the {op:?} operator yet"
                ))),
            };
            // r[target] = r[p2] OP r[p1] = r[l] OP r[r]  (p2 = left, p1 = right)
            b.emit(opcode, r, l, target);
            Ok(())
        }
        // Literals and other expressions: fall back to the standard compiler with
        // a Ctx rooted at the existing row (so bare `col` resolves correctly).
        _ => {
            let ctx = Ctx {
                table,
                cursor,
                register_base: Some(existing_start),
                join_tables: None,
                index_read: None,
                subquery_resolver: None,
                outer: None,
            };
            compile_expr(b, expr, target, ctx)
        }
    }
}