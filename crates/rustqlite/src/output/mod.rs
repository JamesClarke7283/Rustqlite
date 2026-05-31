//! Output formatting (mirrors the per-mode rendering in `shell.c`).
//!
//! Row/column rendering for the various output modes (`list`, `csv`, `column`, `box`, …)
//! lands with the query path in M3, when `SELECT` produces rows. For M1 the only structured
//! output is `.tables`, rendered with `shell.c`'s column-major grid algorithm below.
//!
//! NOTE: the *arrangement* (which name lands in which row/column) matches `shell.c` exactly;
//! the exact inter-column spacing has drifted slightly across SQLite versions, so byte-exact
//! `.tables` spacing parity is tracked for the M9 conformance pass. The engine-level schema
//! read (what `.tables` is built on) is the tested guarantee.
//!
//! Result-row rendering for `SELECT` output lives in [`format`].

pub mod format;

pub use format::format_rows;

use rustsqlite_core::Value;

/// Render an `EXPLAIN QUERY PLAN` result set as the sqlite3 shell's `QUERY PLAN` tree.
///
/// This is a faithful port of `shell.c`'s `eqp_render` / `eqp_render_level`. The input `rows` are
/// the BARE `id|parent|notused|detail` rows the C-API returns (no tree characters). Output is a
/// `QUERY PLAN` header line followed by one line per node, each prefixed with `|--`/`` `-- `` per
/// the shell's algorithm — byte-for-byte matching the oracle for the supported plan shapes.
pub fn render_eqp_tree(rows: &[Vec<Value>]) -> String {
    // Parse the (id, parent, detail) triples out of the raw rows.
    let nodes: Vec<EqpNode> = rows
        .iter()
        .map(|r| EqpNode {
            id: cell_i64(r, 0),
            parent: cell_i64(r, 1),
            detail: cell_text(r, 3),
        })
        .collect();

    if nodes.is_empty() {
        return String::new();
    }
    let mut out = String::from("QUERY PLAN\n");
    render_eqp_level(&nodes, 0, "", &mut out);
    out
}

/// One parsed EXPLAIN QUERY PLAN node.
struct EqpNode {
    id: i64,
    parent: i64,
    detail: String,
}

/// Render every node whose parent is `parent_id`, recursing into children. Mirrors
/// `eqp_render_level`: a node with a following sibling uses `|--` (and `|  ` indent for its
/// children); the last sibling uses `` `-- `` (and `   ` indent).
fn render_eqp_level(nodes: &[EqpNode], parent_id: i64, prefix: &str, out: &mut String) {
    let children: Vec<&EqpNode> = nodes.iter().filter(|n| n.parent == parent_id).collect();
    for (i, node) in children.iter().enumerate() {
        let has_next = i + 1 < children.len();
        let connector = if has_next { "|--" } else { "`--" };
        out.push_str(prefix);
        out.push_str(connector);
        out.push_str(&node.detail);
        out.push('\n');
        // Recurse into this node's children with the appropriate continuation indent.
        if nodes.iter().any(|n| n.parent == node.id) {
            let child_prefix = format!("{prefix}{}", if has_next { "|  " } else { "   " });
            render_eqp_level(nodes, node.id, &child_prefix, out);
        }
    }
}

/// Render a plain `EXPLAIN` (bytecode) result set as a columnar table with headers, regardless of
/// the active `.mode` — matching the shell, which always shows EXPLAIN as a column table. Columns
/// are left-justified and sized to their widest cell (header included), separated by two spaces;
/// a dashed rule sits under the header. EXPLAIN rows are only INTEGER/TEXT, never NULL.
pub fn render_explain_bytecode(columns: &[String], rows: &[Vec<Value>]) -> String {
    let ncol = columns.len();
    if ncol == 0 {
        return String::new();
    }
    let rendered: Vec<Vec<String>> = rows
        .iter()
        .map(|row| (0..ncol).map(|c| cell_text(row, c)).collect())
        .collect();
    let mut width = vec![0usize; ncol];
    for (c, name) in columns.iter().enumerate() {
        width[c] = name.chars().count();
    }
    for cells in &rendered {
        for (c, cell) in cells.iter().enumerate() {
            width[c] = width[c].max(cell.chars().count());
        }
    }

    let push_row = |out: &mut String, cells: &[String]| {
        let mut line = String::new();
        for (c, cell) in cells.iter().enumerate() {
            if c > 0 {
                line.push_str("  ");
            }
            let pad = width[c].saturating_sub(cell.chars().count());
            line.push_str(cell);
            line.push_str(&" ".repeat(pad));
        }
        out.push_str(line.trim_end());
        out.push('\n');
    };

    let mut out = String::new();
    push_row(&mut out, columns);
    let dashes: Vec<String> = width.iter().map(|w| "-".repeat(*w)).collect();
    push_row(&mut out, &dashes);
    for cells in &rendered {
        push_row(&mut out, cells);
    }
    out
}

/// The text of a cell (column `i` of `row`): INTEGER/REAL/TEXT/BLOB rendered, NULL as empty.
fn cell_text(row: &[Value], i: usize) -> String {
    row.get(i).and_then(|v| v.to_text()).unwrap_or_default()
}

/// The integer value of column `i` of `row` (0 when absent or non-integer).
fn cell_i64(row: &[Value], i: usize) -> i64 {
    match row.get(i) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    }
}

/// Render a list of names the way `.tables` does: sorted, packed column-major into an
/// 80-column-wide grid, each cell left-justified to the longest name with two-space gaps.
pub fn tables_grid(names: &[String]) -> String {
    if names.is_empty() {
        return String::new();
    }
    let maxlen = names.iter().map(|n| n.len()).max().unwrap_or(0);
    let n_row_count = names.len();
    let mut n_print_col = 80 / (maxlen + 2);
    if n_print_col < 1 {
        n_print_col = 1;
    }
    let n_print_row = n_row_count.div_ceil(n_print_col);

    let mut out = String::new();
    for i in 0..n_print_row {
        let mut j = i;
        while j < n_row_count {
            let gap = if j < n_print_row { "" } else { "  " };
            out.push_str(gap);
            // Left-justify to maxlen (the final column is not padded in shell.c, but trailing
            // padding is harmless and matched by trimming below).
            out.push_str(&format!("{:<width$}", names[j], width = maxlen));
            j += n_print_row;
        }
        // Trim trailing spaces from the last cell of the row, matching shell.c output.
        while out.ends_with(' ') {
            out.pop();
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build raw EQP rows `[id, parent, notused, detail]`.
    fn eqp_rows(triples: &[(i64, i64, &str)]) -> Vec<Vec<Value>> {
        triples
            .iter()
            .map(|(id, parent, detail)| {
                vec![
                    Value::Int(*id),
                    Value::Int(*parent),
                    Value::Int(0),
                    Value::Text((*detail).to_string()),
                ]
            })
            .collect()
    }

    #[test]
    fn eqp_tree_single_scan_matches_oracle() {
        // Oracle: "QUERY PLAN\n`--SCAN t\n"
        let rows = eqp_rows(&[(1, 0, "SCAN t")]);
        assert_eq!(render_eqp_tree(&rows), "QUERY PLAN\n`--SCAN t\n");
    }

    #[test]
    fn eqp_tree_scan_then_order_by_matches_oracle() {
        // Oracle: "QUERY PLAN\n|--SCAN t\n`--USE TEMP B-TREE FOR ORDER BY\n"
        let rows = eqp_rows(&[(1, 0, "SCAN t"), (2, 0, "USE TEMP B-TREE FOR ORDER BY")]);
        assert_eq!(
            render_eqp_tree(&rows),
            "QUERY PLAN\n|--SCAN t\n`--USE TEMP B-TREE FOR ORDER BY\n"
        );
    }

    #[test]
    fn eqp_tree_constant_row() {
        let rows = eqp_rows(&[(1, 0, "SCAN CONSTANT ROW")]);
        assert_eq!(render_eqp_tree(&rows), "QUERY PLAN\n`--SCAN CONSTANT ROW\n");
    }

    #[test]
    fn eqp_tree_nested_child_indents() {
        // A child (parent=1) renders nested under its parent with the `   ` continuation indent.
        let rows = eqp_rows(&[(1, 0, "SCAN t"), (2, 1, "USE INDEX")]);
        assert_eq!(
            render_eqp_tree(&rows),
            "QUERY PLAN\n`--SCAN t\n   `--USE INDEX\n"
        );
    }

    #[test]
    fn explain_bytecode_columnar() {
        let columns = vec!["addr".to_string(), "opcode".to_string(), "p1".to_string()];
        let rows = vec![
            vec![Value::Int(0), Value::Text("Init".into()), Value::Int(0)],
            vec![Value::Int(1), Value::Text("Halt".into()), Value::Int(0)],
        ];
        // Header, dashed rule, then rows; columns left-justified, 2-space gaps, trailing trimmed.
        assert_eq!(
            render_explain_bytecode(&columns, &rows),
            "addr  opcode  p1\n----  ------  --\n0     Init    0\n1     Halt    0\n"
        );
    }

    #[test]
    fn single_name() {
        assert_eq!(tables_grid(&["t".to_string()]), "t\n");
    }

    #[test]
    fn two_names_same_row() {
        // Both fit within 80 columns => one print row, column-major order alpha,beta.
        let grid = tables_grid(&["alpha".to_string(), "beta".to_string()]);
        assert_eq!(grid, "alpha  beta\n");
    }

    #[test]
    fn empty_is_empty() {
        assert_eq!(tables_grid(&[]), "");
    }
}
