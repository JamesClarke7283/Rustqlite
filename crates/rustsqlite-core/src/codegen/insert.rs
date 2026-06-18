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

use rustqlite_parser::{Expr, InsertStmt};

use crate::error::{Error, Result};
use crate::schema::{IndexObject, Table};
use crate::types::Affinity;
use crate::vdbe::program::{Program, P4, P5_NCHANGE};
use crate::vdbe::Opcode;

use super::builder::ProgramBuilder;
use super::expr::{compile_expr, Ctx};

/// Compile an `INSERT INTO <table> VALUES (...)[, (...)]` statement. `indexes` is the list of
/// indexes attached to `table` (the prepare path passes this from the catalog; an empty slice
/// means "no indexes", matching the M3a behavior). Index maintenance is emitted per row per
/// index.
pub fn compile_insert(ins: &InsertStmt, table: &Table, indexes: &[IndexObject]) -> Result<Program> {
    if ins.rows.is_empty() {
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

    // Validate that every index's columns are present on the table. The prepare path is
    // responsible for this (it built the IndexObject from the catalog), so any error here is a
    // bug in the loader — we still surface a clear message rather than panic.
    for idx in indexes {
        for ic in &idx.columns {
            if table.column_index(&ic.name).is_none() {
                return Err(Error::msg(format!(
                    "index {} references unknown column {} on table {}",
                    idx.name, ic.name, table.name
                )));
            }
        }
    }

    let cursor = 0i32;
    let ctx = Ctx { table, cursor };
    let mut b = ProgramBuilder::new();

    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0); // addr 0
    let after_init = b.cur_addr();

    b.emit(Opcode::Transaction, 0, 1, 0); // open the write transaction
    b.emit(Opcode::OpenWrite, cursor, table.rootpage as i32, 0);

    // Reserve cursor numbers for the indexes (1, 2, …). The table cursor is 0. Each index
    // cursor carries the index's KeyInfo so inserts compare under the correct collation.
    let index_cursor_base: i32 = 1;
    for (i, idx) in indexes.iter().enumerate() {
        let ic = (index_cursor_base + i as i32) as i32;
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

    for row in &ins.rows {
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

        let record = b.alloc_reg();
        b.emit(Opcode::MakeRecord, rec_start, ncol as i32, record);
        b.emit(Opcode::Insert, cursor, record, rowid_reg);

        // Index maintenance: for each index, build the composite key record (indexed columns +
        // trailing rowid), MakeRecord it, IdxInsert into the index cursor.
        for (i, idx) in indexes.iter().enumerate() {
            let ic = (index_cursor_base + i as i32) as i32;
            let indexed_cis = idx.table_column_indices(table)?;
            let nkey = indexed_cis.len() as i32 + 1;
            let key_start = b.alloc_regs(nkey);
            for (j, col_idx) in indexed_cis.iter().enumerate() {
                b.emit(
                    Opcode::SCopy,
                    rec_start + *col_idx as i32,
                    key_start + j as i32,
                    0,
                );
            }
            b.emit(
                Opcode::SCopy,
                rowid_reg,
                key_start + indexed_cis.len() as i32,
                0,
            );
            let key_rec = b.alloc_reg();
            b.emit(Opcode::MakeRecord, key_start, nkey, key_rec);
            let ins_idx = b.emit(Opcode::IdxInsert, ic, key_rec, 0);
            b.set_p4(ins_idx, P4::Int(0)); // nMem = 0
            b.set_p5(ins_idx, P5_NCHANGE);
        }
    }

    b.emit(Opcode::Halt, 0, 0, 0); // commits the write transaction

    b.resolve(setup);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
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
        let prog = compile_insert(&ins, &t, &[]).unwrap();
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
        let prog = compile_insert(&ins, &t, &[]).unwrap();
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
        let prog = compile_insert(&ins, &t, &[]).unwrap();
        // 3 record slots are allocated per row; the unlisted column c is NULL.
        let null_count = prog
            .instructions
            .iter()
            .filter(|i| i.opcode == Opcode::Null)
            .count();
        assert!(null_count >= 1, "unlisted column should load NULL");
    }
}
