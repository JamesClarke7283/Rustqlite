//! RETURNING-clause code generation helper.
//!
//! Mirrors the upstream `Returning` object and `codeReturningTrigger` logic in `trigger.c`.
//! The prepare path expands `*` and resolves column names into a list of expressions, then the
//! write-codegen path calls [`Returning::emit_buffer_row`] per modified row and
//! [`Returning::emit_output_loop`] once after the write transaction.

use rustqlite_parser::{Expr, ResultColumn};

use crate::error::{Error, Result};
use crate::schema::{ColumnRef, Table};
use crate::types::Affinity;
use crate::vdbe::Opcode;

use super::builder::ProgramBuilder;
use super::expr::{compile_expr, Ctx};
use super::select::{default_col_name, expr_to_text};

/// Per-statement RETURNING state produced at prepare time.
#[derive(Debug, Clone)]
pub struct Returning {
    /// The expanded result-column expressions (one per output column).
    pub columns: Vec<(Expr, String)>,
    /// Cursor number for the ephemeral result table. Allocated by the write codegen.
    pub cursor: i32,
    /// First register of the contiguous output block used to buffer and emit rows.
    pub reg_base: i32,
}

impl Returning {
    /// Expand the raw `RETURNING` clause against `table`, producing `(expr, name)` pairs.
    ///
    /// `*` expands to all stored columns in table order (the rowid is NOT included). This matches
    /// upstream's `sqlite3ExpandReturning`. `rowid`/`_rowid_`/`oid` may still be selected
    /// explicitly.
    ///
    /// Table-qualified column references (`t.*`, `t.col`) are accepted but `table` must be the
    /// one referenced. Multi-table `UPDATE ... FROM` and `TABLE.*` in RETURNING are out of scope.
    pub fn new(raw: &[ResultColumn], table: &Table) -> Result<Returning> {
        let mut columns = Vec::new();
        for rc in raw {
            match rc {
                ResultColumn::Star => {
                    for col in &table.columns {
                        columns.push((column_expr(&col.name), col.name.clone()));
                    }
                }
                ResultColumn::TableStar(q) => {
                    if !q.eq_ignore_ascii_case(&table.name) {
                        return Err(Error::msg(
                            "RETURNING may not use \"TABLE.*\" wildcards".to_string(),
                        ));
                    }
                    for col in &table.columns {
                        columns.push((column_expr(&col.name), col.name.clone()));
                    }
                }
                ResultColumn::Expr { expr, alias } => {
                    let name = alias.clone().unwrap_or_else(|| default_col_name(expr));
                    columns.push((expr.clone(), name));
                }
            }
        }
        Ok(Returning {
            columns,
            cursor: -1,
            reg_base: -1,
        })
    }

    /// Number of output columns.
    pub fn n_col(&self) -> usize {
        self.columns.len()
    }

    /// Emit the `OpenEphemeral` instruction and reserve the output register block. Returns
    /// the cursor number and the first output register.
    pub fn emit_open(
        &mut self,
        b: &mut ProgramBuilder,
        proposed_cursor: i32,
    ) -> (i32, i32) {
        let n = self.n_col() as i32;
        b.emit(Opcode::OpenEphemeral, proposed_cursor, n, 0);
        let reg_base = b.alloc_regs(n);
        self.cursor = proposed_cursor;
        self.reg_base = reg_base;
        (proposed_cursor, reg_base)
    }

    /// Emit code to evaluate one RETURNING row from the staged values and insert it into the
    /// ephemeral result table.
    ///
    /// `register_base` is the first register of a contiguous block holding the logical row
    /// values: stored columns in table order, with the rowid-alias column (if any) holding the
    /// actual rowid. `cursor` is the open table cursor (used for `rowid` references when the
    /// table has no INTEGER PRIMARY KEY alias).
    pub fn emit_buffer_row(
        &self,
        b: &mut ProgramBuilder,
        table: &Table,
        cursor: i32,
        register_base: i32,
    ) -> Result<()> {
        let n = self.n_col() as i32;
        let reg_base = self.reg_base;
        for (i, (expr, _name)) in self.columns.iter().enumerate() {
            emit_returning_expr(b, table, cursor, register_base, expr, reg_base + i as i32)?;
        }
        let rec = b.alloc_reg();
        let rowid_reg = b.alloc_reg();
        b.emit(Opcode::MakeRecord, reg_base, n, rec);
        b.emit(Opcode::NewRowid, self.cursor, rowid_reg, 0);
        b.emit(Opcode::Insert, self.cursor, rec, rowid_reg);
        Ok(())
    }

    /// Emit the result-row loop after the write transaction. This must be placed before the
    /// final `Halt`; it rewinds the ephemeral table and yields each buffered row via `ResultRow`.
    pub fn emit_output_loop(
        &self,
        b: &mut ProgramBuilder,
    ) {
        let n = self.n_col() as i32;
        let end = b.new_label();
        let _rewind = b.emit_jump(Opcode::Rewind, self.cursor, end, 0);
        let loop_top_label = b.new_label();
        b.resolve(loop_top_label);
        for i in 0..n {
            b.emit(Opcode::Column, self.cursor, i, self.reg_base + i);
        }
        b.emit(Opcode::ResultRow, self.reg_base, n, 0);
        b.emit_jump(Opcode::Next, self.cursor, loop_top_label, 0);
        b.resolve(end);
    }

    /// The output column names for the C-API `sqlite3_column_name` path.
    pub fn column_names(&self) -> Vec<String> {
        self.columns.iter().map(|(_, name)| name.clone()).collect()
    }
}

/// Compile a single RETURNING expression.
///
/// Bare column references (including `rowid` for a rowid-alias table) read from the staged
/// register block so they reflect the new/updated values. More complex expressions fall back to
/// `compile_expr` with a register-base context.
    fn emit_returning_expr(
        b: &mut ProgramBuilder,
        table: &Table,
        cursor: i32,
        register_base: i32,
        expr: &Expr,
        target: i32,
    ) -> Result<()> {
        if let Expr::Column {
            schema: None,
            table: None,
            name,
        } = expr
        {
            match table.resolve_column(name) {
                Some(ColumnRef::Rowid) => {
                    if let Some(alias_idx) = table.rowid_alias {
                        b.emit(Opcode::SCopy, register_base + alias_idx as i32, target, 0);
                    } else {
                        b.emit(Opcode::Rowid, cursor, target, 0);
                    }
                    return Ok(());
                }
                Some(ColumnRef::Index(i)) => {
                    b.emit(Opcode::SCopy, register_base + i as i32, target, 0);
                    if table.columns[i].affinity == Affinity::Real {
                        b.emit(Opcode::RealAffinity, target, 0, 0);
                    }
                    return Ok(());
                }
                None => {
                    // Let compile_expr report the error below.
                }
            }
        }

    let ctx = Ctx {
        table,
        cursor,
        register_base: Some(register_base), join_tables: None,
        index_read: None,
        subquery_resolver: None,
        outer: None,
    };
    compile_expr(b, expr, target, ctx)
}

fn column_expr(name: &str) -> Expr {
    Expr::Column {
        schema: None,
        table: None,
        name: name.to_string(),
    }
}

/// Render a RETURNING expression back to a SQL-like text form for default column names. This
/// re-uses the SELECT renderer and is exported for tests / EXPLAIN parity.
pub fn returning_expr_to_text(e: &Expr) -> String {
    expr_to_text(e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{SchemaObject, Table};
    use rustqlite_parser::{parse, Stmt};

    fn table_of(sql: &str) -> Table {
        let ast = parse(sql).unwrap().into_iter().next().unwrap();
        let Stmt::CreateTable(ct) = ast else { panic!("expected CREATE TABLE") };
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

    fn returning(sql: &str) -> Vec<ResultColumn> {
        let ast = parse(sql).unwrap().into_iter().next().unwrap();
        match ast {
            Stmt::Insert(ins) => ins.returning.unwrap(),
            Stmt::Update(upd) => upd.returning.unwrap(),
            Stmt::Delete(del) => del.returning.unwrap(),
            _ => panic!("expected DML with RETURNING"),
        }
    }

    #[test]
    fn star_expands_to_stored_columns() {
        let t = table_of("CREATE TABLE t(a, b, c)");
        let ret = Returning::new(
            &returning("INSERT INTO t VALUES(1,2,3) RETURNING *"),
            &t,
        )
        .unwrap();
        assert_eq!(ret.column_names(), vec!["a", "b", "c"]);
    }

    #[test]
    fn rowid_explicitly_selectable() {
        let t = table_of("CREATE TABLE t(a, b)");
        let ret = Returning::new(
            &returning("INSERT INTO t VALUES(1,2) RETURNING rowid, a"),
            &t,
        )
        .unwrap();
        assert_eq!(ret.column_names(), vec!["rowid", "a"]);
    }
}
