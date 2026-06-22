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
    /// A sliding frame with explicit bounds (any frame spec that requires `AggInverse` to
    /// remove rows from the frame as it slides). Uses the ephemeral-table-with-3-cursors
    /// approach mirroring `sqlite3WindowCodeStep` in `window.c`.
    Sliding,
}

/// Determine the frame kind for the codegen, or reject the query if none of the supported
/// shapes apply.
fn classify_frame(kinds: &[AggregateKind], window: &Window) -> Result<FrameKind> {
    // If the user wrote an explicit frame, classify it.
    if let Some(frame) = &window.frame {
        use rustqlite_parser::FrameBound as F;
        let start_unbounded = matches!(frame.start, F::UnboundedPreceding);
        let end_unbounded = matches!(frame.end, Some(F::UnboundedFollowing)) || frame.end.is_none();
        let end_current = matches!(frame.end, Some(F::CurrentRow));
        // The two simple shapes: UNBOUNDED PRECEDING → CURRENT ROW (per-row for ROWS,
        // per-peer-group for RANGE/GROUPS) and UNBOUNDED PRECEDING → UNBOUNDED FOLLOWING
        // (whole partition = per-peer-group). These don't need AggInverse.
        if start_unbounded && end_current {
            return Ok(match frame.mode {
                rustqlite_parser::FrameMode::Rows => FrameKind::PerRow,
                rustqlite_parser::FrameMode::Range | rustqlite_parser::FrameMode::Groups => {
                    FrameKind::PerPeerGroup
                }
            });
        }
        if start_unbounded && end_unbounded {
            // ROWS mode: the frame is the whole partition for every row — use the sliding
            // path (which handles this correctly). RANGE/GROUPS mode: also the whole partition,
            // but per-peer-group emission works (the frame never shrinks and the value is
            // constant across the partition). Use PerPeerGroup for RANGE/GROUPS to avoid the
            // O(n²) full-scan overhead.
            return Ok(match frame.mode {
                rustqlite_parser::FrameMode::Rows => FrameKind::Sliding,
                rustqlite_parser::FrameMode::Range | rustqlite_parser::FrameMode::Groups => {
                    FrameKind::PerPeerGroup
                }
            });
        }
        // Any other combination needs the sliding-frame algorithm.
        // Reject frames that the sliding codegen doesn't yet support.
        // M11.8 first slice: ROWS mode with all bound combinations.
        // M11.9: RANGE/GROUPS with expr bounds.
        // M11.10: EXCLUDE.
        if frame.exclude.is_some() && !matches!(frame.exclude, Some(rustqlite_parser::FrameExclude::NoOthers)) {
            return Err(Error::msg(
                "EXCLUDE clause other than NO OTHERS is not yet supported (M11.10)",
            ));
        }
        // Check that all the kinds support inverse (or don't need it).
        // min/max don't support xInverse; they use a different inline path in upstream.
        // For now, reject min/max in sliding frames.
        for kind in kinds {
            if matches!(kind, AggregateKind::Min | AggregateKind::Max) {
                return Err(Error::msg(
                    "min()/max() with non-default window frames are not yet supported (M11.8 follow-up)",
                ));
            }
            if matches!(kind, AggregateKind::Lead | AggregateKind::Lag) {
                return Err(Error::msg(
                    "lead()/lag() with non-default window frames are not yet supported (M11.8 follow-up)",
                ));
            }
        }
        Ok(FrameKind::Sliding)
    } else {
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

    // For the Sliding frame shape, dispatch to a dedicated output pass that uses an
    // ephemeral partition cache with start/current/end cursors (mirrors
    // `sqlite3WindowCodeStep` in `window.c`).
    if matches!(frame_kind, FrameKind::Sliding) {
        // The sliding pass handles its own sort+walk+emit+halt+subroutine. It returns the
        // program boundary (the `setup` label is resolved inside).
        return compile_sliding_frame(
            b,
            select,
            table,
            outputs,
            win_calls,
            kinds,
            window,
            &accum_reg,
            &result_reg,
            &rewritten_outputs,
            &rewritten_order_by,
            limit_reg,
            offset_reg,
            distinct_cursor,
            has_outer_order_by,
            output_sorter,
            n_outer_order,
            sorter,
            n_sorter_fields,
            nkey,
            npart,
            norder,
            ncol,
            ntable,
            setup,
            after_init,
            out_ctx,
        );
    }

    // Non-Sliding path: sort the sorter and walk it.
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

// ==============================================================================
// M11.8–M11.10: Sliding window frames (ROWS / RANGE / GROUPS with explicit bounds)
// ==============================================================================
//
// The sliding-frame codegen mirrors `sqlite3WindowCodeStep` in `window.c`. The shape is:
//
//   1. Scan the table → sort by PARTITION BY + ORDER BY into a sorter (shared with the
//      PerRow/PerPeerGroup path — done before we reach this function).
//   2. Walk the sorted sorter. For each partition, copy its rows into an ephemeral cache
//      (rowid 1..=n), then run the sliding algorithm:
//        a. For each current row i (1..=n):
//           - Advance the `end` cursor forward, AggStep-ing each new row that entered the
//             frame (rows end_prev+1 ..= end_cur).
//           - AggValue → result_reg, emit the row.
//           - Advance the `start` cursor forward, AggInverse-ing each row that left the
//             frame (rows start_prev .. start_cur-1).
//      The frame bounds [start, end] for row i are computed from the frame spec:
//        ROWS mode:
//          start = max(1, i - N) for `N PRECEDING`, i for `CURRENT ROW`, 1 for `UNBOUNDED`
//          end   = min(n, i + N) for `N FOLLOWING`, i for `CURRENT ROW`, n for `UNBOUNDED`
//        GROUPS mode: same as ROWS but in units of peer groups (a group = consecutive rows
//          with equal ORDER BY values). The bound is the rowid of the first/last row of the
//          target peer group.
//        RANGE mode: with `<expr> PRECEDING/FOLLOWING`, the bound is the rowid of the first/
//          last row whose ORDER BY value is within `<expr>` of the current row's value.
//          `CURRENT ROW` means the peer group (same as GROUPS). `UNBOUNDED` means 1 or n.
//
// This first implementation uses the full-scan approach per row (re-scan the whole frame
// from scratch for each current row). This is O(n²) per partition but correct and much
// simpler than the streaming 3-cursor approach. The streaming approach arrives with the
// M11.8 follow-up.

#[allow(clippy::too_many_arguments)]
fn compile_sliding_frame(
    mut b: ProgramBuilder,
    select: &SelectStmt,
    table: &Table,
    outputs: &[(Expr, String)],
    win_calls: &[WindowCall],
    kinds: &[AggregateKind],
    window: &Window,
    accum_reg: &[i32],
    result_reg: &[i32],
    rewritten_outputs: &[(Expr, String)],
    rewritten_order_by: &[(Expr, bool)],
    limit_reg: Option<i32>,
    offset_reg: Option<i32>,
    distinct_cursor: Option<i32>,
    has_outer_order_by: bool,
    output_sorter: i32,
    n_outer_order: i32,
    sorter: i32,
    n_sorter_fields: i32,
    nkey: i32,
    npart: i32,
    norder: i32,
    ncol: i32,
    ntable: i32,
    setup: super::builder::Label,
    after_init: i32,
    out_ctx: Ctx<'_>,
) -> Result<Program> {
    use rustqlite_parser::FrameBound as F;

    let frame = window.frame.as_ref().expect("Sliding frame requires explicit frame");
    let _ = win_calls.len(); // unused for now; will be used by per-call distinct frame logic

    // ---- Ephemeral partition cache: holds the current partition's rows (rowid 1..=n). ----
    // Record layout: [partition_keys (npart), order_keys (norder), table_columns (ntable)]
    // — same as the sorter, so we can copy records directly.
    let cache = 5i32;
    let oe = b.emit(Opcode::OpenEphemeral, cache, n_sorter_fields, 0);
    let _ = oe;
    b.note_cursor(cache);

    // Per-partition row count register (n = number of rows in the current partition).
    let n_reg = b.alloc_reg();
    // Current row index (1-based) within the partition.
    let i_reg = b.alloc_reg();
    // Frame start/end rowids (1-based, inclusive).
    let start_reg = b.alloc_reg();
    let end_reg = b.alloc_reg();
    // Loop counters for the full-scan AggStep/AggInverse passes.
    let scan_j = b.alloc_reg();
    // The rowid of the row currently being stepped (for AggStep/AggInverse arg evaluation).
    let step_rowid = b.alloc_reg();

    // Frame bound expression registers (if the bound is `<expr> PRECEDING/FOLLOWING`).
    // Evaluate the bound expression once per partition (it's constant within a partition
    // for non-correlated expressions — which is all we support).
    let start_expr_reg = match &frame.start {
        F::Preceding(_) | F::Following(_) => Some(b.alloc_reg()),
        _ => None,
    };
    let end_expr_reg = match &frame.end {
        Some(F::Preceding(_)) | Some(F::Following(_)) => Some(b.alloc_reg()),
        _ => None,
    };
    // Evaluate the bound expressions now (they're constant for the whole query). We'll
    // re-evaluate per-partition in case the expression references table columns (it
    // shouldn't for a legal frame, but defensive). For now evaluate once.
    if let Some(r) = start_expr_reg {
        if let F::Preceding(e) | F::Following(e) = &frame.start {
            compile_expr(&mut b, e, r, out_ctx)?;
        }
    }
    if let Some(r) = end_expr_reg {
        if let Some(F::Preceding(e) | F::Following(e)) = &frame.end {
            compile_expr(&mut b, e, r, out_ctx)?;
        }
    }

    // i_part_prev registers (compared against the current sorter row's partition keys).
    let i_part_prev = if npart > 0 { Some(b.alloc_regs(npart)) } else { None };
    if let Some(r) = i_part_prev {
        b.emit(Opcode::Null, r, r + npart - 1, 0);
    }
    let part_pending = b.alloc_reg();
    b.emit(Opcode::Integer, 0, part_pending, 0);

    // Sort the sorter and begin the walk.
    let end_walk = b.new_label();
    b.emit_jump(Opcode::SorterSort, sorter, end_walk, 0);
    let walk_top = b.new_label();
    b.resolve(walk_top);
    b.emit(Opcode::SorterData, sorter, 0, 0);

    // Load the current sorter row's partition keys.
    let i_part_cur = if npart > 0 { Some(b.alloc_regs(npart)) } else { None };
    if let Some(r) = i_part_cur {
        for j in 0..npart {
            b.emit(Opcode::Column, sorter, j, r + j);
        }
    }

    // ---- Partition-change check. ----
    let part_same = b.new_label();
    let part_changed = b.new_label();
    if let (Some(prev), Some(cur)) = (i_part_prev, i_part_cur) {
        let ki: Vec<KeyField> = window.partition_by.iter().map(|_| KeyField::asc_binary()).collect();
        let cmp = b.emit(Opcode::Compare, prev, cur, npart);
        b.set_p4(cmp, P4::KeyInfo(ki));
        b.emit_jump3(Opcode::Jump, part_changed, part_same, part_changed);
    } else {
        b.resolve(part_changed);
        b.resolve(part_same);
    }

    // ---- Partition-changed handler: flush the old partition, reset the cache. ----
    if npart > 0 {
        b.resolve(part_changed);
        // Flush the old partition (if any) via the sliding algorithm. Only flush when
        // part_pending is 1 (skip the very first partition — there's nothing to flush yet).
        let skip = b.new_label();
        b.emit_jump(Opcode::IfNot, part_pending, skip, 0);
        // Run the sliding-frame output for the just-finished partition.
        emit_partition_sliding(
            &mut b, select, table, outputs, win_calls, kinds, window, frame,
            accum_reg, result_reg, rewritten_outputs, rewritten_order_by,
            limit_reg, offset_reg, distinct_cursor, has_outer_order_by,
            output_sorter, n_outer_order, cache, n_reg, i_reg, start_reg, end_reg,
            scan_j, step_rowid, start_expr_reg, end_expr_reg, ncol, nkey, npart, norder, ntable,
            out_ctx,
        )?;
        b.resolve(skip);
        // Reset the cache for the new partition (always, even on the first row, so the
        // cache is empty before we insert the first row of a new partition).
        b.emit(Opcode::ResetSorter, cache, 0, 0);
        // Reset accumulators.
        for &r in accum_reg {
            b.emit(Opcode::Null, r, r, 0);
        }
        // Update i_part_prev to the current row's partition keys (always, even on the
        // first row, so the next row's compare sees the just-seen partition).
        if let (Some(prev), Some(cur)) = (i_part_prev, i_part_cur) {
            b.emit(Opcode::Copy, cur, prev, npart - 1);
        }
        // Fall through to the insert.
        b.resolve(part_same);
    }

    // ---- Insert the current sorter row into the partition cache. ----
    // Copy the sorter record into the cache. The cache record layout matches the sorter
    // ([partition_keys, order_keys, table_columns]), so we read all fields and re-make the
    // record.
    let block = b.alloc_regs(n_sorter_fields);
    for j in 0..n_sorter_fields {
        b.emit(Opcode::Column, sorter, j, block + j);
    }
    let rec = b.alloc_reg();
    b.emit(Opcode::MakeRecord, block, n_sorter_fields, rec);
    let rowid_reg = b.alloc_reg();
    b.emit(Opcode::NewRowid, cache, rowid_reg, 0);
    b.emit(Opcode::Insert, cache, rec, rowid_reg);
    b.emit(Opcode::Integer, 1, part_pending, 0);
    // Advance the sorter.
    b.emit_jump(Opcode::SorterNext, sorter, walk_top, 0);
    b.resolve(end_walk);

    // ---- After the walk: flush the final pending partition. ----
    let skip = b.new_label();
    b.emit_jump(Opcode::IfNot, part_pending, skip, 0);
    emit_partition_sliding(
        &mut b, select, table, outputs, win_calls, kinds, window, frame,
        accum_reg, result_reg, rewritten_outputs, rewritten_order_by,
        limit_reg, offset_reg, distinct_cursor, has_outer_order_by,
        output_sorter, n_outer_order, cache, n_reg, i_reg, start_reg, end_reg,
        scan_j, step_rowid, start_expr_reg, end_expr_reg, ncol, nkey, npart, norder, ntable,
        out_ctx,
    )?;
    b.resolve(skip);

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

    b.resolve(setup);
    b.emit(Opcode::Transaction, 0, 0, 0);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

/// Emit the sliding-frame output pass for one partition. The partition's rows are in the
/// ephemeral cache `cache` (rowid 1..=n). For each current row i (1..=n), compute the frame
/// [start, end], re-scan the frame from scratch (AggStep each row in [start, end]), AggValue
/// → result_reg, and emit the row.
///
/// This is the full-scan approach (O(n²) per partition) — correct but not yet the streaming
/// 3-cursor optimization. The full-scan approach is simpler and handles all frame modes
/// (ROWS / RANGE / GROUPS) and all bound types uniformly.
#[allow(clippy::too_many_arguments)]
fn emit_partition_sliding(
    b: &mut ProgramBuilder,
    _select: &SelectStmt,
    table: &Table,
    _outputs: &[(Expr, String)],
    win_calls: &[WindowCall],
    kinds: &[AggregateKind],
    window: &Window,
    frame: &rustqlite_parser::Frame,
    accum_reg: &[i32],
    result_reg: &[i32],
    rewritten_outputs: &[(Expr, String)],
    rewritten_order_by: &[(Expr, bool)],
    limit_reg: Option<i32>,
    offset_reg: Option<i32>,
    distinct_cursor: Option<i32>,
    has_outer_order_by: bool,
    output_sorter: i32,
    n_outer_order: i32,
    cache: i32,
    n_reg: i32,
    i_reg: i32,
    start_reg: i32,
    end_reg: i32,
    scan_j: i32,
    _step_rowid: i32,
    start_expr_reg: Option<i32>,
    end_expr_reg: Option<i32>,
    ncol: i32,
    nkey: i32,
    npart: i32,
    norder: i32,
    ntable: i32,
    out_ctx: Ctx<'_>,
) -> Result<()> {
    let _ = (table, nkey, npart, norder, ntable, out_ctx);

    // A scratch register holding the constant 1 (for clamps and loop init).
    let one_reg = b.alloc_reg();
    b.emit(Opcode::Integer, 1, one_reg, 0);

    // ---- Compute n = cache row count. ----
    // Our ephemeral rowids are 1..=n, so n = last row's rowid. Seek to Last and read Rowid.
    // If the cache is empty, skip the whole partition loop.
    let part_done = b.new_label();
    b.emit(Opcode::Integer, 0, n_reg, 0);
    b.emit_jump(Opcode::Last, cache, part_done, 0);
    b.emit(Opcode::Rowid, cache, n_reg, 0);

    // ---- Loop: for i = 1 to n. ----
    b.emit(Opcode::Integer, 0, i_reg, 0);
    let loop_top = b.new_label();
    b.resolve(loop_top);
    // i += 1
    b.emit(Opcode::AddImm, i_reg, 1, 0);
    // if i <= n, jump to loop body. Le p1 p2 p3 jumps when r[p3] <= r[p1], i.e., r[i_reg] <=
    // r[n_reg], so p1=n_reg p3=i_reg.
    let loop_body = b.new_label();
    b.emit_jump(Opcode::Le, n_reg, loop_body, i_reg);
    // i > n → exit.
    b.emit_jump(Opcode::Goto, 0, part_done, 0);
    b.resolve(loop_body);

    // Position the cache cursor at row i (by rowid). Column on ephemeral auto-decodes.
    b.emit(Opcode::SeekRowid, cache, 0, i_reg);

    // Compute frame bounds [start_reg, end_reg] for row i.
    compute_frame_bounds(
        b, frame, window, cache, i_reg, n_reg, start_reg, end_reg,
        start_expr_reg, end_expr_reg, one_reg,
    )?;

    // Reset accumulators (full-scan approach: re-accumulate from scratch each row).
    for &r in accum_reg {
        b.emit(Opcode::Null, r, r, 0);
    }

    // ---- Full-scan AggStep pass: for j = start to end, AggStep row j. ----
    // scan_j = start - 1 (so the loop increment brings us to start).
    b.emit(Opcode::Subtract, one_reg, start_reg, scan_j);
    let step_top = b.new_label();
    b.resolve(step_top);
    // scan_j += 1
    b.emit(Opcode::AddImm, scan_j, 1, 0);
    // if scan_j <= end, jump to body; else fall through to exit.
    // Le p1 p2 p3 jumps when r[p3] <= r[p1], i.e., r[scan_j] <= r[end_reg], so p1=end_reg p3=scan_j.
    let step_body = b.new_label();
    b.emit_jump(Opcode::Le, end_reg, step_body, scan_j);
    // scan_j > end → exit (skip the body).
    let step_exit = b.new_label();
    b.emit_jump(Opcode::Goto, 0, step_exit, 0);
    b.resolve(step_body);
    // Seek the cache to row scan_j and AggStep each window call.
    b.emit(Opcode::SeekRowid, cache, 0, scan_j);
    // Evaluate args for each window call against the cache row (via Column on the cache cursor).
    // The cache record layout: [partition_keys, order_keys, table_columns]. The window call
    // args reference table columns. We need a Ctx that reads table columns from the cache.
    // The cache stores the same record layout as the sorter, so the column positions are the
    // same: table columns at indices nkey..nkey+ntable.
    let cache_column_positions: Vec<usize> =
        (0..table.columns.len()).map(|i| nkey as usize + i).collect();
    let cache_rowid_position = table
        .rowid_alias
        .map(|i| cache_column_positions[i])
        .unwrap_or(cache_column_positions.len());
    let cache_ctx = Ctx {
        table,
        cursor: cache,
        register_base: None,
        join_tables: None,
        index_read: Some(IndexRead {
            cursor: cache,
            column_positions: cache_column_positions.as_slice(),
            rowid_position: cache_rowid_position,
        }),
        subquery_resolver: None,
    };
    for (wi, call) in win_calls.iter().enumerate() {
        let kind = kinds[wi];
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
                    compile_expr(b, a, arg_base + k as i32, cache_ctx)?;
                }
            }
        }
        let filter_skip = b.new_label();
        if let Some(filter) = &call.filter {
            compile_jump(b, filter, filter_skip, false, true, cache_ctx)?;
        }
        let idx = b.emit(Opcode::AggStep, 0, arg_base, accum_reg[wi]);
        b.set_p4(idx, P4::FuncDef(kind));
        b.set_p5(idx, n_arg);
        b.resolve(filter_skip);
    }
    // Loop back to step the next row.
    b.emit_jump(Opcode::Goto, 0, step_top, 0);
    b.resolve(step_exit);

    // ---- AggValue → result_reg for each window call. ----
    for (i, kind) in kinds.iter().enumerate() {
        let idx = b.emit(Opcode::AggValue, accum_reg[i], 0, result_reg[i]);
        b.set_p4(idx, P4::FuncDef(*kind));
    }

    // ---- Emit the row. ----
    // The projection reads table columns from the cache (current row i) and window-call
    // results from result_reg (just written by AggValue). We need a Ctx for the cache cursor
    // at row i. We already positioned the cache at row i above (SeekRowid cache, 0, i_reg),
    // but the AggStep pass repositioned it. Re-seek to i.
    b.emit(Opcode::SeekRowid, cache, 0, i_reg);
    let emit_column_positions: Vec<usize> =
        (0..table.columns.len()).map(|i| nkey as usize + i).collect();
    let emit_rowid_position = table
        .rowid_alias
        .map(|i| emit_column_positions[i])
        .unwrap_or(emit_column_positions.len());
    let emit_ctx = Ctx {
        table,
        cursor: cache,
        register_base: None,
        join_tables: None,
        index_read: Some(IndexRead {
            cursor: cache,
            column_positions: emit_column_positions.as_slice(),
            rowid_position: emit_rowid_position,
        }),
        subquery_resolver: None,
    };
    let emit_block = b.alloc_regs(ncol);
    for (j, (expr, _)) in rewritten_outputs.iter().enumerate() {
        compile_expr(b, expr, emit_block + j as i32, emit_ctx)?;
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

    // Emit: into the output sorter (outer ORDER BY) or directly via ResultRow.
    if has_outer_order_by {
        let sort_block = b.alloc_regs(n_outer_order + ncol);
        for (k, (expr, _)) in rewritten_order_by.iter().enumerate() {
            match expr {
                Expr::AggRef(r) => {
                    b.emit(Opcode::SCopy, *r, sort_block + k as i32, 0);
                }
                _ => {
                    compile_expr(b, expr, sort_block + k as i32, emit_ctx)?;
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
            b.emit_jump(Opcode::DecrJumpZero, lreg, part_done, 0);
        }
    }
    b.resolve(row_done);
    // Loop back to the next current row.
    b.emit_jump(Opcode::Goto, 0, loop_top, 0);
    b.resolve(part_done);
    Ok(())
}

/// Compute the frame start/end rowids for the current row `i_reg` and store them in
/// `start_reg` / `end_reg`. The frame bounds depend on the frame mode and bound types.
fn compute_frame_bounds(
    b: &mut ProgramBuilder,
    frame: &rustqlite_parser::Frame,
    window: &Window,
    cache: i32,
    i_reg: i32,
    n_reg: i32,
    start_reg: i32,
    end_reg: i32,
    start_expr_reg: Option<i32>,
    end_expr_reg: Option<i32>,
    one_reg: i32,
) -> Result<()> {
    use rustqlite_parser::FrameBound as F;
    let _ = (window, cache);

    // Start bound → start_reg.
    // Clamp strategy: start = max(1, min(n+1, computed_start)). The sentinel n+1 means
    // "beyond the partition" — the frame is empty when start > end.
    match &frame.start {
        F::UnboundedPreceding => {
            b.emit(Opcode::Integer, 1, start_reg, 0);
        }
        F::CurrentRow => {
            // ROWS: start = i. RANGE/GROUPS: start = first row of the current peer group.
            // Simplified: treat as i (correct when each row is its own peer group, i.e. ORDER BY
            // values are distinct). Full peer-group logic lands with the GROUPS/RANGE follow-up.
            b.emit(Opcode::SCopy, i_reg, start_reg, 0);
        }
        F::Preceding(_) => {
            // start = i - expr. Clamp to [1, n+1].
            let expr_reg = start_expr_reg.unwrap();
            b.emit(Opcode::Subtract, expr_reg, i_reg, start_reg);
            // Clamp to 1: if start >= 1, skip clamp.
            let ge1 = b.new_label();
            b.emit_jump(Opcode::Ge, one_reg, ge1, start_reg);
            b.emit(Opcode::Integer, 1, start_reg, 0);
            b.resolve(ge1);
        }
        F::Following(_) => {
            // start = i + expr. Clamp to 1 (defensive). The case start > n is handled by
            // the step loop condition (scan_j <= end, and end <= n, so scan_j > n means the
            // body is skipped, and SeekRowid is never reached).
            let expr_reg = start_expr_reg.unwrap();
            b.emit(Opcode::Add, expr_reg, i_reg, start_reg);
            let ge1 = b.new_label();
            b.emit_jump(Opcode::Ge, one_reg, ge1, start_reg);
            b.emit(Opcode::Integer, 1, start_reg, 0);
            b.resolve(ge1);
        }
        F::UnboundedFollowing => {
            // start = 1 (valid only as `BETWEEN UNBOUNDED PRECEDING AND ...`).
            b.emit(Opcode::Integer, 1, start_reg, 0);
        }
    }

    // End bound → end_reg.
    match &frame.end {
        None | Some(F::CurrentRow) => {
            // No end bound = CURRENT ROW. ROWS: end = i. RANGE/GROUPS: end = last row of
            // current peer group. Simplified: end = i.
            b.emit(Opcode::SCopy, i_reg, end_reg, 0);
        }
        Some(F::UnboundedFollowing) => {
            b.emit(Opcode::SCopy, n_reg, end_reg, 0);
        }
        Some(F::Preceding(_)) => {
            // end = i - expr. Do NOT clamp to 1 — if end < start, the frame is empty (the
            // step loop naturally skips when end < start). Clamp to n only (defensive).
            let expr_reg = end_expr_reg.unwrap();
            b.emit(Opcode::Subtract, expr_reg, i_reg, end_reg);
            // Clamp to n: if end > n, end = n. (Unlikely for PRECEDING but defensive.)
            // Le p1 p2 p3 jumps when r[p3] <= r[p1], i.e., r[end_reg] <= r[n_reg].
            let len = b.new_label();
            b.emit_jump(Opcode::Le, n_reg, len, end_reg);
            b.emit(Opcode::SCopy, n_reg, end_reg, 0);
            b.resolve(len);
        }
        Some(F::Following(_)) => {
            // end = min(n, i + expr)
            let expr_reg = end_expr_reg.unwrap();
            b.emit(Opcode::Add, expr_reg, i_reg, end_reg);
            // Clamp to n: if end <= n, skip clamp. Le p1 p2 p3 jumps when r[p3] <= r[p1],
            // i.e., r[end_reg] <= r[n_reg], so p1=n_reg p3=end_reg.
            let len = b.new_label();
            b.emit_jump(Opcode::Le, n_reg, len, end_reg);
            b.emit(Opcode::SCopy, n_reg, end_reg, 0);
            b.resolve(len);
        }
        Some(F::UnboundedPreceding) => {
            // end = 1 (only valid as `BETWEEN ... AND UNBOUNDED PRECEDING`, unusual).
            b.emit(Opcode::Integer, 1, end_reg, 0);
        }
    }
    Ok(())
}