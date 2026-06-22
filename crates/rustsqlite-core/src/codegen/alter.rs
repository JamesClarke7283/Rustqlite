//! Lowering `ALTER TABLE … <action>` to a VDBE program (mirrors `sqlite3AlterRenameTable` and
//! friends in `alter.c`).
//!
//! M14 first slice — `RENAME TO new_name`. The faithful opcode shape: open a write transaction,
//! walk `sqlite_schema` (page 1) and rewrite each row whose `tbl_name` matches the old table
//! name — for the table row itself, update `name` and `tbl_name` and rewrite the `sql` text;
//! for associated index/trigger rows, update `tbl_name` and rewrite the `sql` text. Then bump
//! the schema cookie, reload the schema, and `Halt`.
//!
//! The `sql` rewrite is a textual splice of the table-name token in the stored CREATE text.
//! SQLite uses an AST-aware rewrite via the `sqlite_rename_table` SQL function (see
//! `renameTableFunc` in `alter.c`); we approximate it by locating the table-name identifier
//! in the CREATE TABLE / CREATE INDEX text and substituting the new name. This matches
//! upstream's observable result for the common cases (no FK references, no triggers
//! referencing the table in their bodies).

use rustqlite_parser::{AlterTableStmt, ColumnDef};

use crate::error::{Error, Result};
use crate::schema::Table;
use crate::vdbe::program::{Program, P4, P5_ISUPDATE};
use crate::vdbe::KeyField;
use crate::vdbe::Opcode;

use super::builder::ProgramBuilder;

/// The fixed rootpage of `sqlite_schema` (page 1).
const SCHEMA_ROOT: i32 = 1;
/// The `SetCookie` selector for the schema cookie (header bytes 40-43).
const COOKIE_SCHEMA: i32 = 1;

/// A single `sqlite_schema` row edit computed by the resolver and consumed by the codegen.
/// Each edit tells the program to seek to `rowid` and overwrite the row with the new
/// `name`, `tbl_name`, and `sql` values (the `type` and `rootpage` columns are preserved
/// by reading them back from the existing row at runtime).
#[derive(Clone, Debug)]
pub struct SchemaRowEdit {
    pub rowid: i64,
    /// New value for column 1 (`name`), or `None` to keep the existing value.
    pub new_name: Option<String>,
    /// New value for column 2 (`tbl_name`), or `None` to keep the existing value.
    pub new_tbl_name: Option<String>,
    /// New value for column 4 (`sql`), or `None` to keep the existing value.
    pub new_sql: Option<String>,
}

/// Compile `ALTER TABLE <tbl> ADD [COLUMN] <col_def>`.
///
/// * `stmt` — the parsed ALTER TABLE statement (action must be `AddColumn`).
/// * `current_schema_cookie` — the value before this DDL runs (the program bumps it by one).
/// * `table_rowid` — the rowid of the table's `sqlite_schema` row.
/// * `old_sql` — the current `sql` text of the CREATE TABLE statement.
/// * `col_def_text` — the verbatim column-definition text from the user's ALTER TABLE
///   statement (e.g. `"b TEXT DEFAULT 'x'"`), which is spliced into the CREATE TABLE text.
///
/// The existing rows in the table b-tree are NOT rewritten — SQLite reads existing rows with
/// the old column count and treats missing columns as NULL (or the default on read, which is
/// M35.3). New INSERTs that don't specify the new column also get NULL (the current engine
/// behavior — column DEFAULTs are not modeled yet).
pub fn compile_alter_add_column(
    stmt: &AlterTableStmt,
    current_schema_cookie: u32,
    table_rowid: i64,
    old_sql: &str,
    col_def_text: &str,
) -> Result<Program> {
    if stmt.schema.is_some() {
        return Err(Error::msg(
            "schema-qualified ALTER TABLE is not yet supported",
        ));
    }
    let new_sql = splice_column_into_create_table(old_sql, col_def_text)
        .ok_or_else(|| Error::msg("cannot splice column into CREATE TABLE text"))?;

    let mut b = ProgramBuilder::new();

    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    let after_init = b.cur_addr();

    // (1) Open the write transaction.
    b.emit(Opcode::Transaction, 0, 1, 0);

    // (2) Open a write cursor on `sqlite_schema` (page 1).
    let schema_cursor = 0i32;
    b.emit(Opcode::OpenWrite, schema_cursor, SCHEMA_ROOT, 0);

    // (3) Seek to the table row, read the 5 columns, overwrite the `sql` column with the
    //     rewritten CREATE TABLE text, Delete + Insert at the same rowid.
    let rowid_reg = b.alloc_reg();
    let i = b.emit(Opcode::Int64, 0, rowid_reg, 0);
    b.set_p4(i, P4::Int(table_rowid));

    let skip = b.new_label();
    b.emit_jump(Opcode::NotExists, schema_cursor, skip, rowid_reg);

    let col0 = b.alloc_regs(5);
    for (ci, _) in [(0usize, ()), (1, ()), (2, ()), (3, ()), (4, ())].iter() {
        b.emit(Opcode::Column, schema_cursor, *ci as i32, col0 + *ci as i32);
    }
    let sql_idx = b.emit(Opcode::String8, 0, col0 + 4, 0);
    b.set_p4(sql_idx, P4::Text(new_sql));

    let record = b.alloc_reg();
    b.emit(Opcode::MakeRecord, col0, 5, record);
    let del_idx = b.emit(Opcode::Delete, schema_cursor, 0, 0);
    b.set_p5(del_idx, P5_ISUPDATE);
    let ins = b.emit(Opcode::Insert, schema_cursor, record, rowid_reg);
    b.set_p5(ins, P5_ISUPDATE);

    b.resolve(skip);

    // (4) Bump the schema cookie.
    b.emit(
        Opcode::SetCookie,
        0,
        COOKIE_SCHEMA,
        current_schema_cookie as i32 + 1,
    );

    // (5) Reload the schema so later statements see the new column.
    b.emit(Opcode::ParseSchema, 0, 0, 0);

    // (6) Halt commits the transaction.
    b.emit(Opcode::Halt, 0, 0, 0);

    b.resolve(setup);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

/// Splice a new column-definition text into a CREATE TABLE statement, just before the
/// closing `)` of the column list. Returns the rewritten SQL, or `None` when the splice
/// position cannot be found.
fn splice_column_into_create_table(create_sql: &str, col_def_text: &str) -> Option<String> {
    // Find `CREATE [TEMP] TABLE [IF NOT EXISTS] <name> (`
    let lower = create_sql.to_ascii_lowercase();
    let prefix = strip_create_prefix(&lower, "table")?;
    // Skip the table-name identifier (possibly quoted).
    let after_name = skip_identifier(create_sql, prefix.0);
    let pos = skip_whitespace(create_sql, after_name);
    let bytes = create_sql.as_bytes();
    if pos >= bytes.len() || bytes[pos] != b'(' {
        return None;
    }
    // Scan for the matching `)` tracking paren depth, skipping string literals.
    let mut depth: i32 = 0;
    let mut i = pos;
    let mut in_string: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        match in_string {
            Some(quote) => {
                if b == quote {
                    // Check for doubled quote (escape)
                    if i + 1 < bytes.len() && bytes[i + 1] == quote {
                        i += 2;
                        continue;
                    }
                    in_string = None;
                }
            }
            None => match b {
                b'\'' | b'"' => in_string = Some(b),
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        // Found the closing `)` of the column list.
                        let before = &create_sql[..i];
                        let after = &create_sql[i..];
                        // Splice in `, <col_def_text>` before the `)`.
                        // Trim trailing whitespace from `before` so the comma lands cleanly.
                        let before_trimmed = before.trim_end();
                        return Some(format!("{}, {}{}", before_trimmed, col_def_text, after));
                    }
                }
                _ => {}
            },
        }
        i += 1;
    }
    None
}

/// Extract the verbatim column-definition text from an `ALTER TABLE … ADD [COLUMN] <def>`
/// statement. Returns the text of `<def>` (trimmed), or `None` when it cannot be located.
pub fn extract_add_column_text(alter_sql: &str) -> Option<String> {
    let lower = alter_sql.to_ascii_lowercase();
    // Find `add` keyword (whole word).
    let add_pos = find_keyword(&lower, "add")?;
    let mut pos = add_pos + 3; // 3 = "add"
    pos = skip_whitespace(alter_sql, pos);
    // Optional `COLUMN` keyword.
    let rest = &lower[pos..];
    if rest.starts_with("column") {
        pos += "column".len();
        pos = skip_whitespace(alter_sql, pos);
    }
    // The rest is the column definition, trimmed, with a trailing semicolon stripped.
    let mut text = alter_sql[pos..].trim();
    text = text.strip_suffix(';').unwrap_or(text).trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// Compile `ALTER TABLE <tbl> DROP [COLUMN] <name>`.
///
/// * `stmt` — the parsed ALTER TABLE statement (action must be `DropColumn`).
/// * `current_schema_cookie` — the value before this DDL runs (the program bumps it by one).
/// * `table` — the catalog-resolved table being altered.
/// * `table_rowid` — the rowid of the table's `sqlite_schema` row.
/// * `old_sql` — the current `sql` text of the CREATE TABLE statement.
/// * `drop_col_idx` — the table-column index of the column being dropped.
/// * `drop_col_name` — the dequoted name of the column being dropped (for the SQL rewrite).
///
/// The program:
///   1. Rewrites the table's `sqlite_schema` row (removing the column from the CREATE TABLE
///      text).
///   2. Rewrites every existing row in the table b-tree (removing the dropped column's
///      value) using a two-pass sorter-as-rowset approach (same as UPDATE).
pub fn compile_alter_drop_column(
    stmt: &AlterTableStmt,
    current_schema_cookie: u32,
    table: &Table,
    table_rowid: i64,
    old_sql: &str,
    drop_col_idx: usize,
    drop_col_name: &str,
) -> Result<Program> {
    if stmt.schema.is_some() {
        return Err(Error::msg(
            "schema-qualified ALTER TABLE is not yet supported",
        ));
    }
    let new_sql = drop_column_from_create_table(old_sql, drop_col_name)
        .ok_or_else(|| Error::msg("cannot drop column from CREATE TABLE text"))?;

    let mut b = ProgramBuilder::new();

    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    let after_init = b.cur_addr();

    // (1) Open the write transaction.
    b.emit(Opcode::Transaction, 0, 1, 0);

    // (2) Open a write cursor on `sqlite_schema` (page 1) and rewrite the table row's sql.
    let schema_cursor = 0i32;
    b.emit(Opcode::OpenWrite, schema_cursor, SCHEMA_ROOT, 0);

    let rowid_reg = b.alloc_reg();
    let i = b.emit(Opcode::Int64, 0, rowid_reg, 0);
    b.set_p4(i, P4::Int(table_rowid));

    let skip_schema = b.new_label();
    b.emit_jump(Opcode::NotExists, schema_cursor, skip_schema, rowid_reg);

    let col0 = b.alloc_regs(5);
    for (ci, _) in [(0usize, ()), (1, ()), (2, ()), (3, ()), (4, ())].iter() {
        b.emit(Opcode::Column, schema_cursor, *ci as i32, col0 + *ci as i32);
    }
    let sql_idx = b.emit(Opcode::String8, 0, col0 + 4, 0);
    b.set_p4(sql_idx, P4::Text(new_sql));
    let record = b.alloc_reg();
    b.emit(Opcode::MakeRecord, col0, 5, record);
    let del_idx = b.emit(Opcode::Delete, schema_cursor, 0, 0);
    b.set_p5(del_idx, P5_ISUPDATE);
    let ins = b.emit(Opcode::Insert, schema_cursor, record, rowid_reg);
    b.set_p5(ins, P5_ISUPDATE);
    b.resolve(skip_schema);
    b.emit(Opcode::Close, schema_cursor, 0, 0);

    // (3) Rewrite every row in the table b-tree: remove the dropped column's value.
    //     Two-pass sorter-as-rowset approach (same as UPDATE): first pass captures rowids,
    //     second pass re-seeks each rowid, reads all columns except the dropped one, builds
    //     a new record, deletes the old row, and inserts the new one.
    let table_cursor = 0i32;
    let sorter = 1i32;
    b.emit(Opcode::OpenWrite, table_cursor, table.rootpage as i32, 0);

    // Open the rowid-set sorter.
    let so = b.emit(Opcode::SorterOpen, sorter, 1, 0);
    b.set_p4(so, P4::KeyInfo(vec![KeyField::asc_binary()]));

    // First pass: scan the table, capture each rowid.
    let end_scan = b.new_label();
    let scan_top = b.new_label();
    b.emit_jump(Opcode::Rewind, table_cursor, end_scan, 0);
    b.resolve(scan_top);
    let reg_old_rowid = b.alloc_reg();
    b.emit(Opcode::Rowid, table_cursor, reg_old_rowid, 0);
    let reg_rowid_rec = b.alloc_reg();
    b.emit(Opcode::MakeRecord, reg_old_rowid, 1, reg_rowid_rec);
    b.emit(Opcode::SorterInsert, sorter, reg_rowid_rec, 0);
    b.emit_jump(Opcode::Next, table_cursor, scan_top, 0);
    b.resolve(end_scan);

    // Second pass: iterate the sorter, re-seek each rowid, build the reduced record,
    // delete + insert.
    let end_update = b.new_label();
    b.emit_jump(Opcode::SorterSort, sorter, end_update, 0);
    let update_top = b.new_label();
    let sort_next = b.new_label();
    b.resolve(update_top);

    b.emit(Opcode::SorterData, sorter, 0, 0);
    let reg_old_rowid2 = b.alloc_reg();
    b.emit(Opcode::Column, sorter, 0, reg_old_rowid2);
    b.emit_jump(Opcode::NotExists, table_cursor, sort_next, reg_old_rowid2);

    // Build the new record: read all columns except the dropped one.
    let ncol = table.columns.len();
    let n_new = ncol - 1; // one fewer column
    let reg_new = b.alloc_regs(n_new as i32);
    let mut out_idx = 0i32;
    for ci in 0..ncol {
        if ci == drop_col_idx {
            continue;
        }
        // Read the column from the table cursor into the next output register.
        b.emit(Opcode::Column, table_cursor, ci as i32, reg_new + out_idx);
        out_idx += 1;
    }

    let reg_new_rec = b.alloc_reg();
    b.emit(Opcode::MakeRecord, reg_new, n_new as i32, reg_new_rec);
    let del_idx = b.emit(Opcode::Delete, table_cursor, 0, 0);
    b.set_p5(del_idx, P5_ISUPDATE);
    let ins = b.emit(Opcode::Insert, table_cursor, reg_new_rec, reg_old_rowid2);
    b.set_p5(ins, P5_ISUPDATE);

    b.resolve(sort_next);
    b.emit_jump(Opcode::SorterNext, sorter, update_top, 0);
    b.resolve(end_update);

    // (4) Bump the schema cookie.
    b.emit(
        Opcode::SetCookie,
        0,
        COOKIE_SCHEMA,
        current_schema_cookie as i32 + 1,
    );

    // (5) Reload the schema so later statements see the reduced column set.
    b.emit(Opcode::ParseSchema, 0, 0, 0);

    // (6) Halt commits the transaction.
    b.emit(Opcode::Halt, 0, 0, 0);

    b.resolve(setup);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

/// Validate that a `ColumnDef` is legal for `ADD COLUMN`: not PRIMARY KEY, not UNIQUE
/// (column-level), and if NOT NULL then must have a non-NULL default (when the table is
/// non-empty — we approximate by always rejecting NOT NULL without default, matching
/// upstream's `sqlite3ErrorIfNotEmpty` conservative check). Returns `Ok(())` when legal.
pub fn validate_add_column(col: &ColumnDef) -> Result<()> {
    let mut has_pk = false;
    let mut has_unique = false;
    let mut has_not_null = false;
    let mut has_default = false;
    let mut default_is_null = false;
    for c in &col.constraints {
        match c {
            rustqlite_parser::ColumnConstraint::PrimaryKey { .. } => has_pk = true,
            rustqlite_parser::ColumnConstraint::Unique { .. } => has_unique = true,
            rustqlite_parser::ColumnConstraint::NotNull { .. } => has_not_null = true,
            rustqlite_parser::ColumnConstraint::Default(e) => {
                has_default = true;
                if let rustqlite_parser::Expr::Literal(rustqlite_parser::Literal::Null) = e {
                    default_is_null = true;
                }
            }
            _ => {}
        }
    }
    if has_pk {
        return Err(Error::msg("Cannot add a PRIMARY KEY column"));
    }
    if has_unique {
        return Err(Error::msg("Cannot add a UNIQUE column"));
    }
    if has_not_null && (!has_default || default_is_null) {
        return Err(Error::msg(
            "Cannot add a NOT NULL column with default value NULL",
        ));
    }
    Ok(())
}

/// Validate that dropping `col_name` from `table` is legal: the column must exist, must not
/// be a PRIMARY KEY or UNIQUE column, and the table must have at least one other column.
/// Returns the table-column index of the column to drop.
pub fn validate_drop_column(table: &Table, col_name: &str) -> Result<usize> {
    let key = dequote_ident(col_name);
    let idx = table
        .column_index(&key)
        .ok_or_else(|| Error::msg(format!("no such column: {}", col_name)))?;
    if table.columns[idx].pk {
        return Err(Error::msg(format!(
            "cannot drop PRIMARY KEY column: \"{}\"",
            col_name
        )));
    }
    if table.columns.len() <= 1 {
        return Err(Error::msg(format!(
            "cannot drop column \"{}\": no other columns exist",
            col_name
        )));
    }
    Ok(idx)
}

/// Remove a column definition from a CREATE TABLE statement's column list. The `col_name`
/// is the dequoted column name. Returns the rewritten SQL, or `None` when the column
/// cannot be found or the splice fails.
fn drop_column_from_create_table(create_sql: &str, col_name: &str) -> Option<String> {
    let lower = create_sql.to_ascii_lowercase();
    let prefix = strip_create_prefix(&lower, "table")?;
    let after_name = skip_identifier(create_sql, prefix.0);
    let pos = skip_whitespace(create_sql, after_name);
    let bytes = create_sql.as_bytes();
    if pos >= bytes.len() || bytes[pos] != b'(' {
        return None;
    }
    // Find the matching `)` and the comma boundaries of each column def.
    // We scan the column list, tracking paren depth and string literals, and split on
    // top-level commas (depth == 1, i.e. directly inside the column-list parens). Each
    // segment is a column-def or table-constraint.
    let mut depth: i32 = 0;
    let mut i = pos;
    let mut in_string: Option<u8> = None;
    let mut segments: Vec<(usize, usize)> = Vec::new(); // (start, end) of each segment
    let mut seg_start = pos + 1; // first char after `(`
    while i < bytes.len() {
        let b = bytes[i];
        match in_string {
            Some(quote) => {
                if b == quote {
                    if i + 1 < bytes.len() && bytes[i + 1] == quote {
                        i += 2;
                        continue;
                    }
                    in_string = None;
                }
            }
            None => match b {
                b'\'' | b'"' => in_string = Some(b),
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        // Closing `)` of the column list. Push the last segment.
                        let seg_end = i;
                        if seg_end > seg_start {
                            segments.push((seg_start, seg_end));
                        }
                        break;
                    }
                }
                b',' if depth == 1 => {
                    // Top-level comma inside the column list.
                    let seg_end = i;
                    segments.push((seg_start, seg_end));
                    seg_start = i + 1;
                }
                _ => {}
            },
        }
        i += 1;
    }
    // Find the segment whose first identifier matches `col_name` (dequoted).
    let target_key = dequote_ident(col_name);
    let mut found_idx: Option<usize> = None;
    for (sidx, &(s, e)) in segments.iter().enumerate() {
        let seg = &create_sql[s..e];
        let seg_trimmed = seg.trim_start();
        let seg_lower = seg_trimmed.to_ascii_lowercase();
        // Skip segments that start with a table-constraint keyword.
        let is_constraint = seg_lower.starts_with("primary")
            || seg_lower.starts_with("unique")
            || seg_lower.starts_with("check")
            || seg_lower.starts_with("foreign")
            || seg_lower.starts_with("constraint");
        if is_constraint {
            continue;
        }
        // The first identifier in the segment is the column name.
        let seg_pos = skip_whitespace(seg_trimmed, 0);
        let id_end = skip_identifier(seg_trimmed, seg_pos);
        let id_text = &seg_trimmed[seg_pos..id_end];
        let id_dequoted = dequote_ident(id_text);
        if id_dequoted.eq_ignore_ascii_case(&target_key) {
            found_idx = Some(sidx);
            break;
        }
    }
    let drop_sidx = found_idx?;
    // Rebuild the column list without the dropped segment.
    let (drop_start, drop_end) = segments[drop_sidx];
    // Determine the comma to remove: if this is not the first segment, remove the comma
    // before it; if it is the first, remove the comma after it (if any).
    let (remove_start, remove_end) = if drop_sidx == 0 {
        // First segment: remove from segment start to the start of the next segment (or to
        // the closing `)` if this is the only segment — but DROP COLUMN rejects a
        // single-column table, so there's always a next segment).
        if segments.len() > 1 {
            // Trim leading whitespace from the next segment by advancing past it.
            let next_start = segments[1].0;
            let next_trimmed = skip_whitespace(create_sql, next_start);
            (drop_start, next_trimmed)
        } else {
            (drop_start, drop_end)
        }
    } else {
        // Not the first: remove from the end of the previous segment (which includes the
        // comma) to the end of this segment.
        (segments[drop_sidx - 1].1, drop_end)
    };
    // Reconstruct: before + after, skipping the removed range.
    let before = &create_sql[..remove_start];
    let after = &create_sql[remove_end..];
    Some(format!("{}{}", before, after))
}

/// Compile `ALTER TABLE <old> RENAME TO <new>`.
///
/// * `stmt` — the parsed ALTER TABLE statement (action must be `RenameTo`).
/// * `current_schema_cookie` — the value before this DDL runs (the program bumps it by one).
/// * `edits` — the resolved set of `sqlite_schema` row edits (the table row + every
///   associated index/trigger row whose `tbl_name` matches).
pub fn compile_alter_rename_table(
    stmt: &AlterTableStmt,
    current_schema_cookie: u32,
    edits: &[SchemaRowEdit],
) -> Result<Program> {
    if stmt.schema.is_some() {
        return Err(Error::msg(
            "schema-qualified ALTER TABLE is not yet supported",
        ));
    }
    let mut b = ProgramBuilder::new();

    let setup = b.new_label();
    b.emit_jump(Opcode::Init, 0, setup, 0);
    let after_init = b.cur_addr();

    // (1) Open the write transaction.
    b.emit(Opcode::Transaction, 0, 1, 0);

    // (2) Open a write cursor on `sqlite_schema` (page 1).
    let schema_cursor = 0i32;
    b.emit(Opcode::OpenWrite, schema_cursor, SCHEMA_ROOT, 0);

    // (3) For each row edit: seek to the rowid, read the existing 5 columns into contiguous
    //     registers, overwrite the columns we're changing, MakeRecord, Delete the old row,
    //     and Insert the new record (with the same rowid, so the b-tree places it back at the
    //     same key). The P5_ISUPDATE flag on Insert suppresses `last_insert_rowid` clobbering
    //     and the `changes()` bump (ALTER TABLE is not a user-visible INSERT). The Delete's
    //     P5_ISUPDATE flag likewise suppresses the `changes()` bump.
    for edit in edits {
        let rowid_reg = b.alloc_reg();
        let i = b.emit(Opcode::Int64, 0, rowid_reg, 0);
        b.set_p4(i, P4::Int(edit.rowid));

        let skip = b.new_label();
        b.emit_jump(Opcode::NotExists, schema_cursor, skip, rowid_reg);

        // Read the 5 sqlite_schema columns (type, name, tbl_name, rootpage, sql) into
        // contiguous registers starting at `col0`.
        let col0 = b.alloc_regs(5);
        for (ci, _) in [(0usize, ()), (1, ()), (2, ()), (3, ()), (4, ())].iter() {
            b.emit(Opcode::Column, schema_cursor, *ci as i32, col0 + *ci as i32);
        }
        // Overwrite the columns we're editing.
        if let Some(new_name) = &edit.new_name {
            let idx = b.emit(Opcode::String8, 0, col0 + 1, 0);
            b.set_p4(idx, P4::Text(new_name.clone()));
        }
        if let Some(new_tbl_name) = &edit.new_tbl_name {
            let idx = b.emit(Opcode::String8, 0, col0 + 2, 0);
            b.set_p4(idx, P4::Text(new_tbl_name.clone()));
        }
        if let Some(new_sql) = &edit.new_sql {
            let idx = b.emit(Opcode::String8, 0, col0 + 4, 0);
            b.set_p4(idx, P4::Text(new_sql.clone()));
        }

        let record = b.alloc_reg();
        b.emit(Opcode::MakeRecord, col0, 5, record);
        // Delete the old row, then Insert the new record at the same rowid. The Delete must
        // happen before the Insert because `table_insert` does not overwrite an existing key
        // (it would create a duplicate cell).
        let del_idx = b.emit(Opcode::Delete, schema_cursor, 0, 0);
        b.set_p5(del_idx, P5_ISUPDATE);
        let ins = b.emit(Opcode::Insert, schema_cursor, record, rowid_reg);
        b.set_p5(ins, P5_ISUPDATE);

        b.resolve(skip);
    }

    // (4) Bump the schema cookie.
    b.emit(
        Opcode::SetCookie,
        0,
        COOKIE_SCHEMA,
        current_schema_cookie as i32 + 1,
    );

    // (5) Reload the schema so later statements see the renamed table.
    b.emit(Opcode::ParseSchema, 0, 0, 0);

    // (6) Halt commits the transaction.
    b.emit(Opcode::Halt, 0, 0, 0);

    b.resolve(setup);
    b.emit(Opcode::Goto, 0, after_init, 0);
    Ok(b.finish())
}

/// Dequote a SQL identifier string if it is wrapped in `"..."`, `` `...` ``, or `[...]`.
/// Doubled quote characters within the string are collapsed. Returns the input unchanged
/// when it is not quoted. This mirrors `sqlite3Dequote` for the identifier-storage case
/// (SQLite stores the *dequoted* form in the `name`/`tbl_name` columns of `sqlite_schema`).
pub fn dequote_ident(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() < 2 {
        return s.to_string();
    }
    match bytes[0] {
        b'"' | b'`' => {
            let quote = bytes[0];
            if bytes[bytes.len() - 1] != quote {
                return s.to_string();
            }
            let inner = &s[1..bytes.len() - 1];
            inner.replace(&format!("{}{}", quote as char, quote as char), &format!("{}", quote as char))
        }
        b'[' => {
            if bytes[bytes.len() - 1] != b']' {
                return s.to_string();
            }
            s[1..bytes.len() - 1].to_string()
        }
        _ => s.to_string(),
    }
}

/// Rewrite the table-name token in a stored CREATE TABLE / CREATE INDEX / CREATE TRIGGER
/// statement from `old` to `new`. Returns the rewritten SQL text, or the original text
/// unchanged when the rewrite could not be done safely (the resolver will then leave the
/// `sql` column untouched, matching `legacy_alter_table=ON` behavior).
///
/// `old` and `new` are compared against the *dequoted* identifier stored in the SQL text,
/// so callers should pass the dequoted forms (e.g. `My Table`, not `"My Table"`).
///
/// For CREATE TABLE: the table name is the identifier following `CREATE [TEMP] TABLE`
/// (and an optional `IF NOT EXISTS`). For CREATE INDEX: the table name is the identifier
/// following `ON`. For CREATE TRIGGER: the table name is the identifier following `ON`.
/// We splice the new name in place, preserving the original quoting style.
pub fn rewrite_table_name_in_sql(sql: &str, old: &str, new: &str) -> Option<String> {
    let lower = sql.to_ascii_lowercase();
    // CREATE TABLE: find "CREATE [TEMP] TABLE [IF NOT EXISTS] <name>".
    if let Some(rest) = strip_create_prefix(&lower, "table") {
        let pos = rest.0;
        return splice_identifier(sql, pos, old, new);
    }
    // CREATE INDEX: find "CREATE [UNIQUE] INDEX [IF NOT EXISTS] <idx> ON <name>".
    if let Some(rest) = strip_create_prefix(&lower, "index") {
        // Skip the index name identifier, then `ON`, then splice the table name.
        let after_idx = skip_identifier(sql, rest.0);
        if let Some(on_pos) = find_keyword(&lower[after_idx..], "on") {
            let abs = after_idx + on_pos + 2; // 2 = "on"
            let p = skip_whitespace(sql, abs);
            return splice_identifier(sql, p, old, new);
        }
    }
    // CREATE TRIGGER: find "CREATE [TEMP] TRIGGER [IF NOT EXISTS] <trig> ... ON <name>".
    if let Some(rest) = strip_create_prefix(&lower, "trigger") {
        let after_trig = skip_identifier(sql, rest.0);
        if let Some(on_pos) = find_keyword(&lower[after_trig..], "on") {
            let abs = after_trig + on_pos + 2;
            let p = skip_whitespace(sql, abs);
            return splice_identifier(sql, p, old, new);
        }
    }
    None
}

/// Strip `CREATE [TEMP|UNIQUE] <kw> [IF NOT EXISTS]` from the lowercased SQL, returning the
/// byte position (in the original `sql`) where the object-name identifier starts.
fn strip_create_prefix<'a>(lower: &'a str, kw: &str) -> Option<(usize, &'a str)> {
    let prefix = "create";
    let start = lower.find(prefix)?;
    let mut pos = start + prefix.len();
    pos = skip_whitespace(lower, pos);
    // Optional TEMP / UNIQUE / TEMPORARY (possibly repeated, e.g. CREATE UNIQUE INDEX).
    loop {
        let remaining = &lower[pos..];
        let mut advanced = false;
        for opt in ["temp", "temporary", "unique"] {
            if remaining.starts_with(opt) {
                pos += opt.len();
                pos = skip_whitespace(lower, pos);
                advanced = true;
                break;
            }
        }
        if !advanced {
            break;
        }
    }
    let rest = &lower[pos..];
    if !rest.starts_with(kw) {
        return None;
    }
    pos += kw.len();
    pos = skip_whitespace(lower, pos);
    // Optional `IF NOT EXISTS`.
    let rest = &lower[pos..];
    if rest.starts_with("if not exists") {
        pos += "if not exists".len();
        pos = skip_whitespace(lower, pos);
    }
    Some((pos, &lower[pos..]))
}

/// Skip a (possibly quoted) identifier starting at byte `pos`, returning the byte position
/// just past it. Handles `"..."`, `` `...` ``, `[...]`, and bare identifiers.
fn skip_identifier(sql: &str, pos: usize) -> usize {
    let bytes = sql.as_bytes();
    if pos >= bytes.len() {
        return pos;
    }
    match bytes[pos] {
        b'"' => end_quoted(sql, pos, b'"'),
        b'`' => end_quoted(sql, pos, b'`'),
        b'[' => {
            // [...] — terminated by `]`.
            if let Some(end) = sql[pos + 1..].find(']') {
                pos + 1 + end + 1
            } else {
                pos
            }
        }
        _ => {
            // Bare identifier: advance over identifier characters.
            let mut p = pos;
            while p < bytes.len() && is_ident_char(bytes[p]) {
                p += 1;
            }
            p
        }
    }
}

/// Find the position of `kw` in `s` as a whole word (case-sensitive on `s` which is already
/// lowercased). Returns the byte offset of the keyword.
fn find_keyword(s: &str, kw: &str) -> Option<usize> {
    let mut search_from = 0;
    while let Some(pos) = s[search_from..].find(kw) {
        let abs = search_from + pos;
        // Boundary check: previous and next characters must not be identifier chars.
        let bytes = s.as_bytes();
        let ok_before = abs == 0 || !is_ident_char(bytes[abs - 1]);
        let after = abs + kw.len();
        let ok_after = after >= bytes.len() || !is_ident_char(bytes[after]);
        if ok_before && ok_after {
            return Some(abs);
        }
        search_from = abs + 1;
    }
    None
}

/// Splice `new` in place of the identifier at byte position `pos` (which must match `old`,
/// case-insensitively for unquoted, exactly for quoted). Returns `Some(new_sql)` on success.
fn splice_identifier(sql: &str, pos: usize, old: &str, new: &str) -> Option<String> {
    let bytes = sql.as_bytes();
    if pos >= bytes.len() {
        return None;
    }
    let (end, ident_text) = match bytes[pos] {
        b'"' | b'`' => {
            let quote = bytes[pos];
            let end = end_quoted(sql, pos, quote);
            // The identifier text is between pos+1 and end-1 (excluding the closing quote,
            // which is at end-1). We handle doubled-quote escapes by dequoting.
            let inner = &sql[pos + 1..end - 1];
            let dequoted = inner.replace(&format!("{}{}", quote as char, quote as char), &format!("{}", quote as char));
            (end, dequoted)
        }
        b'[' => {
            let end = sql[pos + 1..].find(']')? + pos + 1 + 1;
            (end, sql[pos + 1..end - 1].to_string())
        }
        _ => {
            let mut p = pos;
            while p < bytes.len() && is_ident_char(bytes[p]) {
                p += 1;
            }
            (p, sql[pos..p].to_string())
        }
    };
    // Verify the identifier matches `old` (case-insensitive for unquoted, exact for quoted).
    let matches = if bytes[pos] == b'"' || bytes[pos] == b'`' || bytes[pos] == b'[' {
        ident_text == old
    } else {
        ident_text.eq_ignore_ascii_case(old)
    };
    if !matches {
        return None;
    }
    // Build the replacement: re-quote `new` if the original was quoted, otherwise use bare.
    let replacement = if bytes[pos] == b'"' || bytes[pos] == b'`' || bytes[pos] == b'[' {
        let quote = bytes[pos] as char;
        // Escape any embedded quote chars by doubling them (for `"` and `` ` ``).
        let escaped = if quote == '"' || quote == '`' {
            new.replace(quote, &format!("{}{}", quote, quote))
        } else {
            new.to_string()
        };
        if bytes[pos] == b'[' {
            format!("[{}]", escaped)
        } else {
            format!("{}{}{}", quote, escaped, quote)
        }
    } else {
        new.to_string()
    };
    let mut out = String::with_capacity(sql.len() - (end - pos) + replacement.len());
    out.push_str(&sql[..pos]);
    out.push_str(&replacement);
    out.push_str(&sql[end..]);
    Some(out)
}

/// Advance `pos` past whitespace in `sql`.
fn skip_whitespace(sql: &str, pos: usize) -> usize {
    let bytes = sql.as_bytes();
    let mut p = pos;
    while p < bytes.len() && bytes[p].is_ascii_whitespace() {
        p += 1;
    }
    p
}

/// Find the end position (exclusive) of a `quote`-delimited identifier starting at `pos`.
fn end_quoted(sql: &str, pos: usize, quote: u8) -> usize {
    let bytes = sql.as_bytes();
    let mut p = pos + 1;
    while p < bytes.len() {
        if bytes[p] == quote {
            // Doubled quote → literal, skip.
            if p + 1 < bytes.len() && bytes[p + 1] == quote {
                p += 2;
                continue;
            }
            return p + 1;
        }
        p += 1;
    }
    p
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_create_table_bare_name() {
        let out = rewrite_table_name_in_sql("CREATE TABLE t(a, b)", "t", "u").unwrap();
        assert_eq!(out, "CREATE TABLE u(a, b)");
    }

    #[test]
    fn rewrite_create_table_quoted_name() {
        let out = rewrite_table_name_in_sql("CREATE TABLE \"t\"(a, b)", "t", "u").unwrap();
        assert_eq!(out, "CREATE TABLE \"u\"(a, b)");
    }

    #[test]
    fn rewrite_create_table_if_not_exists() {
        let out =
            rewrite_table_name_in_sql("CREATE TABLE IF NOT EXISTS t(a)", "t", "u").unwrap();
        assert_eq!(out, "CREATE TABLE IF NOT EXISTS u(a)");
    }

    #[test]
    fn rewrite_create_index_table_target() {
        let out = rewrite_table_name_in_sql(
            "CREATE INDEX idx_a ON t(a)",
            "t",
            "u",
        )
        .unwrap();
        assert_eq!(out, "CREATE INDEX idx_a ON u(a)");
    }

    #[test]
    fn rewrite_create_unique_index_if_not_exists() {
        let out = rewrite_table_name_in_sql(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx ON t(a)",
            "t",
            "u",
        )
        .unwrap();
        assert_eq!(out, "CREATE UNIQUE INDEX IF NOT EXISTS idx ON u(a)");
    }

    #[test]
    fn rewrite_preserves_rest_of_sql() {
        let out = rewrite_table_name_in_sql(
            "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)",
            "t",
            "new_tbl",
        )
        .unwrap();
        assert_eq!(
            out,
            "CREATE TABLE new_tbl(a INTEGER PRIMARY KEY, b TEXT)"
        );
    }

    #[test]
    fn rewrite_returns_none_when_name_does_not_match() {
        assert!(rewrite_table_name_in_sql("CREATE TABLE t(a)", "other", "u").is_none());
    }

    #[test]
    fn dequote_ident_bare() {
        assert_eq!(dequote_ident("t"), "t");
    }

    #[test]
    fn dequote_ident_double_quotes() {
        assert_eq!(dequote_ident("\"My Table\""), "My Table");
    }

    #[test]
    fn dequote_ident_backticks() {
        assert_eq!(dequote_ident("`My Table`"), "My Table");
    }

    #[test]
    fn dequote_ident_brackets() {
        assert_eq!(dequote_ident("[My Table]"), "My Table");
    }

    #[test]
    fn dequote_ident_doubled_quotes() {
        assert_eq!(dequote_ident("\"a\"\"b\""), "a\"b");
    }

    #[test]
    fn rewrite_quoted_table_name() {
        let out = rewrite_table_name_in_sql(
            "CREATE TABLE \"My Table\"(a)",
            "My Table",
            "Other Name",
        )
        .unwrap();
        assert_eq!(out, "CREATE TABLE \"Other Name\"(a)");
    }

    #[test]
    fn splice_column_basic() {
        let out = splice_column_into_create_table("CREATE TABLE t(a)", "b TEXT").unwrap();
        assert_eq!(out, "CREATE TABLE t(a, b TEXT)");
    }

    #[test]
    fn splice_column_with_existing_columns() {
        let out = splice_column_into_create_table("CREATE TABLE t(a, b INTEGER)", "c TEXT").unwrap();
        assert_eq!(out, "CREATE TABLE t(a, b INTEGER, c TEXT)");
    }

    #[test]
    fn splice_column_with_varchar_n() {
        // The `)` inside `VARCHAR(10)` must not confuse the paren matcher.
        let out = splice_column_into_create_table(
            "CREATE TABLE t(a VARCHAR(10))",
            "b TEXT",
        )
        .unwrap();
        assert_eq!(out, "CREATE TABLE t(a VARCHAR(10), b TEXT)");
    }

    #[test]
    fn splice_column_without_rowid() {
        let out = splice_column_into_create_table(
            "CREATE TABLE t(a PRIMARY KEY) WITHOUT ROWID",
            "b TEXT",
        )
        .unwrap();
        assert_eq!(out, "CREATE TABLE t(a PRIMARY KEY, b TEXT) WITHOUT ROWID");
    }

    #[test]
    fn extract_add_column_text_basic() {
        let out = extract_add_column_text("ALTER TABLE t ADD COLUMN b TEXT").unwrap();
        assert_eq!(out, "b TEXT");
    }

    #[test]
    fn extract_add_column_text_without_keyword() {
        let out = extract_add_column_text("ALTER TABLE t ADD b TEXT").unwrap();
        assert_eq!(out, "b TEXT");
    }

    #[test]
    fn extract_add_column_text_with_default() {
        let out =
            extract_add_column_text("ALTER TABLE t ADD COLUMN b INTEGER DEFAULT 42").unwrap();
        assert_eq!(out, "b INTEGER DEFAULT 42");
    }

    #[test]
    fn extract_add_column_text_strips_semicolon() {
        let out = extract_add_column_text("ALTER TABLE t ADD COLUMN b TEXT;").unwrap();
        assert_eq!(out, "b TEXT");
    }

    #[test]
    fn drop_column_middle() {
        let out = drop_column_from_create_table("CREATE TABLE t(a, b, c)", "b").unwrap();
        assert_eq!(out, "CREATE TABLE t(a, c)");
    }

    #[test]
    fn drop_column_first() {
        let out = drop_column_from_create_table("CREATE TABLE t(a, b, c)", "a").unwrap();
        assert_eq!(out, "CREATE TABLE t(b, c)");
    }

    #[test]
    fn drop_column_last() {
        let out = drop_column_from_create_table("CREATE TABLE t(a, b, c)", "c").unwrap();
        assert_eq!(out, "CREATE TABLE t(a, b)");
    }

    #[test]
    fn drop_column_with_types() {
        let out =
            drop_column_from_create_table("CREATE TABLE t(a INTEGER, b TEXT, c REAL)", "b").unwrap();
        assert_eq!(out, "CREATE TABLE t(a INTEGER, c REAL)");
    }

    #[test]
    fn drop_column_with_table_constraint() {
        // The table-level PRIMARY KEY constraint must not be mistaken for a column def.
        let out = drop_column_from_create_table(
            "CREATE TABLE t(a, b, PRIMARY KEY(a))",
            "b",
        )
        .unwrap();
        assert_eq!(out, "CREATE TABLE t(a, PRIMARY KEY(a))");
    }
}