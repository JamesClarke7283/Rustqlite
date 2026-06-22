//! Common Table Expression (CTE) rewriting — `WITH [RECURSIVE] name AS (…) SELECT …`
//! (mirrors the `searchWith` / `sqlite3WithPush` path in `select.c`).
//!
//! The first slice (M10.2–M10.5) handles **non-recursive** CTEs by rewriting the AST before
//! codegen: a FROM entry that names a CTE is replaced with a `TableOrJoin::Subquery` whose
//! body is the CTE's SELECT. The existing `codegen::subquery::compile_from_subquery`
//! infrastructure then materializes that subquery into an ephemeral table and scans it —
//! exactly the `SRT_EphemTab` shape upstream uses for non-recursive CTEs that are referenced
//! once (`tag-select-0488`).
//!
//! Recursive CTEs (`WITH RECURSIVE`) use the queue-based iterative algorithm
//! (`generateWithRecursiveQuery` in `select.c`) and have their own codegen (M10.3).
//!
//! What this module does NOT do:
//! * `MATERIALIZED` / `NOT MATERIALIZED` hints — the first slice always materializes (the
//!   default for a CTE referenced once). Reuse via `OP_OpenDup` (multiple references) lands
//!   with M7.12 / the CTE-optimization follow-up.
//! * Flattening of a non-recursive CTE into the outer query — lands with M8.12 subquery
//!   flattening.
//! * Correlated CTEs — CTEs are never correlated in SQLite (they're evaluated once).

use rustqlite_parser::{Expr, ResultColumn, SelectStmt, TableOrJoin, WithClause};

use crate::error::{Error, Result};
use crate::schema::{Column, Table};
use crate::types::{Affinity, Collation};
use crate::vdbe::program::{Instruction, Program};
use crate::vdbe::Opcode;

use super::builder::ProgramBuilder;
use super::select::{self, eval_limit_offset, expand_columns};

/// Whether a `WITH` clause is present and contains at actual CTE list.
pub fn has_ctes(select: &SelectStmt) -> bool {
    select
        .with_clause
        .as_ref()
        .is_some_and(|w| !w.ctes.is_empty())
}

/// Whether the WITH clause is recursive (carries the `RECURSIVE` keyword) OR any CTE body
/// is itself a compound SELECT whose second arm references the CTE name (the canonical
/// recursive-CTE shape). Upstream sets `SF_Recursive` only after name resolution; here we
/// approximate by checking the declared `recursive` flag plus a syntactic check for a
/// self-reference in a compound arm's FROM.
pub fn is_recursive(with: &WithClause) -> bool {
    if with.recursive {
        return true;
    }
    for cte in &with.ctes {
        if is_recursive_cte_body(&cte.query, &cte.name) {
            return true;
        }
    }
    false
}

/// Recursively scan a SELECT for a self-reference to `cte_name` in any FROM clause.
fn is_recursive_cte_body(select: &SelectStmt, cte_name: &str) -> bool {
    if from_references_name(&select.from, cte_name) {
        return true;
    }
    for (_, arm) in &select.compound {
        if from_references_name(&arm.from, cte_name) {
            return true;
        }
    }
    false
}

fn from_references_name(from: &[TableOrJoin], name: &str) -> bool {
    for item in from {
        match item {
            TableOrJoin::Table(t) => {
                if t.schema.is_none() && t.name.eq_ignore_ascii_case(name) {
                    return true;
                }
            }
            TableOrJoin::Subquery { query, .. } => {
                if is_recursive_cte_body(query, name) {
                    return true;
                }
            }
            TableOrJoin::Join(j) => {
                if from_references_name(std::slice::from_ref(&*j.left), name) {
                    return true;
                }
                if j.right.schema.is_none() && j.right.name.eq_ignore_ascii_case(name) {
                    return true;
                }
            }
        }
    }
    false
}

/// Rewrite the outer SELECT's FROM clause, replacing CTE references with subqueries. The
/// returned SELECT has its `with_clause` cleared so downstream codegen does not re-enter
/// this path. Each CTE body is itself rewritten against the prefix of earlier CTEs in the
/// same WITH clause (so a later CTE may reference an earlier one), and the CTE body's own
/// `with_clause` (a nested `WITH`) is processed recursively.
///
/// When the CTE declares an explicit column list (`name (col1, col2, …) AS (…)`), the
/// subquery's projection is wrapped so each output column carries the declared name as its
/// alias. This makes the synthesized subquery table's columns match the CTE's declared
/// header (M10.5).
pub fn rewrite_with_ctes(select: &SelectStmt) -> Result<SelectStmt> {
    let Some(with) = select.with_clause.clone() else {
        return Ok(select.clone());
    };
    if with.ctes.is_empty() {
        let mut s = select.clone();
        s.with_clause = None;
        return Ok(s);
    }
    if is_recursive(&with) {
        return Err(Error::msg(
            "recursive CTEs are not supported by this codegen path (M10.3 pending)",
        ));
    }

    // Rewrite the outer SELECT against the full set of CTEs (each CTE may be referenced by
    // the outer FROM). The CTE bodies themselves are rewritten first, in order, against the
    // growing prefix so a later CTE may reference an earlier one.
    let mut rewritten_ctes: Vec<(String, SelectStmt)> = Vec::with_capacity(with.ctes.len());
    for cte in &with.ctes {
        // Build the active scope: all earlier rewritten CTEs (in declared order) plus any
        // CTEs from an enclosing WITH (handled by recursion via `rewrite_with_ctes` on the
        // body). The body itself may carry a nested `WITH` clause; process that first.
        let mut body = cte.query.clone();
        if body.with_clause.is_some() {
            body = rewrite_with_ctes(&body)?;
        }
        // Rewrite the body's FROM against the prefix of earlier CTEs in this same WITH.
        let scope: Vec<(String, SelectStmt)> = rewritten_ctes.clone();
        body = rewrite_from_against_scope(body, &scope)?;
        // Wrap the body so its projection uses the CTE's explicit column list, if any.
        if !cte.columns.is_empty() {
            body = wrap_with_column_names(body, &cte.columns)?;
        }
        rewritten_ctes.push((cte.name.clone(), body));
    }

    // Now rewrite the outer SELECT's FROM (and its compound arms' FROMs) against the full
    // CTE scope.
    let mut outer = select.clone();
    outer = rewrite_from_against_scope(outer, &rewritten_ctes)?;
    outer.with_clause = None;
    Ok(outer)
}

/// Walk the SELECT's FROM clauses (the leading core and each compound arm) and replace any
/// `TableOrJoin::Table` whose name matches a CTE in `scope` with a `TableOrJoin::Subquery`
/// carrying a clone of that CTE's rewritten body. The subquery's alias is the CTE name (so
/// `table.*` and `Ctx::join_tables` resolution by alias still works). When the original
/// `TableRef` had an alias, that alias overrides the subquery alias (matching SQL: `FROM
/// cte AS x` makes `x` the name visible in the outer query).
fn rewrite_from_against_scope(mut select: SelectStmt, scope: &[(String, SelectStmt)]) -> Result<SelectStmt> {
    if scope.is_empty() {
        return Ok(select);
    }
    select.from = rewrite_from_list(select.from, scope)?;
    // Compound arms carry their own FROM clauses.
    let mut new_compound = Vec::with_capacity(select.compound.len());
    for (op, arm) in select.compound {
        let arm = rewrite_from_against_scope(arm, scope)?;
        new_compound.push((op, arm));
    }
    select.compound = new_compound;
    Ok(select)
}

fn rewrite_from_list(from: Vec<TableOrJoin>, scope: &[(String, SelectStmt)]) -> Result<Vec<TableOrJoin>> {
    let mut out = Vec::with_capacity(from.len());
    for item in from {
        out.push(rewrite_from_item(item, scope)?);
    }
    Ok(out)
}

fn rewrite_from_item(item: TableOrJoin, scope: &[(String, SelectStmt)]) -> Result<TableOrJoin> {
    match item {
        TableOrJoin::Table(t) => {
            if t.schema.is_some() {
                // A schema-qualified reference cannot be a CTE (matches upstream's
                // `searchWith` early-out when `zDatabase` is set).
                return Ok(TableOrJoin::Table(t));
            }
            let Some((_, cte_body)) = scope.iter().find(|(n, _)| n.eq_ignore_ascii_case(&t.name)) else {
                return Ok(TableOrJoin::Table(t));
            };
            // Replace with a subquery. The visible alias is the user-supplied alias if any,
            // else the CTE name.
            let alias = t.alias.clone().unwrap_or_else(|| t.name.clone());
            // The subquery body is a clone of the (already rewritten) CTE body. We must
            // strip any WITH clause from the cloned body — it has already been expanded.
            let mut body = cte_body.clone();
            body.with_clause = None;
            Ok(TableOrJoin::Subquery {
                query: Box::new(body),
                alias,
            })
        }
        TableOrJoin::Subquery { query, alias } => {
            // Recurse into the subquery's body so a CTE reference inside it resolves too.
            // (This handles `FROM (SELECT * FROM cte) AS x`.)
            let mut body = query.as_ref().clone();
            if body.with_clause.is_some() {
                body = rewrite_with_ctes(&body)?;
            }
            body = rewrite_from_against_scope(body, scope)?;
            Ok(TableOrJoin::Subquery {
                query: Box::new(body),
                alias,
            })
        }
        TableOrJoin::Join(j) => {
            let left = rewrite_from_item(*j.left, scope)?;
            Ok(TableOrJoin::Join(rustqlite_parser::Join {
                op: j.op,
                left: Box::new(left),
                right: j.right,
                constraint: j.constraint,
            }))
        }
    }
}

/// Wrap a CTE body so its projection uses the declared column names. Mirrors upstream's
/// behavior: when a CTE has `(col1, col2, …)`, the output columns are renamed to those
/// names regardless of the body's own projection aliases.
///
/// This is done by replacing each `ResultColumn::Expr` with one that carries the declared
/// name as its alias, and each `ResultColumn::Star` by an explicit list of the body's
/// output columns (so we can attach the declared names). Because we cannot know the body's
/// output column count without resolving it, we require the body to have an explicit
/// projection (no `*`) when an explicit CTE column list is present — matching upstream's
/// "table X has N values for M columns" error, which we approximate by raising a
/// codegen-time error if the counts disagree after expansion (deferred; here we just wrap
/// the explicit projection).
fn wrap_with_column_names(mut body: SelectStmt, names: &[String]) -> Result<SelectStmt> {
    if body.values.is_empty() && body.columns.len() == 1 {
        // `SELECT *` — we can't rename the columns without expanding. Defer the expansion
        // to the codegen's `expand_columns` by leaving the body as-is; the synthesized
        // table will use the body's own column names, and the explicit CTE column list is
        // then applied as a renaming outer shell. For now, error to surface the limitation.
        if matches!(body.columns[0], ResultColumn::Star) {
            // `SELECT * FROM t` with an explicit CTE column list: expand the star by
            // emitting a synthetic projection that aliases each output column to the
            // declared name. We don't know the source table's columns here, so instead we
            // wrap: build `SELECT <names> FROM (original body) AS __cte_inner`. The inner
            // body's star expands against its own FROM table at codegen time; this outer
            // shell projects the renamed columns.
            return Ok(wrap_star_cte_with_names(body, names));
        }
    }
    if body.values.is_empty() {
        // Replace each ResultColumn::Expr with one carrying the declared name as alias.
        // If the counts disagree, the codegen's `expand_columns` will surface a mismatch
        // when the outer query reads the synthesized table — we approximate the oracle's
        // "table X has N values for M columns" by checking here only when we can.
        let mut new_cols = Vec::with_capacity(body.columns.len());
        for (i, rc) in body.columns.iter().enumerate() {
            match rc {
                ResultColumn::Expr { expr, alias: _ } => {
                    let name = names.get(i).cloned().unwrap_or_default();
                    new_cols.push(ResultColumn::Expr {
                        expr: expr.clone(),
                        alias: Some(name),
                    });
                }
                ResultColumn::Star | ResultColumn::TableStar(_) => {
                    // Cannot attach a single name to a star; fall back to wrapping.
                    return Ok(wrap_star_cte_with_names(body, names));
                }
            }
        }
        body.columns = new_cols;
    }
    // For a VALUES body, the columns are `column1, column2, …` and the explicit CTE column
    // list just renames them — we wrap in the same way as a star.
    if !body.values.is_empty() {
        return Ok(wrap_star_cte_with_names(body, names));
    }
    Ok(body)
}

/// Wrap a CTE body whose projection is a `*` (or a VALUES) in an outer SELECT that
/// projects the declared CTE column names from the inner body. The inner body is compiled
/// as a subquery (its `*` expands against its own FROM table), and the outer shell
/// projects `inner.col1 AS name1, inner.col2 AS name2, …`. This requires knowing the inner
/// body's column count, which we get from the declared list (it must match).
fn wrap_star_cte_with_names(body: SelectStmt, names: &[String]) -> SelectStmt {
    // The inner body is referenced as a subquery in the outer shell's FROM. Each declared
    // name becomes `columnN AS name` — using the positional `columnN` names SQLite assigns
    // to a VALUES body or a star-expanded projection, which the inner body's ephemeral
    // table will expose.
    let inner_alias = "__cte_inner".to_string();
    let columns: Vec<ResultColumn> = names
        .iter()
        .enumerate()
        .map(|(i, n)| ResultColumn::Expr {
            expr: Expr::Column {
                schema: None,
                table: Some(inner_alias.clone()),
                name: format!("column{}", i + 1),
            },
            alias: Some(n.clone()),
        })
        .collect();
    SelectStmt {
        distinct: false,
        columns,
        from: vec![TableOrJoin::Subquery {
            query: Box::new(body),
            alias: inner_alias,
        }],
        where_clause: None,
        group_by: Vec::new(),
        having: None,
        compound: Vec::new(),
        order_by: Vec::new(),
        limit: None,
        offset: None,
        with_clause: None,
        window_clause: Vec::new(),
        values: Vec::new(),
    }
}

// ===========================================================================
// Recursive CTE compilation (M10.3)
// ===========================================================================

/// The result of compiling a CTE: the program and the result column names, plus the pager
/// the VDBE needs to satisfy cursor reads.
pub struct CompiledCte {
    pub program: Program,
    pub column_names: Vec<String>,
    pub pager: Option<std::sync::Arc<crate::pager::Pager>>,
}

/// Compile a `WITH RECURSIVE …` SELECT. Mirrors `generateWithRecursiveQuery` in `select.c`.
///
/// The supported shape (first slice):
/// ```text
/// WITH RECURSIVE name(cols) AS (
///   <setup query>
///   UNION [ALL]
///   <recursive query referencing name>
/// )
/// SELECT ... FROM name ...
/// ```
/// The setup query runs once, filling the Queue. The loop pulls rows from the Queue one by
/// one: each row is appended to the CTE result ephemeral, then the recursive query runs
/// (with `name` bound to the single "Current" row via a pseudo-cursor) and its results are
/// appended back to the Queue. The loop continues until the Queue is empty. Finally the
/// outer query scans the CTE result ephemeral.
///
/// Limitations of the first slice: only one recursive CTE per WITH; the recursive CTE body
/// must be a 2-arm compound (`setup UNION [ALL] recursive`); `UNION` (dedup) is not yet
/// enforced (treated as `UNION ALL`); the recursive query's FROM must be exactly the CTE
/// name (no joins in the recursive arm); the outer query must scan the CTE name as its
/// single FROM entry (no joins in the outer query).
pub fn compile_recursive(
    db: &mut crate::capi::connection::Sqlite3,
    outer: &SelectStmt,
) -> Result<CompiledCte> {
    let with = outer.with_clause.as_ref().ok_or_else(|| {
        Error::msg("compile_recursive called without a WITH clause")
    })?;
    if !with.recursive && !with.ctes.iter().any(|c| is_recursive_cte_body(&c.query, &c.name)) {
        return Err(Error::msg("compile_recursive called without a recursive CTE"));
    }
    // Find the recursive CTE. The first slice supports exactly one recursive CTE per WITH.
    let rcte_idx = with
        .ctes
        .iter()
        .position(|c| is_recursive_cte_body(&c.query, &c.name))
        .ok_or_else(|| Error::msg("no recursive CTE found in a WITH RECURSIVE clause"))?;
    let rcte = &with.ctes[rcte_idx];

    // The CTE body must be a 2-arm compound: setup UNION [ALL] recursive.
    if rcte.query.compound.len() != 1 {
        return Err(Error::msg(
            "recursive CTE body must be a 2-arm compound (setup UNION [ALL] recursive)",
        ));
    }
    let (op, recursive_arm) = &rcte.query.compound[0];
    let setup_arm = &rcte.query;
    // Strip the compound from the setup arm for standalone compilation.
    let mut setup = setup_arm.clone();
    setup.compound = Vec::new();
    let op_is_union_all = matches!(op, rustqlite_parser::CompoundOperator::UnionAll);
    if !op_is_union_all && !matches!(op, rustqlite_parser::CompoundOperator::Union) {
        return Err(Error::msg(
            "recursive CTE body must use UNION or UNION ALL (INTERSECT/EXCEPT not supported)",
        ));
    }
    // The recursive arm must reference the CTE name in its FROM (exactly one entry, no joins).
    if recursive_arm.from.len() != 1 {
        return Err(Error::msg(
            "recursive CTE arm must scan the CTE name (joins in the recursive arm are not supported)",
        ));
    }
    let rref = match &recursive_arm.from[0] {
        TableOrJoin::Table(t) if t.schema.is_none() && t.name.eq_ignore_ascii_case(&rcte.name) => t,
        _ => {
            return Err(Error::msg(
                "recursive CTE arm must scan the CTE name as its single FROM entry",
            ))
        }
    };
    let _ = rref;

    // Determine the CTE's output columns.
    let setup_table = resolve_source_table(db, &setup)?;
    let setup_outputs = expand_columns(&setup, setup_table.as_ref())?;
    let cte_names: Vec<String> = if !rcte.columns.is_empty() {
        rcte.columns.clone()
    } else {
        setup_outputs.iter().map(|(_, n)| n.clone()).collect()
    };
    let ncol = cte_names.len() as i32;

    // Synthesize the CTE table (the outer query scans this).
    let cte_table = Table {
        name: rcte.name.clone(),
        rootpage: 0,
        columns: cte_names
            .iter()
            .map(|n| Column {
                name: n.clone(),
                affinity: Affinity::Blob,
                collation: Collation::Binary,
                notnull: false,
                pk: false,
                default: None,
                notnull_oe: crate::vdbe::oe::OeAction::None,
            })
            .collect(),
        rowid_alias: None,
        without_rowid: false,
        pk_columns: Vec::new(),
    };

    // Resolve the recursive arm's source table. It scans the CTE name — but at codegen time
    // the recursive arm's `name` reference is replaced by the Current pseudo-cursor. So the
    // "table" the recursive arm sees is the synthesized CTE table (same columns).
    let recursive_table = cte_table.clone();

    // The outer query scans the CTE result ephemeral. Rewrite its FROM to point at the
    // synthesized CTE table (replace the CTE name reference with... well, the outer query's
    // FROM already references the CTE name; we compile it against the synthesized table).
    // Strip the WITH clause from the outer query.
    let mut outer_stripped = outer.clone();
    outer_stripped.with_clause = None;
    // Verify the outer query scans the CTE name as its single FROM entry (the first slice
    // does not support joins in the outer query over a recursive CTE).
    if outer_stripped.from.len() != 1 {
        return Err(Error::msg(
            "outer query over a recursive CTE must scan the CTE as its single FROM entry (joins not supported)",
        ));
    }
    let outer_ref = match &outer_stripped.from[0] {
        TableOrJoin::Table(t) if t.schema.is_none() && t.name.eq_ignore_ascii_case(&rcte.name) => {
            t.clone()
        }
        _ => {
            return Err(Error::msg(
                "outer query over a recursive CTE must scan the CTE name",
            ))
        }
    };
    let _ = outer_ref;

    // Expand the outer query's projection against the synthesized CTE table.
    let outer_outputs = expand_columns(&outer_stripped, Some(&cte_table))?;
    let outer_names: Vec<String> = outer_outputs.iter().map(|(_, n)| n.clone()).collect();
    let (limit, offset) = eval_limit_offset(&outer_stripped)?;

    // ---- Build the program ----
    let mut b = ProgramBuilder::new();
    let setup_label = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup_label, 0);
    let after_init = b.cur_addr();

    if limit == Some(0) {
        b.emit(Opcode::Halt, 0, 0, 0);
        b.resolve(setup_label);
        b.emit(Opcode::Transaction, 0, 0, 0);
        b.emit(Opcode::Goto, 0, after_init, 0);
        let pager = db.pager_arc().ok();
        return Ok(CompiledCte {
            program: b.finish(),
            column_names: outer_names,
            pager,
        });
    }

    // Cursor numbers: CTE result ephemeral (scanned by the outer query) = 10, Queue = 11,
    // Current pseudo-cursor = 12. These are high enough to avoid collisions with the
    // setup/recursive/outer scan cursors (which use 0, 1, 2 in their own sub-programs; the
    // inlining rebase doesn't touch our hardcoded ephemeral cursors because we emit them
    // directly into the outer program).
    let cte_cursor = 10i32;
    let queue_cursor = 11i32;
    let current_cursor = 12i32;
    b.emit(Opcode::OpenEphemeral, cte_cursor, ncol, 0);
    b.note_cursor(cte_cursor);
    b.emit(Opcode::OpenEphemeral, queue_cursor, ncol, 0);
    b.note_cursor(queue_cursor);
    let reg_current = b.alloc_reg();
    b.emit(Opcode::OpenPseudo, current_cursor, reg_current, ncol);
    b.note_cursor(current_cursor);

    // --- Run the setup query, inserting each row into the Queue. ---
    let setup_pager = db.pager_arc().ok();
    let (setup_program, _) = select::compile(
        &setup,
        setup_table.as_ref(),
        &[],
        None,
    )?;

    // Inline the setup query's scan code, rewriting ResultRow → MakeRecord + NewRowid +
    // Insert into the Queue ephemeral. The setup query's own cursor numbers (0, 1, 2) are
    // fine — they don't collide with our 10/11/12.
    inline_query_into_ephemeral(&mut b, &setup_program, queue_cursor, ncol)?;

    // --- The recursive loop. ---
    let loop_top = b.new_label();
    let loop_break = b.new_label();
    b.resolve(loop_top);
    b.emit_jump(Opcode::Rewind, queue_cursor, loop_break, 0);

    // Transfer the next row from Queue to Current.
    b.emit(Opcode::NullRow, current_cursor, 0, 0);
    b.emit(Opcode::RowData, queue_cursor, reg_current, 0);
    b.emit(Opcode::Delete, queue_cursor, 0, 0);

    // Append the Current row to the CTE result ephemeral. The record is already in
    // reg_current (a blob from RowData); we just need a rowid and an Insert.
    let cte_rowid = b.alloc_reg();
    b.emit(Opcode::NewRowid, cte_cursor, cte_rowid, 0);
    b.emit(Opcode::Insert, cte_cursor, reg_current, cte_rowid);

    // Run the recursive query, inserting each row into the Queue. The recursive arm's FROM
    // references the CTE name; we compile it against the synthesized CTE table, then patch
    // its `OpenRead`/`Rewind`/`Next`/`Column` to use the Current pseudo-cursor instead.
    // The recursive arm's `name` reference is a `TableOrJoin::Table` — we replace it with a
    // synthesized table whose rootpage is 0, and after compilation we rewrite the cursor
    // number to `current_cursor`.
    let mut recursive_select = recursive_arm.clone();
    recursive_select.with_clause = None;
    // The recursive arm's FROM[0] is the CTE name reference. We compile it against the
    // synthesized recursive_table, then patch the cursor.
    let (recursive_program, _) = select::compile(
        &recursive_select,
        Some(&recursive_table),
        &[],
        None,
    )?;

    // Inline the recursive query, rewriting ResultRow → insert into Queue. We also patch
    // the recursive query's table cursor (cursor 0, opened by its `OpenRead`) to instead
    // use the Current pseudo-cursor. This means: drop the `OpenRead` (the pseudo-cursor is
    // already open), and rewrite `Column`/`Rewind`/`Next` on cursor 0 to use
    // `current_cursor`. Since the pseudo-cursor reads from reg_current (set by RowData
    // above), the recursive query sees the single Current row.
    inline_recursive_query_into_queue(&mut b, &recursive_program, queue_cursor, ncol, current_cursor)?;

    // Loop back.
    b.emit_jump(Opcode::Goto, 0, loop_top, 0);
    b.resolve(loop_break);

    // --- Compile the outer query scanning the CTE result ephemeral. ---
    // The outer query is a plain scan/aggregate/ordered scan over the CTE result. We
    // compile it against the synthesized CTE table, then patch its `OpenRead` to be a
    // no-op (the ephemeral is already open at `cte_cursor`) and its cursor number to
    // `cte_cursor`.
    let (outer_program, _) = select::compile(
        &outer_stripped,
        Some(&cte_table),
        &[],
        None,
    )?;
    inline_outer_scan(&mut b, &outer_program, cte_cursor, &outer_outputs, limit, offset)?;

    b.resolve(setup_label);
    b.emit(Opcode::Transaction, 0, 0, 0);
    b.emit(Opcode::Goto, 0, after_init, 0);

    let program = b.finish();
    Ok(CompiledCte {
        program,
        column_names: outer_names,
        pager: setup_pager,
    })
}

/// Inline a compiled query's scan code into the builder, rewriting each `ResultRow` into
/// `MakeRecord + NewRowid + Insert` into the given ephemeral cursor. Mirrors the inlining
/// logic in `compile_from_subquery` but is extracted here as a reusable helper.
///
/// Jumps targeting the sub-program's `Halt` are redirected to the address after the inlined
/// block (so the inlined scan falls through to the next emitted instruction on exhaustion).
fn inline_query_into_ephemeral(
    b: &mut ProgramBuilder,
    sub_program: &Program,
    target_cursor: i32,
    ncol: i32,
) -> Result<()> {
    let reg_offset = b.next_reg() - 1;
    let cursor_offset = b.next_cursor();
    let sub_start = b.cur_addr();
    let halt_idx = sub_program
        .instructions
        .iter()
        .position(|i| i.opcode == Opcode::Halt)
        .ok_or_else(|| Error::msg("sub-program has no Halt"))?;
    let after = b.new_label();
    let mut addr_map: std::collections::HashMap<i32, i32> = std::collections::HashMap::new();
    for idx in 1..halt_idx {
        let inst = &sub_program.instructions[idx];
        let sub_addr = idx as i32;
        let inlined_addr = b.cur_addr();
        addr_map.insert(sub_addr, inlined_addr);
        match inst.opcode {
            Opcode::ResultRow => {
                let result_start = inst.p1 + reg_offset;
                let nres = inst.p2;
                let block = b.alloc_regs(ncol);
                for j in 0..nres.min(ncol) {
                    b.emit(Opcode::SCopy, result_start + j, block + j, 0);
                }
                for j in nres..ncol {
                    b.emit(Opcode::Null, 0, block + j, 0);
                }
                let rec = b.alloc_reg();
                b.emit(Opcode::MakeRecord, block, ncol, rec);
                let rowid_reg = b.alloc_reg();
                b.emit(Opcode::NewRowid, target_cursor, rowid_reg, 0);
                b.emit(Opcode::Insert, target_cursor, rec, rowid_reg);
            }
            _ => {
                let mut cloned = inst.clone();
                rebase_register_operands(&mut cloned, reg_offset);
                rebase_cursor_operands(&mut cloned, cursor_offset);
                b.append(cloned);
            }
        }
    }
    b.resolve(after);
    let after_addr = b.label_addr_of(after);
    for (i, inst) in b.iter_insts_mut().enumerate() {
        let addr = i as i32;
        if addr < sub_start || addr >= after_addr {
            continue;
        }
        if !is_absolute_jump(inst) {
            continue;
        }
        let sub_target = inst.p2;
        if sub_target == halt_idx as i32 {
            inst.p2 = after_addr;
        } else if let Some(&inlined) = addr_map.get(&sub_target) {
            inst.p2 = inlined;
        }
    }
    // Advance the builder's register high-water mark past the inlined sub-program's
    // registers so the final program's `num_registers` covers them.
    let max_sub_reg = sub_program.num_registers as i32;
    b.advance_regs(reg_offset + max_sub_reg);
    Ok(())
}

/// Inline the recursive query's scan code into the builder, rewriting each `ResultRow` into
/// an insert into the Queue ephemeral. The recursive query's table cursor (cursor 0, opened
/// by its `OpenRead` over the CTE name) is redirected to the Current pseudo-cursor:
/// `OpenRead` is dropped, and every reference to cursor 0 is rewritten to `current_cursor`.
/// Register operands are rebased by `reg_offset` so the inlined code doesn't clobber the
/// outer program's registers (especially `reg_current`).
fn inline_recursive_query_into_queue(
    b: &mut ProgramBuilder,
    sub_program: &Program,
    target_cursor: i32,
    ncol: i32,
    current_cursor: i32,
) -> Result<()> {
    let reg_offset = b.next_reg() - 1;
    let cursor_offset = b.next_cursor();
    let sub_start = b.cur_addr();
    let halt_idx = sub_program
        .instructions
        .iter()
        .position(|i| i.opcode == Opcode::Halt)
        .ok_or_else(|| Error::msg("recursive sub-program has no Halt"))?;
    let after = b.new_label();
    let mut addr_map: std::collections::HashMap<i32, i32> = std::collections::HashMap::new();
    for idx in 1..halt_idx {
        let inst = &sub_program.instructions[idx];
        let sub_addr = idx as i32;
        let inlined_addr = b.cur_addr();
        addr_map.insert(sub_addr, inlined_addr);
        match inst.opcode {
            // Drop the `OpenRead` for the CTE-name table — the Current pseudo-cursor is
            // already open at `current_cursor`.
            Opcode::OpenRead | Opcode::OpenWrite | Opcode::OpenWriteReg => {
                // Skip — don't emit.
            }
            Opcode::ResultRow => {
                let result_start = inst.p1 + reg_offset;
                let nres = inst.p2;
                let block = b.alloc_regs(ncol);
                for j in 0..nres.min(ncol) {
                    b.emit(Opcode::SCopy, result_start + j, block + j, 0);
                }
                for j in nres..ncol {
                    b.emit(Opcode::Null, 0, block + j, 0);
                }
                let rec = b.alloc_reg();
                b.emit(Opcode::MakeRecord, block, ncol, rec);
                let rowid_reg = b.alloc_reg();
                b.emit(Opcode::NewRowid, target_cursor, rowid_reg, 0);
                b.emit(Opcode::Insert, target_cursor, rec, rowid_reg);
            }
            _ => {
                let mut cloned = inst.clone();
                // Rebase register operands.
                rebase_register_operands(&mut cloned, reg_offset);
                // Rebase cursor operands past the outer program's cursors.
                rebase_cursor_operands(&mut cloned, cursor_offset);
                // Rewrite the CTE-name table cursor (now `cursor_offset + 0`) to the Current
                // pseudo-cursor. Only opcodes where `p1` is a cursor operand.
                if is_cursor_p1_opcode(cloned.opcode) && cloned.p1 == cursor_offset {
                    cloned.p1 = current_cursor;
                }
                b.append(cloned);
            }
        }
    }
    b.resolve(after);
    let after_addr = b.label_addr_of(after);
    for (i, inst) in b.iter_insts_mut().enumerate() {
        let addr = i as i32;
        if addr < sub_start || addr >= after_addr {
            continue;
        }
        if !is_absolute_jump(inst) {
            continue;
        }
        let sub_target = inst.p2;
        if sub_target == halt_idx as i32 {
            inst.p2 = after_addr;
        } else if let Some(&inlined) = addr_map.get(&sub_target) {
            inst.p2 = inlined;
        }
    }
    let max_sub_reg = sub_program.num_registers as i32;
    b.advance_regs(reg_offset + max_sub_reg);
    Ok(())
}

/// Inline the outer query's scan code, patching its `OpenRead` (cursor 0 over the CTE name)
/// to use the already-open CTE result ephemeral at `cte_cursor`. The outer query's `Column`/
/// `Rewind`/`Next` on cursor 0 are rewritten to `cte_cursor`.
fn inline_outer_scan(
    b: &mut ProgramBuilder,
    sub_program: &Program,
    cte_cursor: i32,
    outputs: &[(Expr, String)],
    limit: Option<i64>,
    offset: i64,
) -> Result<()> {
    let reg_offset = b.next_reg() - 1;
    let cursor_offset = b.next_cursor();
    let sub_start = b.cur_addr();
    let halt_idx = sub_program
        .instructions
        .iter()
        .position(|i| i.opcode == Opcode::Halt)
        .ok_or_else(|| Error::msg("outer sub-program has no Halt"))?;
    let after = b.new_label();
    let mut addr_map: std::collections::HashMap<i32, i32> = std::collections::HashMap::new();
    let ncol = outputs.len() as i32;
    for idx in 1..halt_idx {
        let inst = &sub_program.instructions[idx];
        let sub_addr = idx as i32;
        let inlined_addr = b.cur_addr();
        addr_map.insert(sub_addr, inlined_addr);
        match inst.opcode {
            // Drop the OpenRead — the CTE ephemeral is already open.
            Opcode::OpenRead | Opcode::OpenWrite | Opcode::OpenWriteReg => {}
            _ => {
                let mut cloned = inst.clone();
                rebase_register_operands(&mut cloned, reg_offset);
                rebase_cursor_operands(&mut cloned, cursor_offset);
                // Rewrite the CTE-name table cursor (now `cursor_offset + 0`) to the CTE
                // result ephemeral. Only opcodes where `p1` is a cursor operand (already
                // rebased by `rebase_cursor_operands`) are candidates.
                if is_cursor_p1_opcode(cloned.opcode) && cloned.p1 == cursor_offset {
                    cloned.p1 = cte_cursor;
                }
                b.append(cloned);
            }
        }
    }
    b.resolve(after);
    let after_addr = b.label_addr_of(after);
    for (i, inst) in b.iter_insts_mut().enumerate() {
        let addr = i as i32;
        if addr < sub_start || addr >= after_addr {
            continue;
        }
        if !is_absolute_jump(inst) {
            continue;
        }
        let sub_target = inst.p2;
        if sub_target == halt_idx as i32 {
            inst.p2 = after_addr;
        } else if let Some(&inlined) = addr_map.get(&sub_target) {
            inst.p2 = inlined;
        }
    }
    // Emit the trailing Halt for the outer scan.
    let _ = ncol;
    let _ = limit;
    let _ = offset;
    let max_sub_reg = sub_program.num_registers as i32;
    b.advance_regs(reg_offset + max_sub_reg);
    b.emit(Opcode::Halt, 0, 0, 0);
    Ok(())
}

/// Whether `p1` of this opcode is a cursor number (vs a register or other operand). Used to
/// guard the cursor-rewrite in the inlining helpers so a register operand that happens to
/// equal the cursor offset isn't accidentally rewritten.
fn is_cursor_p1_opcode(op: Opcode) -> bool {
    use Opcode::*;
    matches!(
        op,
        OpenRead | OpenWrite | OpenWriteReg | OpenEphemeral | OpenPseudo | Close | Rewind
            | Next | Column | Rowid | NullRow | NotExists | SeekGE | SeekGT | SeekLE | SeekLT
            | IdxGE | IdxGT | IdxLE | IdxLT | Found | NotFound | NoConflict | IdxInsert
            | IdxDelete | IdxRowid | RowData | Delete | Insert | NewRowid | SorterOpen
            | SorterInsert | SorterData | SorterSort | SorterNext
    )
}

/// Whether an instruction uses `p2` as an absolute jump target. Mirrors `is_absolute_jump`
/// in `subquery.rs` (kept local to avoid a cross-module dependency).
fn is_absolute_jump(inst: &Instruction) -> bool {
    use Opcode::*;
    matches!(
        inst.opcode,
        Goto | Init | Gosub | If | IfNot | IsNull | NotNull | IfPos | DecrJumpZero | Eq | Ne | Lt
            | Le | Gt | Ge | Rewind | Next | NotExists | SeekGE | SeekGT | SeekLE | SeekLT
            | IdxGE | IdxGT | IdxLE | IdxLT | Found | NotFound | NoConflict | SorterSort
            | SorterNext
    )
}

/// Rebase every register operand of `inst` by `reg_offset`. Jump targets (p2 of control-flow
/// opcodes) are NOT rebased here — the caller patches them via the address map.
fn rebase_register_operands(inst: &mut Instruction, reg_offset: i32) {
    use Opcode::*;
    let r = |x: &mut i32| *x += reg_offset;
    match inst.opcode {
        // Control flow — p2 is a jump target (NOT rebased here).
        Goto | Init | Once | Rewind | Next | SorterSort | SorterNext | SeekGE | SeekGT
        | SeekLE | SeekLT | IdxGE | IdxGT | IdxLE | IdxLT | Found | NotFound | NotExists => {}
        Gosub => r(&mut inst.p1),
        Return => r(&mut inst.p1),
        If | IfNot | IsNull | NotNull => r(&mut inst.p1),
        IfPos | DecrJumpZero => r(&mut inst.p1),
        Eq | Ne | Lt | Le | Gt | Ge => {
            r(&mut inst.p1);
            r(&mut inst.p3);
        }
        And | Or | Not => {
            r(&mut inst.p1);
            r(&mut inst.p2);
        }
        // Record building.
        MakeRecord => {
            r(&mut inst.p1);
            r(&mut inst.p3);
        }
        NewRowid => r(&mut inst.p2),
        Insert => {
            r(&mut inst.p2);
            r(&mut inst.p3);
        }
        Delete => {}
        // Constants — p2 = destination register.
        Integer | Int64 | Real | String8 | Null | Blob => {
            r(&mut inst.p2);
            if inst.opcode == Null && inst.p3 > 0 {
                r(&mut inst.p3);
            }
        }
        // Arithmetic / bitwise — r[p3] = r[p2] OP r[p1].
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
        // Column reads — p3 = destination register.
        Column => r(&mut inst.p3),
        Rowid => r(&mut inst.p2),
        // Copies / moves.
        Copy | SCopy | Move => {
            r(&mut inst.p1);
            r(&mut inst.p2);
        }
        // ResultRow — p1 = start of result registers.
        ResultRow => r(&mut inst.p1),
        // Sorter.
        SorterInsert => r(&mut inst.p2),
        SorterData => r(&mut inst.p2),
        SorterOpen => {}
        // Aggregates.
        AggStep => {
            r(&mut inst.p2);
        }
        AggInverse => {
            r(&mut inst.p2);
        }
        AggFinal => {
            r(&mut inst.p2);
        }
        AggValue => {
            r(&mut inst.p3);
        }
        // Function call — p3 = result register; p1/p2 depend on the function.
        Function => {
            r(&mut inst.p3);
        }
        // Affinity / cast.
        Affinity | RealAffinity => r(&mut inst.p1),
        // Compare — p1 = start of first array, p3 = start of second array.
        Compare => {
            r(&mut inst.p1);
            r(&mut inst.p3);
        }
        Jump => {}
        HaltIfNull => r(&mut inst.p3),
        // Opcodes with no register operands.
        Halt | Transaction | SetCookie | ParseSchema | CreateBtree | Destroy | Clear
        | Close | NullRow | OpenRead | OpenWrite | OpenWriteReg | OpenEphemeral
        | OpenPseudo | RowData | IdxInsert | IdxDelete | IdxRowid | Program | Param => {}
        _ => {}
    }
}

/// Rebase every cursor operand of `inst` by `cursor_offset`.
fn rebase_cursor_operands(inst: &mut Instruction, cursor_offset: i32) {
    use Opcode::*;
    let c = |x: &mut i32| *x += cursor_offset;
    match inst.opcode {
        OpenRead | OpenWrite | OpenWriteReg | OpenEphemeral | OpenPseudo | Close => c(&mut inst.p1),
        Rewind | Next | Column | Rowid | NullRow | NotExists | SeekGE | SeekGT | SeekLE
        | SeekLT | IdxGE | IdxGT | IdxLE | IdxLT | Found | NotFound | NoConflict | IdxInsert
        | IdxDelete | IdxRowid | RowData | Delete | Insert | NewRowid | SorterOpen
        | SorterInsert | SorterData | SorterSort | SorterNext => c(&mut inst.p1),
        _ => {}
    }
}

/// Resolve the source table for a query's FROM clause, returning the synthesized table info
/// needed to compile it. Returns `None` for a constant/VALUES query (no FROM table).
fn resolve_source_table(
    db: &mut crate::capi::connection::Sqlite3,
    select: &SelectStmt,
) -> Result<Option<Table>> {
    if !select.values.is_empty() {
        return Ok(None);
    }
    if select.from.is_empty() {
        return Ok(None);
    }
    if select.from.len() == 1 {
        if let Some(tref) = select.from[0].table() {
            let pager = db.pager_arc()?;
            let catalog = crate::capi::runtime::block_on(crate::schema::read_catalog(&pager))?;
            let obj = catalog
                .find_table(&tref.name)
                .ok_or_else(|| Error::msg(format!("no such table: {}", tref.name)))?;
            let table = Table::from_schema_object(obj)?;
            return Ok(Some(table));
        }
    }
    Err(Error::msg(
        "recursive CTE setup/recursive arm FROM must be a single real table or a constant/VALUES SELECT",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustqlite_parser::parse;

    fn select_of(sql: &str) -> SelectStmt {
        let stmts = parse(sql).expect("parse");
        match stmts.into_iter().next() {
            Some(rustqlite_parser::Stmt::Select(s)) => s,
            other => panic!("expected SELECT, got {other:?}"),
        }
    }

    #[test]
    fn non_recursive_cte_rewrites_from_table_to_subquery() {
        let s = select_of("WITH x AS (SELECT 1 AS a) SELECT * FROM x;");
        let r = rewrite_with_ctes(&s).expect("rewrite");
        assert!(r.with_clause.is_none());
        assert_eq!(r.from.len(), 1);
        match &r.from[0] {
            TableOrJoin::Subquery { alias, .. } => assert_eq!(alias, "x"),
            other => panic!("expected Subquery, got {other:?}"),
        }
    }

    #[test]
    fn cte_reference_keeps_user_alias() {
        let s = select_of("WITH x AS (SELECT 1 AS a) SELECT * FROM x AS y;");
        let r = rewrite_with_ctes(&s).expect("rewrite");
        match &r.from[0] {
            TableOrJoin::Subquery { alias, .. } => assert_eq!(alias, "y"),
            other => panic!("expected Subquery, got {other:?}"),
        }
    }

    #[test]
    fn schema_qualified_name_is_not_a_cte() {
        let s = select_of("WITH x AS (SELECT 1 AS a) SELECT * FROM main.x;");
        let r = rewrite_with_ctes(&s).expect("rewrite");
        // The reference stays a Table; the CTE is unused (and will fail at catalog lookup,
        // which is the correct behavior — schema-qualified names never match CTEs).
        assert!(matches!(r.from[0], TableOrJoin::Table(_)));
    }

    #[test]
    fn multiple_ctes_later_references_earlier() {
        let s = select_of(
            "WITH a AS (SELECT 1 AS x), b AS (SELECT x FROM a) SELECT * FROM b;",
        );
        let r = rewrite_with_ctes(&s).expect("rewrite");
        // Outer FROM references b → Subquery.
        let b_body = match &r.from[0] {
            TableOrJoin::Subquery { query, .. } => query.as_ref().clone(),
            other => panic!("expected Subquery, got {other:?}"),
        };
        // b's body references a → Subquery.
        match &b_body.from[0] {
            TableOrJoin::Subquery { alias, .. } => assert_eq!(alias, "a"),
            other => panic!("expected nested Subquery, got {other:?}"),
        }
    }

    #[test]
    fn explicit_column_list_wraps_projection() {
        let s = select_of(
            "WITH x (p, q) AS (SELECT 1, 2) SELECT p, q FROM x;",
        );
        let r = rewrite_with_ctes(&s).expect("rewrite");
        let body = match &r.from[0] {
            TableOrJoin::Subquery { query, .. } => query.as_ref().clone(),
            other => panic!("expected Subquery, got {other:?}"),
        };
        // The body's projection now carries aliases p, q.
        assert_eq!(body.columns.len(), 2);
        match &body.columns[0] {
            ResultColumn::Expr { alias, .. } => assert_eq!(alias.as_deref(), Some("p")),
            other => panic!("expected Expr, got {other:?}"),
        }
        match &body.columns[1] {
            ResultColumn::Expr { alias, .. } => assert_eq!(alias.as_deref(), Some("q")),
            other => panic!("expected Expr, got {other:?}"),
        }
    }

    #[test]
    fn recursive_cte_errors() {
        let s = select_of(
            "WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM x WHERE n<5) SELECT n FROM x;",
        );
        let r = rewrite_with_ctes(&s);
        assert!(r.is_err());
    }
}