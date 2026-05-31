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
