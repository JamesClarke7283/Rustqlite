//! The schema-aware table model used by the code generator and the C-API column accessors
//! (mirrors the `Table`/`Column` structures built in `build.c`).
//!
//! A [`Table`] is derived from a `sqlite_schema` row by parsing its stored `CREATE TABLE` text
//! and resolving each column's affinity, collation, and constraints — plus detecting the
//! INTEGER-PRIMARY-KEY rowid alias, which codegen reads from the cell rowid (an `Rowid` opcode)
//! rather than the record body (a `Column` opcode), because it is stored as NULL on disk.
//!
//! M5.1 adds [`IndexObject`] — the analogous view of a `sqlite_schema` index row, derived
//! from its stored `CREATE INDEX` text. The codegen and the planner read the indexed columns
//! from this struct; the page-level index b-tree is rooted at `rootpage`.

use rustqlite_parser::{parse, ColumnConstraint, CreateIndex, CreateTable, Stmt};

use crate::error::{Error, Result};
use crate::schema::SchemaObject;
use crate::types::{affinity_of, Affinity, Collation};

/// One column of a table.
#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub name: String,
    pub affinity: Affinity,
    pub collation: Collation,
    pub notnull: bool,
    pub pk: bool,
}

/// A resolved table: its name, root b-tree page, columns, and (if any) the column that aliases
/// the rowid.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    pub name: String,
    pub rootpage: i64,
    pub columns: Vec<Column>,
    /// Index into `columns` of the `INTEGER PRIMARY KEY` rowid-alias column, if there is one.
    pub rowid_alias: Option<usize>,
}

/// A resolved index: the table it indexes, the b-tree root page, the indexed columns, and
/// whether `UNIQUE` was declared. From M5.2 onward multi-column indexes are accepted; the
/// `unique` flag is recorded but uniqueness is not enforced yet (see the milestone doc-comment).
#[derive(Clone, Debug, PartialEq)]
pub struct IndexObject {
    pub name: String,
    pub table: String,
    pub rootpage: i64,
    pub columns: Vec<IndexedColumn>,
    pub unique: bool,
    /// `true` when the index is a `UNIQUE` index and every indexed column is `NOT NULL`.
    /// This mirrors SQLite's `Index.uniqNotNull`: uniqueness is only enforced on rows where
    /// none of the key columns are NULL (NULL != NULL in SQL).
    pub unique_not_null: bool,
    /// Optional partial-index predicate (`WHERE expr`). From M5.2.9 onward.
    pub where_clause: Option<rustqlite_parser::Expr>,
}

/// One column entry in an `IndexObject`. The M5.2 runtime uses `name` to map plain columns back
/// to the table; `expr` carries the AST for expression-index keys. The per-column `collation`
/// is the resolved comparison rule used by the index cursor.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexedColumn {
    pub name: String,
    /// For expression indexes, the parsed expression; `None` for a plain column index.
    pub expr: Option<rustqlite_parser::Expr>,
    pub collation: Collation,
    pub desc: bool,
}

impl IndexedColumn {
    /// True when this indexed key is a real expression rather than a plain column reference.
    /// A bare column (including one wrapped in `COLLATE`) is *not* considered an expression
    /// here, even though it is stored as an `Expr::Column`/`Expr::Collate`, because downstream
    /// code still needs to map it to a table column by name for error messages and key-info.
    pub fn is_expression(&self) -> bool {
        if self.expr.is_none() {
            return false;
        }
        // A bare column reference keeps the column name in `name` and is stored as an
        // `Expr::Column`. A `COLLATE`-wrapped column is still a plain column for this purpose;
        // the per-column collation lives in `IndexedColumn::collation`. Real expression
        // indexes have an empty `name` (the parser cannot derive a single column name from
        // an arbitrary expression).
        self.name.is_empty()
    }
}

impl IndexObject {
    /// Map each plain indexed column to its table column index. Expression-index keys are
    /// skipped (the caller must evaluate the expression itself); returns `Ok(indices)` when
    /// all plain columns exist, otherwise an error naming the missing column.
    pub fn table_column_indices(&self, table: &Table) -> Result<Vec<usize>> {
        let mut out = Vec::with_capacity(self.columns.len());
        for ic in &self.columns {
            if ic.is_expression() {
                continue;
            }
            let idx = table.column_index(&ic.name).ok_or_else(|| {
                Error::msg(format!(
                    "index {} references unknown column {} on table {}",
                    self.name, ic.name, table.name
                ))
            })?;
            out.push(idx);
        }
        Ok(out)
    }

    /// The number of indexed key fields (columns or expressions) in this index.
    pub fn nkey_fields(&self) -> usize {
        self.columns.len()
    }
}

/// How a column reference resolves against a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnRef {
    /// The rowid (an `INTEGER PRIMARY KEY` alias, or one of the magic names): use `Rowid`.
    Rowid,
    /// An ordinary stored column at this index: use `Column`.
    Index(usize),
}

impl Table {
    /// Build a [`Table`] from a `sqlite_schema` row by parsing its `CREATE TABLE` text.
    pub fn from_schema_object(obj: &SchemaObject) -> Result<Table> {
        let sql = obj
            .sql
            .as_deref()
            .ok_or_else(|| Error::msg(format!("table \"{}\" has no CREATE statement", obj.name)))?;
        // The current grammar does not parse `WITHOUT ROWID`; give a precise unsupported error
        // rather than a confusing parse failure.
        if sql.to_ascii_uppercase().contains("WITHOUT ROWID") {
            return Err(Error::msg(format!(
                "WITHOUT ROWID tables are not supported yet (table \"{}\")",
                obj.name
            )));
        }
        let stmts = parse(sql)
            .map_err(|e| Error::msg(format!("cannot parse schema for \"{}\": {e}", obj.name)))?;
        let ct = match stmts.into_iter().next() {
            Some(Stmt::CreateTable(ct)) => ct,
            _ => {
                return Err(Error::msg(format!(
                    "schema object \"{}\" is not a CREATE TABLE this build can model",
                    obj.name
                )))
            }
        };
        Ok(Table::from_create(&ct, obj.rootpage))
    }

    fn from_create(ct: &CreateTable, rootpage: i64) -> Table {
        let mut columns = Vec::with_capacity(ct.columns.len());
        // Track which columns carry a column-level PRIMARY KEY (with its ASC/DESC).
        let mut pk_cols: Vec<(usize, bool)> = Vec::new();

        for (i, cd) in ct.columns.iter().enumerate() {
            let affinity = affinity_of(cd.type_name.as_deref());
            let mut notnull = false;
            let mut pk = false;
            for c in &cd.constraints {
                match c {
                    ColumnConstraint::NotNull => notnull = true,
                    ColumnConstraint::PrimaryKey { desc, .. } => {
                        pk = true;
                        pk_cols.push((i, *desc));
                    }
                    _ => {}
                }
            }
            columns.push(Column {
                name: cd.name.clone(),
                affinity,
                collation: Collation::Binary,
                notnull,
                pk,
            });
        }

        // The rowid alias is a *single-column* PRIMARY KEY whose declared type is exactly
        // "INTEGER" (not "INT") and which is ASC. AUTOINCREMENT is allowed.
        let rowid_alias = if pk_cols.len() == 1 {
            let (idx, desc) = pk_cols[0];
            let is_integer = ct.columns[idx]
                .type_name
                .as_deref()
                .is_some_and(|t| t.trim().eq_ignore_ascii_case("INTEGER"));
            (is_integer && !desc).then_some(idx)
        } else {
            None
        };

        Table {
            name: ct.name.clone(),
            rootpage,
            columns,
            rowid_alias,
        }
    }

    /// The index of a column by name (case-insensitive), if it exists.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// Resolve a bare column name to either the rowid or a stored column index, applying the
    /// rowid-alias rule and the magic names `rowid`/`_rowid_`/`oid` (which name the rowid only
    /// when no real column shadows them).
    pub fn resolve_column(&self, name: &str) -> Option<ColumnRef> {
        if let Some(i) = self.column_index(name) {
            if Some(i) == self.rowid_alias {
                return Some(ColumnRef::Rowid);
            }
            return Some(ColumnRef::Index(i));
        }
        if is_rowid_name(name) {
            return Some(ColumnRef::Rowid);
        }
        None
    }
}

impl IndexObject {
    /// Build an `IndexObject` from a `sqlite_schema` row by parsing its `CREATE INDEX` text.
    /// Returns an error when the stored SQL is missing, doesn't parse, or doesn't reduce to
    /// a `CREATE INDEX` statement.
    ///
    /// The optional `catalog` argument lets the loader supply the parent table so the
    /// `unique_not_null` flag can be computed from the indexed columns' NOT NULL status.
    pub fn from_schema_object(obj: &SchemaObject) -> Result<IndexObject> {
        IndexObject::from_schema_object_with_catalog(obj, None)
    }

    /// Build an `IndexObject` from a `sqlite_schema` row, optionally using the surrounding
    /// catalog to resolve the parent table for NOT NULL status.
    pub fn from_schema_object_with_catalog(
        obj: &SchemaObject,
        catalog: Option<&crate::schema::Catalog>,
    ) -> Result<IndexObject> {
        let sql = obj
            .sql
            .as_deref()
            .ok_or_else(|| Error::msg(format!("index \"{}\" has no CREATE statement", obj.name)))?;
        let stmts = parse(sql).map_err(|e| {
            Error::msg(format!(
                "cannot parse schema for index \"{}\": {e}",
                obj.name
            ))
        })?;
        let ci = match stmts.into_iter().next() {
            Some(Stmt::CreateIndex(ci)) => ci,
            _ => {
                return Err(Error::msg(format!(
                    "schema object \"{}\" is not a CREATE INDEX this build can model",
                    obj.name
                )))
            }
        };
        // The table object is needed to compute `unique_not_null` (all indexed columns NOT NULL).
        // The catalog loader already has the table resolved when it builds the index list, but
        // standalone callers (tests) may not. Build a throwaway table from the stored CREATE TABLE
        // SQL when available; otherwise fall back to a table with no not-null information.
        let table = catalog
            .as_ref()
            .and_then(|cat| cat.find_table(&ci.table))
            .and_then(|t| t.sql.as_deref())
            .and_then(|sql| {
                Table::from_schema_object(&SchemaObject {
                    rowid: 0,
                    obj_type: "table".to_string(),
                    name: ci.table.clone(),
                    tbl_name: ci.table.clone(),
                    rootpage: 0,
                    sql: Some(sql.to_string()),
                })
                .ok()
            })
            .unwrap_or_else(|| Table {
                name: ci.table.clone(),
                rootpage: 0,
                columns: Vec::new(),
                rowid_alias: None,
            });
        Ok(IndexObject::from_create(
            &ci,
            obj.rootpage,
            obj.name.clone(),
            &table,
        ))
    }

    fn from_create(ci: &CreateIndex, rootpage: i64, name: String, table: &Table) -> IndexObject {
        let columns: Vec<IndexedColumn> = ci
            .columns
            .iter()
            .map(|c| IndexedColumn {
                name: c.name.clone(),
                expr: c.expr.clone(),
                collation: c
                    .collation
                    .as_deref()
                    .and_then(Collation::from_name)
                    .unwrap_or(Collation::Binary),
                desc: c.desc,
            })
            .collect();
        let unique_not_null = ci.unique
            && columns.iter().all(|ic| {
                !ic.is_expression()
                    && table
                        .column_index(&ic.name)
                        .map(|idx| table.columns[idx].notnull)
                        .unwrap_or(false)
            });
        IndexObject {
            name,
            table: ci.table.clone(),
            rootpage,
            columns,
            unique: ci.unique,
            unique_not_null,
            where_clause: ci.where_clause.clone(),
        }
    }

    /// The first indexed column, if any. Multi-column indexes use `table_column_indices`.
    pub fn first_column(&self) -> Option<&IndexedColumn> {
        self.columns.first()
    }

    /// True when this index covers exactly the given columns (case-insensitive) in order.
    /// Expression indexes are never matched by this helper.
    pub fn covers_columns(&self, names: &[&str]) -> bool {
        if self.columns.len() != names.len() {
            return false;
        }
        self.columns
            .iter()
            .zip(names.iter())
            .all(|(ic, name)| !ic.is_expression() && ic.name.eq_ignore_ascii_case(name))
    }

    /// The "UNIQUE constraint failed: ..." message used by C SQLite for this index.
    /// Returns `None` when the index is not unique.
    ///
    /// C SQLite uses two forms:
    ///   * plain-column indexes: `UNIQUE constraint failed: table.col1, table.col2, ...`
    ///   * expression indexes (any key is an expression): `UNIQUE constraint failed: index 'name'`
    pub fn unique_constraint_message(&self, table: &Table) -> Option<String> {
        if !self.unique {
            return None;
        }
        let has_expression = self.columns.iter().any(|ic| ic.is_expression());
        if has_expression {
            return Some(format!("UNIQUE constraint failed: index '{}'", self.name));
        }
        let names: Vec<String> = self
            .columns
            .iter()
            .map(|ic| format!("{}.{}", table.name, ic.name))
            .collect();
        Some(format!("UNIQUE constraint failed: {}", names.join(", ")))
    }

    /// Build an `IndexObject` from a parsed `CREATE INDEX` and a resolved parent table.
    /// Used by `compile_create_index` and tests when the table is already known.
    pub fn from_create_and_table(ci: &CreateIndex, rootpage: i64, table: &Table) -> IndexObject {
        IndexObject::from_create(ci, rootpage, ci.name.clone(), table)
    }
}

/// Whether `name` is one of SQLite's magic rowid names (case-insensitive).
pub fn is_rowid_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("rowid")
        || name.eq_ignore_ascii_case("_rowid_")
        || name.eq_ignore_ascii_case("oid")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_from_sql(sql: &str) -> Table {
        let obj = SchemaObject {
            rowid: 1,
            obj_type: "table".into(),
            name: "t".into(),
            tbl_name: "t".into(),
            rootpage: 2,
            sql: Some(sql.into()),
        };
        Table::from_schema_object(&obj).unwrap()
    }

    #[test]
    fn affinities_and_columns() {
        let t = table_from_sql("CREATE TABLE t(a INTEGER, b TEXT, c REAL, d, e VARCHAR(10))");
        assert_eq!(t.columns.len(), 5);
        assert_eq!(t.columns[0].affinity, Affinity::Integer);
        assert_eq!(t.columns[1].affinity, Affinity::Text);
        assert_eq!(t.columns[2].affinity, Affinity::Real);
        assert_eq!(t.columns[3].affinity, Affinity::Blob); // typeless → BLOB
        assert_eq!(t.columns[4].affinity, Affinity::Text);
        assert_eq!(t.rowid_alias, None);
    }

    #[test]
    fn integer_primary_key_is_rowid_alias() {
        let t = table_from_sql("CREATE TABLE t(id INTEGER PRIMARY KEY, x)");
        assert_eq!(t.rowid_alias, Some(0));
        assert_eq!(t.resolve_column("id"), Some(ColumnRef::Rowid));
        assert_eq!(t.resolve_column("x"), Some(ColumnRef::Index(1)));
        assert_eq!(t.resolve_column("rowid"), Some(ColumnRef::Rowid));
        assert_eq!(t.resolve_column("_rowid_"), Some(ColumnRef::Rowid));
        assert_eq!(t.resolve_column("nope"), None);
    }

    #[test]
    fn int_primary_key_is_not_rowid_alias() {
        // "INT" (not "INTEGER") PRIMARY KEY is a normal integer-affinity PK, stored in the row.
        let t = table_from_sql("CREATE TABLE t(id INT PRIMARY KEY, x)");
        assert_eq!(t.rowid_alias, None);
        assert_eq!(t.resolve_column("id"), Some(ColumnRef::Index(0)));
        // The magic name still resolves to the rowid.
        assert_eq!(t.resolve_column("rowid"), Some(ColumnRef::Rowid));
    }

    #[test]
    fn real_column_named_rowid_shadows_magic_name() {
        let t = table_from_sql("CREATE TABLE t(rowid TEXT, x)");
        assert_eq!(t.resolve_column("rowid"), Some(ColumnRef::Index(0)));
    }

    #[test]
    fn without_rowid_is_unsupported() {
        let obj = SchemaObject {
            rowid: 1,
            obj_type: "table".into(),
            name: "t".into(),
            tbl_name: "t".into(),
            rootpage: 2,
            sql: Some("CREATE TABLE t(a PRIMARY KEY, b) WITHOUT ROWID".into()),
        };
        assert!(Table::from_schema_object(&obj).is_err());
    }
}
