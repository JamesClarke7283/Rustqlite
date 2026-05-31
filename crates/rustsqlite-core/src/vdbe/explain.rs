//! `EXPLAIN` / `EXPLAIN QUERY PLAN` rendering (mirrors the EXPLAIN output paths in `vdbeaux.c`
//! and the high-level query-plan emission in `select.c` / `where.c`).
//!
//! Two row sets are produced, each a `Vec<Vec<Value>>` ready to feed straight back through the
//! C-API column accessors (a `Backing::Static` prepared statement):
//!
//!   * [`bytecode_rows`] — the `addr|opcode|p1|p2|p3|p4|p5|comment` listing for plain `EXPLAIN`.
//!     This is NOT differentially compared to upstream: rustqlite's register allocation and lack
//!     of constant hoisting legitimately differ, so plain `EXPLAIN` is pinned by golden tests on
//!     our own program (see `tests/explain.rs`), not opcode-for-opcode against the oracle.
//!   * [`query_plan_rows`] — the `id|parent|notused|detail` rows for `EXPLAIN QUERY PLAN`. The
//!     `detail` strings here ARE matched to the oracle's exact wording (`SCAN t`,
//!     `USE TEMP B-TREE FOR ORDER BY`, `SCAN CONSTANT ROW`); they are emitted BARE (no tree
//!     characters). The CLI reproduces the shell's tree rendering on top of these rows.

use rustqlite_parser::SelectStmt;

use crate::types::Value;
use crate::util::fp::fp_to_text;

use super::program::{Instruction, Program, P4};

/// The eight column headers for a plain `EXPLAIN` (bytecode) result set.
pub const BYTECODE_HEADER: [&str; 8] = ["addr", "opcode", "p1", "p2", "p3", "p4", "p5", "comment"];

/// The four column headers for an `EXPLAIN QUERY PLAN` result set.
pub const QUERY_PLAN_HEADER: [&str; 4] = ["id", "parent", "notused", "detail"];

/// Render a compiled [`Program`] as the plain-`EXPLAIN` bytecode listing: one row per instruction
/// in program order, each row `[addr, opcode, p1, p2, p3, p4, p5, comment]`.
pub fn bytecode_rows(program: &Program) -> Vec<Vec<Value>> {
    program
        .instructions
        .iter()
        .enumerate()
        .map(|(addr, inst)| {
            vec![
                Value::Int(addr as i64),
                Value::Text(inst.opcode.name().to_string()),
                Value::Int(inst.p1 as i64),
                Value::Int(inst.p2 as i64),
                Value::Int(inst.p3 as i64),
                Value::Text(render_p4(&inst.p4)),
                Value::Int(inst.p5 as i64),
                Value::Text(synopsis(inst)),
            ]
        })
        .collect()
}

/// Render a single instruction's `p4` operand the way upstream's `displayP4` does, as far as the
/// P4 variants the codegen emits go:
///   * `Symbol`/`Text` → the bare string,
///   * `Int` → its decimal text,
///   * `Real` → the engine's faithful REAL→text formatter,
///   * `KeyInfo` → upstream's `k(n,...)` form (`-` prefix per DESC field, `B` for BINARY collation),
///   * `Blob`/`None` → the empty string.
pub fn render_p4(p4: &P4) -> String {
    match p4 {
        P4::None => String::new(),
        P4::Int(i) => i.to_string(),
        P4::Real(r) => fp_to_text(*r),
        P4::Text(s) | P4::Symbol(s) => s.clone(),
        P4::Blob(_) => String::new(),
        P4::KeyInfo(fields) => {
            // displayP4 renders KeyInfo as "k(N,<f1>,<f2>,...)" where each field is the collation
            // name prefixed with "-" when that key sorts DESC. BINARY renders as "B".
            let mut out = format!("k({}", fields.len());
            for f in fields {
                out.push(',');
                if f.desc {
                    out.push('-');
                }
                out.push_str(collation_token(f));
            }
            out.push(')');
            out
        }
    }
}

/// The single-letter collation token displayP4 uses (BINARY → "B"); other collations use their
/// name. The codegen only ever attaches BINARY today.
fn collation_token(field: &super::program::KeyField) -> &'static str {
    use crate::types::Collation;
    match field.collation {
        Collation::Binary => "B",
        Collation::NoCase => "NOCASE",
        Collation::RTrim => "RTRIM",
    }
}

/// A short, best-effort `comment` synopsis close to vdbe.c's per-opcode synopsis comments, for the
/// opcodes our codegen actually emits. Comments are NOT differentially tested, so anything we have
/// no synopsis for is left blank.
fn synopsis(inst: &Instruction) -> String {
    use super::opcode::Opcode::*;
    let (p1, p2, p3) = (inst.p1, inst.p2, inst.p3);
    match inst.opcode {
        Init => format!("Start at {p2}"),
        Column => format!("r[{p3}]=cursor {p1} column {p2}"),
        ResultRow => {
            if p2 == 1 {
                format!("output=r[{p1}]")
            } else {
                format!("output=r[{p1}..{}]", p1 + p2 - 1)
            }
        }
        Integer => format!("r[{p2}]={p1}"),
        Int64 => format!("r[{p2}]={}", render_p4(&inst.p4)),
        Real => format!("r[{p2}]={}", render_p4(&inst.p4)),
        String8 => format!("r[{p2}]='{}'", render_p4(&inst.p4)),
        Null => {
            if p3 > p2 {
                format!("r[{p2}..{p3}]=NULL")
            } else {
                format!("r[{p2}]=NULL")
            }
        }
        Blob => format!("r[{p2}]=blob"),
        Add => format!("r[{p3}]=r[{p2}]+r[{p1}]"),
        Subtract => format!("r[{p3}]=r[{p2}]-r[{p1}]"),
        Multiply => format!("r[{p3}]=r[{p2}]*r[{p1}]"),
        Divide => format!("r[{p3}]=r[{p2}]/r[{p1}]"),
        Remainder => format!("r[{p3}]=r[{p2}]%r[{p1}]"),
        Concat => format!("r[{p3}]=r[{p2}]+r[{p1}]"),
        Eq => format!("IF r[{p3}]==r[{p1}]"),
        Ne => format!("IF r[{p3}]!=r[{p1}]"),
        Lt => format!("IF r[{p3}]<r[{p1}]"),
        Le => format!("IF r[{p3}]<=r[{p1}]"),
        Gt => format!("IF r[{p3}]>r[{p1}]"),
        Ge => format!("IF r[{p3}]>=r[{p1}]"),
        MakeRecord => format!("r[{p3}]=mkrec(r[{p1}..{}])", p1 + p2 - 1),
        SCopy => format!("r[{p2}]=r[{p1}]"),
        Copy => format!("r[{p2}..{}]=r[{p1}..]", p2 + p3),
        Function => format!("r[{p3}]={}(...)", render_p4(&inst.p4)),
        // Cursor/scan/sorter/control opcodes have no concise value synopsis; leave blank.
        _ => String::new(),
    }
}

/// Render the `EXPLAIN QUERY PLAN` rows reflecting OUR actual plan for `select`. `table_name` is
/// the resolved single-FROM table's name, or `None` for a FROM-less (constant) SELECT.
///
/// Rows are emitted BARE (no tree characters). The id/parent scheme is simple and documented:
/// every plan node is a sibling at the root (parent 0) with a sequential id starting at 1 — this
/// reproduces the oracle's RENDERED tree for the shapes we support (a lone `SCAN`, and `SCAN`
/// followed by `USE TEMP B-TREE FOR ORDER BY` as a sibling, not nested). `notused` is always 0.
pub fn query_plan_rows(select: &SelectStmt, table_name: Option<&str>) -> Vec<Vec<Value>> {
    let mut details: Vec<String> = Vec::new();
    match table_name {
        Some(name) => details.push(format!("SCAN {name}")),
        None => details.push("SCAN CONSTANT ROW".to_string()),
    }
    if !select.order_by.is_empty() {
        // Our engine always materializes ORDER BY through the in-memory sorter, so this row is
        // honest. It renders as a sibling of the SCAN (parent 0), matching the oracle's tree.
        details.push("USE TEMP B-TREE FOR ORDER BY".to_string());
    }

    details
        .into_iter()
        .enumerate()
        .map(|(i, detail)| {
            vec![
                Value::Int((i + 1) as i64), // id: 1, 2, ...
                Value::Int(0),              // parent: all siblings at the root
                Value::Int(0),              // notused
                Value::Text(detail),
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Collation;
    use crate::vdbe::program::KeyField;
    use rustqlite_parser::{parse, Stmt};

    fn select(sql: &str) -> SelectStmt {
        match parse(sql).unwrap().into_iter().next().unwrap() {
            Stmt::Select(s) => s,
            other => panic!("expected SELECT, got {other:?}"),
        }
    }

    fn details(rows: &[Vec<Value>]) -> Vec<String> {
        rows.iter()
            .map(|r| match &r[3] {
                Value::Text(s) => s.clone(),
                other => panic!("detail is not text: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn query_plan_scan_table() {
        let rows = query_plan_rows(&select("SELECT * FROM t;"), Some("t"));
        assert_eq!(details(&rows), vec!["SCAN t"]);
        // id=1, parent=0, notused=0.
        assert_eq!(rows[0][0], Value::Int(1));
        assert_eq!(rows[0][1], Value::Int(0));
        assert_eq!(rows[0][2], Value::Int(0));
    }

    #[test]
    fn query_plan_scan_with_order_by() {
        let rows = query_plan_rows(&select("SELECT * FROM t ORDER BY a;"), Some("t"));
        assert_eq!(
            details(&rows),
            vec!["SCAN t", "USE TEMP B-TREE FOR ORDER BY"]
        );
        // Both are siblings at parent 0, with sequential ids.
        assert_eq!(rows[0][0], Value::Int(1));
        assert_eq!(rows[1][0], Value::Int(2));
        assert_eq!(rows[0][1], Value::Int(0));
        assert_eq!(rows[1][1], Value::Int(0));
    }

    #[test]
    fn query_plan_constant_row() {
        let rows = query_plan_rows(&select("SELECT 1;"), None);
        assert_eq!(details(&rows), vec!["SCAN CONSTANT ROW"]);
    }

    #[test]
    fn query_plan_where_does_not_change_plan() {
        // A WHERE clause does not change our plan (still a full scan).
        let rows = query_plan_rows(&select("SELECT * FROM t WHERE a = 1;"), Some("t"));
        assert_eq!(details(&rows), vec!["SCAN t"]);
    }

    #[test]
    fn p4_rendering() {
        assert_eq!(render_p4(&P4::None), "");
        assert_eq!(render_p4(&P4::Int(2)), "2");
        assert_eq!(render_p4(&P4::Text("hi".into())), "hi");
        assert_eq!(render_p4(&P4::Symbol("nocase".into())), "nocase");
        assert_eq!(render_p4(&P4::Blob(vec![1, 2])), "");
        assert_eq!(render_p4(&P4::Real(3.5)), "3.5");
        // KeyInfo: "k(N,...)" with "-" for DESC and "B" for BINARY.
        let ki = P4::KeyInfo(vec![
            KeyField {
                desc: false,
                collation: Collation::Binary,
            },
            KeyField {
                desc: true,
                collation: Collation::Binary,
            },
        ]);
        assert_eq!(render_p4(&ki), "k(2,B,-B)");
    }
}
