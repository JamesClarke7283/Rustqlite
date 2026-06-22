//! Compound `SELECT` codegen (`UNION` / `UNION ALL` / `INTERSECT` / `EXCEPT`) — mirrors the
//! `multiSelect` / `multiSelectByMerge` paths in `select.c`.
//!
//! Two shapes are produced, matching upstream's choice:
//!
//! * **`UNION ALL` without `ORDER BY`** — the simplest path: the left arm runs, then the right
//!   arm runs, both writing their `ResultRow`s directly to the caller. Upstream's `multiSelect`
//!   takes this same shortcut when `op == TK_ALL` and there is no `ORDER BY`. LIMIT/OFFSET
//!   apply across both arms via shared counter registers.
//!
//! * **Everything else** (`UNION` / `INTERSECT` / `EXCEPT`, and any compound with `ORDER BY`) —
//!   the merge algorithm: each arm is compiled as a coroutine that yields its rows in
//!   `ORDER BY`-key order (a synthesized `ORDER BY 1, 2, … ncol` is added when the user did not
//!   supply one, matching upstream's "invent one first" step). The main loop runs both
//!   coroutines in parallel, compares the current row of each arm under the KeyInfo, and routes
//!   to one of `AltB` / `AeqB` / `AgtB` / `EofA` / `EofB` handlers that implement the
//!   operator-specific merge logic. Duplicate removal for `UNION` / `INTERSECT` / `EXCEPT` runs
//!   inside the output subroutines (`outA` / `outB`) by keeping the previous emitted row in a
//!   `regPrev` block and skipping when the new row compares equal.
//!
//! The merge shape mirrors upstream's `multiSelectByMerge` (select.c) and the bytecode SQLite
//! 3.53 emits for `UNION` / `INTERSECT` / `EXCEPT` (with or without `ORDER BY`). A three-way
//! compound `A UNION B INTERSECT C` is lowered left-associatively: the outer `INTERSECT` is the
//! merge, and its left arm is itself a `UNION` merge compiled via `compile_multi_arm_merge`.

use rustqlite_parser::{CompoundOperator, Expr, OrderingTerm, SelectStmt};

use crate::error::{Error, Result};
use crate::schema::{IndexObject, Table};
use crate::types::Value;
use crate::vdbe::program::{Instruction, Program, P4};
use crate::vdbe::{KeyField, Opcode};

use super::builder::{Label, ProgramBuilder};
use super::expr::SubqueryResolver;
use super::select::{self, eval_limit_offset, expand_columns, emit_int};

/// Compile a compound `SELECT` (the leading `select` plus `select.compound` arms) into a VDBE
/// program plus the result column names (taken from the leading arm, matching SQLite).
pub fn compile_compound(
    select: &SelectStmt,
    table: Option<&Table>,
    indexes: &[IndexObject],
    subquery_resolver: Option<&dyn SubqueryResolver>,
) -> Result<(Program, Vec<String>)> {
    if select.compound.is_empty() {
        return Err(Error::msg("compile_compound called on a non-compound SELECT"));
    }

    // Result column names come from the leading arm (matching upstream).
    let outputs = expand_columns(select, table)?;
    let names: Vec<String> = outputs.iter().map(|(_, n)| n.clone()).collect();
    let ncol = outputs.len() as usize;

    // Validate that every arm has the same number of result columns. Mirrors upstream's
    // `sqlite3SelectWrongNumTermsError`. The error text matches the oracle.
    for (i, (op, arm)) in select.compound.iter().enumerate() {
        let arm_table = resolve_arm_table(arm, subquery_resolver)?;
        let arm_outputs = expand_columns(arm, arm_table.as_ref())?;
        if arm_outputs.len() != ncol {
            let op_name = match op {
                CompoundOperator::UnionAll => "UNION ALL",
                CompoundOperator::Union => "UNION",
                CompoundOperator::Intersect => "INTERSECT",
                CompoundOperator::Except => "EXCEPT",
            };
            // Upstream blames the operator between the left and the mismatched right arm. For
            // a multi-arm compound the mismatched arm is `select.compound[i].1`; the operator
            // naming uses that arm's operator (which is `op`).
            let _ = i;
            return Err(Error::msg(format!(
                "SELECTs to the left and right of {op_name} do not have the same number of result columns"
            )));
        }
    }

    let (limit, offset) = eval_limit_offset(select)?;

    // UNION ALL without ORDER BY (and only 2 arms) → the simple chain path. Any other operator
    // (or any compound with an ORDER BY, or a 3+-arm compound) takes the merge path.
    let is_union_all_only = select.compound.len() == 1
        && select.compound[0].0 == CompoundOperator::UnionAll
        && select.order_by.is_empty();
    let program = if is_union_all_only {
        compile_union_all_chain(select, table, indexes, subquery_resolver, limit, offset)?
    } else {
        compile_compound_merge(select, table, indexes, subquery_resolver, limit, offset)?
    };

    Ok((program, names))
}

/// `UNION ALL` without `ORDER BY`: compile the leading arm, then the trailing arm, as one
/// program. LIMIT/OFFSET apply across both (shared counter registers). The leading arm's
/// `Halt` is replaced with a fall-through to the trailing arm's scan code.
fn compile_union_all_chain(
    select: &SelectStmt,
    table: Option<&Table>,
    indexes: &[IndexObject],
    subquery_resolver: Option<&dyn SubqueryResolver>,
    limit: Option<i64>,
    offset: i64,
) -> Result<Program> {
    let (_op, trailing) = &select.compound[0];
    let trailing_table = resolve_arm_table(trailing, subquery_resolver)?;
    let trailing_indexes: Vec<IndexObject> = if trailing_table.is_some() {
        if let Some(r) = subquery_resolver {
            r.resolve(trailing).map(|(_, idxs)| idxs).unwrap_or_default()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    let mut b = ProgramBuilder::new();
    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    let after_init = b.cur_addr();

    if limit == Some(0) {
        b.emit(Opcode::Halt, 0, 0, 0);
        b.resolve(setup);
        b.emit(Opcode::Transaction, 0, 0, 0);
        b.emit(Opcode::Goto, 0, after_init, 0);
        return Ok(b.finish());
    }

    // Shared LIMIT/OFFSET counters across both arms.
    let limit_reg = match limit {
        Some(n) if n > 0 => Some(emit_int(&mut b, n)),
        _ => None,
    };
    let offset_reg = (offset > 0).then(|| emit_int(&mut b, offset));

    // Compile and inline the leading arm. Its own LIMIT/OFFSET (compiled inside its sub-program)
    // would apply only to itself; for UNION ALL we want the compound's LIMIT to span both arms.
    // Strip the leading arm's LIMIT/OFFSET before compiling so the sub-program doesn't emit its
    // own counters (we use the shared ones above via post-inlined patching). Actually simpler:
    // compile the leading arm with LIMIT/OFFSET cleared, and wrap *its* ResultRow in the shared
    // counters here.
    let mut leading_arm = select.clone();
    leading_arm.compound.clear();
    leading_arm.limit = None;
    leading_arm.offset = None;
    let leading_prog = select::compile(&leading_arm, table, indexes, subquery_resolver)?.0;

    let leading_halt_idx = leading_prog
        .instructions
        .iter()
        .position(|i| i.opcode == Opcode::Halt)
        .ok_or_else(|| Error::msg("leading arm program has no Halt"))?;
    let leading_reg_offset = b.next_reg() - 1;
    let leading_cursor_offset = b.next_cursor();
    let _ = b.alloc_regs(leading_prog.num_registers as i32);
    // Advance the outer builder's `next_cursor` past the leading arm's cursors so the trailing
    // arm's cursors land in a free range (both arms' cursors are open simultaneously in the
    // UNION ALL chain — the trailing arm runs right after the leading arm).
    b.note_cursor(leading_cursor_offset + leading_prog.num_cursors as i32 - 1);
    let mut leading_addr_map: std::collections::HashMap<i32, i32> = std::collections::HashMap::new();
    let leading_start = b.cur_addr();
    let ncol = expand_columns(select, table)?.len() as i32;

    // Inline the leading arm's scan code. Rewrite ResultRow to apply the shared OFFSET/LIMIT
    // counters (wrapping the original ResultRow). The Halt is replaced with a Goto to the
    // trailing arm's prologue (bound later).
    let trailing_prologue_label = b.new_label();
    inline_scan_with_resultrow_wrap(
        &mut b,
        &leading_prog,
        leading_halt_idx,
        leading_reg_offset,
        leading_cursor_offset,
        &mut leading_addr_map,
        ncol,
        limit_reg,
        offset_reg,
        Some(trailing_prologue_label),
    )?;
    let leading_end_addr = b.cur_addr();

    // Patch the leading arm's jumps. Collect fixups first, then apply after the borrow loop
    // (the loop holds a mutable borrow on `b.iter_insts_mut()`; `add_fixup`/`label_addr_of`
    // need a separate borrow).
    let mut fixups: Vec<(usize, Label)> = Vec::new();
    for (i, inst) in b.iter_insts_mut().enumerate() {
        let addr = i as i32;
        if addr < leading_start || addr >= leading_end_addr {
            continue;
        }
        if !is_absolute_jump(inst) {
            continue;
        }
        let sub_target = inst.p2;
        if sub_target == leading_halt_idx as i32 {
            // Jump to the trailing arm prologue. The label is unresolved here, so register a
            // fixup to patch p2 at finish() time.
            fixups.push((i, trailing_prologue_label));
        } else if let Some(&inlined) = leading_addr_map.get(&sub_target) {
            inst.p2 = inlined;
        } else if sub_target == 0 {
            inst.p2 = 0;
        }
    }
    for (idx, label) in fixups {
        b.add_fixup(idx, label);
    }

    // Trailing arm prologue.
    b.resolve(trailing_prologue_label);
    let trailing_prog = select::compile(trailing, trailing_table.as_ref(), &trailing_indexes, subquery_resolver)?.0;
    let trailing_halt_idx = trailing_prog
        .instructions
        .iter()
        .position(|i| i.opcode == Opcode::Halt)
        .ok_or_else(|| Error::msg("trailing arm program has no Halt"))?;
    let trailing_reg_offset = b.next_reg() - 1;
    let trailing_cursor_offset = b.next_cursor();
    let _ = b.alloc_regs(trailing_prog.num_registers as i32);
    b.note_cursor(trailing_cursor_offset + trailing_prog.num_cursors as i32 - 1);
    let mut trailing_addr_map: std::collections::HashMap<i32, i32> = std::collections::HashMap::new();
    let trailing_start = b.cur_addr();
    // The trailing arm's Halt stays a Halt (the real end of the compound).
    inline_scan_with_resultrow_wrap(
        &mut b,
        &trailing_prog,
        trailing_halt_idx,
        trailing_reg_offset,
        trailing_cursor_offset,
        &mut trailing_addr_map,
        ncol,
        limit_reg,
        offset_reg,
        None,
    )?;
    let trailing_end_addr = b.cur_addr();
    for (i, inst) in b.iter_insts_mut().enumerate() {
        let addr = i as i32;
        if addr < trailing_start || addr >= trailing_end_addr {
            continue;
        }
        if !is_absolute_jump(inst) {
            continue;
        }
        let sub_target = inst.p2;
        if sub_target == trailing_halt_idx as i32 {
            // Jump to the trailing arm's Halt (the last inlined instruction).
            inst.p2 = trailing_end_addr - 1;
        } else if let Some(&inlined) = trailing_addr_map.get(&sub_target) {
            inst.p2 = inlined;
        } else if sub_target == 0 {
            inst.p2 = 0;
        }
    }

    b.resolve(setup);
    b.emit(Opcode::Transaction, 0, 0, 0);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

/// Inline a compiled arm's scan code, wrapping each `ResultRow` with the shared OFFSET/LIMIT
/// counters. The arm's `Init` (idx 0) is skipped; the `Halt` at `halt_idx` is either replaced
/// with a `Goto` to `halt_replacement` (if provided) or kept as a `Halt`.
///
/// Each `ResultRow p1 p2 p3` in the arm is rewritten to:
///   <original ResultRow>
///   IfPos offset_reg, next, 1     (if offset_reg set)
///   DecrJumpZero limit_reg, end   (if limit_reg set)
///   <next>:
/// But the original ResultRow already sits inside the arm's own OFFSET/LIMIT counters (which
/// are None since we cleared LIMIT/OFFSET before compiling). So we just need to wrap the
/// ResultRow with the shared counters. To keep the wrap minimal, we emit the OFFSET check
/// BEFORE the ResultRow and the LIMIT check AFTER (matching `compile_scan_unordered`'s shape).
fn inline_scan_with_resultrow_wrap(
    b: &mut ProgramBuilder,
    sub_program: &Program,
    halt_idx: usize,
    reg_offset: i32,
    cursor_offset: i32,
    addr_map: &mut std::collections::HashMap<i32, i32>,
    _ncol: i32,
    limit_reg: Option<i32>,
    offset_reg: Option<i32>,
    halt_replacement: Option<Label>,
) -> Result<()> {
    // The "end" target for LIMIT: a dedicated Halt emitted after the inlined arm. A LIMIT hit
    // in either arm halts the whole compound (the trailing arm never runs). We emit this Halt
    // once, after the arm's inlined scan code, and bind `end_label` to it.
    let end_label = b.new_label();
    let has_limit = limit_reg.is_some();
    for idx in 1..=halt_idx {
        let inst = &sub_program.instructions[idx];
        let sub_addr = idx as i32;
        let inlined_addr = b.cur_addr();
        addr_map.insert(sub_addr, inlined_addr);
        match inst.opcode {
            Opcode::ResultRow => {
                // Rebase the ResultRow's p1 (result start register).
                let mut new_inst = inst.clone();
                rebase_operands(&mut new_inst, reg_offset, cursor_offset);
                // OFFSET check before the ResultRow: if offset_reg > 0, skip this row.
                let next_label = b.new_label();
                if let Some(oreg) = offset_reg {
                    b.emit_jump(Opcode::IfPos, oreg, next_label, 1);
                }
                b.append(new_inst);
                if let Some(lreg) = limit_reg {
                    b.emit_jump(Opcode::DecrJumpZero, lreg, end_label, 0);
                }
                b.resolve(next_label);
            }
            Opcode::Halt => {
                if let Some(label) = halt_replacement {
                    b.emit_jump(Opcode::Goto, 0, label, 0);
                } else {
                    let mut new_inst = inst.clone();
                    rebase_operands(&mut new_inst, reg_offset, cursor_offset);
                    b.append(new_inst);
                }
            }
            _ => {
                let mut new_inst = inst.clone();
                rebase_operands(&mut new_inst, reg_offset, cursor_offset);
                b.append(new_inst);
            }
        }
    }
    // Bind `end_label` to a dedicated Halt emitted after the inlined arm. A LIMIT hit jumps
    // here and halts the whole compound. (When there's no LIMIT, `end_label` is unused but we
    // still bind it to keep the label-fixup machinery happy.)
    if has_limit {
        b.resolve(end_label);
        b.emit(Opcode::Halt, 0, 0, 0);
    } else {
        // No LIMIT: bind to the next instruction (whatever the caller emits). No DecrJumpZero
        // references the label, so this is just to satisfy the fixup machinery.
        b.resolve(end_label);
    }
    Ok(())
}

/// The merge algorithm for `UNION` / `INTERSECT` / `EXCEPT` / `UNION ALL`-with-`ORDER BY` /
/// 3+-arm compounds. Mirrors `multiSelectByMerge` in `select.c`.
fn compile_compound_merge(
    select: &SelectStmt,
    table: Option<&Table>,
    indexes: &[IndexObject],
    subquery_resolver: Option<&dyn SubqueryResolver>,
    limit: Option<i64>,
    offset: i64,
) -> Result<Program> {
    if select.compound.len() == 1 {
        compile_two_arm_merge(select, table, indexes, subquery_resolver, limit, offset)
    } else {
        compile_multi_arm_merge(select, table, indexes, subquery_resolver, limit, offset)
    }
}

/// The 2-arm merge: `leading <OP> trailing` with optional `ORDER BY`/`LIMIT`/`OFFSET` on the
/// whole compound.
fn compile_two_arm_merge(
    select: &SelectStmt,
    table: Option<&Table>,
    indexes: &[IndexObject],
    subquery_resolver: Option<&dyn SubqueryResolver>,
    limit: Option<i64>,
    offset: i64,
) -> Result<Program> {
    let (outer_op, trailing) = (select.compound[0].0, &select.compound[0].1);
    let leading_outputs = expand_columns(select, table)?;
    let ncol = leading_outputs.len() as i32;

    let trailing_table = resolve_arm_table(trailing, subquery_resolver)?;
    let trailing_indexes: Vec<IndexObject> = if trailing_table.is_some() {
        if let Some(r) = subquery_resolver {
            r.resolve(trailing).map(|(_, idxs)| idxs).unwrap_or_default()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    // Synthesize the merge ORDER BY. If the user supplied one, use it; otherwise invent
    // `ORDER BY 1, 2, … ncol` so both arms produce sorted output (upstream's approach).
    let order_terms: Vec<OrderingTerm> = if !select.order_by.is_empty() {
        select.order_by.clone()
    } else {
        (1..=ncol)
            .map(|i| OrderingTerm {
                expr: Expr::Literal(rustqlite_parser::Literal::Integer(i as i64)),
                desc: false,
                nulls: None,
            })
            .collect()
    };
    let nkey = order_terms.len() as i32;

    // Build the arm SELECTs: each gets the merge ORDER BY appended; LIMIT/OFFSET cleared (the
    // merge loop enforces them on the final output).
    let mut leading_arm = select.clone();
    leading_arm.compound.clear();
    leading_arm.order_by = order_terms.clone();
    leading_arm.limit = None;
    leading_arm.offset = None;
    let mut trailing_arm = trailing.clone();
    trailing_arm.order_by = order_terms.clone();
    trailing_arm.limit = None;
    trailing_arm.offset = None;

    let leading_prog = select::compile(&leading_arm, table, indexes, subquery_resolver)?.0;
    let trailing_prog =
        select::compile(&trailing_arm, trailing_table.as_ref(), &trailing_indexes, subquery_resolver)?.0;

    let mut b = ProgramBuilder::new();
    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    let after_init = b.cur_addr();

    if limit == Some(0) {
        b.emit(Opcode::Halt, 0, 0, 0);
        b.resolve(setup);
        b.emit(Opcode::Transaction, 0, 0, 0);
        b.emit(Opcode::Goto, 0, after_init, 0);
        return Ok(b.finish());
    }

    let limit_reg = match limit {
        Some(n) if n > 0 => Some(emit_int(&mut b, n)),
        _ => None,
    };
    let offset_reg = (offset > 0).then(|| emit_int(&mut b, offset));

    // Merge working registers.
    let reg_addr_a = b.alloc_reg();
    let reg_addr_b = b.alloc_reg();
    let reg_out_a = b.alloc_reg();
    let reg_out_b = b.alloc_reg();
    let reg_block_a = b.alloc_regs(ncol);
    let reg_block_b = b.alloc_regs(ncol);

    let needs_dedup = outer_op != CompoundOperator::UnionAll;
    let reg_prev_flag = if needs_dedup {
        let r = b.alloc_reg();
        b.emit(Opcode::Integer, 0, r, 0);
        r
    } else {
        0
    };
    let reg_prev_block = if needs_dedup { b.alloc_regs(ncol) } else { 0 };

    let merge_keyinfo: Vec<KeyField> = order_terms
        .iter()
        .map(|t| KeyField {
            desc: t.desc,
            collation: crate::types::Collation::Binary,
        })
        .collect();

    // --- Coroutine A: leading arm. ---
    let co_a_start = b.new_label();
    let after_co_a = b.new_label();
    let init_a = b.emit_jump(Opcode::InitCoroutine, reg_addr_a, after_co_a, 0);
    b.resolve(co_a_start);
    let leading_halt_idx = leading_prog
        .instructions
        .iter()
        .position(|i| i.opcode == Opcode::Halt)
        .ok_or_else(|| Error::msg("leading arm program has no Halt"))?;
    let leading_reg_offset = b.next_reg() - 1;
    let leading_cursor_offset = b.next_cursor();
    let _ = b.alloc_regs(leading_prog.num_registers as i32);
    b.note_cursor(leading_cursor_offset + leading_prog.num_cursors as i32 - 1);
    let mut leading_addr_map: std::collections::HashMap<i32, i32> = std::collections::HashMap::new();
    let leading_start = b.cur_addr();
    inline_coroutine_arm(
        &mut b,
        &leading_prog,
        leading_halt_idx,
        leading_reg_offset,
        leading_cursor_offset,
        &mut leading_addr_map,
        reg_block_a,
        ncol,
        reg_addr_a,
    )?;
    let leading_end_addr = b.cur_addr();
    {
        let co_a_addr = b.label_addr_of(co_a_start);
        let init_inst = b
            .iter_insts_mut()
            .nth(init_a)
            .expect("init_a in range");
        init_inst.p3 = co_a_addr;
    }
    patch_arm_jumps(&mut b, leading_start, leading_end_addr, leading_halt_idx, &leading_addr_map);
    // NOTE: `after_co_a` is resolved right before coB's InitCoroutine so coA's InitCoroutine
    // jumps to coB's InitCoroutine (which sets r[reg_addr_b] and then jumps to Init). This
    // matches the oracle's two-InitCoroutine chaining.

    // --- Coroutine B: trailing arm. ---
    let co_b_start = b.new_label();
    let after_co_b = b.new_label();
    b.resolve(after_co_a); // coA's InitCoroutine jumps here (to coB's InitCoroutine).
    let init_b = b.emit_jump(Opcode::InitCoroutine, reg_addr_b, after_co_b, 0);
    b.resolve(co_b_start);
    let trailing_halt_idx = trailing_prog
        .instructions
        .iter()
        .position(|i| i.opcode == Opcode::Halt)
        .ok_or_else(|| Error::msg("trailing arm program has no Halt"))?;
    let trailing_reg_offset = b.next_reg() - 1;
    let trailing_cursor_offset = b.next_cursor();
    let _ = b.alloc_regs(trailing_prog.num_registers as i32);
    b.note_cursor(trailing_cursor_offset + trailing_prog.num_cursors as i32 - 1);
    let mut trailing_addr_map: std::collections::HashMap<i32, i32> = std::collections::HashMap::new();
    let trailing_start = b.cur_addr();
    inline_coroutine_arm(
        &mut b,
        &trailing_prog,
        trailing_halt_idx,
        trailing_reg_offset,
        trailing_cursor_offset,
        &mut trailing_addr_map,
        reg_block_b,
        ncol,
        reg_addr_b,
    )?;
    let trailing_end_addr = b.cur_addr();
    {
        let co_b_addr = b.label_addr_of(co_b_start);
        let init_inst = b
            .iter_insts_mut()
            .nth(init_b)
            .expect("init_b in range");
        init_inst.p3 = co_b_addr;
    }
    patch_arm_jumps(&mut b, trailing_start, trailing_end_addr, trailing_halt_idx, &trailing_addr_map);
    // NOTE: `after_co_b` is NOT resolved here — it's resolved inside `emit_merge_control` at
    // the Init section.

    // Emit the merge control flow (outA/outB, EofA/EofB, AltB/AeqB/AgtB, Init, Cmpr, End).
    // `after_co_a`/`after_co_b` are resolved inside `emit_merge_control` at the Init section
    // (both InitCoroutines jump past all subroutine bodies to Init).
    emit_merge_control(
        &mut b,
        outer_op,
        ncol,
        nkey,
        merge_keyinfo,
        reg_addr_a,
        reg_addr_b,
        reg_out_a,
        reg_out_b,
        reg_block_a,
        reg_block_b,
        needs_dedup,
        reg_prev_flag,
        reg_prev_block,
        limit_reg,
        offset_reg,
        after_co_a,
        after_co_b,
    );

    b.resolve(setup);
    b.emit(Opcode::Transaction, 0, 0, 0);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

/// A 3+-arm compound: split off the rightmost arm; the left side is `select` with all but the
/// last compound arm, compiled recursively via `compile_compound` and materialized into a
/// sorter that serves as the merge's "A" coroutine. The rightmost arm is the "B" coroutine.
fn compile_multi_arm_merge(
    select: &SelectStmt,
    table: Option<&Table>,
    indexes: &[IndexObject],
    subquery_resolver: Option<&dyn SubqueryResolver>,
    limit: Option<i64>,
    offset: i64,
) -> Result<Program> {
    let last_idx = select.compound.len() - 1;
    let outer_op = select.compound[last_idx].0;
    let rightmost = select.compound[last_idx].1.clone();

    let mut left_select = select.clone();
    left_select.compound.truncate(last_idx);
    left_select.order_by = Vec::new();
    left_select.limit = None;
    left_select.offset = None;

    let (left_prog, left_names) =
        compile_compound(&left_select, table, indexes, subquery_resolver)?;
    let ncol = left_names.len() as i32;

    let order_terms: Vec<OrderingTerm> = if !select.order_by.is_empty() {
        select.order_by.clone()
    } else {
        (1..=ncol)
            .map(|i| OrderingTerm {
                expr: Expr::Literal(rustqlite_parser::Literal::Integer(i as i64)),
                desc: false,
                nulls: None,
            })
            .collect()
    };
    let nkey = order_terms.len() as i32;

    let mut b = ProgramBuilder::new();
    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    let after_init = b.cur_addr();

    if limit == Some(0) {
        b.emit(Opcode::Halt, 0, 0, 0);
        b.resolve(setup);
        b.emit(Opcode::Transaction, 0, 0, 0);
        b.emit(Opcode::Goto, 0, after_init, 0);
        return Ok(b.finish());
    }
    let limit_reg = match limit {
        Some(n) if n > 0 => Some(emit_int(&mut b, n)),
        _ => None,
    };
    let offset_reg = (offset > 0).then(|| emit_int(&mut b, offset));

    // Sorter for the left sub-compound's rows, keyed by the merge ORDER BY.
    let left_sorter = b.next_cursor();
    let left_keyinfo: Vec<KeyField> = order_terms
        .iter()
        .map(|t| KeyField {
            desc: t.desc,
            collation: crate::types::Collation::Binary,
        })
        .collect();
    let so = b.emit(Opcode::SorterOpen, left_sorter, nkey + ncol, 0);
    b.set_p4(so, P4::KeyInfo(left_keyinfo.clone()));
    b.note_cursor(left_sorter);

    // Inline the left sub-compound, rewriting ResultRow → [keys..., cols...] MakeRecord +
    // SorterInsert into left_sorter.
    let left_halt_idx = left_prog
        .instructions
        .iter()
        .position(|i| i.opcode == Opcode::Halt)
        .ok_or_else(|| Error::msg("left sub-compound program has no Halt"))?;
    let left_reg_offset = b.next_reg() - 1;
    let left_cursor_offset = b.next_cursor();
    let _ = b.alloc_regs(left_prog.num_registers as i32);
    b.note_cursor(left_cursor_offset + left_prog.num_cursors as i32 - 1);
    let mut left_addr_map: std::collections::HashMap<i32, i32> = std::collections::HashMap::new();
    let left_start = b.cur_addr();
    let left_block = b.alloc_regs(nkey + ncol);
    for idx in 1..left_halt_idx {
        let inst = &left_prog.instructions[idx];
        let sub_addr = idx as i32;
        let inlined_addr = b.cur_addr();
        left_addr_map.insert(sub_addr, inlined_addr);
        match inst.opcode {
            Opcode::ResultRow => {
                for (k, term) in order_terms.iter().enumerate() {
                    let ord = match &term.expr {
                        Expr::Literal(rustqlite_parser::Literal::Integer(n)) => *n as i32,
                        _ => 0,
                    };
                    if ord >= 1 && ord <= ncol {
                        b.emit(
                            Opcode::SCopy,
                            left_reg_offset + inst.p1 + (ord - 1),
                            left_block + k as i32,
                            0,
                        );
                    } else {
                        b.emit(Opcode::Null, 0, left_block + k as i32, 0);
                    }
                }
                for j in 0..ncol {
                    b.emit(
                        Opcode::SCopy,
                        left_reg_offset + inst.p1 + j,
                        left_block + nkey + j,
                        0,
                    );
                }
                let rec = b.alloc_reg();
                b.emit(Opcode::MakeRecord, left_block, nkey + ncol, rec);
                b.emit(Opcode::SorterInsert, left_sorter, rec, 0);
            }
            _ => {
                let mut new_inst = inst.clone();
                rebase_operands(&mut new_inst, left_reg_offset, left_cursor_offset);
                b.append(new_inst);
            }
        }
    }
    let left_scan_end = b.cur_addr();
    // Patch the left sub-compound's jumps. Its Halt (at `left_halt_idx`) was NOT inlined
    // (the loop above is 1..left_halt_idx, exclusive), so jumps targeting it go to
    // `left_scan_end` (the next instruction — the start of the outer merge's code), NOT to
    // `left_scan_end - 1`.
    {
        let halt_target = left_scan_end;
        let map_target = |sub_target: i32| -> i32 {
            if sub_target == left_halt_idx as i32 {
                halt_target
            } else if let Some(&inlined) = left_addr_map.get(&sub_target) {
                inlined
            } else if sub_target == 0 {
                0
            } else {
                sub_target
            }
        };
        for (i, inst) in b.iter_insts_mut().enumerate() {
            let addr = i as i32;
            if addr < left_start || addr >= left_scan_end {
                continue;
            }
            if inst.opcode == Opcode::Jump {
                inst.p1 = map_target(inst.p1);
                inst.p2 = map_target(inst.p2);
                inst.p3 = map_target(inst.p3);
            } else if inst.opcode == Opcode::InitCoroutine {
                inst.p3 = map_target(inst.p3);
                inst.p2 = map_target(inst.p2);
            } else if is_absolute_jump(inst) {
                inst.p2 = map_target(inst.p2);
            }
        }
    }

    // Merge working registers.
    let reg_addr_a = b.alloc_reg();
    let reg_addr_b = b.alloc_reg();
    let reg_out_a = b.alloc_reg();
    let reg_out_b = b.alloc_reg();
    let reg_block_a = b.alloc_regs(ncol);
    let reg_block_b = b.alloc_regs(ncol);
    let needs_dedup = outer_op != CompoundOperator::UnionAll;
    let reg_prev_flag = if needs_dedup {
        let r = b.alloc_reg();
        b.emit(Opcode::Integer, 0, r, 0);
        r
    } else {
        0
    };
    let reg_prev_block = if needs_dedup { b.alloc_regs(ncol) } else { 0 };

    // Coroutine A: walk left_sorter.
    let co_a_start = b.new_label();
    let after_co_a = b.new_label();
    let init_a = b.emit_jump(Opcode::InitCoroutine, reg_addr_a, after_co_a, 0);
    b.resolve(co_a_start);
    let co_a_end = b.new_label();
    b.emit_jump(Opcode::SorterSort, left_sorter, co_a_end, 0);
    let co_a_top = b.cur_addr();
    b.emit(Opcode::SorterData, left_sorter, 0, 0);
    for j in 0..ncol {
        b.emit(Opcode::Column, left_sorter, nkey + j, reg_block_a + j);
    }
    b.emit(Opcode::Yield, reg_addr_a, 0, 0);
    b.emit(Opcode::SorterNext, left_sorter, co_a_top, 0);
    b.resolve(co_a_end);
    b.emit(Opcode::EndCoroutine, reg_addr_a, 0, 0);
    {
        let co_a_addr = b.label_addr_of(co_a_start);
        let init_inst = b
            .iter_insts_mut()
            .nth(init_a)
            .expect("init_a in range");
        init_inst.p3 = co_a_addr;
    }
    // NOTE: `after_co_a` is resolved right before coB's InitCoroutine (see `compile_two_arm_merge`
    // for the same pattern).

    // Coroutine B: the rightmost arm, compiled with the synthesized ORDER BY.
    let rightmost_table = resolve_arm_table(&rightmost, subquery_resolver)?;
    let rightmost_indexes: Vec<IndexObject> = if rightmost_table.is_some() {
        if let Some(r) = subquery_resolver {
            r.resolve(&rightmost).map(|(_, idxs)| idxs).unwrap_or_default()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    let mut rightmost_arm = rightmost.clone();
    rightmost_arm.order_by = order_terms.clone();
    rightmost_arm.limit = None;
    rightmost_arm.offset = None;
    let rightmost_prog = select::compile(
        &rightmost_arm,
        rightmost_table.as_ref(),
        &rightmost_indexes,
        subquery_resolver,
    )?
    .0;
    let co_b_start = b.new_label();
    let after_co_b = b.new_label();
    b.resolve(after_co_a); // coA's InitCoroutine jumps here (to coB's InitCoroutine).
    let init_b = b.emit_jump(Opcode::InitCoroutine, reg_addr_b, after_co_b, 0);
    b.resolve(co_b_start);
    let rightmost_halt_idx = rightmost_prog
        .instructions
        .iter()
        .position(|i| i.opcode == Opcode::Halt)
        .ok_or_else(|| Error::msg("rightmost arm program has no Halt"))?;
    let rightmost_reg_offset = b.next_reg() - 1;
    let rightmost_cursor_offset = b.next_cursor();
    let _ = b.alloc_regs(rightmost_prog.num_registers as i32);
    b.note_cursor(rightmost_cursor_offset + rightmost_prog.num_cursors as i32 - 1);
    let mut rightmost_addr_map: std::collections::HashMap<i32, i32> = std::collections::HashMap::new();
    let rightmost_start = b.cur_addr();
    inline_coroutine_arm(
        &mut b,
        &rightmost_prog,
        rightmost_halt_idx,
        rightmost_reg_offset,
        rightmost_cursor_offset,
        &mut rightmost_addr_map,
        reg_block_b,
        ncol,
        reg_addr_b,
    )?;
    let rightmost_end_addr = b.cur_addr();
    {
        let co_b_addr = b.label_addr_of(co_b_start);
        let init_inst = b
            .iter_insts_mut()
            .nth(init_b)
            .expect("init_b in range");
        init_inst.p3 = co_b_addr;
    }
    patch_arm_jumps(
        &mut b,
        rightmost_start,
        rightmost_end_addr,
        rightmost_halt_idx,
        &rightmost_addr_map,
    );
    // NOTE: `after_co_b` is resolved inside `emit_merge_control` at the Init section.

    let merge_keyinfo: Vec<KeyField> = order_terms
        .iter()
        .map(|t| KeyField {
            desc: t.desc,
            collation: crate::types::Collation::Binary,
        })
        .collect();

    emit_merge_control(
        &mut b,
        outer_op,
        ncol,
        nkey,
        merge_keyinfo,
        reg_addr_a,
        reg_addr_b,
        reg_out_a,
        reg_out_b,
        reg_block_a,
        reg_block_b,
        needs_dedup,
        reg_prev_flag,
        reg_prev_block,
        limit_reg,
        offset_reg,
        after_co_a,
        after_co_b,
    );

    b.resolve(setup);
    b.emit(Opcode::Transaction, 0, 0, 0);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

/// Emit the merge control flow: outA/outB subroutines, EofA/EofB, AltB/AeqB/AgtB, Init (prime
/// both coroutines), Cmpr (Compare + Jump), and End (Halt). This is the heart of
/// `multiSelectByMerge`.
#[allow(clippy::too_many_arguments)]
fn emit_merge_control(
    b: &mut ProgramBuilder,
    outer_op: CompoundOperator,
    ncol: i32,
    nkey: i32,
    merge_keyinfo: Vec<KeyField>,
    reg_addr_a: i32,
    reg_addr_b: i32,
    reg_out_a: i32,
    reg_out_b: i32,
    reg_block_a: i32,
    reg_block_b: i32,
    needs_dedup: bool,
    reg_prev_flag: i32,
    reg_prev_block: i32,
    limit_reg: Option<i32>,
    offset_reg: Option<i32>,
    _after_co_a: Label,
    after_co_b: Label,
) {
    let out_a_label = b.new_label();
    let out_b_label = b.new_label();
    let label_end = b.new_label();
    let label_cmpr = b.new_label();

    // outA subroutine.
    b.resolve(out_a_label);
    emit_out_subroutine(
        b,
        reg_block_a,
        ncol,
        needs_dedup,
        reg_prev_flag,
        reg_prev_block,
        limit_reg,
        offset_reg,
        label_end,
    );
    b.emit(Opcode::Return, reg_out_a, 0, 1);

    // outB subroutine (only for UNION / UNION ALL).
    let emit_out_b = outer_op == CompoundOperator::UnionAll || outer_op == CompoundOperator::Union;
    if emit_out_b {
        b.resolve(out_b_label);
        emit_out_subroutine(
            b,
            reg_block_b,
            ncol,
            needs_dedup,
            reg_prev_flag,
            reg_prev_block,
            limit_reg,
            offset_reg,
            label_end,
        );
        b.emit(Opcode::Return, reg_out_b, 0, 1);
    }

    // EofA / EofA_noB / EofB handlers.
    let eof_a_label = b.new_label();
    let eof_a_no_b_label = b.new_label();
    let eof_b_label = b.new_label();

    b.resolve(eof_a_label);
    if outer_op == CompoundOperator::Union || outer_op == CompoundOperator::UnionAll {
        // EofA: A is exhausted, drain B. Yield B jumps to `label_end` on B-exhaustion (NOT to
        // EofB — otherwise EofA and EofB would infinitely ping-pong when both are exhausted).
        // `eof_a_no_b_label` is the alternate entry that skips the first `Gosub outB` (used
        // during Init when B hasn't been primed yet — it does `Yield B` to prime B first).
        let eof_a_loop = b.new_label();
        b.resolve(eof_a_loop);
        b.emit_jump(Opcode::Gosub, reg_out_b, out_b_label, 0);
        b.emit_jump(Opcode::Yield, reg_addr_b, label_end, 0);
        b.emit_jump(Opcode::Goto, 0, eof_a_loop, 0);
        // EofA_noB: the Yield B instruction (skip the Gosub outB). Used during Init.
        b.resolve(eof_a_no_b_label);
        b.emit_jump(Opcode::Yield, reg_addr_b, label_end, 0);
        b.emit_jump(Opcode::Goto, 0, eof_a_loop, 0);
    } else {
        b.emit_jump(Opcode::Goto, 0, label_end, 0);
        b.resolve(eof_a_no_b_label);
        b.emit_jump(Opcode::Goto, 0, label_end, 0);
    }

    b.resolve(eof_b_label);
    if outer_op == CompoundOperator::Intersect {
        b.emit_jump(Opcode::Goto, 0, label_end, 0);
    } else {
        // EofB: B is exhausted, drain A. Yield A jumps to `label_end` on A-exhaustion.
        let eof_b_loop = b.new_label();
        b.resolve(eof_b_loop);
        b.emit_jump(Opcode::Gosub, reg_out_a, out_a_label, 0);
        b.emit_jump(Opcode::Yield, reg_addr_a, label_end, 0);
        b.emit_jump(Opcode::Goto, 0, eof_b_loop, 0);
    }

    // AltB / AeqB / AgtB handlers.
    let alt_b_label = b.new_label();
    let aeq_b_label = b.new_label();
    let agt_b_label = b.new_label();

    b.resolve(alt_b_label);
    if outer_op == CompoundOperator::Intersect {
        // INTERSECT AltB: just next-A (no output).
        b.emit_jump(Opcode::Yield, reg_addr_a, eof_a_label, 0);
        b.emit_jump(Opcode::Goto, 0, label_cmpr, 0);
    } else {
        b.emit_jump(Opcode::Gosub, reg_out_a, out_a_label, 0);
        b.emit_jump(Opcode::Yield, reg_addr_a, eof_a_label, 0);
        b.emit_jump(Opcode::Goto, 0, label_cmpr, 0);
    }

    b.resolve(aeq_b_label);
    if outer_op == CompoundOperator::Intersect {
        // INTERSECT AeqB: outA, next-A (A is in both, emit it).
        b.emit_jump(Opcode::Gosub, reg_out_a, out_a_label, 0);
        b.emit_jump(Opcode::Yield, reg_addr_a, eof_a_label, 0);
        b.emit_jump(Opcode::Goto, 0, label_cmpr, 0);
    } else if outer_op == CompoundOperator::UnionAll {
        // UNION ALL AeqB: outA, next-A (same as AltB — no dedup).
        b.emit_jump(Opcode::Gosub, reg_out_a, out_a_label, 0);
        b.emit_jump(Opcode::Yield, reg_addr_a, eof_a_label, 0);
        b.emit_jump(Opcode::Goto, 0, label_cmpr, 0);
    } else {
        // UNION/EXCEPT AeqB: next-A (the duplicate is suppressed by the outA dedup check for
        // UNION; EXCEPT skips A since it also appears in B).
        b.emit_jump(Opcode::Yield, reg_addr_a, eof_a_label, 0);
        b.emit_jump(Opcode::Goto, 0, label_cmpr, 0);
    }

    b.resolve(agt_b_label);
    if outer_op == CompoundOperator::Union || outer_op == CompoundOperator::UnionAll {
        b.emit_jump(Opcode::Gosub, reg_out_b, out_b_label, 0);
        b.emit_jump(Opcode::Yield, reg_addr_b, eof_b_label, 0);
        b.emit_jump(Opcode::Goto, 0, label_cmpr, 0);
    } else {
        // EXCEPT/INTERSECT AgtB: next-B (no output).
        b.emit_jump(Opcode::Yield, reg_addr_b, eof_b_label, 0);
        b.emit_jump(Opcode::Goto, 0, label_cmpr, 0);
    }

    // Init: prime both coroutines. The first Yield of A jumps to EofA_noB if A is empty (B not
    // yet primed); the first Yield of B jumps to EofB if B is empty.
    // `after_co_b` is resolved here — coB's InitCoroutine jumps here (past all subroutine bodies)
    // so coB's body doesn't run during setup. (`after_co_a` was already resolved to coB's
    // InitCoroutine in the caller.)
    b.resolve(after_co_b);
    b.emit_jump(Opcode::Yield, reg_addr_a, eof_a_no_b_label, 0);
    b.emit_jump(Opcode::Yield, reg_addr_b, eof_b_label, 0);

    // Cmpr: Compare + Jump.
    b.resolve(label_cmpr);
    let cmp = b.emit(Opcode::Compare, reg_block_a, reg_block_b, nkey);
    b.set_p4(cmp, P4::KeyInfo(merge_keyinfo));
    b.emit_jump3(Opcode::Jump, alt_b_label, aeq_b_label, agt_b_label);

    // End.
    b.resolve(label_end);
    b.emit(Opcode::Halt, 0, 0, 0);
}

/// Emit the body of an output subroutine (outA or outB): suppress duplicates (if `dedup` is
/// set), apply OFFSET/LIMIT, then emit a `ResultRow`. Mirrors `generateOutputSubroutine` in
/// `select.c`. The subroutine ends with a `Return` emitted by the caller.
#[allow(clippy::too_many_arguments)]
fn emit_out_subroutine(
    b: &mut ProgramBuilder,
    block: i32,
    ncol: i32,
    dedup: bool,
    reg_prev_flag: i32,
    reg_prev_block: i32,
    limit_reg: Option<i32>,
    offset_reg: Option<i32>,
    label_end: Label,
) {
    // The "skip" target for dedup: jump past the ResultRow/LIMIT to the subroutine's Return
    // (emitted by the caller right after this subroutine body). Bound at the bottom.
    let skip_label = b.new_label();
    if dedup {
        // The "emit" block: copy current row → prev, set prev_flag, then fall through to the
        // OFFSET/LIMIT/ResultRow below. Bound right after the Jump's fall-through position.
        let emit_label = b.new_label();
        // If reg_prev_flag is NOT set (first row), skip the comparison and go straight to emit.
        b.emit_jump(Opcode::IfNot, reg_prev_flag, emit_label, 0);
        // Compare current vs prev. Equal → skip (duplicate); Less/Greater → emit.
        let cmp = b.emit(Opcode::Compare, block, reg_prev_block, ncol);
        b.set_p4(
            cmp,
            P4::KeyInfo((0..ncol).map(|_| KeyField::asc_binary()).collect()),
        );
        b.emit_jump3(Opcode::Jump, emit_label, skip_label, emit_label);
        b.resolve(emit_label);
        // Copy current → prev and set prev_flag.
        for j in 0..ncol {
            b.emit(Opcode::SCopy, block + j, reg_prev_block + j, 0);
        }
        b.emit(Opcode::Integer, 1, reg_prev_flag, 0);
    }
    if let Some(oreg) = offset_reg {
        // IfPos: if offset > 0, decrement and skip this row (jump past the ResultRow/LIMIT to
        // the subroutine's Return emitted by the caller). `skip_label` is bound at the bottom
        // of this subroutine, right before the Return.
        b.emit_jump(Opcode::IfPos, oreg, skip_label, 1);
    }
    b.emit(Opcode::ResultRow, block, ncol, 0);
    if let Some(lreg) = limit_reg {
        b.emit_jump(Opcode::DecrJumpZero, lreg, label_end, 0);
    }
    b.resolve(skip_label);
}

/// Inline a compiled arm as a coroutine body: rewrite each `ResultRow` into `SCopy` of its
/// columns to `out_block` + `Yield yield_reg`, and rewrite the `Halt` at `halt_idx` into an
/// `EndCoroutine`. Register/cursor operands are rebased by the given offsets. Builds
/// `addr_map` for the caller's jump-patch loop.
#[allow(clippy::too_many_arguments)]
fn inline_coroutine_arm(
    b: &mut ProgramBuilder,
    sub_program: &Program,
    halt_idx: usize,
    reg_offset: i32,
    cursor_offset: i32,
    addr_map: &mut std::collections::HashMap<i32, i32>,
    out_block: i32,
    ncol: i32,
    yield_reg: i32,
) -> Result<()> {
    for idx in 1..=halt_idx {
        let inst = &sub_program.instructions[idx];
        let sub_addr = idx as i32;
        let inlined_addr = b.cur_addr();
        addr_map.insert(sub_addr, inlined_addr);
        match inst.opcode {
            Opcode::ResultRow => {
                for j in 0..ncol {
                    b.emit(Opcode::SCopy, reg_offset + inst.p1 + j, out_block + j, 0);
                }
                b.emit(Opcode::Yield, yield_reg, 0, 0);
            }
            Opcode::Halt => {
                b.emit(Opcode::EndCoroutine, yield_reg, 0, 0);
            }
            _ => {
                let mut new_inst = inst.clone();
                rebase_operands(&mut new_inst, reg_offset, cursor_offset);
                b.append(new_inst);
            }
        }
    }
    Ok(())
}

/// Patch every inlined jump's `p2` using the address map. Jumps targeting the arm's `Halt`
/// (`halt_idx`) are redirected to the last inlined instruction (the rewritten Halt /
/// EndCoroutine / Goto). Mirrors the patch loops in `subquery.rs`.
fn patch_arm_jumps(
    b: &mut ProgramBuilder,
    start_addr: i32,
    end_addr: i32,
    halt_idx: usize,
    addr_map: &std::collections::HashMap<i32, i32>,
) {
    // The Halt at `halt_idx` is the last inlined instruction (rewritten to EndCoroutine or
    // Goto by the caller). Jumps targeting it go to `end_addr - 1` (the last inlined
    // instruction). For the left sub-compound inlining (where the Halt is skipped, not
    // inlined), the caller passes `halt_idx` as the exclusive upper bound so `end_addr - 1`
    // still lands on the correct continuation — but that case uses a separate code path (see
    // `compile_multi_arm_merge`'s own patch loop). Here we assume the Halt WAS inlined.
    let halt_target = end_addr - 1;
    let map_target = |sub_target: i32| -> i32 {
        if sub_target == halt_idx as i32 {
            halt_target
        } else if let Some(&inlined) = addr_map.get(&sub_target) {
            inlined
        } else if sub_target == 0 {
            0
        } else {
            sub_target
        }
    };
    for (i, inst) in b.iter_insts_mut().enumerate() {
        let addr = i as i32;
        if addr < start_addr || addr >= end_addr {
            continue;
        }
        if inst.opcode == Opcode::Jump {
            inst.p1 = map_target(inst.p1);
            inst.p2 = map_target(inst.p2);
            inst.p3 = map_target(inst.p3);
        } else if inst.opcode == Opcode::InitCoroutine {
            // InitCoroutine p3 is the coroutine entry address (a sub-program address that
            // needs rebasing via the address map). p2 is a jump target (patched below).
            inst.p3 = map_target(inst.p3);
            inst.p2 = map_target(inst.p2);
        } else if is_absolute_jump(inst) {
            inst.p2 = map_target(inst.p2);
        }
    }
}

/// Rebase every register/cursor operand of `inst` by the given offsets. Mirrors the same-named
/// helper in `subquery.rs` (kept local to avoid a cross-module dependency). Jump targets (p2 of
/// control-flow opcodes) are NOT rebased here — the caller patches them via the address map.
fn rebase_operands(inst: &mut Instruction, reg_offset: i32, cursor_offset: i32) {
    use Opcode::*;
    let r = |x: &mut i32| *x += reg_offset;
    let c = |x: &mut i32| *x += cursor_offset;
    match inst.opcode {
        Goto | Init | Once => {}
        Gosub => r(&mut inst.p1),
        Return => r(&mut inst.p1),
        If | IfNot | IsNull | NotNull => r(&mut inst.p1),
        IfPos | DecrJumpZero => r(&mut inst.p1),
        Eq | Ne | Lt | Le | Gt | Ge => {
            r(&mut inst.p1);
            r(&mut inst.p3);
        }
        Rewind | Next => c(&mut inst.p1),
        NotExists => {
            c(&mut inst.p1);
            r(&mut inst.p3);
        }
        SeekGE | SeekGT | SeekLE | SeekLT => {
            c(&mut inst.p1);
            r(&mut inst.p3);
        }
        IdxGE | IdxGT | IdxLE | IdxLT => {
            c(&mut inst.p1);
            r(&mut inst.p3);
        }
        Found | NotFound => {
            c(&mut inst.p1);
            r(&mut inst.p3);
        }
        SorterSort | SorterNext => c(&mut inst.p1),
        OpenRead | OpenWrite | OpenWriteReg | OpenEphemeral | OpenPseudo | Close => {
            c(&mut inst.p1);
            if inst.opcode == OpenPseudo {
                r(&mut inst.p2);
            }
        }
        RowData => {
            c(&mut inst.p1);
            r(&mut inst.p2);
        }
        Column => {
            c(&mut inst.p1);
            r(&mut inst.p3);
        }
        Rowid => {
            c(&mut inst.p1);
            r(&mut inst.p2);
        }
        NullRow => c(&mut inst.p1),
        Clear | Destroy => c(&mut inst.p1),
        IdxInsert => {
            c(&mut inst.p1);
            r(&mut inst.p2);
        }
        IdxDelete => {
            c(&mut inst.p1);
            r(&mut inst.p2);
        }
        IdxRowid => {
            c(&mut inst.p1);
            r(&mut inst.p2);
        }
        SorterOpen => c(&mut inst.p1),
        SorterInsert => {
            c(&mut inst.p1);
            r(&mut inst.p2);
        }
        SorterData => {
            c(&mut inst.p1);
            r(&mut inst.p2);
        }
        OpenDup => {
            c(&mut inst.p1);
            c(&mut inst.p2);
        }
        ResetSorter | Last | Prev => c(&mut inst.p1),
        MakeRecord => {
            r(&mut inst.p1);
            r(&mut inst.p3);
        }
        NewRowid => {
            c(&mut inst.p1);
            r(&mut inst.p2);
        }
        Insert => {
            c(&mut inst.p1);
            r(&mut inst.p2);
            r(&mut inst.p3);
        }
        Delete => {
            c(&mut inst.p1);
            r(&mut inst.p2);
        }
        Integer | Int64 | Real | String8 | Null | Blob => {
            r(&mut inst.p2);
            if inst.opcode == Null && inst.p3 > 0 {
                r(&mut inst.p3);
            }
        }
        Add | Subtract | Multiply | Divide | Remainder | Concat | BitAnd | BitOr
        | ShiftLeft | ShiftRight => {
            r(&mut inst.p1);
            r(&mut inst.p2);
            r(&mut inst.p3);
        }
        BitNot => {
            r(&mut inst.p1);
            r(&mut inst.p2);
        }
        Not => {
            r(&mut inst.p1);
            r(&mut inst.p2);
        }
        And | Or => {
            r(&mut inst.p1);
            r(&mut inst.p2);
        }
        SCopy => {
            r(&mut inst.p1);
            r(&mut inst.p2);
        }
        Move | Copy => {
            r(&mut inst.p1);
            r(&mut inst.p2);
        }
        Affinity => r(&mut inst.p1),
        RealAffinity => r(&mut inst.p1),
        Function => {
            r(&mut inst.p2);
            r(&mut inst.p3);
        }
        AggStep => {
            r(&mut inst.p2);
            r(&mut inst.p3);
        }
        AggInverse => {
            r(&mut inst.p2);
            r(&mut inst.p3);
        }
        AggFinal => r(&mut inst.p1),
        AggValue => r(&mut inst.p3),
        HaltIfNull => r(&mut inst.p3),
        AddImm => r(&mut inst.p1),
        SeekRowid => {
            c(&mut inst.p1);
            r(&mut inst.p3);
        }
        Compare | Jump | Transaction | SetCookie | ParseSchema | CreateBtree | Halt => {}
        ResultRow => {
            // p1 is the result start register; rebase it. p2 is the column count (not a
            // register). The merge coroutine rewriter handles ResultRow separately (rewriting
            // it to SCopy+Yield), so this arm only matters for the UNION ALL chain path which
            // keeps the ResultRow.
            r(&mut inst.p1);
        }
        InitCoroutine => r(&mut inst.p1),
        EndCoroutine => r(&mut inst.p1),
        Yield => r(&mut inst.p1),
        Program => {}
        Param => r(&mut inst.p2),
    }
}

/// `true` for opcodes whose `p2` operand is an absolute jump target (not a register/cursor).
fn is_absolute_jump(inst: &Instruction) -> bool {
    use Opcode::*;
    matches!(
        inst.opcode,
        Goto | Init | Gosub | If | IfNot | IsNull | NotNull | IfPos | DecrJumpZero | Eq | Ne | Lt
            | Le | Gt | Ge | Rewind | Next | NotExists | SeekGE | SeekGT | SeekLE | SeekLT
            | IdxGE | IdxGT | IdxLE | IdxLT | Found | NotFound | SorterSort | SorterNext | Yield
            | Jump | InitCoroutine | SeekRowid | Last | Prev
    )
}

/// Resolve an arm's FROM table (if any) through the `subquery_resolver`. Returns `None` for a
/// constant / `VALUES` arm (no FROM) or when no resolver is available.
fn resolve_arm_table(
    arm: &SelectStmt,
    subquery_resolver: Option<&dyn SubqueryResolver>,
) -> Result<Option<Table>> {
    if !arm.values.is_empty() {
        return Ok(None);
    }
    if arm.from.is_empty() {
        return Ok(None);
    }
    if let Some(r) = subquery_resolver {
        let (t, _) = r.resolve(arm)?;
        Ok(t)
    } else {
        Ok(None)
    }
}

// ---- EXPLAIN QUERY PLAN ----

/// EXPLAIN QUERY PLAN rows for a compound SELECT, matching upstream's wording for the
/// 2-arm case. The leading arm's sub-plan is summarized as a single `SCAN <table>` (or
/// `SCAN CONSTANT ROW`) line; the trailing arm likewise. A full rendering would recurse into
/// each arm's plan; this matches the oracle for the shapes the M9 first slice supports.
pub fn explain_compound_rows(
    select: &SelectStmt,
    table_name: Option<&str>,
    _index_plan: Option<&crate::vdbe::explain::IndexPlanInfo>,
) -> Vec<Vec<Value>> {
    let has_order_by = !select.order_by.is_empty();
    let op_name = match select.compound.last().map(|(o, _)| *o) {
        Some(CompoundOperator::UnionAll) => "UNION ALL",
        Some(CompoundOperator::Union) => "UNION",
        Some(CompoundOperator::Intersect) => "INTERSECT",
        Some(CompoundOperator::Except) => "EXCEPT",
        None => "COMPOUND",
    };

    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut id = 1i64;

    if has_order_by {
        rows.push(vec![
            Value::Int(id),
            Value::Int(0),
            Value::Int(0),
            Value::Text(format!("MERGE ({op_name})")),
        ]);
        id += 1;
        rows.push(vec![Value::Int(id), Value::Int(1), Value::Int(0), Value::Text("LEFT".to_string())]);
        id += 1;
        rows.push(vec![
            Value::Int(id),
            Value::Int(2),
            Value::Int(0),
            Value::Text(scan_detail(table_name)),
        ]);
        id += 1;
        rows.push(vec![
            Value::Int(id),
            Value::Int(2),
            Value::Int(0),
            Value::Text("USE TEMP B-TREE FOR ORDER BY".to_string()),
        ]);
        id += 1;
        rows.push(vec![Value::Int(id), Value::Int(1), Value::Int(0), Value::Text("RIGHT".to_string())]);
        id += 1;
        rows.push(vec![
            Value::Int(id),
            Value::Int(5),
            Value::Int(0),
            Value::Text(scan_detail_for_arm(&select.compound[0].1)),
        ]);
        id += 1;
        rows.push(vec![
            Value::Int(id),
            Value::Int(5),
            Value::Int(0),
            Value::Text("USE TEMP B-TREE FOR ORDER BY".to_string()),
        ]);
    } else {
        rows.push(vec![
            Value::Int(id),
            Value::Int(0),
            Value::Int(0),
            Value::Text("COMPOUND QUERY".to_string()),
        ]);
        id += 1;
        rows.push(vec![
            Value::Int(id),
            Value::Int(1),
            Value::Int(0),
            Value::Text("LEFT-MOST SUBQUERY".to_string()),
        ]);
        id += 1;
        rows.push(vec![
            Value::Int(id),
            Value::Int(2),
            Value::Int(0),
            Value::Text(scan_detail(table_name)),
        ]);
        id += 1;
        let using_temp_btree = select.compound[0].0 != CompoundOperator::UnionAll;
        let op_detail = if using_temp_btree {
            format!("{op_name} USING TEMP B-TREE")
        } else {
            op_name.to_string()
        };
        rows.push(vec![Value::Int(id), Value::Int(1), Value::Int(0), Value::Text(op_detail)]);
        id += 1;
        rows.push(vec![
            Value::Int(id),
            Value::Int(id - 1),
            Value::Int(0),
            Value::Text(scan_detail_for_arm(&select.compound[0].1)),
        ]);
    }

    rows
}

fn scan_detail(table_name: Option<&str>) -> String {
    match table_name {
        Some(n) => format!("SCAN {n}"),
        None => "SCAN CONSTANT ROW".to_string(),
    }
}

fn scan_detail_for_arm(arm: &SelectStmt) -> String {
    if !arm.values.is_empty() {
        if arm.values.len() == 1 {
            return "SCAN CONSTANT ROW".to_string();
        }
        return format!("SCAN {}-ROW VALUES CLAUSE", arm.values.len());
    }
    if let Some(tor) = arm.from.first().and_then(|t| t.table()) {
        return format!("SCAN {}", tor.name);
    }
    "SCAN CONSTANT ROW".to_string()
}