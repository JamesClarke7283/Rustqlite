//! Result-row formatting for the shell's output modes (mirrors the per-mode rendering in
//! `shell.c`).
//!
//! Every output mode the sqlite3 shell supports is rendered here. Values are rendered to text by
//! the engine ([`Value::to_text`]) so REAL formatting matches `sqlite3_column_text` exactly; NULL
//! uses the shell's `.nullvalue` string for the text-oriented modes, while the typed modes
//! (`json`, `insert`, `quote`) inspect the [`Value`] variant directly to emit SQL/JSON literals.
//!
//! Faithfulness scope (verified byte-for-byte against sqlite3 3.53.1): all modes match the oracle
//! for result sets of NULL / INTEGER / REAL / plain-TEXT cells. A few gaps remain for exotic
//! inputs and are deferred (each needs either engine changes or substantial per-mode machinery):
//!   * BLOB cells in text modes — the shell prints the raw blob bytes, whereas `Value::to_text`
//!     here lossily UTF-8-decodes the blob. Fixing this belongs in `rustsqlite-core`'s value
//!     rendering, not the shell.
//!   * Control characters in columnar modes (`column`/`box`/`table`/`markdown`/`line`) — the shell
//!     expands TAB to spaces, shows other controls in caret notation (`^A`), and wraps embedded
//!     newlines onto multiple physical rows; we pass the raw character through.

use rustsqlite_core::Value;

use crate::cli::OutputMode;
use crate::state::ShellState;

/// Render a result set for `mode`. `columns` are the column names (headers); `rows` are the
/// decoded result rows. Returns the text to print (including a trailing newline per row).
pub fn format_rows(
    mode: OutputMode,
    state: &ShellState,
    columns: &[String],
    rows: &[Vec<Value>],
) -> String {
    match mode {
        OutputMode::List => format_list(state, columns, rows),
        OutputMode::Csv => format_csv(state, columns, rows),
        OutputMode::Column => format_column(state, columns, rows),
        OutputMode::Tabs => format_tabs(state, columns, rows),
        OutputMode::Line => format_line(state, columns, rows),
        OutputMode::Quote => format_quote(state, columns, rows),
        OutputMode::Ascii => format_ascii(state, columns, rows),
        OutputMode::Html => format_html(state, columns, rows),
        OutputMode::Markdown => format_markdown(state, columns, rows),
        OutputMode::Boxed => format_box(state, columns, rows, &BOX_GLYPHS),
        OutputMode::Table => format_box(state, columns, rows, &TABLE_GLYPHS),
        OutputMode::Json => format_json(columns, rows),
        OutputMode::Insert => format_insert(state, columns, rows),
    }
}

/// The display text of a cell: the engine's column text, or the `.nullvalue` string for NULL.
fn cell_text(state: &ShellState, v: &Value) -> String {
    v.to_text().unwrap_or_else(|| state.nullvalue.clone())
}

fn format_list(state: &ShellState, columns: &[String], rows: &[Vec<Value>]) -> String {
    let sep = &state.colsep;
    let mut out = String::new();
    if state.headers {
        out.push_str(&columns.join(sep));
        out.push_str(&state.rowsep);
    }
    for row in rows {
        let cells: Vec<String> = row.iter().map(|v| cell_text(state, v)).collect();
        out.push_str(&cells.join(sep));
        out.push_str(&state.rowsep);
    }
    out
}

fn format_csv(state: &ShellState, columns: &[String], rows: &[Vec<Value>]) -> String {
    let mut out = String::new();
    if state.headers {
        let cells: Vec<String> = columns.iter().map(|c| csv_quote(c)).collect();
        out.push_str(&cells.join(","));
        out.push_str("\r\n");
    }
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .map(|v| csv_quote(&cell_text(state, v)))
            .collect();
        out.push_str(&cells.join(","));
        out.push_str("\r\n");
    }
    out
}

/// CSV field quoting: wrap in double quotes (doubling embedded quotes) when the field contains
/// a comma, double quote, CR, LF, or TAB — matching `shell.c`'s `output_csv`.
fn csv_quote(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r', '\t']) {
        let mut s = String::with_capacity(field.len() + 2);
        s.push('"');
        for ch in field.chars() {
            if ch == '"' {
                s.push('"');
            }
            s.push(ch);
        }
        s.push('"');
        s
    } else {
        field.to_string()
    }
}

/// `tabs` mode: like `list` but with a hard TAB as the field separator (the row separator is left
/// at the shell default, `\n`). Honors `.headers` and `.nullvalue`. Values are CSV-quoted with TAB
/// as the trigger, matching `shell.c` (tabs mode is CSV with a tab field separator).
fn format_tabs(state: &ShellState, columns: &[String], rows: &[Vec<Value>]) -> String {
    let mut out = String::new();
    if state.headers {
        let cells: Vec<String> = columns.iter().map(|c| csv_quote(c)).collect();
        out.push_str(&cells.join("\t"));
        out.push_str(&state.rowsep);
    }
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .map(|v| csv_quote(&cell_text(state, v)))
            .collect();
        out.push_str(&cells.join("\t"));
        out.push_str(&state.rowsep);
    }
    out
}

/// `line` mode: one `name: value` pair per line, the name right-aligned to the widest column
/// name. A blank line separates rows; there is no trailing blank line after the final row.
/// `.headers` is ignored (names are always shown); NULL uses `.nullvalue`.
fn format_line(state: &ShellState, columns: &[String], rows: &[Vec<Value>]) -> String {
    if columns.is_empty() {
        return String::new();
    }
    let w = columns.iter().map(|c| display_width(c)).max().unwrap_or(0);
    let mut out = String::new();
    for (r, row) in rows.iter().enumerate() {
        if r > 0 {
            out.push('\n');
        }
        for (c, v) in row.iter().enumerate() {
            out.push_str(&pad_left(&columns[c], w));
            out.push_str(": ");
            out.push_str(&cell_text(state, v));
            out.push('\n');
        }
    }
    out
}

/// `quote` mode: every value rendered as a SQL literal, comma-separated. Header names (when
/// `.headers` is on) are emitted as single-quoted text literals. TEXT → `'...'` (embedded `'`
/// doubled, or `unistr('..\uXXXX..')` when it contains control characters), BLOB → `x'..'`,
/// NULL → `NULL`, INTEGER/REAL emitted bare (REAL via the engine's faithful text). This ignores
/// `.nullvalue` — NULL is always the SQL keyword.
fn format_quote(state: &ShellState, columns: &[String], rows: &[Vec<Value>]) -> String {
    let mut out = String::new();
    if state.headers {
        let cells: Vec<String> = columns.iter().map(|c| quote_text(c)).collect();
        out.push_str(&cells.join(","));
        out.push('\n');
    }
    for row in rows {
        let cells: Vec<String> = row.iter().map(sql_literal).collect();
        out.push_str(&cells.join(","));
        out.push('\n');
    }
    out
}

/// `ascii` mode: field separator US (0x1F), record separator RS (0x1E). No quoting; honors
/// `.headers` and `.nullvalue`. The record separator follows every row (header included) with no
/// trailing newline, matching `shell.c`.
fn format_ascii(state: &ShellState, columns: &[String], rows: &[Vec<Value>]) -> String {
    const US: char = '\u{1f}';
    const RS: char = '\u{1e}';
    let mut out = String::new();
    if state.headers {
        out.push_str(&columns.join(&US.to_string()));
        out.push(RS);
    }
    for row in rows {
        let cells: Vec<String> = row.iter().map(|v| cell_text(state, v)).collect();
        out.push_str(&cells.join(&US.to_string()));
        out.push(RS);
    }
    out
}

/// `html` mode: a `<TR>`/`<TH>` header row (when `.headers` is on) and `<TR>`/`<TD>` data rows.
/// The shell writes `<TR>` on its own line, then each cell as `<TH>`/`<TD>` + escaped value on its
/// own line (no closing `</TH>`/`</TD>`), then `</TR>`. HTML-escapes `&`→`&amp;`, `"`→`&quot;`,
/// `'`→`&#39;`, and BOTH `<` and `>` → `&lt;` (matching `shell.c`'s `output_html_string`). NULL
/// data cells render as the literal `null` (the shell ignores `.nullvalue` here).
fn format_html(state: &ShellState, columns: &[String], rows: &[Vec<Value>]) -> String {
    fn emit_row(out: &mut String, cells: &[String], tag: &str) {
        out.push_str("<TR>\n");
        for cell in cells {
            out.push('<');
            out.push_str(tag);
            out.push('>');
            out.push_str(&html_escape(cell));
            out.push('\n');
        }
        out.push_str("</TR>\n");
    }

    let mut out = String::new();
    if state.headers {
        emit_row(&mut out, columns, "TH");
    }
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .map(|v| match v {
                Value::Null => "null".to_string(),
                _ => cell_text(state, v),
            })
            .collect();
        emit_row(&mut out, &cells, "TD");
    }
    out
}

/// `markdown` mode: a `| a | b |` header, a `|---|` rule row sized to each column (width + 2
/// dashes, no alignment colons), and `| v | v |` data rows. Header cells and all-numeric columns
/// are centered/right-justified; text columns are left-aligned — matching the sqlite3 shell.
fn format_markdown(state: &ShellState, columns: &[String], rows: &[Vec<Value>]) -> String {
    let ncol = columns.len();
    if ncol == 0 {
        return String::new();
    }
    let rendered = render_cells(state, rows);
    let width = column_widths(state, columns, &rendered, ncol);
    let numeric = numeric_columns(rows, ncol);

    let mut out = String::new();
    if state.headers {
        push_tabular_row(&mut out, columns, &width, &numeric, true, ("|", "|", "|"));
        // Rule row: width+2 dashes per column.
        out.push('|');
        for w in &width {
            out.push_str(&"-".repeat(w + 2));
            out.push('|');
        }
        out.push('\n');
    }
    for cells in &rendered {
        push_tabular_row(&mut out, cells, &width, &numeric, false, ("|", "|", "|"));
    }
    out
}

/// Box-drawing glyphs for a tabular border style (`box` vs legacy `table`).
///
/// The sqlite3 3.53.1 `box` style uses ROUNDED corners (`╭╮╰╯`) and a DOUBLE-line divider
/// (`╞═╪╡`) between the header and the body, distinct from the single-line top/bottom borders.
struct BoxGlyphs {
    /// Horizontal fill for the top/bottom borders.
    h: &'static str,
    /// Horizontal fill for the header/body divider.
    h2: &'static str,
    /// Vertical bar between cells.
    v: &'static str,
    tl: &'static str,
    tm: &'static str,
    tr: &'static str,
    /// Header/body divider: left, middle (cross), right.
    ml: &'static str,
    mm: &'static str,
    mr: &'static str,
    bl: &'static str,
    bm: &'static str,
    br: &'static str,
}

const BOX_GLYPHS: BoxGlyphs = BoxGlyphs {
    h: "─",
    h2: "═",
    v: "│",
    tl: "╭",
    tm: "┬",
    tr: "╮",
    ml: "╞",
    mm: "╪",
    mr: "╡",
    bl: "╰",
    bm: "┴",
    br: "╯",
};

const TABLE_GLYPHS: BoxGlyphs = BoxGlyphs {
    h: "-",
    h2: "-",
    v: "|",
    tl: "+",
    tm: "+",
    tr: "+",
    ml: "+",
    mm: "+",
    mr: "+",
    bl: "+",
    bm: "+",
    br: "+",
};

/// `box` mode: Unicode box-drawing borders around a columnar table. Header cells and all-numeric
/// columns are centered/right-justified; text columns are left-aligned (as in `markdown`/`column`).
/// The same machinery renders the legacy ASCII `table` style via [`TABLE_GLYPHS`].
fn format_box(
    state: &ShellState,
    columns: &[String],
    rows: &[Vec<Value>],
    g: &BoxGlyphs,
) -> String {
    let ncol = columns.len();
    if ncol == 0 {
        return String::new();
    }
    let rendered = render_cells(state, rows);
    let width = column_widths(state, columns, &rendered, ncol);
    let numeric = numeric_columns(rows, ncol);

    let border = |fill: &str, left: &str, mid: &str, right: &str| -> String {
        let mut s = String::new();
        s.push_str(left);
        for (i, w) in width.iter().enumerate() {
            if i > 0 {
                s.push_str(mid);
            }
            s.push_str(&fill.repeat(w + 2));
        }
        s.push_str(right);
        s.push('\n');
        s
    };

    let mut out = String::new();
    out.push_str(&border(g.h, g.tl, g.tm, g.tr));
    if state.headers {
        push_tabular_row(&mut out, columns, &width, &numeric, true, (g.v, g.v, g.v));
        out.push_str(&border(g.h2, g.ml, g.mm, g.mr));
    }
    for cells in &rendered {
        push_tabular_row(&mut out, cells, &width, &numeric, false, (g.v, g.v, g.v));
    }
    out.push_str(&border(g.h, g.bl, g.bm, g.br));
    out
}

/// `json` mode: an array of objects, one per row, joined by `,\n`. INTEGER/REAL become JSON
/// numbers (REAL via the engine's faithful text), TEXT becomes a JSON-escaped string, NULL is
/// `null`, and BLOB is a string of `\u00XX` escapes (one per byte) — matching the sqlite3 shell.
/// `.headers` is ignored.
fn format_json(columns: &[String], rows: &[Vec<Value>]) -> String {
    let mut out = String::from("[");
    for (r, row) in rows.iter().enumerate() {
        if r > 0 {
            out.push_str(",\n");
        }
        out.push('{');
        for (c, v) in row.iter().enumerate() {
            if c > 0 {
                out.push(',');
            }
            out.push_str(&json_string(
                columns.get(c).map(|s| s.as_str()).unwrap_or(""),
            ));
            out.push(':');
            out.push_str(&json_value(v));
        }
        out.push('}');
    }
    out.push_str("]\n");
    out
}

/// `insert` mode: one `INSERT INTO <table> VALUES(...);` statement per row. The sqlite3 shell emits
/// the column list ONLY when `.headers` is on; with headers off it omits it. The table name (and
/// each column name) is quoted as a SQL identifier only when it is not a plain identifier. Literals
/// follow SQL syntax: TEXT → `'...'` (or `unistr('..')` with control chars), BLOB → `x'..'`,
/// NULL → `NULL`, INTEGER/REAL bare.
fn format_insert(state: &ShellState, columns: &[String], rows: &[Vec<Value>]) -> String {
    let table = quote_identifier_if_needed(&state.insert_table);
    let col_list = if state.headers {
        let cols: Vec<String> = columns
            .iter()
            .map(|c| quote_identifier_if_needed(c))
            .collect();
        Some(format!("({})", cols.join(",")))
    } else {
        None
    };
    let mut out = String::new();
    for row in rows {
        out.push_str("INSERT INTO ");
        out.push_str(&table);
        if let Some(cl) = &col_list {
            out.push_str(cl);
        }
        out.push_str(" VALUES(");
        let cells: Vec<String> = row.iter().map(sql_literal).collect();
        out.push_str(&cells.join(","));
        out.push_str(");\n");
    }
    out
}

/// Pre-render every cell to its display text (honoring `.nullvalue`), one inner vec per row.
fn render_cells(state: &ShellState, rows: &[Vec<Value>]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| row.iter().map(|v| cell_text(state, v)).collect())
        .collect()
}

/// Per-column display width: the max over the (header, when `.headers` is on) and all cell texts.
fn column_widths(
    state: &ShellState,
    columns: &[String],
    rendered: &[Vec<String>],
    ncol: usize,
) -> Vec<usize> {
    let mut width = vec![0usize; ncol];
    if state.headers {
        for (c, name) in columns.iter().enumerate() {
            width[c] = display_width(name);
        }
    }
    for cells in rendered {
        for (c, cell) in cells.iter().enumerate() {
            width[c] = width[c].max(display_width(cell));
        }
    }
    width
}

/// Per-column "all-numeric" flag: true when every value in the column is INTEGER or REAL. Numeric
/// columns are right-justified in the columnar modes, matching the shell.
fn numeric_columns(rows: &[Vec<Value>], ncol: usize) -> Vec<bool> {
    let mut numeric = vec![true; ncol];
    for row in rows {
        for (c, v) in row.iter().enumerate() {
            if !matches!(v, Value::Int(_) | Value::Real(_)) {
                numeric[c] = false;
            }
        }
    }
    numeric
}

/// Emit one row of a framed tabular mode (`markdown`/`box`/`table`): `left cell sep cell … right`,
/// where each cell is ` ` + justified content + ` `. Header cells (and all-numeric data columns)
/// are centered/right-justified per `is_header`; text data columns are left-justified. `frame` is
/// `(left, separator, right)`.
fn push_tabular_row(
    out: &mut String,
    cells: &[String],
    width: &[usize],
    numeric: &[bool],
    is_header: bool,
    frame: (&str, &str, &str),
) {
    let (left, sep, right) = frame;
    out.push_str(left);
    for (c, cell) in cells.iter().enumerate() {
        if c > 0 {
            out.push_str(sep);
        }
        let w = width[c];
        let body = if is_header {
            center(cell, w)
        } else if numeric[c] {
            pad_left(cell, w)
        } else {
            pad_right(cell, w)
        };
        out.push(' ');
        out.push_str(&body);
        out.push(' ');
    }
    out.push_str(right);
    out.push('\n');
}

/// Quote a string as a SQL text literal. Plain text is single-quoted with embedded `'` doubled;
/// text containing control characters (bytes < 0x20) is emitted as `unistr('..\uXXXX..')`, where
/// non-control characters keep their literal form and `'` is doubled — matching `shell.c`.
fn quote_text(s: &str) -> String {
    if s.chars().any(|c| (c as u32) < 0x20) {
        let mut out = String::from("unistr('");
        for ch in s.chars() {
            match ch {
                '\'' => out.push_str("''"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push_str("')");
        out
    } else {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('\'');
        for ch in s.chars() {
            if ch == '\'' {
                out.push('\'');
            }
            out.push(ch);
        }
        out.push('\'');
        out
    }
}

/// Quote a blob as a SQL `x'..'` hex literal (lowercase `x` and hex digits, as the sqlite3 shell).
fn quote_blob(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2 + 3);
    out.push_str("x'");
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out.push('\'');
    out
}

/// Render a value as a SQL literal (for `quote`/`insert` modes): TEXT → `'...'`/`unistr('..')`,
/// BLOB → `x'..'`, NULL → `NULL`, INTEGER/REAL bare via the engine's faithful text.
fn sql_literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Int(i) => i.to_string(),
        Value::Real(_) => v.to_text().unwrap_or_default(),
        Value::Text(s) => quote_text(s),
        Value::Blob(b) => quote_blob(b),
    }
}

/// Quote an identifier (table/column name) as a SQL identifier only when it is not already a plain
/// identifier: an ASCII letter or `_` followed by ASCII alphanumerics/`_`, and not a SQL keyword.
/// Otherwise it is wrapped in double quotes (doubling embedded `"`), matching `shell.c`.
fn quote_identifier_if_needed(name: &str) -> String {
    let is_plain = {
        let mut chars = name.chars();
        let head_ok = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_');
        head_ok && chars.all(|c| c.is_ascii_alphanumeric() || c == '_') && !is_sql_keyword(name)
    };
    if is_plain {
        name.to_string()
    } else {
        let mut out = String::with_capacity(name.len() + 2);
        out.push('"');
        for ch in name.chars() {
            if ch == '"' {
                out.push('"');
            }
            out.push(ch);
        }
        out.push('"');
        out
    }
}

/// SQL keywords that the shell quotes when used as an identifier in `insert` mode (the subset the
/// shell's `sqlite3_keyword_check` would flag — notably `table`, the value of `.mode insert table`).
/// The list is kept sorted so the lookup can use a binary search.
fn is_sql_keyword(name: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "abort",
        "action",
        "add",
        "after",
        "all",
        "alter",
        "always",
        "analyze",
        "and",
        "as",
        "asc",
        "attach",
        "autoincrement",
        "before",
        "begin",
        "between",
        "by",
        "cascade",
        "case",
        "cast",
        "check",
        "collate",
        "column",
        "commit",
        "conflict",
        "constraint",
        "create",
        "cross",
        "current",
        "current_date",
        "current_time",
        "current_timestamp",
        "database",
        "default",
        "deferrable",
        "deferred",
        "delete",
        "desc",
        "detach",
        "distinct",
        "do",
        "drop",
        "each",
        "else",
        "end",
        "escape",
        "except",
        "exclude",
        "exclusive",
        "exists",
        "explain",
        "fail",
        "filter",
        "first",
        "following",
        "for",
        "foreign",
        "from",
        "full",
        "generated",
        "glob",
        "group",
        "groups",
        "having",
        "if",
        "ignore",
        "immediate",
        "in",
        "index",
        "indexed",
        "initially",
        "inner",
        "insert",
        "instead",
        "intersect",
        "into",
        "is",
        "isnull",
        "join",
        "key",
        "last",
        "left",
        "like",
        "limit",
        "match",
        "materialized",
        "natural",
        "no",
        "not",
        "nothing",
        "notnull",
        "null",
        "nulls",
        "of",
        "offset",
        "on",
        "or",
        "order",
        "others",
        "outer",
        "over",
        "partition",
        "plan",
        "pragma",
        "preceding",
        "primary",
        "query",
        "raise",
        "range",
        "recursive",
        "references",
        "regexp",
        "reindex",
        "release",
        "rename",
        "replace",
        "restrict",
        "returning",
        "right",
        "rollback",
        "row",
        "rows",
        "savepoint",
        "select",
        "set",
        "table",
        "temp",
        "temporary",
        "then",
        "ties",
        "to",
        "transaction",
        "trigger",
        "unbounded",
        "union",
        "unique",
        "update",
        "using",
        "vacuum",
        "values",
        "view",
        "virtual",
        "when",
        "where",
        "window",
        "with",
        "without",
    ];
    let lower = name.to_ascii_lowercase();
    KEYWORDS.binary_search(&lower.as_str()).is_ok()
}

/// HTML-escape a cell for `html` mode: `&`→`&amp;`, `"`→`&quot;`, `'`→`&#39;`, and both `<` and
/// `>` → `&lt;` (matching `shell.c`'s `output_html_string`, which maps `>` to `&lt;`).
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' | '>' => out.push_str("&lt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// JSON-escape a string and wrap it in double quotes, matching `shell.c`'s `output_json_string`:
/// `"`, `\`, and the named controls (`\b \f \n \r \t`) use short escapes; other control bytes use
/// `\u00XX`.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Render a value as a JSON value for `json` mode: INTEGER/REAL as bare numbers (REAL via the
/// engine's faithful text), TEXT as a JSON string, NULL as `null`, and BLOB as a JSON string of
/// `\u00XX` escapes (one per byte) — matching the sqlite3 shell.
fn json_value(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Int(i) => i.to_string(),
        Value::Real(_) => v.to_text().unwrap_or_default(),
        Value::Text(s) => json_string(s),
        Value::Blob(b) => {
            let mut out = String::with_capacity(b.len() * 6 + 2);
            out.push('"');
            for byte in b {
                out.push_str(&format!("\\u{:04x}", *byte as u32));
            }
            out.push('"');
            out
        }
    }
}

fn format_column(state: &ShellState, columns: &[String], rows: &[Vec<Value>]) -> String {
    let ncol = columns.len();
    if ncol == 0 {
        return String::new();
    }

    // Pre-render every cell and track per-column width and whether the column is all-numeric
    // (numeric columns are right-justified, matching the shell).
    let rendered = render_cells(state, rows);
    let width = column_widths(state, columns, &rendered, ncol);
    let numeric = numeric_columns(rows, ncol);

    let mut out = String::new();
    let push_row = |out: &mut String, cells: &[String], justify_numeric: bool| {
        let mut line = String::new();
        for (c, cell) in cells.iter().enumerate() {
            if c > 0 {
                line.push_str("  ");
            }
            let w = width[c];
            if justify_numeric && numeric[c] {
                line.push_str(&pad_left(cell, w));
            } else {
                line.push_str(&pad_right(cell, w));
            }
        }
        // Trailing spaces on a line are trimmed (as the shell does).
        out.push_str(line.trim_end());
        out.push('\n');
    };

    if state.headers {
        // Header row: each name centered in its column.
        let centered: Vec<String> = columns
            .iter()
            .enumerate()
            .map(|(c, name)| center(name, width[c]))
            .collect();
        push_row(&mut out, &centered, false);
        // Separator row of dashes.
        let dashes: Vec<String> = width.iter().map(|w| "-".repeat(*w)).collect();
        push_row(&mut out, &dashes, false);
    }
    for cells in &rendered {
        push_row(&mut out, cells, true);
    }
    out
}

fn display_width(s: &str) -> usize {
    s.chars().count()
}

fn pad_right(s: &str, w: usize) -> String {
    let n = display_width(s);
    if n >= w {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(w - n))
    }
}

fn pad_left(s: &str, w: usize) -> String {
    let n = display_width(s);
    if n >= w {
        s.to_string()
    } else {
        format!("{}{s}", " ".repeat(w - n))
    }
}

fn center(s: &str, w: usize) -> String {
    let n = display_width(s);
    if n >= w {
        return s.to_string();
    }
    let pad = w - n;
    let left = pad / 2;
    let right = pad - left;
    format!("{}{s}{}", " ".repeat(left), " ".repeat(right))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;

    fn state(mode: OutputMode, headers: bool) -> ShellState {
        use clap::Parser;
        let mut s = ShellState::from_cli(&Cli::parse_from(["rustsqlite"]));
        s.mode = mode;
        s.headers = headers;
        s
    }

    fn rows() -> (Vec<String>, Vec<Vec<Value>>) {
        (
            vec!["a".into(), "b".into()],
            vec![
                vec![Value::Int(2), Value::Text("yy".into())],
                vec![Value::Int(1), Value::Text("x".into())],
                vec![Value::Int(100), Value::Text("zzz".into())],
            ],
        )
    }

    #[test]
    fn list_mode_matches_sqlite3() {
        let (cols, r) = rows();
        let s = state(OutputMode::List, false);
        assert_eq!(
            format_rows(OutputMode::List, &s, &cols, &r),
            "2|yy\n1|x\n100|zzz\n"
        );
    }

    #[test]
    fn column_mode_aligns_like_sqlite3() {
        let (cols, r) = rows();
        let s = state(OutputMode::Column, true);
        // Numeric column right-justified, text left-justified, header centered, dashes under.
        assert_eq!(
            format_rows(OutputMode::Column, &s, &cols, &r),
            " a    b\n---  ---\n  2  yy\n  1  x\n100  zzz\n"
        );
    }

    #[test]
    fn csv_mode_quotes_and_uses_crlf() {
        let cols = vec!["x".into()];
        let r = vec![
            vec![Value::Text("a,b".into())],
            vec![Value::Text("he \"q\"".into())],
            vec![Value::Null],
        ];
        let s = state(OutputMode::Csv, false);
        assert_eq!(
            format_rows(OutputMode::Csv, &s, &cols, &r),
            "\"a,b\"\r\n\"he \"\"q\"\"\"\r\n\r\n"
        );
    }

    // ---- M3b golden tests --------------------------------------------------------------------
    //
    // Every expected string below was captured byte-for-byte from the sqlite3 3.53.1 shell
    // (`/usr/bin/sqlite3`, dumped via `od -c` / python `repr`) over this fixture:
    //
    //     CREATE TABLE t(a, b, c);
    //     INSERT INTO t VALUES(2,'yy',NULL),(1,'x',3.5),(100,'zzz','hi');
    //     SELECT * FROM t;          -- with .headers on
    //
    // so the columns are `a`,`b`,`c` and the rows mix an INTEGER, a TEXT, a NULL, and a REAL.

    /// Rebuild the oracle fixture (`SELECT * FROM t`) as engine values.
    fn fixture() -> (Vec<String>, Vec<Vec<Value>>) {
        (
            vec!["a".into(), "b".into(), "c".into()],
            vec![
                vec![Value::Int(2), Value::Text("yy".into()), Value::Null],
                vec![Value::Int(1), Value::Text("x".into()), Value::Real(3.5)],
                vec![
                    Value::Int(100),
                    Value::Text("zzz".into()),
                    Value::Text("hi".into()),
                ],
            ],
        )
    }

    #[test]
    fn line_mode_matches_oracle() {
        let (cols, r) = fixture();
        let s = state(OutputMode::Line, true);
        assert_eq!(
            format_rows(OutputMode::Line, &s, &cols, &r),
            "a: 2\nb: yy\nc: \n\na: 1\nb: x\nc: 3.5\n\na: 100\nb: zzz\nc: hi\n"
        );
    }

    #[test]
    fn line_mode_right_aligns_names() {
        // Names of differing width are right-aligned to the widest; the `: ` follows.
        let cols = vec!["a".into(), "bcdef".into()];
        let r = vec![vec![Value::Int(1), Value::Int(2)]];
        let s = state(OutputMode::Line, true);
        assert_eq!(
            format_rows(OutputMode::Line, &s, &cols, &r),
            "    a: 1\nbcdef: 2\n"
        );
    }

    #[test]
    fn tabs_mode_matches_oracle() {
        let (cols, r) = fixture();
        let s = state(OutputMode::Tabs, true);
        assert_eq!(
            format_rows(OutputMode::Tabs, &s, &cols, &r),
            "a\tb\tc\n2\tyy\t\n1\tx\t3.5\n100\tzzz\thi\n"
        );
    }

    #[test]
    fn quote_mode_matches_oracle() {
        let (cols, r) = fixture();
        let s = state(OutputMode::Quote, true);
        // Headers are single-quoted; NULL is the keyword; REAL/INT bare.
        assert_eq!(
            format_rows(OutputMode::Quote, &s, &cols, &r),
            "'a','b','c'\n2,'yy',NULL\n1,'x',3.5\n100,'zzz','hi'\n"
        );
    }

    #[test]
    fn quote_mode_blob_and_escapes() {
        // x'00ff', 'a''b', NULL, 3.5 → BLOB hex literal, doubled quote, keyword NULL, bare REAL.
        let cols = vec!["bl".into(), "s".into(), "n".into(), "r".into()];
        let r = vec![vec![
            Value::Blob(vec![0x00, 0xff]),
            Value::Text("a'b".into()),
            Value::Null,
            Value::Real(3.5),
        ]];
        let s = state(OutputMode::Quote, false);
        assert_eq!(
            format_rows(OutputMode::Quote, &s, &cols, &r),
            "x'00ff','a''b',NULL,3.5\n"
        );
    }

    #[test]
    fn quote_mode_unistr_for_control_chars() {
        // Oracle: text containing a control char uses unistr('..\uXXXX..').
        let cols = vec!["v".into()];
        let r = vec![vec![Value::Text("tab\tt".into())]];
        let s = state(OutputMode::Quote, false);
        assert_eq!(
            format_rows(OutputMode::Quote, &s, &cols, &r),
            "unistr('tab\\u0009t')\n"
        );
    }

    #[test]
    fn ascii_mode_matches_oracle() {
        let (cols, r) = fixture();
        let s = state(OutputMode::Ascii, true);
        assert_eq!(
            format_rows(OutputMode::Ascii, &s, &cols, &r),
            "a\u{1f}b\u{1f}c\u{1e}2\u{1f}yy\u{1f}\u{1e}1\u{1f}x\u{1f}3.5\u{1e}100\u{1f}zzz\u{1f}hi\u{1e}"
        );
    }

    #[test]
    fn html_mode_matches_oracle() {
        let (cols, r) = fixture();
        let s = state(OutputMode::Html, true);
        // <TR> on its own line; opening tags only; NULL → literal `null`.
        assert_eq!(
            format_rows(OutputMode::Html, &s, &cols, &r),
            "<TR>\n<TH>a\n<TH>b\n<TH>c\n</TR>\n\
             <TR>\n<TD>2\n<TD>yy\n<TD>null\n</TR>\n\
             <TR>\n<TD>1\n<TD>x\n<TD>3.5\n</TR>\n\
             <TR>\n<TD>100\n<TD>zzz\n<TD>hi\n</TR>\n"
        );
    }

    #[test]
    fn html_mode_escapes() {
        // Oracle: '<>&"a'' → &lt;&lt;&amp;&quot;a&#39;  (both < and > map to &lt;).
        let cols = vec!["v".into()];
        let r = vec![vec![Value::Text("<>&\"a'".into())]];
        let s = state(OutputMode::Html, true);
        assert_eq!(
            format_rows(OutputMode::Html, &s, &cols, &r),
            "<TR>\n<TH>v\n</TR>\n<TR>\n<TD>&lt;&lt;&amp;&quot;a&#39;\n</TR>\n"
        );
    }

    #[test]
    fn html_mode_null_is_literal_null() {
        // A NULL data cell renders as the literal `null` (ignoring .nullvalue); empty string empty.
        let cols = vec!["e".into(), "n".into()];
        let r = vec![vec![Value::Text(String::new()), Value::Null]];
        let mut s = state(OutputMode::Html, false);
        s.nullvalue = "ZZ".into();
        assert_eq!(
            format_rows(OutputMode::Html, &s, &cols, &r),
            "<TR>\n<TD>\n<TD>null\n</TR>\n"
        );
    }

    #[test]
    fn markdown_mode_matches_oracle() {
        let (cols, r) = fixture();
        let s = state(OutputMode::Markdown, true);
        assert_eq!(
            format_rows(OutputMode::Markdown, &s, &cols, &r),
            "|  a  |  b  |  c  |\n\
             |-----|-----|-----|\n\
             |   2 | yy  |     |\n\
             |   1 | x   | 3.5 |\n\
             | 100 | zzz | hi  |\n"
        );
    }

    #[test]
    fn box_mode_matches_oracle() {
        let (cols, r) = fixture();
        let s = state(OutputMode::Boxed, true);
        // Rounded corners (╭╮╰╯), double-line header divider (╞═╪╡), single-line top/bottom.
        assert_eq!(
            format_rows(OutputMode::Boxed, &s, &cols, &r),
            "╭─────┬─────┬─────╮\n\
             │  a  │  b  │  c  │\n\
             ╞═════╪═════╪═════╡\n\
             │   2 │ yy  │     │\n\
             │   1 │ x   │ 3.5 │\n\
             │ 100 │ zzz │ hi  │\n\
             ╰─────┴─────┴─────╯\n"
        );
    }

    #[test]
    fn table_mode_matches_oracle() {
        let (cols, r) = fixture();
        let s = state(OutputMode::Table, true);
        assert_eq!(
            format_rows(OutputMode::Table, &s, &cols, &r),
            "+-----+-----+-----+\n\
             |  a  |  b  |  c  |\n\
             +-----+-----+-----+\n\
             |   2 | yy  |     |\n\
             |   1 | x   | 3.5 |\n\
             | 100 | zzz | hi  |\n\
             +-----+-----+-----+\n"
        );
    }

    #[test]
    fn json_mode_matches_oracle() {
        let (cols, r) = fixture();
        let s = state(OutputMode::Json, true);
        assert_eq!(
            format_rows(OutputMode::Json, &s, &cols, &r),
            "[{\"a\":2,\"b\":\"yy\",\"c\":null},\n\
             {\"a\":1,\"b\":\"x\",\"c\":3.5},\n\
             {\"a\":100,\"b\":\"zzz\",\"c\":\"hi\"}]\n"
        );
    }

    #[test]
    fn json_mode_blob_and_escapes() {
        // Oracle: x'00ff' → " ÿ"; 'a"b' → "a\"b"; control bytes → short escapes.
        let cols = vec!["bl".into(), "s".into(), "ctl".into()];
        let r = vec![vec![
            Value::Blob(vec![0x00, 0xff]),
            Value::Text("a\"b".into()),
            Value::Text("\u{01}\u{08}\u{0c}\r\t\n\\".into()),
        ]];
        let s = state(OutputMode::Json, false);
        assert_eq!(
            format_rows(OutputMode::Json, &s, &cols, &r),
            "[{\"bl\":\"\\u0000\\u00ff\",\"s\":\"a\\\"b\",\"ctl\":\"\\u0001\\b\\f\\r\\t\\n\\\\\"}]\n"
        );
    }

    #[test]
    fn insert_mode_with_headers_emits_column_list() {
        let (cols, r) = fixture();
        let mut s = state(OutputMode::Insert, true);
        s.insert_table = "mytab".into();
        assert_eq!(
            format_rows(OutputMode::Insert, &s, &cols, &r),
            "INSERT INTO mytab(a,b,c) VALUES(2,'yy',NULL);\n\
             INSERT INTO mytab(a,b,c) VALUES(1,'x',3.5);\n\
             INSERT INTO mytab(a,b,c) VALUES(100,'zzz','hi');\n"
        );
    }

    #[test]
    fn insert_mode_no_column_list_without_headers() {
        // With .headers off (the default) the oracle omits the column list; default table is "tab".
        let (cols, r) = fixture();
        let mut s = state(OutputMode::Insert, false);
        s.insert_table = "tab".into();
        assert_eq!(
            format_rows(OutputMode::Insert, &s, &cols, &r),
            "INSERT INTO tab VALUES(2,'yy',NULL);\n\
             INSERT INTO tab VALUES(1,'x',3.5);\n\
             INSERT INTO tab VALUES(100,'zzz','hi');\n"
        );
    }

    #[test]
    fn insert_mode_quotes_keyword_table_and_blob() {
        // Table name "table" is a keyword → double-quoted. BLOB → x'..'.
        let cols = vec!["bl".into()];
        let r = vec![vec![Value::Blob(vec![0x00, 0xff])]];
        let mut s = state(OutputMode::Insert, false);
        s.insert_table = "table".into();
        assert_eq!(
            format_rows(OutputMode::Insert, &s, &cols, &r),
            "INSERT INTO \"table\" VALUES(x'00ff');\n"
        );
    }

    #[test]
    fn insert_mode_quotes_column_name_with_space() {
        // With headers on, a column name containing a space is double-quoted.
        let cols = vec!["co l".into(), "x".into()];
        let r = vec![vec![Value::Int(1), Value::Int(2)]];
        let mut s = state(OutputMode::Insert, true);
        s.insert_table = "dest".into();
        assert_eq!(
            format_rows(OutputMode::Insert, &s, &cols, &r),
            "INSERT INTO dest(\"co l\",x) VALUES(1,2);\n"
        );
    }
}
