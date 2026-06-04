//! `rustqlite_parser` — SQL text → AST for Rustqlite.
//!
//! The grammar ([`sqlite.pest`](../src/sqlite.pest)) is a PEG ported (incrementally) from
//! SQLite's `parse.y`; operator precedence is applied with pest's `PrattParser` in
//! [`expr`]. This crate has **no** dependency on the engine — it is a pure
//! `&str` → [`Stmt`] transformation, mirroring SQLite's `tokenize.c` + `parse.y` split.
//!
//! ```
//! use rustqlite_parser::{parse, Stmt};
//! let stmts = parse("SELECT 1 + 2 * 3;").unwrap();
//! assert_eq!(stmts.len(), 1);
//! assert!(matches!(stmts[0], Stmt::Select(_)));
//! ```

mod ast;
mod error;
mod expr;

pub use ast::*;
pub use error::ParseError;

use pest::iterators::Pair;
use pest::Parser as _;
use pest_derive::Parser;

#[derive(Parser)]
#[grammar = "sqlite.pest"]
struct SqliteParser;

/// Parse a string containing zero or more `;`-separated SQL statements into a list of ASTs.
///
/// Returns a [`ParseError`] (with a location-annotated message) on the first syntax error.
pub fn parse(sql: &str) -> Result<Vec<Stmt>, ParseError> {
    let mut top =
        SqliteParser::parse(Rule::sql, sql).map_err(|e| ParseError::new(e.to_string()))?;
    let sql_pair = top.next().expect("rule `sql` always produces one pair");

    let mut stmts = Vec::new();
    for pair in sql_pair.into_inner() {
        if pair.as_rule() == Rule::statement {
            stmts.push(build_statement(pair));
        }
        // Rule::EOI and bare `;` separators produce no statement.
    }
    Ok(stmts)
}

/// Parse exactly one statement, returning it together with the unparsed tail. Convenience
/// for the CLI's REPL, which feeds statements one at a time.
pub fn parse_one(sql: &str) -> Result<Option<Stmt>, ParseError> {
    Ok(parse(sql)?.into_iter().next())
}

fn build_statement(pair: Pair<'_, Rule>) -> Stmt {
    let mut inner = pair.into_inner();
    let first = inner.next().expect("statement has at least one child");
    if first.as_rule() == Rule::explain_prefix {
        // An `explain_prefix` is followed by exactly one statement child (select/create/insert).
        let kind = explain_kind(&first);
        let body = inner.next().expect("explain_prefix precedes a statement");
        return Stmt::Explain(Box::new(build_inner_stmt(body)), kind);
    }
    build_inner_stmt(first)
}

/// Build the select/create/insert/delete statement from its grammar pair.
fn build_inner_stmt(pair: Pair<'_, Rule>) -> Stmt {
    match pair.as_rule() {
        Rule::select_stmt => Stmt::Select(build_select(pair)),
        Rule::create_table_stmt => Stmt::CreateTable(build_create_table(pair)),
        Rule::insert_stmt => Stmt::Insert(build_insert(pair)),
        Rule::delete_stmt => Stmt::Delete(build_delete(pair)),
        other => unreachable!("unexpected statement {other:?}"),
    }
}

/// Classify an `explain_prefix` pair: a `query plan` descendant means [`ExplainKind::QueryPlan`],
/// otherwise it is a plain bytecode [`ExplainKind::Bytecode`].
fn explain_kind(prefix: &Pair<'_, Rule>) -> ExplainKind {
    if prefix
        .clone()
        .into_inner()
        .any(|p| p.as_rule() == Rule::explain_query_plan)
    {
        ExplainKind::QueryPlan
    } else {
        ExplainKind::Bytecode
    }
}

fn build_select(pair: Pair<'_, Rule>) -> SelectStmt {
    let mut stmt = SelectStmt {
        distinct: false,
        columns: Vec::new(),
        from: Vec::new(),
        where_clause: None,
        group_by: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
    };

    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::K_DISTINCT => stmt.distinct = true,
            Rule::result_columns => stmt.columns = build_result_columns(part),
            Rule::from_item => stmt.from = build_from_item(part),
            Rule::where_item => stmt.where_clause = Some(build_expr_item(part)),
            Rule::group_item => stmt.group_by = build_group_item(part),
            Rule::having_item => stmt.having = Some(build_expr_item(part)),
            Rule::order_item => stmt.order_by = build_order_item(part),
            Rule::limit_item => build_limit_item(part, &mut stmt),
            _ => {} // K_SELECT, K_ALL
        }
    }
    stmt
}

fn build_result_columns(pair: Pair<'_, Rule>) -> Vec<ResultColumn> {
    pair.into_inner().map(build_result_column).collect()
}

fn build_result_column(pair: Pair<'_, Rule>) -> ResultColumn {
    let mut inner = pair.into_inner();
    let first = inner.next().expect("result_column has content");
    match first.as_rule() {
        Rule::result_star => ResultColumn::Star,
        Rule::table_star => {
            let table = first
                .into_inner()
                .next()
                .expect("table_star has an ident")
                .as_str()
                .to_string();
            ResultColumn::TableStar(table)
        }
        Rule::expr => {
            let expr = expr::build_expr(first);
            let alias = inner.next().map(build_as_alias);
            ResultColumn::Expr { expr, alias }
        }
        other => unreachable!("unexpected result_column child {other:?}"),
    }
}

fn build_as_alias(pair: Pair<'_, Rule>) -> String {
    // as_alias = { K_AS? ~ alias }
    pair.into_inner()
        .find(|p| p.as_rule() == Rule::alias)
        .expect("as_alias has an alias")
        .as_str()
        .to_string()
}

fn build_from_item(pair: Pair<'_, Rule>) -> Vec<TableRef> {
    // from_item = { K_FROM ~ from_clause }
    let from_clause = pair
        .into_inner()
        .find(|p| p.as_rule() == Rule::from_clause)
        .expect("from_item has from_clause");
    from_clause.into_inner().map(build_table_ref).collect()
}

fn build_table_ref(pair: Pair<'_, Rule>) -> TableRef {
    let mut schema = None;
    let mut name = String::new();
    let mut alias = None;
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::qualified_name => {
                let (s, n) = build_qualified_name(part);
                schema = s;
                name = n;
            }
            Rule::as_alias => alias = Some(build_as_alias(part)),
            _ => {}
        }
    }
    TableRef {
        schema,
        name,
        alias,
    }
}

fn build_qualified_name(pair: Pair<'_, Rule>) -> (Option<String>, String) {
    let parts: Vec<String> = pair.into_inner().map(|p| p.as_str().to_string()).collect();
    match parts.as_slice() {
        [name] => (None, name.clone()),
        [schema, name] => (Some(schema.clone()), name.clone()),
        _ => unreachable!("qualified_name has 1 or 2 idents"),
    }
}

fn build_expr_item(pair: Pair<'_, Rule>) -> Expr {
    // where_item / having_item = { KEYWORD ~ expr }
    let expr_pair = pair
        .into_inner()
        .find(|p| p.as_rule() == Rule::expr)
        .expect("clause has an expr");
    expr::build_expr(expr_pair)
}

fn build_group_item(pair: Pair<'_, Rule>) -> Vec<Expr> {
    let group_by = pair
        .into_inner()
        .find(|p| p.as_rule() == Rule::group_by)
        .expect("group_item has group_by");
    group_by.into_inner().map(expr::build_expr).collect()
}

fn build_order_item(pair: Pair<'_, Rule>) -> Vec<OrderingTerm> {
    let order_by = pair
        .into_inner()
        .find(|p| p.as_rule() == Rule::order_by)
        .expect("order_item has order_by");
    order_by
        .into_inner()
        .map(|term| {
            let mut desc = false;
            let mut expr = None;
            for part in term.into_inner() {
                match part.as_rule() {
                    Rule::expr => expr = Some(expr::build_expr(part)),
                    Rule::K_DESC => desc = true,
                    Rule::K_ASC => desc = false,
                    _ => {}
                }
            }
            OrderingTerm {
                expr: expr.expect("ordering_term has an expr"),
                desc,
            }
        })
        .collect()
}

fn build_limit_item(pair: Pair<'_, Rule>, stmt: &mut SelectStmt) {
    // limit_item = { K_LIMIT ~ expr ~ (offset_item | limit_comma)? }
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::expr => stmt.limit = Some(expr::build_expr(part)),
            Rule::offset_item | Rule::limit_comma => {
                let e = part
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::expr)
                    .expect("offset clause has an expr");
                stmt.offset = Some(expr::build_expr(e));
            }
            _ => {}
        }
    }
}

fn build_create_table(pair: Pair<'_, Rule>) -> CreateTable {
    let mut ct = CreateTable {
        temporary: false,
        if_not_exists: false,
        schema: None,
        name: String::new(),
        columns: Vec::new(),
    };
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::K_TEMPORARY | Rule::K_TEMP => ct.temporary = true,
            Rule::if_not_exists => ct.if_not_exists = true,
            Rule::qualified_name => {
                let (s, n) = build_qualified_name(part);
                ct.schema = s;
                ct.name = n;
            }
            Rule::column_def => ct.columns.push(build_column_def(part)),
            _ => {}
        }
    }
    ct
}

fn build_column_def(pair: Pair<'_, Rule>) -> ColumnDef {
    let mut name = String::new();
    let mut type_name = None;
    let mut constraints = Vec::new();
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::ident => name = part.as_str().to_string(),
            Rule::type_name => {
                type_name = Some(normalize_type_name(part.as_str()));
            }
            Rule::column_constraint => constraints.push(build_column_constraint(part)),
            _ => {}
        }
    }
    ColumnDef {
        name,
        type_name,
        constraints,
    }
}

fn normalize_type_name(raw: &str) -> String {
    // Collapse internal whitespace so "DOUBLE   PRECISION" reads as "DOUBLE PRECISION".
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn build_column_constraint(pair: Pair<'_, Rule>) -> ColumnConstraint {
    let inner = pair.into_inner().next().expect("constraint has a kind");
    match inner.as_rule() {
        Rule::c_primary_key => {
            let mut desc = false;
            let mut autoincrement = false;
            for part in inner.into_inner() {
                match part.as_rule() {
                    Rule::K_DESC => desc = true,
                    Rule::K_AUTOINCREMENT => autoincrement = true,
                    _ => {}
                }
            }
            ColumnConstraint::PrimaryKey {
                desc,
                autoincrement,
            }
        }
        Rule::c_not_null => ColumnConstraint::NotNull,
        Rule::c_unique => ColumnConstraint::Unique,
        Rule::c_default => {
            let expr_pair = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::expr || p.as_rule() == Rule::literal);
            let e = match expr_pair {
                Some(p) if p.as_rule() == Rule::expr => expr::build_expr(p),
                _ => Expr::Literal(Literal::Null),
            };
            ColumnConstraint::Default(e)
        }
        other => unreachable!("unexpected constraint {other:?}"),
    }
}

fn build_insert(pair: Pair<'_, Rule>) -> InsertStmt {
    let mut stmt = InsertStmt {
        or_action: None,
        schema: None,
        table: String::new(),
        columns: Vec::new(),
        rows: Vec::new(),
    };
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::insert_verb => stmt.or_action = build_insert_verb(part),
            Rule::qualified_name => {
                let (s, n) = build_qualified_name(part);
                stmt.schema = s;
                stmt.table = n;
            }
            Rule::column_list => {
                stmt.columns = part
                    .into_inner()
                    .filter(|p| p.as_rule() == Rule::ident)
                    .map(|p| p.as_str().to_string())
                    .collect();
            }
            Rule::values_clause => {
                stmt.rows = part
                    .into_inner()
                    .filter(|p| p.as_rule() == Rule::value_row)
                    .map(|row| row.into_inner().map(expr::build_expr).collect())
                    .collect();
            }
            _ => {}
        }
    }
    stmt
}

fn build_insert_verb(pair: Pair<'_, Rule>) -> Option<ConflictAction> {
    // insert_verb = { (K_INSERT ~ (K_OR ~ conflict_action)?) | K_REPLACE }
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::K_REPLACE => return Some(ConflictAction::Replace),
            Rule::conflict_action => {
                let kind = part
                    .into_inner()
                    .next()
                    .expect("conflict_action has a kind");
                return Some(match kind.as_rule() {
                    Rule::K_ROLLBACK => ConflictAction::Rollback,
                    Rule::K_ABORT => ConflictAction::Abort,
                    Rule::K_FAIL => ConflictAction::Fail,
                    Rule::K_IGNORE => ConflictAction::Ignore,
                    Rule::K_REPLACE => ConflictAction::Replace,
                    _ => ConflictAction::Abort,
                });
            }
            _ => {}
        }
    }
    None
}

fn build_delete(pair: Pair<'_, Rule>) -> DeleteStmt {
    let mut stmt = DeleteStmt {
        schema: None,
        table: String::new(),
        where_clause: None,
    };
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::qualified_name => {
                let (s, n) = build_qualified_name(part);
                stmt.schema = s;
                stmt.table = n;
            }
            Rule::where_item => stmt.where_clause = Some(build_expr_item(part)),
            _ => {}
        }
    }
    stmt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_empty_and_semicolons() {
        assert_eq!(parse("").unwrap().len(), 0);
        assert_eq!(parse(";").unwrap().len(), 0);
        assert_eq!(parse(";;;").unwrap().len(), 0);
    }

    #[test]
    fn parses_simple_select() {
        let stmts = parse("SELECT a, b FROM t WHERE a = 1;").unwrap();
        let Stmt::Select(s) = &stmts[0] else {
            panic!("expected select")
        };
        assert_eq!(s.columns.len(), 2);
        assert_eq!(s.from[0].name, "t");
        assert!(s.where_clause.is_some());
    }

    #[test]
    fn select_star_and_alias() {
        let Stmt::Select(s) = &parse("SELECT * FROM t;").unwrap()[0] else {
            panic!()
        };
        assert_eq!(s.columns, vec![ResultColumn::Star]);

        let Stmt::Select(s) = &parse("SELECT a AS x, t.b FROM t alias;").unwrap()[0] else {
            panic!()
        };
        assert!(matches!(&s.columns[0], ResultColumn::Expr { alias: Some(a), .. } if a == "x"));
        assert_eq!(s.from[0].alias.as_deref(), Some("alias"));
    }

    #[test]
    fn pratt_precedence_mul_binds_tighter_than_add() {
        // 1 + 2 * 3  ==  1 + (2 * 3)
        let Stmt::Select(s) = &parse("SELECT 1 + 2 * 3;").unwrap()[0] else {
            panic!()
        };
        let ResultColumn::Expr { expr, .. } = &s.columns[0] else {
            panic!()
        };
        match expr {
            Expr::Binary {
                op: BinaryOp::Add,
                right,
                ..
            } => assert!(matches!(
                right.as_ref(),
                Expr::Binary {
                    op: BinaryOp::Mul,
                    ..
                }
            )),
            other => panic!("expected Add at root, got {other:?}"),
        }
    }

    #[test]
    fn pratt_and_binds_tighter_than_or() {
        // a OR b AND c == a OR (b AND c)
        let Stmt::Select(s) = &parse("SELECT a OR b AND c;").unwrap()[0] else {
            panic!()
        };
        let ResultColumn::Expr { expr, .. } = &s.columns[0] else {
            panic!()
        };
        assert!(matches!(
            expr,
            Expr::Binary {
                op: BinaryOp::Or,
                ..
            }
        ));
    }

    #[test]
    fn literals_and_bind_params() {
        let Stmt::Select(s) =
            &parse("SELECT 42, 3.5, 'it''s', x'4869', NULL, ?, :name;").unwrap()[0]
        else {
            panic!()
        };
        let lits: Vec<&Expr> = s
            .columns
            .iter()
            .map(|c| match c {
                ResultColumn::Expr { expr, .. } => expr,
                _ => panic!(),
            })
            .collect();
        assert_eq!(lits[0], &Expr::Literal(Literal::Integer(42)));
        assert_eq!(lits[1], &Expr::Literal(Literal::Real(3.5)));
        assert_eq!(lits[2], &Expr::Literal(Literal::Text("it's".into())));
        assert_eq!(lits[3], &Expr::Literal(Literal::Blob(vec![0x48, 0x69])));
        assert_eq!(lits[4], &Expr::Literal(Literal::Null));
        assert_eq!(lits[5], &Expr::BindParam("?".into()));
        assert_eq!(lits[6], &Expr::BindParam(":name".into()));
    }

    #[test]
    fn create_table_with_constraints() {
        let Stmt::CreateTable(ct) =
            &parse("CREATE TABLE IF NOT EXISTS main.t (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, qty INT DEFAULT 0);")
                .unwrap()[0]
        else {
            panic!()
        };
        assert!(ct.if_not_exists);
        assert_eq!(ct.schema.as_deref(), Some("main"));
        assert_eq!(ct.name, "t");
        assert_eq!(ct.columns.len(), 3);
        assert_eq!(ct.columns[0].name, "id");
        assert_eq!(ct.columns[0].type_name.as_deref(), Some("INTEGER"));
        assert!(matches!(
            ct.columns[0].constraints[0],
            ColumnConstraint::PrimaryKey {
                autoincrement: true,
                ..
            }
        ));
        assert!(matches!(
            ct.columns[1].constraints[0],
            ColumnConstraint::NotNull
        ));
    }

    #[test]
    fn insert_values() {
        let Stmt::Insert(ins) =
            &parse("INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y');").unwrap()[0]
        else {
            panic!()
        };
        assert_eq!(ins.table, "t");
        assert_eq!(ins.columns, vec!["a", "b"]);
        assert_eq!(ins.rows.len(), 2);
        assert_eq!(ins.rows[0][0], Expr::Literal(Literal::Integer(1)));

        let Stmt::Insert(ins) = &parse("INSERT OR IGNORE INTO t VALUES (1);").unwrap()[0] else {
            panic!()
        };
        assert_eq!(ins.or_action, Some(ConflictAction::Ignore));
    }

    #[test]
    fn keyword_prefixed_identifiers_are_allowed() {
        // "select_count" must not be lexed as SELECT + "_count".
        let Stmt::Select(s) = &parse("SELECT select_count FROM orders;").unwrap()[0] else {
            panic!()
        };
        assert!(matches!(
            &s.columns[0],
            ResultColumn::Expr { expr: Expr::Column { name, .. }, .. } if name == "select_count"
        ));
    }

    #[test]
    fn explain_plain_wraps_select() {
        let stmts = parse("EXPLAIN SELECT 1;").unwrap();
        let Stmt::Explain(inner, kind) = &stmts[0] else {
            panic!("expected EXPLAIN")
        };
        assert_eq!(*kind, ExplainKind::Bytecode);
        assert!(matches!(inner.as_ref(), Stmt::Select(_)));
    }

    #[test]
    fn explain_query_plan_wraps_select() {
        let stmts = parse("EXPLAIN QUERY PLAN SELECT * FROM t;").unwrap();
        let Stmt::Explain(inner, kind) = &stmts[0] else {
            panic!("expected EXPLAIN QUERY PLAN")
        };
        assert_eq!(*kind, ExplainKind::QueryPlan);
        assert!(matches!(inner.as_ref(), Stmt::Select(_)));
    }

    #[test]
    fn delete_with_optional_where() {
        let Stmt::Delete(d) = &parse("DELETE FROM t;").unwrap()[0] else {
            panic!("expected DELETE")
        };
        assert_eq!(d.table, "t");
        assert!(d.where_clause.is_none());

        let Stmt::Delete(d) = &parse("DELETE FROM main.t WHERE x > 1;").unwrap()[0] else {
            panic!("expected DELETE")
        };
        assert_eq!(d.schema.as_deref(), Some("main"));
        assert_eq!(d.table, "t");
        assert!(d.where_clause.is_some());
    }

    #[test]
    fn plain_select_is_not_explain() {
        // Regression: an ordinary SELECT must still parse to `Stmt::Select`, not `Explain`.
        assert!(matches!(&parse("SELECT 1;").unwrap()[0], Stmt::Select(_)));
    }

    #[test]
    fn query_and_plan_are_non_reserved_identifiers() {
        // SQLite reserves EXPLAIN but NOT `query`/`plan`, so they remain valid column names
        // (verified against the oracle). The grammar must match that.
        let Stmt::Select(s) = &parse("SELECT plan, query FROM t;").unwrap()[0] else {
            panic!("expected SELECT")
        };
        assert!(matches!(
            &s.columns[0],
            ResultColumn::Expr { expr: Expr::Column { name, .. }, .. } if name == "plan"
        ));
        assert!(matches!(
            &s.columns[1],
            ResultColumn::Expr { expr: Expr::Column { name, .. }, .. } if name == "query"
        ));
    }
}

#[cfg(test)]
mod tmp_debug_tests {
    use super::*;
    #[test]
    fn tmp_create_semicolon() {
        let v = parse("CREATE TABLE t(a, b);").unwrap();
        assert_eq!(v.len(), 1, "parsed {} stmts", v.len());
    }
}

#[cfg(test)]
mod tmp_dbg2 {
    use super::*;
    #[test]
    fn dbg_create_pairs() {
        for s in ["SELECT 1;", "CREATE TABLE t(a, b);"] {
            let n = parse(s).map(|v| v.len());
            eprintln!("PARSERCRATE {s:?} -> {n:?}");
        }
    }
}
