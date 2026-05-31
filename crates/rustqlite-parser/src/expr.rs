//! Expression construction using pest's `PrattParser`.
//!
//! PEG grammars cannot express left recursion, so `expr` in `sqlite.pest` is a flat stream
//! of prefix operators, primaries, and infix operators. This module folds that stream into
//! an [`Expr`] tree using a precedence/associativity table that mirrors SQLite's `expr.c`.

use std::sync::OnceLock;

use pest::iterators::{Pair, Pairs};
use pest::pratt_parser::{Assoc, Op, PrattParser};

use crate::ast::*;
use crate::Rule;

/// Operator precedence, lowest binding first, matching SQLite's documented table
/// (<https://www.sqlite.org/lang_expr.html>): OR < AND < NOT < (= IS LIKE GLOB) <
/// (< <= > >=) < (+ -) < (* / %) < `||` < unary (- +).
fn pratt() -> &'static PrattParser<Rule> {
    static PRATT: OnceLock<PrattParser<Rule>> = OnceLock::new();
    PRATT.get_or_init(|| {
        PrattParser::new()
            .op(Op::infix(Rule::op_or, Assoc::Left))
            .op(Op::infix(Rule::op_and, Assoc::Left))
            .op(Op::prefix(Rule::K_NOT))
            .op(Op::infix(Rule::op_eq, Assoc::Left)
                | Op::infix(Rule::op_ne, Assoc::Left)
                | Op::infix(Rule::op_is, Assoc::Left)
                | Op::infix(Rule::op_isnot, Assoc::Left)
                | Op::infix(Rule::op_like, Assoc::Left)
                | Op::infix(Rule::op_glob, Assoc::Left))
            .op(Op::infix(Rule::op_lt, Assoc::Left)
                | Op::infix(Rule::op_le, Assoc::Left)
                | Op::infix(Rule::op_gt, Assoc::Left)
                | Op::infix(Rule::op_ge, Assoc::Left))
            .op(Op::infix(Rule::op_add, Assoc::Left) | Op::infix(Rule::op_sub, Assoc::Left))
            .op(Op::infix(Rule::op_mul, Assoc::Left)
                | Op::infix(Rule::op_div, Assoc::Left)
                | Op::infix(Rule::op_mod, Assoc::Left))
            .op(Op::infix(Rule::op_concat, Assoc::Left))
            .op(Op::prefix(Rule::neg) | Op::prefix(Rule::pos))
    })
}

/// Build an [`Expr`] from a `Rule::expr` pair.
pub(crate) fn build_expr(pair: Pair<'_, Rule>) -> Expr {
    debug_assert_eq!(pair.as_rule(), Rule::expr);
    fold(pair.into_inner())
}

fn fold(pairs: Pairs<'_, Rule>) -> Expr {
    pratt()
        .map_primary(map_primary)
        .map_prefix(|op, rhs| {
            let op = match op.as_rule() {
                Rule::neg => UnaryOp::Negate,
                Rule::pos => UnaryOp::Positive,
                Rule::K_NOT => UnaryOp::Not,
                other => unreachable!("unexpected prefix operator {other:?}"),
            };
            Expr::Unary {
                op,
                expr: Box::new(rhs),
            }
        })
        .map_infix(|lhs, op, rhs| {
            let op = match op.as_rule() {
                Rule::op_or => BinaryOp::Or,
                Rule::op_and => BinaryOp::And,
                Rule::op_eq => BinaryOp::Eq,
                Rule::op_ne => BinaryOp::Ne,
                Rule::op_lt => BinaryOp::Lt,
                Rule::op_le => BinaryOp::Le,
                Rule::op_gt => BinaryOp::Gt,
                Rule::op_ge => BinaryOp::Ge,
                Rule::op_add => BinaryOp::Add,
                Rule::op_sub => BinaryOp::Sub,
                Rule::op_mul => BinaryOp::Mul,
                Rule::op_div => BinaryOp::Div,
                Rule::op_mod => BinaryOp::Mod,
                Rule::op_concat => BinaryOp::Concat,
                Rule::op_is => BinaryOp::Is,
                Rule::op_isnot => BinaryOp::IsNot,
                Rule::op_like => BinaryOp::Like,
                Rule::op_glob => BinaryOp::Glob,
                other => unreachable!("unexpected infix operator {other:?}"),
            };
            Expr::Binary {
                op,
                left: Box::new(lhs),
                right: Box::new(rhs),
            }
        })
        .parse(pairs)
}

fn map_primary(pair: Pair<'_, Rule>) -> Expr {
    match pair.as_rule() {
        Rule::expr => fold(pair.into_inner()), // parenthesised sub-expression
        Rule::literal => build_literal_expr(pair),
        Rule::column_ref => build_column_ref(pair),
        Rule::func_call => build_func_call(pair),
        other => unreachable!("unexpected primary {other:?}"),
    }
}

/// The grammar's `literal` rule also carries bind parameters (`?`, `:name`, …), which are a
/// distinct [`Expr`] variant rather than a [`Literal`]; split them out here.
fn build_literal_expr(pair: Pair<'_, Rule>) -> Expr {
    let inner = pair.into_inner().next().expect("literal has one child");
    match inner.as_rule() {
        Rule::bind_param => Expr::BindParam(inner.as_str().to_string()),
        Rule::number => Expr::Literal(build_number(inner.as_str())),
        Rule::string => Expr::Literal(Literal::Text(unquote_string(inner.as_str()))),
        Rule::blob => Expr::Literal(Literal::Blob(parse_blob(inner.as_str()))),
        Rule::K_NULL => Expr::Literal(Literal::Null),
        Rule::K_TRUE => Expr::Literal(Literal::Bool(true)),
        Rule::K_FALSE => Expr::Literal(Literal::Bool(false)),
        other => unreachable!("unexpected literal {other:?}"),
    }
}

fn build_number(text: &str) -> Literal {
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return match u64::from_str_radix(hex, 16) {
            Ok(v) => Literal::Integer(v as i64),
            // A hex literal that overflows 64 bits is out of subset scope; fall back to real.
            Err(_) => Literal::Real(f64::INFINITY),
        };
    }
    if text.contains('.') || text.contains('e') || text.contains('E') {
        return Literal::Real(text.parse::<f64>().unwrap_or(0.0));
    }
    match text.parse::<i64>() {
        Ok(v) => Literal::Integer(v),
        // Integer literal too large for i64 becomes a real, as in SQLite.
        Err(_) => Literal::Real(text.parse::<f64>().unwrap_or(0.0)),
    }
}

fn build_column_ref(pair: Pair<'_, Rule>) -> Expr {
    let mut parts: Vec<String> = pair
        .into_inner()
        .map(|p| unquote_ident(p.as_str()))
        .collect();
    match parts.len() {
        1 => Expr::Column {
            schema: None,
            table: None,
            name: parts.pop().unwrap(),
        },
        2 => {
            let name = parts.pop().unwrap();
            let table = parts.pop().unwrap();
            Expr::Column {
                schema: None,
                table: Some(table),
                name,
            }
        }
        _ => {
            let name = parts.pop().unwrap();
            let table = parts.pop().unwrap();
            let schema = parts.pop().unwrap();
            Expr::Column {
                schema: Some(schema),
                table: Some(table),
                name,
            }
        }
    }
}

fn build_func_call(pair: Pair<'_, Rule>) -> Expr {
    let mut name = String::new();
    let mut distinct = false;
    let mut args = FunctionArgs::List(Vec::new());
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::func_name => name = unquote_ident(child.as_str()),
            Rule::func_star => args = FunctionArgs::Star,
            Rule::arg_list => {
                let mut list = Vec::new();
                for a in child.into_inner() {
                    match a.as_rule() {
                        Rule::K_DISTINCT => distinct = true,
                        Rule::expr => list.push(build_expr(a)),
                        _ => {}
                    }
                }
                args = FunctionArgs::List(list);
            }
            _ => {}
        }
    }
    Expr::Function {
        name,
        distinct,
        args,
    }
}

// ---- small text helpers ----

fn unquote_ident(s: &str) -> String {
    let bytes = s.as_bytes();
    if s.len() >= 2 {
        let (first, last) = (bytes[0], bytes[s.len() - 1]);
        if first == b'"' && last == b'"' {
            return s[1..s.len() - 1].replace("\"\"", "\"");
        }
        if first == b'`' && last == b'`' {
            return s[1..s.len() - 1].replace("``", "`");
        }
        if first == b'[' && last == b']' {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

fn unquote_string(s: &str) -> String {
    // s includes the surrounding single quotes.
    s[1..s.len() - 1].replace("''", "'")
}

fn parse_blob(s: &str) -> Vec<u8> {
    // s looks like x'48656C6C6F' (the leading char may be x or X).
    let inner = &s[2..s.len() - 1];
    let bytes = inner.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i + 2 <= bytes.len() {
        let hi = hex_val(bytes[i]);
        let lo = hex_val(bytes[i + 1]);
        out.push((hi << 4) | lo);
        i += 2;
    }
    out
}

fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}
