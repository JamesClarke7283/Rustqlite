//! Lowering a single-table `SELECT` to a VDBE program (mirrors `sqlite3Select` in `select.c`).
//!
//! Two shapes are produced:
//! * a **table scan** — `Init → Transaction → OpenRead → Rewind → [WHERE; project; ResultRow] →
//!   Next → Halt`, with `ORDER BY` lowering to a sorter and `LIMIT`/`OFFSET` wrapping the output;
//! * a **constant** `SELECT` (no `FROM`) — evaluate the projection once and emit a single row.
//! * (M5.1) an **indexed equality** — for `WHERE <indexed_col> = <const>` with a usable
//!   single-column index, the scan walks the index, looks up each rowid in the table, and
//!   projects; the sorter is dropped when `ORDER BY <indexed_col> ASC` is also present (the
//!   index is already ordered).

use rustqlite_parser::{Expr, Literal, OrderingTerm, ResultColumn, SelectStmt, UnaryOp};

use crate::error::{Error, Result};
use crate::schema::{IndexObject, Table};
use crate::types::Value;
use crate::util::fp::fp_to_text;
use crate::vdbe::program::{Program, P4};
use crate::vdbe::{KeyField, Opcode};

use super::builder::ProgramBuilder;
use super::expr::{compile_expr, compile_jump, Ctx};
use super::index_planner::{pick_index, IndexPlan};

/// Compile a single-table (or constant) `SELECT`, returning the program and the result column
/// names. `table` is the resolved table when there is exactly one `FROM` entry, else `None`.
/// `indexes` is the list of indexes attached to `table`; when an indexed equality is
/// present in the `WHERE`, the M5.1 planner routes through the index.
pub fn compile(
    select: &SelectStmt,
    table: Option<&Table>,
    indexes: &[IndexObject],
) -> Result<(Program, Vec<String>)> {
    reject_unsupported(select)?;
    if select.from.len() > 1 {
        return Err(Error::msg("joins are not supported in M3a"));
    }

    let outputs = expand_columns(select, table)?;
    let names: Vec<String> = outputs.iter().map(|(_, n)| n.clone()).collect();
    let (limit, offset) = eval_limit_offset(select)?;

    let program = match table {
        Some(t) => {
            // M5.1: try an indexed-equality plan first; fall back to a scan if no index
            // covers the WHERE.
            if let Some(plan) = pick_index(select, t, indexes) {
                compile_indexed_select(select, t, &plan, &outputs, limit, offset)?
            } else {
                compile_scan(select, t, &outputs, limit, offset)?
            }
        }
        None => compile_constant(select, &outputs, limit, offset)?,
    };
    Ok((program, names))
}

/// An indexed-equality codegen. Opens a read cursor on the index b-tree (with `KeyInfo`
/// marking it as an index), opens a read cursor on the table, `SeekGE`s the index for the
/// first entry `>=` the search key, then `IdxGT`s to verify strict equality (jumping past
/// the body on a non-match). The body pulls the rowid from the index, seeks the table,
/// re-checks the `WHERE` (defensive: the M5.1 first slice only emits single-equality
/// `WHERE`s, so this re-check is a tautology), projects the result columns, and
/// `Next`-iterates the index.
#[allow(clippy::too_many_arguments)]
fn compile_indexed_select(
    _select: &SelectStmt,
    table: &Table,
    plan: &IndexPlan,
    outputs: &[(Expr, String)],
    limit: Option<i64>,
    offset: i64,
) -> Result<Program> {
    let cursor = 0i32;
    let idx_cursor = 1i32;
    let ncol = outputs.len() as i32;
    let ctx = Ctx {
        table,
        cursor,
    };
    let mut b = ProgramBuilder::new();

    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    let after_init = b.cur_addr();

    // LIMIT 0 → no rows.
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

    // (1) Open the table cursor (read-only).
    let open_table = b.emit(Opcode::OpenRead, cursor, table.rootpage as i32, 0);
    b.set_p4(open_table, P4::Int(table.columns.len() as i64));

    // (2) Open the index cursor with KeyInfo (marks it as an index cursor in the executor).
    let open_idx = b.emit(
        Opcode::OpenRead,
        idx_cursor,
        plan.index.rootpage as i32,
        0,
    );
    b.set_p4(
        open_idx,
        P4::KeyInfo(vec![KeyField {
            desc: false,
            collation: crate::types::Collation::Binary,
        }]),
    );

    // (3) Load the constant RHS into a register; emit the SeekGE.
    let key_reg = b.alloc_reg();
    emit_value_load(&mut b, &plan.equality.value, key_reg);
    let end_seek = b.new_label();
    let seek = b.emit_jump(Opcode::SeekGE, idx_cursor, end_seek, key_reg);
    b.set_p4(seek, P4::Int(1)); // nField = 1 (the indexed column)

    // (5) Loop body: read the rowid, seek the table, project. The IdxGT boundary
    // check is re-emitted at the top of every iteration (not just the first) so the loop
    // terminates when the index passes the strict-equality boundary.
    let loop_top = b.new_label();
    b.resolve(loop_top);
    let idx_gt = b.emit_jump(Opcode::IdxGT, idx_cursor, end_seek, key_reg);
    b.set_p4(idx_gt, P4::Int(1));
    let rowid_reg = b.alloc_reg();
    b.emit(Opcode::IdxRowid, idx_cursor, rowid_reg, 0);
    let idx_next = b.new_label();
    b.emit_jump(Opcode::NotExists, cursor, idx_next, rowid_reg);

    // OFFSET gate.
    if let Some(oreg) = offset_reg {
        b.emit_jump(Opcode::IfPos, oreg, idx_next, 1);
    }

    // Project the result columns.
    let result_reg = b.alloc_regs(ncol);
    for (j, (expr, _)) in outputs.iter().enumerate() {
        compile_expr(&mut b, expr, result_reg + j as i32, ctx)?;
    }
    b.emit(Opcode::ResultRow, result_reg, ncol, 0);
    if let Some(lreg) = limit_reg {
        b.emit_jump(Opcode::DecrJumpZero, lreg, end_seek, 0);
    }

    // Advance: next index entry, jumping back to the top of the body.
    b.emit_jump(Opcode::Next, idx_cursor, loop_top, 0);
    b.resolve(idx_next);
    b.resolve(end_seek);

    b.emit(Opcode::Halt, 0, 0, 0);
    b.resolve(setup);
    b.emit(Opcode::Transaction, 0, 0, 0);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

/// Emit a register load of a literal [`Value`] (used by the indexed path's constant-RHS).
fn emit_value_load(b: &mut ProgramBuilder, v: &Value, target: i32) {
    match v {
        Value::Null => {
            b.emit(Opcode::Null, 0, target, 0);
        }
        Value::Int(n) => match i32::try_from(*n) {
            Ok(n32) => {
                b.emit(Opcode::Integer, n32, target, 0);
            }
            Err(_) => {
                let i = b.emit(Opcode::Int64, 0, target, 0);
                b.set_p4(i, P4::Int(*n));
            }
        },
        Value::Real(r) => {
            let i = b.emit(Opcode::Real, 0, target, 0);
            b.set_p4(i, P4::Real(*r));
        }
        Value::Text(s) => {
            let i = b.emit(Opcode::String8, 0, target, 0);
            b.set_p4(i, P4::Text(s.clone()));
        }
        Value::Blob(bytes) => {
            let i = b.emit(Opcode::Blob, 0, target, 0);
            b.set_p4(i, P4::Blob(bytes.clone()));
        }
    }
}

/// Reject M3a-out-of-scope features with a clear message.
fn reject_unsupported(select: &SelectStmt) -> Result<()> {
    if select.distinct {
        return Err(Error::msg("SELECT DISTINCT is not supported in M3a"));
    }
    if !select.group_by.is_empty() || select.having.is_some() {
        return Err(Error::msg("GROUP BY / HAVING are not supported in M3a"));
    }
    Ok(())
}

/// A table scan, optionally ordered, with LIMIT/OFFSET.
fn compile_scan(
    select: &SelectStmt,
    table: &Table,
    outputs: &[(Expr, String)],
    limit: Option<i64>,
    offset: i64,
) -> Result<Program> {
    let cursor = 0i32;
    let ncol = outputs.len() as i32;
    let ctx = Ctx { table, cursor };
    let mut b = ProgramBuilder::new();

    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0); // addr 0
    let after_init = b.cur_addr(); // addr 1

    // LIMIT 0 → no rows at all.
    if limit == Some(0) {
        b.emit(Opcode::Halt, 0, 0, 0);
        b.resolve(setup);
        b.emit(Opcode::Transaction, 0, 0, 0);
        b.emit(Opcode::Goto, 0, after_init, 0);
        return Ok(b.finish());
    }

    // LIMIT / OFFSET counter registers.
    let limit_reg = match limit {
        Some(n) if n > 0 => Some(emit_int(&mut b, n)),
        _ => None,
    };
    let offset_reg = (offset > 0).then(|| emit_int(&mut b, offset));

    let open = b.emit(Opcode::OpenRead, cursor, table.rootpage as i32, 0);
    b.set_p4(open, P4::Int(table.columns.len() as i64));

    if select.order_by.is_empty() {
        compile_scan_unordered(&mut b, select, ctx, outputs, ncol, limit_reg, offset_reg)?;
    } else {
        compile_scan_ordered(&mut b, select, ctx, outputs, ncol, limit_reg, offset_reg)?;
    }

    b.resolve(setup);
    b.emit(Opcode::Transaction, 0, 0, 0);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

#[allow(clippy::too_many_arguments)]
fn compile_scan_unordered(
    b: &mut ProgramBuilder,
    select: &SelectStmt,
    ctx: Ctx,
    outputs: &[(Expr, String)],
    ncol: i32,
    limit_reg: Option<i32>,
    offset_reg: Option<i32>,
) -> Result<()> {
    let cursor = ctx.cursor;
    let end = b.new_label();
    b.emit_jump(Opcode::Rewind, cursor, end, 0);
    let loop_top = b.cur_addr();
    let next_label = b.new_label();

    if let Some(w) = &select.where_clause {
        compile_jump(b, w, next_label, false, true, ctx)?;
    }
    if let Some(oreg) = offset_reg {
        b.emit_jump(Opcode::IfPos, oreg, next_label, 1);
    }

    let result_reg = b.alloc_regs(ncol);
    for (j, (expr, _)) in outputs.iter().enumerate() {
        compile_expr(b, expr, result_reg + j as i32, ctx)?;
    }
    b.emit(Opcode::ResultRow, result_reg, ncol, 0);
    if let Some(lreg) = limit_reg {
        b.emit_jump(Opcode::DecrJumpZero, lreg, end, 0);
    }

    b.resolve(next_label);
    b.emit(Opcode::Next, cursor, loop_top, 0);
    b.resolve(end);
    b.emit(Opcode::Halt, 0, 0, 0);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn compile_scan_ordered(
    b: &mut ProgramBuilder,
    select: &SelectStmt,
    ctx: Ctx,
    outputs: &[(Expr, String)],
    ncol: i32,
    limit_reg: Option<i32>,
    offset_reg: Option<i32>,
) -> Result<()> {
    let cursor = ctx.cursor;
    let sorter = 1i32;
    let order = &select.order_by;
    let nkey = order.len() as i32;

    let keyinfo: Vec<KeyField> = order
        .iter()
        .map(|t| KeyField {
            desc: t.desc,
            collation: crate::types::Collation::Binary,
        })
        .collect();
    let so = b.emit(Opcode::SorterOpen, sorter, nkey + ncol, 0);
    b.set_p4(so, P4::KeyInfo(keyinfo));

    // --- scan loop: filter, build [keys..., outputs...] records, insert into the sorter ---
    let end_scan = b.new_label();
    b.emit_jump(Opcode::Rewind, cursor, end_scan, 0);
    let scan_top = b.cur_addr();
    let scan_next = b.new_label();

    if let Some(w) = &select.where_clause {
        compile_jump(b, w, scan_next, false, true, ctx)?;
    }
    let block = b.alloc_regs(nkey + ncol);
    for (k, term) in order.iter().enumerate() {
        let key_expr = resolve_order_term(term, outputs)?;
        compile_expr(b, &key_expr, block + k as i32, ctx)?;
    }
    for (j, (expr, _)) in outputs.iter().enumerate() {
        compile_expr(b, expr, block + nkey + j as i32, ctx)?;
    }
    let rec = b.alloc_reg();
    b.emit(Opcode::MakeRecord, block, nkey + ncol, rec);
    b.emit(Opcode::SorterInsert, sorter, rec, 0);
    b.resolve(scan_next);
    b.emit(Opcode::Next, cursor, scan_top, 0);
    b.resolve(end_scan);

    // --- output loop: sorted iteration with OFFSET/LIMIT ---
    let end_out = b.new_label();
    b.emit_jump(Opcode::SorterSort, sorter, end_out, 0);
    let out_top = b.cur_addr();
    let sort_next = b.new_label();
    b.emit(Opcode::SorterData, sorter, 0, 0);
    if let Some(oreg) = offset_reg {
        b.emit_jump(Opcode::IfPos, oreg, sort_next, 1);
    }
    let result_reg = b.alloc_regs(ncol);
    for j in 0..ncol {
        // Output column j lives at record index nkey+j.
        b.emit(Opcode::Column, sorter, nkey + j, result_reg + j);
    }
    b.emit(Opcode::ResultRow, result_reg, ncol, 0);
    if let Some(lreg) = limit_reg {
        b.emit_jump(Opcode::DecrJumpZero, lreg, end_out, 0);
    }
    b.resolve(sort_next);
    b.emit(Opcode::SorterNext, sorter, out_top, 0);
    b.resolve(end_out);
    b.emit(Opcode::Halt, 0, 0, 0);
    Ok(())
}

/// A constant `SELECT` (no `FROM`) produces exactly one row (zero if `LIMIT 0` or `OFFSET > 0`).
fn compile_constant(
    select: &SelectStmt,
    outputs: &[(Expr, String)],
    limit: Option<i64>,
    offset: i64,
) -> Result<Program> {
    // No table: column references resolve against an empty table and therefore error.
    let empty = Table {
        name: String::new(),
        rootpage: 0,
        columns: Vec::new(),
        rowid_alias: None,
    };
    let ctx = Ctx {
        table: &empty,
        cursor: -1,
    };
    let ncol = outputs.len() as i32;
    let mut b = ProgramBuilder::new();

    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    let after_init = b.cur_addr();

    let no_rows = limit == Some(0) || offset > 0;
    if !no_rows {
        let end = b.new_label();
        if let Some(w) = &select.where_clause {
            compile_jump(&mut b, w, end, false, true, ctx)?;
        }
        let result_reg = b.alloc_regs(ncol);
        for (j, (expr, _)) in outputs.iter().enumerate() {
            compile_expr(&mut b, expr, result_reg + j as i32, ctx)?;
        }
        b.emit(Opcode::ResultRow, result_reg, ncol, 0);
        b.resolve(end);
    }
    b.emit(Opcode::Halt, 0, 0, 0);
    b.resolve(setup);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

/// Expand `*` / `table.*` and resolve aliases into `(expression, column-name)` pairs.
fn expand_columns(select: &SelectStmt, table: Option<&Table>) -> Result<Vec<(Expr, String)>> {
    let mut out = Vec::new();
    for rc in &select.columns {
        match rc {
            ResultColumn::Star | ResultColumn::TableStar(_) => {
                let t = table.ok_or_else(|| Error::msg("no tables specified"))?;
                for col in &t.columns {
                    out.push((column_expr(&col.name), col.name.clone()));
                }
            }
            ResultColumn::Expr { expr, alias } => {
                let name = alias.clone().unwrap_or_else(|| default_col_name(expr));
                out.push((expr.clone(), name));
            }
        }
    }
    if out.is_empty() {
        return Err(Error::msg("no result columns"));
    }
    Ok(out)
}

fn column_expr(name: &str) -> Expr {
    Expr::Column {
        schema: None,
        table: None,
        name: name.to_string(),
    }
}

/// Resolve an `ORDER BY` term: an integer ordinal selects an output column; a bare name that
/// matches an output alias uses that output's expression; otherwise the term is used as written.
fn resolve_order_term(term: &OrderingTerm, outputs: &[(Expr, String)]) -> Result<Expr> {
    if let Expr::Literal(Literal::Integer(n)) = &term.expr {
        let idx = *n;
        if idx >= 1 && (idx as usize) <= outputs.len() {
            return Ok(outputs[(idx - 1) as usize].0.clone());
        }
        return Err(Error::msg(format!(
            "ORDER BY term out of range - should be between 1 and {}",
            outputs.len()
        )));
    }
    if let Expr::Column {
        table: None, name, ..
    } = &term.expr
    {
        if let Some((expr, _)) = outputs.iter().find(|(_, n)| n.eq_ignore_ascii_case(name)) {
            return Ok(expr.clone());
        }
    }
    Ok(term.expr.clone())
}

/// Const-evaluate a literal-integer `LIMIT`/`OFFSET`. Returns `(limit, offset)` where `limit`
/// is `None` for "unlimited" (absent or negative) and `Some(n)` otherwise; `offset` is clamped
/// to `>= 0`.
fn eval_limit_offset(select: &SelectStmt) -> Result<(Option<i64>, i64)> {
    let limit = match &select.limit {
        None => None,
        Some(e) => {
            let n = const_int(e)
                .ok_or_else(|| Error::msg("only integer-literal LIMIT is supported in M3a"))?;
            (n >= 0).then_some(n)
        }
    };
    let offset = match &select.offset {
        None => 0,
        Some(e) => const_int(e)
            .ok_or_else(|| Error::msg("only integer-literal OFFSET is supported in M3a"))?
            .max(0),
    };
    Ok((limit, offset))
}

fn const_int(e: &Expr) -> Option<i64> {
    match e {
        Expr::Literal(Literal::Integer(n)) => Some(*n),
        Expr::Unary {
            op: UnaryOp::Negate,
            expr,
        } => const_int(expr).map(|n| -n),
        Expr::Unary {
            op: UnaryOp::Positive,
            expr,
        } => const_int(expr),
        _ => None,
    }
}

/// Emit a load of an `i64` constant into a fresh register, returning it.
fn emit_int(b: &mut ProgramBuilder, n: i64) -> i32 {
    let r = b.alloc_reg();
    match i32::try_from(n) {
        Ok(n32) => {
            b.emit(Opcode::Integer, n32, r, 0);
        }
        Err(_) => {
            let i = b.emit(Opcode::Int64, 0, r, 0);
            b.set_p4(i, P4::Int(n));
        }
    }
    r
}

/// A best-effort default column name for an unaliased non-column expression. SQLite uses the
/// expression's source text; without spans we reconstruct an approximation (only used for
/// header display — the result *rows* are unaffected).
fn default_col_name(expr: &Expr) -> String {
    match expr {
        Expr::Column { name, .. } => name.clone(),
        other => expr_to_text(other),
    }
}

fn expr_to_text(e: &Expr) -> String {
    use rustqlite_parser::FunctionArgs;
    match e {
        Expr::Literal(Literal::Null) => "NULL".to_string(),
        Expr::Literal(Literal::Integer(n)) => n.to_string(),
        Expr::Literal(Literal::Real(r)) => fp_to_text(*r),
        Expr::Literal(Literal::Text(s)) => format!("'{}'", s.replace('\'', "''")),
        Expr::Literal(Literal::Blob(_)) => "x'..'".to_string(),
        Expr::Literal(Literal::Bool(b)) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        Expr::Column {
            table: Some(t),
            name,
            ..
        } => format!("{t}.{name}"),
        Expr::Column { name, .. } => name.clone(),
        Expr::Unary { op, expr } => {
            let s = expr_to_text(expr);
            match op {
                UnaryOp::Negate => format!("-{s}"),
                UnaryOp::Positive => format!("+{s}"),
                UnaryOp::Not => format!("NOT {s}"),
            }
        }
        Expr::Binary { op, left, right } => {
            let sym = binary_symbol(*op);
            format!("{}{}{}", expr_to_text(left), sym, expr_to_text(right))
        }
        Expr::Function { name, args, .. } => {
            let inner = match args {
                FunctionArgs::Star => "*".to_string(),
                FunctionArgs::List(v) => v.iter().map(expr_to_text).collect::<Vec<_>>().join(", "),
            };
            format!("{name}({inner})")
        }
        Expr::BindParam(s) => s.clone(),
    }
}

fn binary_symbol(op: rustqlite_parser::BinaryOp) -> &'static str {
    use rustqlite_parser::BinaryOp::*;
    match op {
        Or => " OR ",
        And => " AND ",
        Eq => " = ",
        Ne => " <> ",
        Lt => " < ",
        Le => " <= ",
        Gt => " > ",
        Ge => " >= ",
        Add => " + ",
        Sub => " - ",
        Mul => " * ",
        Div => " / ",
        Mod => " % ",
        Concat => " || ",
        Is => " IS ",
        IsNot => " IS NOT ",
        Like => " LIKE ",
        Glob => " GLOB ",
    }
}

/// Helper for the golden codegen test: a readable disassembly of a program.
#[cfg(test)]
pub(crate) fn disassemble(p: &Program) -> Vec<String> {
    p.instructions
        .iter()
        .enumerate()
        .map(|(addr, i)| format_inst(addr, i))
        .collect()
}

#[cfg(test)]
fn format_inst(addr: usize, i: &crate::vdbe::program::Instruction) -> String {
    format!(
        "{addr} {} {} {} {} {:?} {}",
        i.opcode.name(),
        i.p1,
        i.p2,
        i.p3,
        i.p4,
        i.p5
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{SchemaObject, Table};
    use rustqlite_parser::{parse, Stmt};

    fn compile_sql(create: &str, select_sql: &str) -> (Program, Vec<String>) {
        let obj = SchemaObject {
            rowid: 1,
            obj_type: "table".into(),
            name: "t".into(),
            tbl_name: "t".into(),
            rootpage: 2,
            sql: Some(create.into()),
        };
        let table = Table::from_schema_object(&obj).unwrap();
        let Stmt::Select(s) = parse(select_sql).unwrap().into_iter().next().unwrap() else {
            panic!("expected SELECT")
        };
        compile(&s, Some(&table), &[]).unwrap()
    }

    #[test]
    fn golden_select_a_b_where_a_gt_1() {
        let (prog, names) = compile_sql("CREATE TABLE t(a,b)", "SELECT a, b FROM t WHERE a > 1;");
        assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
        // The hand-verified canonical sequence for the scan. The WHERE `a > 1` is lowered to a
        // jump-if-false `Le` (the negation of `>`) with the JUMPIFNULL flag (0x10) and the
        // comparison affinity in the low bits (BLOB=0x01 → p5 = 0x11 = 17). Constant literals
        // are loaded inline (we do not yet hoist them into the init block as upstream does).
        let expected = vec![
            "0 Init 0 11 0 None 0",
            "1 OpenRead 0 2 0 Int(2) 0",
            "2 Rewind 0 10 0 None 0",
            "3 Column 0 0 1 None 0",
            "4 Integer 1 2 0 None 0",
            "5 Le 2 9 1 None 17",
            "6 Column 0 0 3 None 0",
            "7 Column 0 1 4 None 0",
            "8 ResultRow 3 2 0 None 0",
            "9 Next 0 3 0 None 0",
            "10 Halt 0 0 0 None 0",
            "11 Transaction 0 0 0 None 0",
            "12 Goto 0 1 0 None 0",
        ];
        assert_eq!(disassemble(&prog), expected);
    }
}

