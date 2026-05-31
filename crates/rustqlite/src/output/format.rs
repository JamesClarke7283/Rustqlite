//! Result-row formatting for the shell's output modes (mirrors the per-mode rendering in
//! `shell.c`).
//!
//! M3a implements `list`, `csv`, and `column`; the remaining modes fall back to `list` with a
//! one-line note (added in M3b). Values are rendered to text by the engine
//! ([`Value::to_text`]) so REAL formatting matches `sqlite3_column_text` exactly; NULL uses the
//! shell's `.nullvalue` string.

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
        // Other modes are not implemented yet; fall back to list with a note.
        other => {
            let mut out = format!(
                "-- (output mode {other:?} is not implemented yet in M3a; showing 'list')\n"
            );
            out.push_str(&format_list(state, columns, rows));
            out
        }
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
/// a comma, double quote, CR, or LF — matching `shell.c`'s `output_csv`.
fn csv_quote(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r']) {
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

fn format_column(state: &ShellState, columns: &[String], rows: &[Vec<Value>]) -> String {
    let ncol = columns.len();
    if ncol == 0 {
        return String::new();
    }

    // Pre-render every cell and track per-column width and whether the column is all-numeric
    // (numeric columns are right-justified, matching the shell).
    let rendered: Vec<Vec<String>> = rows
        .iter()
        .map(|row| row.iter().map(|v| cell_text(state, v)).collect())
        .collect();
    let mut width = vec![0usize; ncol];
    let mut numeric = vec![true; ncol];
    if state.headers {
        for (c, name) in columns.iter().enumerate() {
            width[c] = display_width(name);
        }
    }
    for row in rows {
        for (c, v) in row.iter().enumerate() {
            if !matches!(v, Value::Int(_) | Value::Real(_)) {
                numeric[c] = false;
            }
        }
    }
    for cells in &rendered {
        for (c, cell) in cells.iter().enumerate() {
            width[c] = width[c].max(display_width(cell));
        }
    }

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
}
