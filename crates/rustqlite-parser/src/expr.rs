//! Expression construction using pest's `PrattParser`.
//!
//! PEG grammars cannot express left recursion, so `expr` in `sqlite.pest` is a flat stream
//! of prefix operators, primaries, and infix operators. This module folds that stream into
//! an [`Expr`] tree using a precedence/associativity table that mirrors SQLite's `expr.c`.

use std::sync::OnceLock;

use pest::iterators::{Pair, Pairs};
use pest::pratt_parser::{Assoc, Op, PrattParser};

use crate::ast::*;
use crate::{build_select, build_ordering_term, Rule};

/// Operator precedence, lowest binding first, matching SQLite's documented table
/// (<https://www.sqlite.org/lang_expr.html>): OR < AND < NOT < (= IS LIKE GLOB) <
/// (< <= > >=) < (& | << >>) < (+ -) < (* / %) < (`||` `->` `->>`) < unary (~ - +).
fn pratt() -> &'static PrattParser<Rule> {
    static PRATT: OnceLock<PrattParser<Rule>> = OnceLock::new();
    PRATT.get_or_init(|| {
        PrattParser::new()
            .op(Op::infix(Rule::op_or, Assoc::Left))
            .op(Op::infix(Rule::op_and, Assoc::Left))
            .op(Op::prefix(Rule::K_NOT))
            // `X LIKE Y ESCAPE Z`: ESCAPE is modeled as an infix operator joining the whole LIKE
            // comparison to its escape operand, registered LOOSER (earlier, here) than the LIKE row
            // below so the comparison folds first. `map_infix` then receives `lhs = (X LIKE Y)` and
            // `rhs = Z` and rewrites them to the 3-arg `like(Y, X, Z)` call.
            .op(Op::infix(Rule::op_escape, Assoc::Left))
            .op(Op::infix(Rule::op_eq, Assoc::Left)
                | Op::infix(Rule::op_ne, Assoc::Left)
                | Op::infix(Rule::op_is, Assoc::Left)
                | Op::infix(Rule::op_isnot, Assoc::Left)
                | Op::infix(Rule::op_like, Assoc::Left)
                | Op::infix(Rule::op_glob, Assoc::Left)
                | Op::infix(Rule::op_regexp, Assoc::Left)
                | Op::infix(Rule::op_match, Assoc::Left)
                | Op::infix(Rule::op_not_like, Assoc::Left)
                | Op::infix(Rule::op_not_glob, Assoc::Left)
                | Op::infix(Rule::op_not_regexp, Assoc::Left)
                | Op::infix(Rule::op_not_match, Assoc::Left))
            .op(Op::infix(Rule::op_lt, Assoc::Left)
                | Op::infix(Rule::op_le, Assoc::Left)
                | Op::infix(Rule::op_gt, Assoc::Left)
                | Op::infix(Rule::op_ge, Assoc::Left))
            // Bitwise tier — `& | << >>` share ONE precedence level (left-assoc), looser than
            // `+`/`-` and tighter than the comparison operators, per SQLite's table at
            // <https://www.sqlite.org/lang_expr.html>.  pest's PrattParser registers the LOWEST
            // precedence FIRST, so this row sits just above the comparisons and below `+`/`-`,
            // and the rows below get progressively tighter: + - < * / % < `||` < unary.
            //
            // Oracle-verified (C sqlite3 3.53):
            //   `1+2*3`=7, `1&2|4`=4, `1|2<<4`=48, `1+2<<4`=48, `1+2&4`=0,
            //   `2+3*4<<1`=28, `1<<2|4<<8`=1024, `1|2<<4|8`=56, `1<<2+1`=8.
            .op(Op::infix(Rule::op_bitand, Assoc::Left)
                | Op::infix(Rule::op_bitor, Assoc::Left)
                | Op::infix(Rule::op_shiftleft, Assoc::Left)
                | Op::infix(Rule::op_shiftright, Assoc::Left))
            .op(Op::infix(Rule::op_add, Assoc::Left) | Op::infix(Rule::op_sub, Assoc::Left))
            .op(Op::infix(Rule::op_mul, Assoc::Left)
                | Op::infix(Rule::op_div, Assoc::Left)
                | Op::infix(Rule::op_mod, Assoc::Left))
            // `||`, `->`, `->>` share one left-associative precedence level (SQLite's table).
            .op(Op::infix(Rule::op_concat, Assoc::Left)
                | Op::infix(Rule::op_jsonextract, Assoc::Left)
                | Op::infix(Rule::op_jsonextracttext, Assoc::Left))
            .op(Op::prefix(Rule::neg) | Op::prefix(Rule::pos) | Op::prefix(Rule::bitnot))
    })
}

/// Build an [`Expr`] from a `Rule::expr` pair.
pub(crate) fn build_expr(pair: Pair<'_, Rule>) -> Expr {
    debug_assert_eq!(pair.as_rule(), Rule::expr);
    fold_expr(pair.into_inner())
}

/// Fold the flat token stream inside an `expr`.  Because `expr` now contains suffix constructs
/// (BETWEEN, IN, EXISTS, CAST, CASE, COLLATE, IS DISTINCT FROM) that are *not* part of the Pratt
/// operator table, we strip them out before feeding the remaining tokens to the Pratt parser and
/// then re-attach them as AST wrappers after the fold.
fn fold_expr(pairs: Pairs<'_, Rule>) -> Expr {
    let mut pairs: Vec<Pair<'_, Rule>> = pairs.collect();
    let mut suffixes = Vec::new();

    // Split suffix operators off the end of the stream, working right-to-left so chained
    // suffixes attach in the correct (left-to-right) order.  For example `a COLLATE b COLLATE c`
    // becomes Collate(Collate(a, b), c).
    while let Some(last) = pairs.last() {
        if matches!(
            last.as_rule(),
            Rule::between_suffix
                | Rule::in_suffix
                | Rule::collate_suffix
                | Rule::is_distinct_suffix
        ) {
            suffixes.push(pairs.pop().unwrap());
        } else {
            break;
        }
    }

    if pairs.is_empty() {
        // Suffix-only `expr` should not happen; but if it does, fall back to a null literal to
        // avoid a panic.  The grammar always supplies a primary before a suffix.
        return Expr::Literal(Literal::Null);
    }

    // If the *only* remaining token is a primary that itself contains an expression tree
    // (parenthesised expression, CASE, EXISTS, CAST), we do not need the Pratt fold at all.
    if pairs.len() == 1 {
        let single = pairs.into_iter().next().unwrap();
        let folded = match single.as_rule() {
            Rule::literal | Rule::column_ref | Rule::func_call | Rule::ctime_expr => {
                map_primary(single)
            }
            Rule::expr | Rule::exists_expr | Rule::subquery | Rule::cast_expr | Rule::case_expr
            | Rule::row_value => map_primary(single),
            other => unreachable!("unexpected sole expr child {other:?}"),
        };
        return suffixes.into_iter().rev().fold(folded, apply_suffix);
    }

    let pairs_vec: Vec<Pair<'_, Rule>> = pairs.into_iter().collect();
    let folded = fold(pairs_vec.into_iter().peekable());

    suffixes.into_iter().rev().fold(folded, apply_suffix)
}

fn fold<'a, P: Iterator<Item = Pair<'a, Rule>>>(pairs: P) -> Expr {
    pratt()
        .map_primary(map_primary)
        .map_prefix(|op, rhs| {
            let op = match op.as_rule() {
                Rule::neg => UnaryOp::Negate,
                Rule::pos => UnaryOp::Positive,
                Rule::K_NOT => UnaryOp::Not,
                Rule::bitnot => UnaryOp::BitNot,
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
                Rule::op_jsonextract => BinaryOp::JsonExtract,
                Rule::op_jsonextracttext => BinaryOp::JsonExtractText,
                Rule::op_bitand => BinaryOp::BitAnd,
                Rule::op_bitor => BinaryOp::BitOr,
                Rule::op_shiftleft => BinaryOp::ShiftLeft,
                Rule::op_shiftright => BinaryOp::ShiftRight,
                Rule::op_is => BinaryOp::Is,
                Rule::op_isnot => BinaryOp::IsNot,
                Rule::op_like => BinaryOp::Like,
                Rule::op_glob => BinaryOp::Glob,
                Rule::op_regexp => BinaryOp::Regexp,
                Rule::op_match => BinaryOp::Match,
                // `X NOT LIKE Y` ≡ `NOT (X LIKE Y)` and similarly for NOT GLOB/REGEXP/MATCH — mirror
                // upstream's `likeexpr`, which builds the negation around the plain comparison so
                // NULL propagates through `OP_Not` (NOT NULL = NULL). No codegen change is needed.
                Rule::op_not_like | Rule::op_not_glob | Rule::op_not_regexp | Rule::op_not_match => {
                    let inner_op = match op.as_rule() {
                        Rule::op_not_like => BinaryOp::Like,
                        Rule::op_not_glob => BinaryOp::Glob,
                        Rule::op_not_regexp => BinaryOp::Regexp,
                        Rule::op_not_match => BinaryOp::Match,
                        _ => unreachable!(),
                    };
                    return Expr::Unary {
                        op: UnaryOp::Not,
                        expr: Box::new(Expr::Binary {
                            op: inner_op,
                            left: Box::new(lhs),
                            right: Box::new(rhs),
                        }),
                    };
                }
                // `X LIKE Y ESCAPE Z`: `lhs` is the already-folded LIKE comparison (`X LIKE Y` or
                // `NOT (X LIKE Y)`) and `rhs` is the escape operand `Z`. Rewrite to the 3-arg
                // `like(Y, X, Z)` builtin (preserving any wrapping NOT). The grammar only emits
                // `op_escape` after a LIKE-family comparison, never after GLOB.
                Rule::op_escape => return apply_like_escape(lhs, rhs),
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

/// Rewrite a folded LIKE comparison (`X LIKE Y`, or `NOT (X LIKE Y)`) plus its ESCAPE operand `Z`
/// into the 3-argument `like(Y, X, Z)` builtin (pattern, text, escape — the same arg order as the
/// 2-arg LIKE lowering in codegen), preserving any wrapping `NOT`. The grammar only attaches an
/// ESCAPE to a LIKE-family comparison, so any other shape is unreachable.
fn apply_like_escape(like_cmp: Expr, escape: Expr) -> Expr {
    match like_cmp {
        Expr::Binary {
            op: BinaryOp::Like,
            left,
            right,
        } => Expr::Function {
            name: "like".to_string(),
            distinct: false,
            // left = X (text), right = Y (pattern) per the AST built above; the builtin takes
            // (pattern, text, escape).
            args: FunctionArgs::List(vec![*right, *left, escape]),
            filter: None,
            over: None,
        },
        Expr::Unary {
            op: UnaryOp::Not,
            expr,
        } => Expr::Unary {
            op: UnaryOp::Not,
            expr: Box::new(apply_like_escape(*expr, escape)),
        },
        other => unreachable!("ESCAPE clause must follow a LIKE comparison, got {other:?}"),
    }
}

/// Apply a non-Pratt suffix construct (BETWEEN, IN, CASE, COLLATE, IS DISTINCT FROM) returned by
/// the grammar as a wrapper around the already-folded left-hand expression.  EXISTS/CAST are handled
/// as `primary` and never reach here.
fn apply_suffix(expr: Expr, suffix: Pair<'_, Rule>) -> Expr {
    match suffix.as_rule() {
        Rule::between_suffix => build_between(expr, suffix),
        Rule::in_suffix => build_in(expr, suffix),
        Rule::case_expr => {
            // CASE is a primary, so `expr` will be a placeholder null.  Build the CASE from the
            // suffix children directly.
            build_case(Expr::Literal(Literal::Null), suffix)
        }
        Rule::collate_suffix => {
            let name = suffix
                .into_inner()
                .find(|p| p.as_rule() == Rule::ident)
                .expect("collate_suffix has an ident")
                .as_str()
                .to_string();
            Expr::Collate {
                expr: Box::new(expr),
                collation: name,
            }
        }
        Rule::is_distinct_suffix => build_is_distinct(expr, suffix),
        other => unreachable!("unexpected suffix {other:?}"),
    }
}

fn build_between(expr: Expr, suffix: Pair<'_, Rule>) -> Expr {
    let mut negated = false;
    let mut exprs: Vec<Expr> = Vec::with_capacity(2);
    for part in suffix.into_inner() {
        match part.as_rule() {
            Rule::K_NOT => negated = true,
            Rule::literal => {
                // Literal primaries inside BETWEEN are not wrapped in `expr` by the grammar.
                exprs.push(map_primary(part));
            }
            Rule::expr => exprs.push(build_expr(part)),
            _ => {}
        }
    }
    if exprs.len() != 2 {
        unreachable!("BETWEEN suffix must contain exactly two operands");
    }
    Expr::Between {
        expr: Box::new(expr),
        low: Box::new(exprs.remove(0)),
        high: Box::new(exprs.remove(0)),
        negated,
    }
}

fn build_in(expr: Expr, suffix: Pair<'_, Rule>) -> Expr {
    let mut negated = false;
    // The inner `in_rhs` resolves to either a `select_stmt` (the subquery form, M2.60) or a
    // possibly-empty sequence of `expr` pairs (the inline value list). Pest flattens the
    // `(expr ~ ("," ~ expr)*)?` alternative directly into `in_suffix`'s inner pairs, so we
    // see `expr` siblings directly; the subquery case is detected by a single `select_stmt`
    // child.
    let mut inner = suffix.into_inner().collect::<Vec<_>>().into_iter();
    // The leading `K_NOT` (if present) is the first child.
    if let Some(first) = inner.next() {
        match first.as_rule() {
            Rule::K_NOT => negated = true,
            Rule::K_IN => {}
            Rule::select_stmt => {
                // `X [NOT] IN (SELECT …)` — wrap as InSubquery.  The K_NOT/K_IN tokens are emitted
                // before the in_rhs alternative, so when the very first inner token is a
                // select_stmt the negation was absent.
                return Expr::InSubquery {
                    expr: Box::new(expr),
                    subquery: Box::new(
                        build_select(first.clone()).expect("IN subquery select"),
                    ),
                    negated,
                };
            }
            Rule::expr => {
                let mut values = vec![build_expr(first)];
                for part in inner {
                    match part.as_rule() {
                        Rule::expr => values.push(build_expr(part)),
                        _ => {}
                    }
                }
                return Expr::In {
                    expr: Box::new(expr),
                    values,
                    negated,
                };
            }
            other => unreachable!("unexpected in_suffix child {other:?}"),
        }
    }
    // `X IN ()` — empty value list.  Upstream simplifies this to a constant; we keep the literal
    // empty list and let codegen lower it.  Negated stays false because no K_NOT was seen.
    let mut values = Vec::new();
    for part in inner {
        match part.as_rule() {
            Rule::K_IN => {}
            Rule::select_stmt => {
                return Expr::InSubquery {
                    expr: Box::new(expr),
                    subquery: Box::new(build_select(part).expect("IN subquery select")),
                    negated,
                };
            }
            Rule::expr => values.push(build_expr(part)),
            _ => {}
        }
    }
    Expr::In {
        expr: Box::new(expr),
        values,
        negated,
    }
}
fn build_case(_base: Expr, suffix: Pair<'_, Rule>) -> Expr {
    // The suffix is `K_CASE ~ expr? ~ (K_WHEN ~ expr ~ K_THEN ~ expr)+ ~ (K_ELSE ~ expr)? ~ K_END`.
    // Pest returns all `expr` children in source order: optional base first, then (when, then)
    // pairs, then optional else.  We tag the tokens structurally to disambiguate.
    #[derive(Clone)]
    enum Tag<'a> {
        Expr(Pair<'a, Rule>),
        When,
        Then,
        Else,
    }
    let mut tags: Vec<Tag<'_>> = Vec::new();
    for part in suffix.clone().into_inner() {
        match part.as_rule() {
            Rule::expr => tags.push(Tag::Expr(part)),
            Rule::K_WHEN => tags.push(Tag::When),
            Rule::K_THEN => tags.push(Tag::Then),
            Rule::K_ELSE => tags.push(Tag::Else),
            _ => {}
        }
    }

    // Build the expression list once and remember the source position of each expr.
    let mut exprs: Vec<Expr> = Vec::new();
    for tag in &tags {
        if let Tag::Expr(p) = tag {
            exprs.push(build_expr(p.clone()));
        }
    }

    let mut base: Option<Box<Expr>> = None;
    let mut when_then: Vec<(Expr, Expr)> = Vec::new();
    let mut else_expr: Option<Box<Expr>> = None;
    let mut expr_iter = exprs.into_iter();
    let mut when_buf: Option<Expr> = None;
    let mut after_when = false;
    let mut else_seen = false;
    for tag in &tags {
        match tag {
            Tag::When => {
                after_when = true;
            }
            Tag::Expr(_) => {
                let e = expr_iter.next().unwrap();
                if else_seen {
                    else_expr = Some(Box::new(e));
                } else if !after_when && base.is_none() {
                    // Optional base expression (only exprs before the first K_WHEN).
                    base = Some(Box::new(e));
                } else if when_buf.is_none() {
                    when_buf = Some(e);
                } else {
                    when_then.push((when_buf.take().unwrap(), e));
                }
            }
            Tag::Then => {}
            Tag::Else => {
                else_seen = true;
            }
        }
    }
    Expr::Case {
        base,
        when_then,
        else_expr,
    }
}

fn build_is_distinct(expr: Expr, suffix: Pair<'_, Rule>) -> Expr {
    let mut negated = false;
    let mut rhs = None;
    for part in suffix.into_inner() {
        match part.as_rule() {
            Rule::K_NOT => negated = true,
            Rule::literal => rhs = Some(map_primary(part)),
            Rule::expr => rhs = Some(build_expr(part)),
            _ => {}
        }
    }
    Expr::IsDistinctFrom {
        left: Box::new(expr),
        right: Box::new(rhs.expect("is_distinct_suffix has rhs")),
        negated,
    }
}

fn map_primary(pair: Pair<'_, Rule>) -> Expr {
    match pair.as_rule() {
        Rule::expr => fold_expr(pair.into_inner()), // parenthesised sub-expression
        Rule::exists_expr => {
            let select_pair = pair
                .into_inner()
                .find(|p| p.as_rule() == Rule::select_stmt)
                .expect("exists_expr has a select_stmt");
            Expr::Exists(Box::new(
                build_select(select_pair).expect("subquery select"),
            ))
        }
        Rule::subquery => {
            let select_pair = pair
                .into_inner()
                .find(|p| p.as_rule() == Rule::select_stmt)
                .expect("subquery has a select_stmt");
            Expr::Subquery(Box::new(
                build_select(select_pair).expect("subquery select"),
            ))
        }
        Rule::cast_expr => {
            let mut inner = pair.into_inner();
            let expr_pair = inner
                .find(|p| p.as_rule() == Rule::expr)
                .expect("cast_expr has expr");
            let type_name = inner
                .find(|p| p.as_rule() == Rule::type_name)
                .expect("cast_expr has type_name")
                .as_str()
                .to_string();
            Expr::Cast {
                expr: Box::new(build_expr(expr_pair)),
                type_name,
            }
        }
        Rule::case_expr => build_case(Expr::Literal(Literal::Null), pair),
        Rule::literal => build_literal_expr(pair),
        Rule::column_ref => build_column_ref(pair),
        Rule::func_call => build_func_call(pair),
        Rule::ctime_expr => build_ctime_expr(pair),
        Rule::row_value => Expr::Row(
            pair.into_inner()
                .filter(|p| p.as_rule() == Rule::expr)
                .map(build_expr)
                .collect(),
        ),
        other => unreachable!("unexpected primary {other:?}"),
    }
}

/// The grammar's `literal` rule also carries bind parameters (`?`, `:name`, …), which are a
/// distinct [`Expr`] variant rather than a [`Literal`]; split them out here.
pub(crate) fn build_literal_expr(pair: Pair<'_, Rule>) -> Expr {
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

pub(crate) fn build_number(text: &str) -> Literal {
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return match u64::from_str_radix(hex, 16) {
            Ok(v) => Literal::Integer(v as i64),
            // A hex literal that overflows 64 bits is out of subset scope; fall back to real.
            Err(_) => Literal::Real(f64::INFINITY),
        };
    }

    // Preserve the explicit sign so the minimum i64 value can be parsed directly.
    let (sign, unsigned) = match text.as_bytes().first() {
        Some(b'-') => (-1i64, &text[1..]),
        Some(b'+') => (1i64, &text[1..]),
        _ => (1i64, text),
    };

    if unsigned.contains('.') || unsigned.contains('e') || unsigned.contains('E') {
        return Literal::Real(text.parse::<f64>().unwrap_or(0.0));
    }

    match unsigned.parse::<u64>() {
        Ok(v) if sign < 0 && v == 9223372036854775808 => {
            // SQLite's exact minimum signed 64-bit integer literal stays INTEGER, not REAL.
            Literal::Integer(i64::MIN)
        }
        Ok(v) if v <= i64::MAX as u64 => Literal::Integer(sign.wrapping_mul(v as i64)),
        // Out of signed 64-bit range but no decimal point/exponent: SQLite treats it as REAL.
        _ => Literal::Real(text.parse::<f64>().unwrap_or(0.0)),
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
    let mut filter = None;
    let mut over = None;
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
            Rule::filter_over => {
                for fo in child.into_inner() {
                    match fo.as_rule() {
                        Rule::filter_clause => {
                            filter = Some(Box::new(build_expr(
                                fo.into_inner()
                                    .find(|p| p.as_rule() == Rule::expr)
                                    .expect("filter_clause has an expr"),
                            )));
                        }
                        Rule::over_clause => over = Some(build_over_clause(fo)),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Expr::Function {
        name,
        distinct,
        args,
        filter,
        over,
    }
}

/// Build a `current_date` / `current_time` / `current_timestamp` keyword
/// primary (upstream `TK_CTIME_KW`) into a zero-argument `Expr::Function`.
/// The keyword is emitted in canonical lowercase so the VDBE executor's
/// case-insensitive match finds it.
fn build_ctime_expr(pair: Pair<'_, Rule>) -> Expr {
    let inner = pair
        .into_inner()
        .next()
        .expect("ctime_expr has one keyword child");
    let name = match inner.as_rule() {
        Rule::K_CURRENT_DATE => "current_date",
        Rule::K_CURRENT_TIME => "current_time",
        Rule::K_CURRENT_TIMESTAMP => "current_timestamp",
        _ => unreachable!("ctime_expr matched unexpected rule"),
    };
    Expr::Function {
        name: name.to_string(),
        distinct: false,
        args: FunctionArgs::List(Vec::new()),
        filter: None,
        over: None,
    }
}

/// Build an `over_clause` (`OVER (window_spec)` or `OVER name`) into a `Window`.
fn build_over_clause(pair: Pair<'_, Rule>) -> Window {
    let mut name: Option<String> = None;
    let mut partition_by: Vec<Expr> = Vec::new();
    let mut order_by: Vec<OrderingTerm> = Vec::new();
    let mut frame: Option<Frame> = None;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::ident => name = Some(child.as_str().to_string()),
            Rule::window => {
                let w = build_window_spec(child);
                partition_by = w.partition_by;
                order_by = w.order_by;
                frame = w.frame;
                if name.is_none() {
                    name = w.name;
                }
            }
            _ => {}
        }
    }
    Window {
        name,
        partition_by,
        order_by,
        frame,
    }
}

/// Build a `window` rule pair into a `Window` (without the named-window reference).
pub(crate) fn build_window_spec(pair: Pair<'_, Rule>) -> Window {
    let mut partition_by: Vec<Expr> = Vec::new();
    let mut order_by: Vec<OrderingTerm> = Vec::new();
    let mut frame: Option<Frame> = None;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::group_by => {
                partition_by = child.into_inner().map(build_expr).collect();
            }
            Rule::order_item => {
                let order_by_pair = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::order_by)
                    .expect("order_item has order_by");
                order_by = order_by_pair
                    .into_inner()
                    .map(build_ordering_term)
                    .collect();
            }
            Rule::frame_opt => frame = Some(build_frame(child)),
            _ => {}
        }
    }
    Window {
        name: None,
        partition_by,
        order_by,
        frame,
    }
}

/// Build a `frame_opt` pair into a `Frame`.
fn build_frame(pair: Pair<'_, Rule>) -> Frame {
    let mut mode = FrameMode::Rows;
    let mut start = FrameBound::UnboundedPreceding;
    let mut end: Option<FrameBound> = None;
    let mut exclude: Option<FrameExclude> = None;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::range_or_rows => {
                mode = match child.into_inner().next().expect("range_or_rows has a kind").as_rule() {
                    Rule::K_RANGE => FrameMode::Range,
                    Rule::K_ROWS => FrameMode::Rows,
                    Rule::K_GROUPS => FrameMode::Groups,
                    other => unreachable!("unexpected range_or_rows child {other:?}"),
                };
            }
            Rule::frame_bound_s => start = build_frame_bound_s(child),
            Rule::frame_bound_e => end = Some(build_frame_bound_e(child)),
            Rule::frame_exclude_opt => {
                let fe = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::frame_exclude)
                    .expect("frame_exclude_opt has frame_exclude");
                exclude = Some(build_frame_exclude(fe));
            }
            _ => {}
        }
    }
    Frame {
        mode,
        start,
        end,
        exclude,
    }
}

fn build_frame_bound_s(pair: Pair<'_, Rule>) -> FrameBound {
    let mut inner = pair.into_inner();
    let first = inner.next().expect("frame_bound_s has a child");
    match first.as_rule() {
        Rule::K_UNBOUNDED => FrameBound::UnboundedPreceding,
        Rule::frame_bound => build_frame_bound(first),
        other => unreachable!("unexpected frame_bound_s child {other:?}"),
    }
}

fn build_frame_bound_e(pair: Pair<'_, Rule>) -> FrameBound {
    let mut inner = pair.into_inner();
    let first = inner.next().expect("frame_bound_e has a child");
    match first.as_rule() {
        Rule::K_UNBOUNDED => FrameBound::UnboundedFollowing,
        Rule::frame_bound => build_frame_bound(first),
        other => unreachable!("unexpected frame_bound_e child {other:?}"),
    }
}

fn build_frame_bound(pair: Pair<'_, Rule>) -> FrameBound {
    let mut inner = pair.into_inner();
    let first = inner.next().expect("frame_bound has a child");
    match first.as_rule() {
        Rule::K_CURRENT => FrameBound::CurrentRow,
        Rule::expr => {
            let e = build_expr(first);
            let dir = inner.next().expect("frame_bound has a direction");
            match dir.as_rule() {
                Rule::K_PRECEDING => FrameBound::Preceding(Box::new(e)),
                Rule::K_FOLLOWING => FrameBound::Following(Box::new(e)),
                other => unreachable!("unexpected frame_bound direction {other:?}"),
            }
        }
        other => unreachable!("unexpected frame_bound child {other:?}"),
    }
}

fn build_frame_exclude(pair: Pair<'_, Rule>) -> FrameExclude {
    let inner = pair.into_inner().next().expect("frame_exclude has a child");
    match inner.as_rule() {
        Rule::K_NO => FrameExclude::NoOthers,
        Rule::K_CURRENT => FrameExclude::CurrentRow,
        Rule::K_GROUP => FrameExclude::Group,
        Rule::K_TIES => FrameExclude::Ties,
        other => unreachable!("unexpected frame_exclude child {other:?}"),
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
