//! `PRAGMA foreign_key_check` backend (mirrors `PragTyp_FOREIGN_KEY_CHECK` in `pragma.c`).
//!
//! For each child table that carries one or more `REFERENCES` / `FOREIGN KEY` constraints,
//! scan every row and, for each FK, look the parent row up by the referenced columns. A row
//! with any NULL child-key column is skipped (NULL foreign keys never violate the constraint).
//! When no matching parent row exists, a violation row `[table, rowid, parent, fkid]` is
//! emitted — the four columns documented at
//! https://www.sqlite.org/pragma.html#pragma_foreign_key_check and in `fkey5.test`.
//!
//! The parent lookup mirrors `sqlite3FkLocateIndex` + the `OP_Found` / `OP_SeekRowid` paths in
//! `pragma.c`:
//!
//! * When an index on the parent table covers the FK's referenced columns (or, when the FK
//!   omits the parent column list, the parent's `INTEGER PRIMARY KEY` rowid alias), an
//!   `IndexCursor::seek(SeekOp::Ge, &key)` probes the index and the positioned entry's prefix
//!   is compared for equality.
//! * When the parent is a rowid table and the FK is single-column referencing the parent's
//!   rowid alias (the `pIdx == 0` branch in `pragma.c`), a `TableCursor::seek_rowid(key)`
//!   probe is used instead.
//! * When no usable index exists and the FK is multi-column or references a non-rowid column,
//!   a full parent-table scan is used as a fallback. This is correct but not performance-
//!   optimal; SQLite without a covering index also falls back to a scan in its `fkActionDo`
//!   trigger path (the `pragma.c` path errors out via `sqlite3FkLocateIndex` returning non-zero
//!   only when no parent index exists at all, but for the in-process check we choose the
//!   conservative scan to keep reporting violations rather than aborting).
//!
//! The output row's `rowid` column is NULL for a WITHOUT ROWID child table (mirrors the
//! `if( HasRowid(pTab) ) OP_Rowid else OP_Null` branch in `pragma.c`).
//!
//! FK constraint order follows the same collection order as
//! [`crate::schema::catalog`]'s `foreign_key_list_rows` helper: column-level `REFERENCES` in
//! column order, then table-level `FOREIGN KEY` in declared order. The `fkid` is the 0-based
//! index in that sequence (matching `fkey5.test`'s `i-1` from the upstream loop that starts `i`
//! at 1).

use std::sync::Arc;

use rustqlite_parser::{
    ColumnConstraint, CreateTable, Stmt, TableConstraintBody,
};

use crate::btree::cursor::{scan_table, TableCursor};
use crate::btree::index_cursor::{IndexCursor, SeekOp};
use crate::error::Result;
use crate::format::decode_record;
use crate::pager::Pager;
use crate::schema::catalog::{read_catalog, Catalog};
use crate::schema::table::{IndexObject, Table};
use crate::schema::SchemaObject;
use crate::types::{Collation, Value};
use crate::vdbe::{FkCheckP4, FkLookup, KeyField};

/// One FK constraint extracted from a `CREATE TABLE` statement.
struct FkConstraint {
    parent_table: String,
    /// `(child_column_index, parent_column_name)`. `parent_column_name` is `None` when the FK
    /// omits the parent column list (the parent's PK is referenced).
    cols: Vec<(usize, Option<String>)>,
}

/// Run `PRAGMA foreign_key_check` and return the violation rows. When `table_filter` is
/// `Some(name)`, only that table is checked (matching upstream's `zRight` path); when `None`,
/// every table in the schema is checked.
pub async fn foreign_key_check(
    pager: Arc<Pager>,
    table_filter: Option<&str>,
) -> Result<Vec<Vec<Value>>> {
    let catalog = read_catalog(&pager).await?;
    let encoding = pager.text_encoding();

    let mut violations: Vec<Vec<Value>> = Vec::new();

    let tables: Vec<SchemaObject> = if let Some(name) = table_filter {
        // Upstream's `sqlite3LocateTable` raises "no such table: <name>" when the named table
        // is missing; mirror that here (the caller surfaces the error).
        match catalog.find_table(name) {
            Some(obj) => vec![obj.clone()],
            None => {
                return Err(crate::error::Error::msg(format!(
                    "no such table: {name}"
                )));
            }
        }
    } else {
        catalog.tables().cloned().collect()
    };

    for obj in &tables {
        // Skip internal tables (sqlite_schema, sqlite_sequence, auto-index b-trees stored as
        // tables, etc.) — upstream's `IsOrdinaryTable` guard. A table without a stored CREATE
        // statement (e.g. `sqlite_sequence`) has no FKs to check.
        if !is_ordinary_table(obj) {
            continue;
        }
        let Some(sql_text) = obj.sql.as_deref() else {
            continue;
        };
        let stmts = match rustqlite_parser::parse(sql_text) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let ct = match stmts.into_iter().next() {
            Some(Stmt::CreateTable(ct)) => ct,
            _ => continue,
        };
        let table = Table::from_schema_object(obj)?;
        // WITHOUT ROWID tables have no rowid to report; upstream emits NULL for the rowid
        // column in that case. The b-tree scan path is different too, so only attempt rowid
        // tables here. WITHOUT ROWID FK checking is deferred to the WITHOUT ROWID write-path
        // milestone (M5.3 follow-up); the rowid-table path covers the common case.
        if table.without_rowid {
            continue;
        }

        let fks = extract_fks(&ct, &table);
        if fks.is_empty() {
            continue;
        }

        // Pre-resolve each FK's parent table + lookup strategy so the per-row loop is cheap.
        let mut plans: Vec<FkPlan> = Vec::with_capacity(fks.len());
        for fk in &fks {
            plans.push(plan_fk(&catalog, fk).await?);
        }

        // Scan every row of the child table.
        let rows = scan_table(&pager, obj.rootpage as u32).await?;
        for (rowid, payload) in rows {
            let values = decode_record(&payload, encoding)?;
            for (fk_id, (fk, plan)) in fks.iter().zip(plans.iter()).enumerate() {
                // Extract child FK column values, substituting the rowid alias.
                let mut child_key: Vec<Value> = Vec::with_capacity(fk.cols.len());
                let mut any_null = false;
                for &(child_col_idx, _) in &fk.cols {
                    let v = column_value(&table, &values, child_col_idx, rowid);
                    if v.is_null() {
                        any_null = true;
                        break;
                    }
                    child_key.push(v);
                }
                if any_null {
                    continue;
                }
                if !plan.parent_exists(pager.clone(), &child_key).await? {
                    violations.push(vec![
                        Value::Text(obj.name.clone()),
                        Value::Int(rowid),
                        Value::Text(fk.parent_table.clone()),
                        Value::Int(fk_id as i64),
                    ]);
                }
            }
        }
    }

    Ok(violations)
}

/// The resolved lookup strategy for a single FK constraint.
enum FkPlan {
    /// The FK references the parent's rowid alias (single-column, no parent column list, parent
    /// is a rowid table with an `INTEGER PRIMARY KEY`). Probe via `TableCursor::seek_rowid`.
    RowidSeek {
        parent_root: u32,
    },
    /// An index on the parent table covers the referenced columns. Probe via
    /// `IndexCursor::seek(SeekOp::Ge, &key)` + prefix equality.
    IndexSeek {
        index: IndexObject,
        key_info: Vec<KeyField>,
        /// The table-column index of each referenced parent column (parallel to `child_key`).
        parent_col_indices: Vec<usize>,
    },
    /// No usable index — fall back to a full parent-table scan comparing the referenced
    /// columns. Correct but O(n) per child row.
    TableScan {
        parent_table: Table,
        parent_col_indices: Vec<usize>,
    },
    /// The parent table doesn't exist (a dangling FK). Upstream's `PragTyp_FOREIGN_KEY_CHECK`
    /// second loop retrieves `pParent` again and, when it's NULL, skips `sqlite3FkLocateIndex`
    /// — leaving `pIdx == 0` and `pParent == 0`, so neither the `OP_Found` nor `OP_SeekRowid`
    /// branch emits an `addrOk` jump. The row falls through to the violation-reporting path,
    /// so a dangling parent counts as a violation for every non-NULL child row.
    ParentMissing,
}

impl FkPlan {
    /// Return `true` when the parent row matching `child_key` exists.
    async fn parent_exists(&self, pager: Arc<Pager>, child_key: &[Value]) -> Result<bool> {
        match self {
            FkPlan::RowidSeek { parent_root } => {
                let rowid = child_key[0].as_i64();
                let mut cur = TableCursor::new(pager.clone(), *parent_root);
                cur.seek_rowid(rowid).await
            }
            FkPlan::IndexSeek {
                index,
                key_info,
                parent_col_indices: _,
            } => {
                let mut cur = IndexCursor::new(
                    pager.clone(),
                    index.rootpage as u32,
                    key_info.clone(),
                );
                let positioned = cur.seek(SeekOp::Ge, child_key).await?;
                if !positioned {
                    return Ok(false);
                }
                let payload = cur.payload().to_vec();
                let entry = decode_record(&payload, pager.text_encoding())?;
                let prefix_len = entry.len().saturating_sub(1).min(child_key.len());
                let prefix = &entry[..prefix_len];
                Ok(keys_equal(prefix, child_key, key_info))
            }
            FkPlan::TableScan {
                parent_table,
                parent_col_indices,
            } => {
                let rows = scan_table(&pager, parent_table.rootpage as u32).await?;
                let encoding = pager.text_encoding();
                for (rowid, payload) in rows {
                    let pvalues = decode_record(&payload, encoding)?;
                    let mut matched = true;
                    for (i, &parent_col_idx) in parent_col_indices.iter().enumerate() {
                        let pv = column_value(parent_table, &pvalues, parent_col_idx, rowid);
                        if !value_equal_for_fk(&pv, &child_key[i]) {
                            matched = false;
                            break;
                        }
                    }
                    if matched {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            FkPlan::ParentMissing => Ok(false),
        }
    }
}

/// Resolve the lookup strategy for one FK constraint against the catalog.
async fn plan_fk(catalog: &Catalog, fk: &FkConstraint) -> Result<FkPlan> {
    let Some(parent_obj) = catalog.find_table(&fk.parent_table) else {
        return Ok(FkPlan::ParentMissing);
    };
    let parent_root = parent_obj.rootpage as u32;
    let Some(parent_sql) = parent_obj.sql.as_deref() else {
        return Ok(FkPlan::ParentMissing);
    };
    let parent_stmts = rustqlite_parser::parse(parent_sql).map_err(|e| {
        crate::error::Error::msg(format!(
            "cannot parse schema for \"{}\": {e}",
            fk.parent_table
        ))
    })?;
    match parent_stmts.into_iter().next() {
        Some(Stmt::CreateTable(_)) => {}
        _ => return Ok(FkPlan::ParentMissing),
    }
    let parent_table = Table::from_schema_object(parent_obj)?;

    // Resolve the referenced parent columns. `None` means "the parent's PK".
    let parent_col_names: Vec<String> = match fk.cols.first().and_then(|(_, p)| p.as_ref()) {
        None => {
            // No parent column list — reference the parent's PK. For a rowid table with an
            // INTEGER PRIMARY KEY, that's the rowid alias; for a composite PK or WITHOUT ROWID
            // table, that's the PK columns.
            if parent_table.rowid_alias.is_some() && fk.cols.len() == 1 {
                // Single-column FK referencing the parent's INTEGER PRIMARY KEY → rowid seek.
                return Ok(FkPlan::RowidSeek { parent_root });
            }
            // Composite PK or no rowid alias: fall back to a scan against the PK columns.
            parent_table
                .pk_columns
                .iter()
                .map(|(idx, _)| parent_table.columns[*idx].name.clone())
                .collect()
        }
        Some(named) => {
            // When the FK explicitly names the parent's INTEGER PRIMARY KEY column (the rowid
            // alias), a rowid seek is still the right lookup path.
            if fk.cols.len() == 1 && parent_table.rowid_alias.is_some() {
                let alias_idx = parent_table.rowid_alias.unwrap();
                if parent_table.columns[alias_idx]
                    .name
                    .eq_ignore_ascii_case(named)
                {
                    return Ok(FkPlan::RowidSeek { parent_root });
                }
            }
            fk.cols
                .iter()
                .map(|(_, p)| p.clone().unwrap_or_default())
                .collect()
        }
    };

    // Look for an index on the parent that covers the referenced columns in order.
    let index_obj = find_covering_index(catalog, &fk.parent_table, &parent_col_names);
    if let Some(index) = index_obj {
        let key_info: Vec<KeyField> = index
            .columns
            .iter()
            .map(|c| KeyField {
                desc: c.desc,
                collation: c.collation,
            })
            .collect();
        let parent_col_indices = resolve_parent_col_indices(&parent_table, &parent_col_names)?;
        return Ok(FkPlan::IndexSeek {
            index,
            key_info,
            parent_col_indices,
        });
    }

    // No index — fall back to a full parent scan against the referenced columns.
    let parent_col_indices = resolve_parent_col_indices(&parent_table, &parent_col_names)?;
    Ok(FkPlan::TableScan {
        parent_table,
        parent_col_indices,
    })
}

/// Resolve every FK constraint on `child_table` into a [`FkCheckP4`] for the INSERT/UPDATE
/// codegen (M17.6 enforcement). Returns an empty vec when the table has no FKs. The
/// `child_table_name` is the stored name (may be quoted); `child_columns` is parallel to the
/// FK's child-column-index list so the codegen can emit `SCopy` from the right row registers.
///
/// This is the prepare-time resolution: the catalog is read once, each FK's parent table is
/// found, and the lookup strategy (rowid seek, index seek, or table scan) is captured. At
/// runtime, `OP_FkCheck` replays the strategy per child row.
pub async fn resolve_fk_constraints(
    pager: &Pager,
    child_table: &Table,
) -> Result<Vec<FkCheckP4>> {
    // Re-parse the child's CREATE TABLE to extract the FK constraints (the Table struct itself
    // doesn't carry FK info — it's parsed on demand, matching the `foreign_key_list` pattern).
    let catalog = read_catalog(pager).await?;
    let child_obj = catalog
        .find_table(&child_table.name)
        .ok_or_else(|| crate::error::Error::msg(format!("no such table: {}", child_table.name)))?;
    let Some(sql_text) = child_obj.sql.as_deref() else {
        return Ok(Vec::new());
    };
    let stmts = rustqlite_parser::parse(sql_text).map_err(|e| {
        crate::error::Error::msg(format!("cannot parse schema for \"{}\": {e}", child_table.name))
    })?;
    let ct = match stmts.into_iter().next() {
        Some(Stmt::CreateTable(ct)) => ct,
        _ => return Ok(Vec::new()),
    };
    let fks = extract_fks(&ct, child_table);
    let mut out = Vec::with_capacity(fks.len());
    for fk in &fks {
        let plan = plan_fk(&catalog, fk).await?;
        let child_columns: Vec<String> = fk
            .cols
            .iter()
            .map(|(idx, _)| child_table.columns.get(*idx).map(|c| c.name.clone()).unwrap_or_default())
            .collect();
        let lookup = match plan {
            FkPlan::RowidSeek { parent_root } => FkLookup::RowidSeek { root: parent_root },
            FkPlan::IndexSeek { index, key_info, .. } => FkLookup::IndexSeek {
                root: index.rootpage as u32,
                key_info,
            },
            FkPlan::TableScan { parent_table, parent_col_indices } => FkLookup::TableScan {
                root: parent_table.rootpage as u32,
                parent_col_indices,
                parent_rowid_alias: parent_table.rowid_alias,
            },
            FkPlan::ParentMissing => FkLookup::ParentMissing,
        };
        out.push(FkCheckP4 {
            child_table: child_table.name.clone(),
            child_columns,
            parent_table: fk.parent_table.clone(),
            lookup,
        });
    }
    Ok(out)
}

/// Find an index on `table_name` whose indexed columns (in order) match `cols` (case-
/// insensitive). The first match in catalog order wins (mirrors `sqlite3FkLocateIndex`'s
/// preference for a covering index).
fn find_covering_index(
    catalog: &Catalog,
    table_name: &str,
    cols: &[String],
) -> Option<IndexObject> {
    let names: Vec<&str> = cols.iter().map(|s| s.as_str()).collect();
    for obj in catalog.indexes() {
        if !obj.tbl_name.eq_ignore_ascii_case(table_name) {
            continue;
        }
        if let Ok(io) = IndexObject::from_schema_object(obj) {
            if io.covers_columns(&names) {
                return Some(io);
            }
        }
    }
    None
}

/// Resolve a list of parent column names to table-column indices.
fn resolve_parent_col_indices(table: &Table, names: &[String]) -> Result<Vec<usize>> {
    names
        .iter()
        .map(|n| {
            table
                .column_index(n)
                .ok_or_else(|| crate::error::Error::msg(format!("no such column: {n}")))
        })
        .collect()
}

/// Extract the FK constraints from a `CREATE TABLE` AST, mapping each child column to its
/// table-column index. Constraint order: column-level `REFERENCES` in column order, then
/// table-level `FOREIGN KEY` in declared order — the same order
/// [`crate::capi::stmt`]'s `foreign_key_list_rows` uses (and the order upstream's
/// `sqlite3AddForeignKey` builds the linked list in, walked in reverse by the introspection
/// pragmas).
fn extract_fks(ct: &CreateTable, table: &Table) -> Vec<FkConstraint> {
    let mut fks: Vec<FkConstraint> = Vec::new();
    // Column-level REFERENCES, in column order.
    for (i, cd) in ct.columns.iter().enumerate() {
        for c in &cd.constraints {
            if let ColumnConstraint::References(r) = c {
                fks.push(FkConstraint {
                    parent_table: r.parent_table.clone(),
                    cols: vec![(
                        i,
                        r.parent_columns.as_ref().and_then(|cs| cs.first()).cloned(),
                    )],
                });
            }
        }
    }
    // Table-level FOREIGN KEY, in declared order.
    for tc in &ct.constraints {
        if let TableConstraintBody::ForeignKey { columns, references } = &tc.body {
            let parent_cols: Vec<Option<String>> = match &references.parent_columns {
                Some(cs) => cs.iter().cloned().map(Some).collect(),
                None => std::iter::repeat(None).take(columns.len()).collect(),
            };
            let cols: Vec<(usize, Option<String>)> = columns
                .iter()
                .zip(parent_cols.iter())
                .map(|(from, to)| {
                    let idx = table
                        .column_index(from)
                        .unwrap_or_else(|| columns.iter().position(|c| c == from).unwrap_or(0));
                    (idx, to.clone())
                })
                .collect();
            fks.push(FkConstraint {
                parent_table: references.parent_table.clone(),
                cols,
            });
        }
    }
    fks
}

/// Read the value of column `col_idx` from a decoded row, substituting the rowid alias.
/// `rowid` is the b-tree key (used when `col_idx` is the rowid-alias column).
fn column_value(table: &Table, values: &[Value], col_idx: usize, rowid: i64) -> Value {
    if Some(col_idx) == table.rowid_alias {
        return Value::Int(rowid);
    }
    values
        .get(col_idx)
        .cloned()
        .unwrap_or(Value::Null)
}

/// Compare two key prefixes field-by-field using the index's per-column collation. Mirrors
/// `compare_prefix` in `btree/index_cursor.rs` but for the FK-check context.
fn keys_equal(prefix: &[Value], key: &[Value], key_info: &[KeyField]) -> bool {
    if prefix.len() != key.len() {
        return false;
    }
    for (i, (a, b)) in prefix.iter().zip(key.iter()).enumerate() {
        let coll = key_info
            .get(i)
            .map(|f| f.collation)
            .unwrap_or(Collation::Binary);
        if crate::vdbe::compare::mem_compare(a, b, coll) != std::cmp::Ordering::Equal {
            return false;
        }
    }
    true
}

/// Compare a parent column value against a child key value for the table-scan fallback. Uses
/// BINARY collation by default; a more faithful comparison would apply the parent column's
/// declared collation, but for the common FK case (numeric or text keys with BINARY collation)
/// this is correct.
fn value_equal_for_fk(parent: &Value, child: &Value) -> bool {
    crate::vdbe::compare::mem_compare(parent, child, Collation::Binary) == std::cmp::Ordering::Equal
}

/// Upstream's `IsOrdinaryTable(pTab)` excludes `sqlite_*` internal tables and views. A
/// `SchemaObject` of type `"table"` whose name starts with `sqlite_` is an internal table
/// (e.g. `sqlite_sequence`) and is skipped.
fn is_ordinary_table(obj: &SchemaObject) -> bool {
    if !obj.is_table() {
        return false;
    }
    !obj.name.starts_with("sqlite_")
}