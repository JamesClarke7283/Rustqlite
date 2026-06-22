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

use rustqlite_parser::AlterTableStmt;

use crate::error::{Error, Result};
use crate::vdbe::program::{Program, P4, P5_ISUPDATE};
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
}