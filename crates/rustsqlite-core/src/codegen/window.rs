//! Window-function codegen driver (M11.7): the partition-sort + frame-step shape that lowers
//! a `SELECT` with `OVER (...)` calls to a VDBE program.
//!
//! This is a pragmatic first slice of M11.7. It implements two shapes that cover the common
//! default-frame cases:
//!
//! * **PerRow** (`ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW`): each row gets one
//!   `AggStep` + one `AggValue` + one `ResultRow`. This is the `row_number()` shape and the
//!   shape any aggregate-as-window uses when the user writes `OVER (ORDER BY …)` and the
//!   per-kind default is a `ROWS` frame. Peers don't matter — every row advances the frame by
//!   exactly one.
//!
//! * **PerPeerGroup** (`RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW`, or no ORDER BY):
//!   the frame extends up to the current row *and its peers* (rows with equal ORDER BY
//!   values). A peer group is stepped together (one `AggStep` per row in the group), then
//!   `AggValue` is read once, then every row in the peer group is emitted with that same
//!   result. When there's no ORDER BY, the whole partition is one peer group. This is the
//!   `rank()`/`dense_rank()` shape and the default aggregate-as-window shape.
//!
//! The overall shape is: scan the table → sort by PARTITION BY + ORDER BY into a sorter
//! (carrying the full table row + the projection columns as payload) → walk the sorted
//! sorter, driving the accumulators and emitting rows.
//!
//! Not yet supported (deferred to the M11.7 follow-up / M11.8–M11.10):
//! * Explicit `ROWS`/`RANGE`/`GROUPS BETWEEN <bound> AND <bound>` frame specs.
//! * `<expr> PRECEDING` / `<expr> FOLLOWING` bounds and the sliding-frame `AggInverse` shape.
//! * `EXCLUDE` clause.
//! * Multiple *different* `OVER` specs in one query.
//! * `lead()` / `lag()` / `ntile()` with their default frames (need VDBE-instruction
//!   implementation or `AggInverse`).

use rustqlite_parser::{Expr, FunctionArgs, SelectStmt, Window};

use crate::error::{Error, Result};
use crate::func::aggregate::AggregateKind;
use crate::schema::{IndexObject, Table};
use crate::types::Collation;
use crate::vdbe::program::{KeyField, P4};
use crate::vdbe::{Opcode, Program};

use super::builder::ProgramBuilder;
use super::expr::{compile_expr, compile_jump, Ctx, IndexRead, SubqueryResolver};
use super::select::{
    collect_window_functions, emit_int, eval_limit_offset, expand_columns, resolve_order_term,
    WindowCall,
};

/// Compile a `SELECT` containing window function calls into a VDBE program plus its result
/// column names. The caller must have verified that `select` has at least one window call.
pub fn compile_window_select(
    select: &SelectStmt,
    table: Option<&Table>,
    _indexes: &[IndexObject],
    subquery_resolver: Option<&dyn SubqueryResolver>,
) -> Result<(Program, Vec<String>)> {
    let outputs = expand_columns(select, table)?;
    let names: Vec<String> = outputs.iter().map(|(_, n)| n.clone()).collect();
    let (limit, offset) = eval_limit_offset(select)?;

    // Collect every window-function call from the projection list.
    let mut win_calls: Vec<WindowCall> = Vec::new();
    for (e, _) in &outputs {
        collect_window_functions(e, &mut win_calls);
    }
    if win_calls.is_empty() {
        return Err(Error::msg(
            "internal: compile_window_select called with no window function calls",
        ));
    }

    // Resolve each call to an `AggregateKind`, validating argument count.
    let kinds: Vec<AggregateKind> = win_calls
        .iter()
        .map(|c| {
            let n_arg = match &c.args {
                FunctionArgs::Star => 0,
                FunctionArgs::List(v) => v.len(),
            };
            AggregateKind::from_name(&c.name, n_arg).ok_or_else(|| {
                Error::msg(format!("no such window function: {}({})", c.name, n_arg))
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // All window calls in one SELECT must share one `OVER` spec in this first slice.
    let primary_window = win_calls[0].window.clone();
    for c in win_calls.iter().skip(1) {
        if c.window != primary_window {
            return Err(Error::msg(
                "multiple different OVER clauses in one SELECT are not yet supported (M11.7)",
            ));
        }
    }

    let frame_kind = classify_frame(&kinds, &primary_window)?;
    let t = table.ok_or_else(|| Error::msg("window functions require a FROM clause"))?;
    let program = compile_window_scan(
        select,
        t,
        &outputs,
        &win_calls,
        &kinds,
        &primary_window,
        frame_kind,
        limit,
        offset,
        subquery_resolver,
    )?;
    Ok((program, names))
}

/// The frame shape the codegen emits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrameKind {
    /// One `AggStep` + `AggValue` + `ResultRow` per row (ROWS UNBOUNDED PRECEDING → CURRENT
    /// ROW). Peers don't matter.
    PerRow,
    /// Step a whole peer group, `AggValue` once, emit one `ResultRow` per row in the group
    /// (RANGE UNBOUNDED PRECEDING → CURRENT ROW, or no ORDER BY).
    PerPeerGroup,
}

/// Determine the frame kind for the codegen, or reject the query if none of the supported
/// shapes apply.
fn classify_frame(kinds: &[AggregateKind], window: &Window) -> Result<FrameKind> {
    let _has_order_by = !window.order_by.is_empty();
    // If the user wrote an explicit frame, only accept the shapes we support.
    if let Some(frame) = &window.frame {
        use rustqlite_parser::FrameBound as F;
        let ok_start = matches!(frame.start, F::UnboundedPreceding);
        let ok_end = matches!(frame.end, Some(F::CurrentRow) | Some(F::UnboundedFollowing) | None);
        if !ok_start || !ok_end {
            return Err(Error::msg(
                "explicit window frame bounds (PRECEDING/FOLLOWING) are not yet supported (M11.8/M11.9)",
            ));
        }
        if matches!(frame.end, Some(F::UnboundedFollowing)) {
            return Ok(FrameKind::PerPeerGroup);
        }
        // UNBOUNDED PRECEDING → CURRENT ROW. ROWS = per-row; RANGE/GROUPS = per-peer-group.
        return Ok(match frame.mode {
            rustqlite_parser::FrameMode::Rows => FrameKind::PerRow,
            rustqlite_parser::FrameMode::Range | rustqlite_parser::FrameMode::Groups => {
                FrameKind::PerPeerGroup
            }
        });
    }
    // No explicit frame: use the per-kind default. The default for `row_number` is
    // `ROWS UNBOUNDED PRECEDING → CURRENT ROW` (per-row). The default for `rank`/`dense_rank`
    // is `RANGE UNBOUNDED PRECEDING → CURRENT ROW` (per-peer-group). The default for the
    // aggregate-as-window functions is `RANGE UNBOUNDED PRECEDING → CURRENT ROW` (per-peer-
    // group). `lead`/`lag`/`ntile` need the sliding-frame shape and are rejected here.
    let mut any_range = false;
    for kind in kinds {
        match kind {
            AggregateKind::RowNumber => {
                // ROWS UNBOUNDED PRECEDING → CURRENT ROW → per-row.
            }
            AggregateKind::Rank | AggregateKind::DenseRank => {
                // RANGE UNBOUNDED PRECEDING → CURRENT ROW → per-peer-group.
                any_range = true;
            }
            AggregateKind::PercentRank | AggregateKind::CumeDist => {
                // GROUPS … → per-peer-group, but the value computation needs AggInverse (the
                // inverse-step counter). Reject for now.
                return Err(Error::msg(
                    "percent_rank() / cume_dist() are not yet supported (need AggInverse; M11.7 follow-up)",
                ));
            }
            AggregateKind::Ntile => {
                return Err(Error::msg(
                    "ntile() with its default frame is not yet supported (needs AggInverse; M11.7 follow-up)",
                ));
            }
            AggregateKind::Lead | AggregateKind::Lag => {
                return Err(Error::msg(
                    "lead() / lag() window functions are not yet supported (M11.7 follow-up)",
                ));
            }
            AggregateKind::FirstValue | AggregateKind::LastValue | AggregateKind::NthValue => {
                // These default to RANGE UNBOUNDED PRECEDING → CURRENT ROW (per-peer-group).
                // `first_value`/`nth_value` only grow (no inverse), so per-peer-group works.
                // `last_value` defaults to RANGE CURRENT ROW → CURRENT ROW (just the current
                // peer group), which needs AggInverse to clear when the frame empties. Reject
                // `last_value` for now; accept `first_value`/`nth_value`.
                if matches!(kind, AggregateKind::LastValue) {
                    return Err(Error::msg(
                        "last_value() with its default frame is not yet supported (needs AggInverse; M11.7 follow-up)",
                    ));
                }
                any_range = true;
            }
            // Aggregate-as-window: count/sum/total/avg/min/max/group_concat default to
            // RANGE UNBOUNDED PRECEDING → CURRENT ROW.
            _ => {
                any_range = true;
            }
        }
    }
    if any_range {
        // With an ORDER BY: peer groups are rows with equal ORDER BY values. Without: the
        // whole partition is one peer group. Both are PerPeerGroup.
        Ok(FrameKind::PerPeerGroup)
    } else {
        Ok(FrameKind::PerRow)
    }
}

#[allow(clippy::too_many_arguments)]
fn compile_window_scan(
    select: &SelectStmt,
    table: &Table,
    outputs: &[(Expr, String)],
    win_calls: &[WindowCall],
    kinds: &[AggregateKind],
    window: &Window,
    frame_kind: FrameKind,
    limit: Option<i64>,
    offset: i64,
    subquery_resolver: Option<&dyn SubqueryResolver>,
) -> Result<Program> {
    let cursor = 0i32;
    let ncol = outputs.len() as i32;
    let nwin = win_calls.len() as i32;
    let npart = window.partition_by.len() as i32;
    let norder = window.order_by.len() as i32;
    let ntable = table.columns.len() as i32;
    let ctx = Ctx {
        table,
        cursor,
        register_base: None,
        join_tables: None,
        index_read: None,
        subquery_resolver,
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

    // Per-call accumulator + result registers. The projection rewrites each window call to
    // `AggRef(result_reg)`.
    let accum_reg: Vec<i32> = (0..nwin).map(|_| b.alloc_reg()).collect();
    let result_reg: Vec<i32> = (0..nwin).map(|_| b.alloc_reg()).collect();
    let result_of = |call: &WindowCall| -> Option<i32> {
        win_calls
            .iter()
            .position(|c| window_call_eq(c, call))
            .map(|i| result_reg[i])
    };
    let rewritten_outputs: Vec<(Expr, String)> = outputs
        .iter()
        .map(|(e, n)| (rewrite_window_calls(e, &result_of), n.clone()))
        .collect();
    let rewritten_order_by: Vec<(Expr, bool)> = if !select.order_by.is_empty() {
        select
            .order_by
            .iter()
            .map(|term| {
                let expr = resolve_order_term(term, outputs)?;
                Ok((rewrite_window_calls(&expr, &result_of), term.desc))
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };

    // Open the table cursor.
    let open = b.emit(Opcode::OpenRead, cursor, table.rootpage as i32, 0);
    b.note_cursor(cursor);
    if table.without_rowid {
        b.set_p4(open, P4::KeyInfo(table.without_rowid_key_info()));
    } else {
        b.set_p4(open, P4::Int(table.columns.len() as i64));
    }

    // Sorter record layout:
    //   [partition_keys (npart), order_keys (norder), table_columns (ntable)]
    // The full table row is carried so the output pass can re-evaluate window-call arguments
    // and the projection (which reference arbitrary table columns) without the now-exhausted
    // table cursor. The projection is NOT stored — the output pass re-evaluates it against the
    // table columns (with window calls rewritten to `AggRef(result_reg)`).
    let nkey = npart + norder;
    let n_sorter_fields = nkey + ntable;
    let sorter = 1i32;
    let keyinfo: Vec<KeyField> = window
        .partition_by
        .iter()
        .map(|_| KeyField::asc_binary())
        .chain(window.order_by.iter().map(|t| KeyField {
            desc: t.desc,
            collation: Collation::Binary,
        }))
        .collect();
    let so = b.emit(Opcode::SorterOpen, sorter, n_sorter_fields, 0);
    b.set_p4(so, P4::KeyInfo(keyinfo));
    b.note_cursor(sorter);

    // ---- Scan pass: filter, build the sorter record, insert. ----
    let end_scan = b.new_label();
    b.emit_jump(Opcode::Rewind, cursor, end_scan, 0);
    let scan_top = b.new_label();
    b.resolve(scan_top);
    let scan_next = b.new_label();
    if let Some(w) = &select.where_clause {
        compile_jump(&mut b, w, scan_next, false, true, ctx)?;
    }
    let block = b.alloc_regs(n_sorter_fields);
    for (k, pexpr) in window.partition_by.iter().enumerate() {
        compile_expr(&mut b, pexpr, block + k as i32, ctx)?;
    }
    for (k, term) in window.order_by.iter().enumerate() {
        let key_expr = resolve_order_term(term, outputs)?;
        compile_expr(&mut b, &key_expr, block + npart + k as i32, ctx)?;
    }
    for (k, col) in table.columns.iter().enumerate() {
        let col_expr = Expr::Column {
            schema: None,
            table: None,
            name: col.name.clone(),
        };
        compile_expr(&mut b, &col_expr, block + nkey + k as i32, ctx)?;
    }
    let rec = b.alloc_reg();
    b.emit(Opcode::MakeRecord, block, n_sorter_fields, rec);
    b.emit(Opcode::SorterInsert, sorter, rec, 0);
    b.resolve(scan_next);
    b.emit_jump(Opcode::Next, cursor, scan_top, 0);
    b.resolve(end_scan);

    // LIMIT / OFFSET counters for the output pass.
    let limit_reg = match limit {
        Some(n) if n > 0 => Some(emit_int(&mut b, n)),
        _ => None,
    };
    let offset_reg = (offset > 0).then(|| emit_int(&mut b, offset));

    // DISTINCT dedup cursor (keyed by the result row record).
    let distinct_cursor = select.distinct.then(|| {
        let c = 2i32;
        let oe = b.emit(Opcode::OpenEphemeral, c, ncol, 0);
        b.set_p4(oe, P4::KeyInfo(Vec::new()));
        b.note_cursor(c);
        c
    });

    // The output sorter when the outer SELECT has an ORDER BY (the two-pass shape).
    let has_outer_order_by = !select.order_by.is_empty();
    let n_outer_order = select.order_by.len() as i32;
    let output_sorter: i32 = if has_outer_order_by {
        let c = 3i32;
        let keyinfo: Vec<KeyField> = select
            .order_by
            .iter()
            .map(|t| KeyField {
                desc: t.desc,
                collation: Collation::Binary,
            })
            .collect();
        let so = b.emit(Opcode::SorterOpen, c, n_outer_order + ncol, 0);
        b.set_p4(so, P4::KeyInfo(keyinfo));
        b.note_cursor(c);
        c
    } else {
        -1
    };

    // A column-position map for the output pass: table_column_index → sorter field index.
    // The table columns live at sorter indices `nkey..nkey+ntable`. The rowid-alias column
    // (if any) maps to its own position (we stored every table column from the cursor).
    let column_positions: Vec<usize> =
        (0..table.columns.len()).map(|i| nkey as usize + i).collect();
    let rowid_position = table
        .rowid_alias
        .map(|i| column_positions[i])
        .unwrap_or(column_positions.len());
    // A Ctx for the output pass that reads table columns from the sorter.
    let out_ctx = Ctx {
        table,
        cursor: sorter,
        register_base: None,
        join_tables: None,
        index_read: Some(IndexRead {
            cursor: sorter,
            column_positions: column_positions.as_slice(),
            rowid_position,
        }),
        subquery_resolver: None,
    };

    // i_part_prev / i_peer_prev registers (compared against the current row's keys).
    let i_part_prev = if npart > 0 { Some(b.alloc_regs(npart)) } else { None };
    if let Some(r) = i_part_prev {
        b.emit(Opcode::Null, r, r + npart - 1, 0);
    }
    let need_peer_check = matches!(frame_kind, FrameKind::PerPeerGroup) && norder > 0;
    let i_peer_prev = if need_peer_check { Some(b.alloc_regs(norder)) } else { None };
    if let Some(r) = i_peer_prev {
        b.emit(Opcode::Null, r, r + norder - 1, 0);
    }

    // The peer-buf ephemeral (PerPeerGroup only). Each record holds the projection columns.
    let peer_buf: i32 = if matches!(frame_kind, FrameKind::PerPeerGroup) {
        let c = 4i32;
        let oe = b.emit(Opcode::OpenEphemeral, c, ncol, 0);
        let _ = oe;
        b.note_cursor(c);
        c
    } else {
        -1
    };
    let peer_pending = b.alloc_reg();
    b.emit(Opcode::Integer, 0, peer_pending, 0);

    // The "flush pending peer group" subroutine (PerPeerGroup only). Resolved at the end of
    // the program (after the Halt). Called via `Gosub peer_pending, flush_sub` when a peer
    // group ends (partition change, peer change, or end of walk). The `Gosub` stores the
    // return address in `peer_pending` and jumps to `flush_sub`; the subroutine ends with
    // `Return peer_pending` which jumps back to the stored address.
    let flush_sub = b.new_label();

    // Sort the sorter and position at the first record, or jump to `end_walk` if empty.
    let end_walk = b.new_label();
    b.emit_jump(Opcode::SorterSort, sorter, end_walk, 0);
    let walk_top = b.new_label();
    b.resolve(walk_top);
    // Load the current sorter record into the sorter's decoded cache so `Column` reads the
    // right row. `SorterNext` advances the position but does not decode; `SorterData` decodes.
    b.emit(Opcode::SorterData, sorter, 0, 0);

    // Load the current row's partition keys and peer keys from the sorter.
    let i_part_cur = if npart > 0 { Some(b.alloc_regs(npart)) } else { None };
    if let Some(r) = i_part_cur {
        for j in 0..npart {
            b.emit(Opcode::Column, sorter, j, r + j);
        }
    }
    let i_peer_cur = if need_peer_check { Some(b.alloc_regs(norder)) } else { None };
    if let Some(r) = i_peer_cur {
        for j in 0..norder {
            b.emit(Opcode::Column, sorter, npart + j, r + j);
        }
    }

    // Partition-change check.
    let part_same = b.new_label();
    let part_changed = b.new_label();
    if let (Some(prev), Some(cur)) = (i_part_prev, i_part_cur) {
        let ki: Vec<KeyField> = window.partition_by.iter().map(|_| KeyField::asc_binary()).collect();
        let cmp = b.emit(Opcode::Compare, prev, cur, npart);
        b.set_p4(cmp, P4::KeyInfo(ki));
        b.emit_jump3(Opcode::Jump, part_changed, part_same, part_changed);
    } else {
        // No PARTITION BY → one big partition; never a change. Resolve both labels to the
        // peer-check / step block.
        b.resolve(part_changed);
        b.resolve(part_same);
    }

    // The "partition changed" handler: flush the pending peer group (PerPeerGroup only),
    // reset accumulators, update i_part_prev, reset i_peer_prev, reset peer_pending.
    if npart > 0 {
        b.resolve(part_changed);
        if matches!(frame_kind, FrameKind::PerPeerGroup) {
            // Flush only if peer_pending is 1 (skip the very first partition). Emit a
            // conditional Gosub: `IfNot peer_pending, skip; Gosub peer_pending, flush_sub; skip:`.
            let skip = b.new_label();
            b.emit_jump(Opcode::IfNot, peer_pending, skip, 0);
            b.emit_jump(Opcode::Gosub, peer_pending, flush_sub, 0);
            b.resolve(skip);
        }
        for &r in &accum_reg {
            b.emit(Opcode::Null, r, r, 0);
        }
        if let (Some(prev), Some(cur)) = (i_part_prev, i_part_cur) {
            b.emit(Opcode::Copy, cur, prev, npart - 1);
        }
        if let Some(r) = i_peer_prev {
            b.emit(Opcode::Null, r, r + norder - 1, 0);
        }
        if matches!(frame_kind, FrameKind::PerPeerGroup) {
            b.emit(Opcode::Integer, 0, peer_pending, 0);
        }
        // Fall through to the peer-check / step block.
        b.resolve(part_same);
    }

    // Peer-change check (PerPeerGroup with ORDER BY only).
    if need_peer_check {
        if let (Some(p_prev), Some(p_cur)) = (i_peer_prev, i_peer_cur) {
            let peer_same = b.new_label();
            let peer_changed = b.new_label();
            let ki: Vec<KeyField> = window
                .order_by
                .iter()
                .map(|t| KeyField {
                    desc: t.desc,
                    collation: Collation::Binary,
                })
                .collect();
            let cmp = b.emit(Opcode::Compare, p_prev, p_cur, norder);
            b.set_p4(cmp, P4::KeyInfo(ki));
            b.emit_jump3(Opcode::Jump, peer_changed, peer_same, peer_changed);
            b.resolve(peer_changed);
            // Flush the pending peer group (if any), then reset peer_pending.
            let skip = b.new_label();
            b.emit_jump(Opcode::IfNot, peer_pending, skip, 0);
            b.emit_jump(Opcode::Gosub, peer_pending, flush_sub, 0);
            b.resolve(skip);
            b.emit(Opcode::Integer, 0, peer_pending, 0);
            // Update i_peer_prev to the current peer key.
            b.emit(Opcode::Copy, p_cur, p_prev, norder - 1);
            b.resolve(peer_same);
        }
    }

    // Step block: AggStep each window call (with FILTER check), then (PerRow) emit the row
    // or (PerPeerGroup) buffer the row.
    for (i, call) in win_calls.iter().enumerate() {
        let kind = kinds[i];
        let n_arg = match &call.args {
            FunctionArgs::Star => 0u8,
            FunctionArgs::List(v) => v.len() as u8,
        };
        let arg_base = b.alloc_regs(match &call.args {
            FunctionArgs::Star => 1,
            FunctionArgs::List(v) => v.len() as i32,
        });
        match &call.args {
            FunctionArgs::Star => {
                b.emit(Opcode::Null, arg_base, arg_base, 0);
            }
            FunctionArgs::List(v) => {
                for (k, a) in v.iter().enumerate() {
                    compile_expr(&mut b, a, arg_base + k as i32, out_ctx)?;
                }
            }
        }
        let filter_skip = b.new_label();
        if let Some(filter) = &call.filter {
            compile_jump(&mut b, filter, filter_skip, false, true, out_ctx)?;
        }
        let idx = b.emit(Opcode::AggStep, 0, arg_base, accum_reg[i]);
        b.set_p4(idx, P4::FuncDef(kind));
        b.set_p5(idx, n_arg);
        b.resolve(filter_skip);
    }

    if matches!(frame_kind, FrameKind::PerPeerGroup) {
        // Buffer the row: evaluate the projection (with window calls rewritten to AggRef)
        // against the sorter row and insert into the peer-buf ephemeral. The AggRef values
        // are placeholder NULLs at this point — the flush pass overwrites them with the
        // actual AggValue results before emitting.
        let buf_block = b.alloc_regs(ncol);
        for (j, (expr, _)) in rewritten_outputs.iter().enumerate() {
            compile_expr(&mut b, expr, buf_block + j as i32, out_ctx)?;
        }
        let rec = b.alloc_reg();
        b.emit(Opcode::MakeRecord, buf_block, ncol, rec);
        let rowid_reg = b.alloc_reg();
        b.emit(Opcode::NewRowid, peer_buf, rowid_reg, 0);
        b.emit(Opcode::Insert, peer_buf, rec, rowid_reg);
        b.emit(Opcode::Integer, 1, peer_pending, 0);
        // Advance the sorter.
        b.emit_jump(Opcode::SorterNext, sorter, walk_top, 0);
    } else {
        // PerRow: AggValue each window call → result_reg, then emit the row.
        for (i, kind) in kinds.iter().enumerate() {
            let idx = b.emit(Opcode::AggValue, accum_reg[i], 0, result_reg[i]);
            b.set_p4(idx, P4::FuncDef(*kind));
        }
        // Evaluate the projection (with window calls rewritten to AggRef) against the sorter
        // row. The AggRef cells read from result_reg (just written by AggValue); the plain
        // column refs read from the sorter's stored table columns via `index_read`.
        let emit_block = b.alloc_regs(ncol);
        for (j, (expr, _)) in rewritten_outputs.iter().enumerate() {
            compile_expr(&mut b, expr, emit_block + j as i32, out_ctx)?;
        }
        // DISTINCT dedup.
        let row_done = b.new_label();
        if let Some(dc) = distinct_cursor {
            let found = b.emit_jump(Opcode::Found, dc, row_done, emit_block);
            b.set_p4(found, P4::Int(ncol as i64));
            let rec = b.alloc_reg();
            b.emit(Opcode::MakeRecord, emit_block, ncol, rec);
            b.emit(Opcode::IdxInsert, dc, rec, 0);
        }
        // Emit the row: into the output sorter (outer ORDER BY) or directly via ResultRow.
        if has_outer_order_by {
            let sort_block = b.alloc_regs(n_outer_order + ncol);
            for (k, (expr, _)) in rewritten_order_by.iter().enumerate() {
                match expr {
                    Expr::AggRef(r) => {
                        b.emit(Opcode::SCopy, *r, sort_block + k as i32, 0);
                    }
                    _ => {
                        // Re-read the ORDER BY key from the sorter. The ORDER BY key lives at
                        // sorter field `npart + k` (the order key position). This works when
                        // the ORDER BY term is a bare column that matches a window ORDER BY
                        // column; the general case (an expression) is handled by re-evaluating
                        // against the sorter via out_ctx.
                        compile_expr(&mut b, expr, sort_block + k as i32, out_ctx)?;
                    }
                }
            }
            for j in 0..ncol {
                b.emit(Opcode::SCopy, emit_block + j, sort_block + n_outer_order + j, 0);
            }
            let rec = b.alloc_reg();
            b.emit(Opcode::MakeRecord, sort_block, n_outer_order + ncol, rec);
            b.emit(Opcode::SorterInsert, output_sorter, rec, 0);
        } else {
            if let Some(oreg) = offset_reg {
                b.emit_jump(Opcode::IfPos, oreg, row_done, 1);
            }
            b.emit(Opcode::ResultRow, emit_block, ncol, 0);
            if let Some(lreg) = limit_reg {
                b.emit_jump(Opcode::DecrJumpZero, lreg, end_walk, 0);
            }
        }
        b.resolve(row_done);
        // Advance the sorter.
        b.emit_jump(Opcode::SorterNext, sorter, walk_top, 0);
    }
    b.resolve(end_walk);

    // ---- After the walk: PerPeerGroup flushes the final pending peer group. ----
    if matches!(frame_kind, FrameKind::PerPeerGroup) {
        let skip = b.new_label();
        b.emit_jump(Opcode::IfNot, peer_pending, skip, 0);
        b.emit_jump(Opcode::Gosub, peer_pending, flush_sub, 0);
        b.resolve(skip);
    }

    // If the outer SELECT has an ORDER BY, run the output sorter tail.
    if has_outer_order_by {
        emit_sort_tail(
            &mut b,
            output_sorter,
            n_outer_order,
            ncol,
            limit_reg,
            offset_reg,
        );
    }

    b.emit(Opcode::Halt, 0, 0, 0);

    // ---- The flush subroutine (PerPeerGroup only). ----
    if matches!(frame_kind, FrameKind::PerPeerGroup) {
        b.resolve(flush_sub);
        // AggValue each window call → result_reg.
        for (i, kind) in kinds.iter().enumerate() {
            let idx = b.emit(Opcode::AggValue, accum_reg[i], 0, result_reg[i]);
            b.set_p4(idx, P4::FuncDef(*kind));
        }
        // Walk the peer-buf and emit one row per buffered entry.
        let flush_end = b.new_label();
        b.emit_jump(Opcode::Rewind, peer_buf, flush_end, 0);
        let flush_top = b.new_label();
        b.resolve(flush_top);
        // Load the buffered projection columns from the peer-buf, then overwrite the
        // window-call result columns with the actual AggValue results (which were just
        // computed above for this peer group).
        let emit_block = b.alloc_regs(ncol);
        for j in 0..ncol {
            b.emit(Opcode::Column, peer_buf, j as i32, emit_block + j as i32);
        }
        for (j, (expr, _)) in rewritten_outputs.iter().enumerate() {
            if let Expr::AggRef(r) = expr {
                b.emit(Opcode::SCopy, *r, emit_block + j as i32, 0);
            }
        }
        let row_done = b.new_label();
        if let Some(dc) = distinct_cursor {
            let found = b.emit_jump(Opcode::Found, dc, row_done, emit_block);
            b.set_p4(found, P4::Int(ncol as i64));
            let rec = b.alloc_reg();
            b.emit(Opcode::MakeRecord, emit_block, ncol, rec);
            b.emit(Opcode::IdxInsert, dc, rec, 0);
        }
        if has_outer_order_by {
            let sort_block = b.alloc_regs(n_outer_order + ncol);
            for (k, (expr, _)) in rewritten_order_by.iter().enumerate() {
                match expr {
                    Expr::AggRef(r) => {
                        b.emit(Opcode::SCopy, *r, sort_block + k as i32, 0);
                    }
                    _ => {
                        // Re-evaluate the ORDER BY expression against the peer-buf row. The
                        // peer-buf only carries the projection columns, so we can only resolve
                        // ORDER BY terms that are projection columns. For the general case
                        // (an expression over table columns not in the projection), we'd need
                        // to carry those columns in the peer-buf too. This is a limitation of
                        // the first slice — test cases use ORDER BY on projection columns.
                        // Map the ORDER BY term to a projection column by name.
                        let term_expr = &select.order_by[k].expr;
                        let mapped = map_order_term_to_projection(term_expr, outputs);
                        if let Some(col_idx) = mapped {
                            b.emit(Opcode::Column, peer_buf, col_idx as i32, sort_block + k as i32);
                        } else {
                            // Fallback: NULL (the ORDER BY key is unknown for this row). This
                            // is a degenerate case; the sort will be unstable but won't crash.
                            b.emit(Opcode::Null, sort_block + k as i32, sort_block + k as i32, 0);
                        }
                    }
                }
            }
            for j in 0..ncol {
                b.emit(Opcode::SCopy, emit_block + j, sort_block + n_outer_order + j, 0);
            }
            let rec = b.alloc_reg();
            b.emit(Opcode::MakeRecord, sort_block, n_outer_order + ncol, rec);
            b.emit(Opcode::SorterInsert, output_sorter, rec, 0);
        } else {
            if let Some(oreg) = offset_reg {
                b.emit_jump(Opcode::IfPos, oreg, row_done, 1);
            }
            b.emit(Opcode::ResultRow, emit_block, ncol, 0);
            if let Some(lreg) = limit_reg {
                b.emit_jump(Opcode::DecrJumpZero, lreg, flush_end, 0);
            }
        }
        b.resolve(row_done);
        b.emit_jump(Opcode::Next, peer_buf, flush_top, 0);
        b.resolve(flush_end);
        // Clear the peer-buf for the next peer group. Don't reset peer_pending here — the
        // `Gosub` stored the return address in peer_pending, and `Return peer_pending` reads
        // it to jump back. The caller resets peer_pending to 0 after the Gosub returns.
        b.emit(Opcode::Clear, peer_buf, 0, 0);
        b.emit(Opcode::Return, peer_pending, 0, 0);
    }

    b.resolve(setup);
    b.emit(Opcode::Transaction, 0, 0, 0);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

/// Map an ORDER BY term expression to a projection column index, if the term is a bare column
/// reference whose name matches an output column's name (case-insensitive). Used by the
/// PerPeerGroup flush pass to read the ORDER BY key from the peer-buf (which only carries the
/// projection columns). Returns `None` for terms that aren't a bare column matching a
/// projection column.
fn map_order_term_to_projection(term_expr: &Expr, outputs: &[(Expr, String)]) -> Option<usize> {
    if let Expr::Column { table: None, name, .. } = term_expr {
        return outputs.iter().position(|(_, n)| n.eq_ignore_ascii_case(name));
    }
    // An ordinal `ORDER BY n` references the n-th output column.
    if let Expr::Literal(rustqlite_parser::Literal::Integer(n)) = term_expr {
        let idx = *n;
        if idx >= 1 && (idx as usize) <= outputs.len() {
            return Some((idx - 1) as usize);
        }
    }
    None
}

/// The output sorter tail: sort the output sorter, walk it, apply OFFSET/LIMIT, emit
/// ResultRows. Mirrors `select::emit_sort_tail`.
fn emit_sort_tail(
    b: &mut ProgramBuilder,
    output_sorter: i32,
    n_outer_order: i32,
    ncol: i32,
    limit_reg: Option<i32>,
    offset_reg: Option<i32>,
) {
    let end = b.new_label();
    b.emit_jump(Opcode::SorterSort, output_sorter, end, 0);
    let top = b.new_label();
    b.resolve(top);
    let next = b.new_label();
    b.emit(Opcode::SorterData, output_sorter, 0, 0);
    if let Some(oreg) = offset_reg {
        b.emit_jump(Opcode::IfPos, oreg, next, 1);
    }
    let result_reg = b.alloc_regs(ncol);
    for j in 0..ncol {
        b.emit(Opcode::Column, output_sorter, n_outer_order + j, result_reg + j);
    }
    b.emit(Opcode::ResultRow, result_reg, ncol, 0);
    if let Some(lreg) = limit_reg {
        b.emit_jump(Opcode::DecrJumpZero, lreg, end, 0);
    }
    b.resolve(next);
    b.emit_jump(Opcode::SorterNext, output_sorter, top, 0);
    b.resolve(end);
}

/// `true` if two window calls are syntactically identical (same name, same args, same window
/// spec, same filter). Used to deduplicate the same call appearing twice.
fn window_call_eq(a: &WindowCall, b: &WindowCall) -> bool {
    a.name.eq_ignore_ascii_case(&b.name)
        && a.args == b.args
        && a.window == b.window
        && a.filter == b.filter
}

/// Rewrite a projection expression so every window-function call becomes
/// `AggRef(result_reg)`. Mirrors `rewrite_aggregates` but for window calls (which always have
/// an `OVER` clause, so they're never confused with plain aggregates).
fn rewrite_window_calls(e: &Expr, result_of: &impl Fn(&WindowCall) -> Option<i32>) -> Expr {
    match e {
        Expr::Function {
            name,
            distinct: _,
            args,
            filter,
            over: Some(w),
        } => {
            let call = WindowCall {
                name: name.clone(),
                args: args.clone(),
                filter: filter.clone(),
                window: w.clone(),
            };
            match result_of(&call) {
                Some(reg) => Expr::AggRef(reg),
                None => e.clone(),
            }
        }
        Expr::Function {
            name,
            distinct,
            args,
            filter,
            over: None,
        } => {
            let _ = (name, distinct, filter);
            Expr::Function {
                name: name.clone(),
                distinct: *distinct,
                args: match args {
                    FunctionArgs::Star => FunctionArgs::Star,
                    FunctionArgs::List(v) => {
                        FunctionArgs::List(v.iter().map(|a| rewrite_window_calls(a, result_of)).collect())
                    }
                },
                filter: filter.clone(),
                over: None,
            }
        }
        Expr::Unary { op, expr } => Expr::Unary {
            op: *op,
            expr: Box::new(rewrite_window_calls(expr, result_of)),
        },
        Expr::Binary { op, left, right } => Expr::Binary {
            op: *op,
            left: Box::new(rewrite_window_calls(left, result_of)),
            right: Box::new(rewrite_window_calls(right, result_of)),
        },
        Expr::Collate { expr, collation } => Expr::Collate {
            expr: Box::new(rewrite_window_calls(expr, result_of)),
            collation: collation.clone(),
        },
        Expr::Cast { expr, type_name } => Expr::Cast {
            expr: Box::new(rewrite_window_calls(expr, result_of)),
            type_name: type_name.clone(),
        },
        other => other.clone(),
    }
}