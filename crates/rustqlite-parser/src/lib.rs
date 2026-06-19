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
            stmts.push(build_statement(pair)?);
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

fn build_statement(pair: Pair<'_, Rule>) -> Result<Stmt, ParseError> {
    let mut inner = pair.into_inner();
    let first = inner.next().expect("statement has at least one child");
    if first.as_rule() == Rule::explain_prefix {
        // An `explain_prefix` is followed by exactly one statement child (select/create/insert).
        let kind = explain_kind(&first);
        let body = inner.next().expect("explain_prefix precedes a statement");
        return Ok(Stmt::Explain(Box::new(build_inner_stmt(body)?), kind));
    }
    build_inner_stmt(first)
}

/// Build the select/create/insert/delete/drop/update statement from its grammar pair.
fn build_inner_stmt(pair: Pair<'_, Rule>) -> Result<Stmt, ParseError> {
    match pair.as_rule() {
        Rule::select_stmt => Ok(Stmt::Select(build_select(pair)?)),
        Rule::create_table_stmt => Ok(Stmt::CreateTable(build_create_table(pair))),
        Rule::insert_stmt => Ok(Stmt::Insert(build_insert(pair))),
        Rule::delete_stmt => Ok(Stmt::Delete(build_delete(pair))),
        Rule::drop_table_stmt => Ok(Stmt::DropTable(build_drop_table(pair))),
        Rule::update_stmt => Ok(Stmt::Update(build_update(pair))),
        Rule::create_index_stmt => Ok(Stmt::CreateIndex(build_create_index(pair))),
        Rule::drop_index_stmt => Ok(Stmt::DropIndex(build_drop_index(pair))),
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

fn build_select(pair: Pair<'_, Rule>) -> Result<SelectStmt, ParseError> {
    // select_stmt = with_clause? ~ select_core ~ (compound_op ~ select_core)*
    //               ~ order_item? ~ limit_item?
    let mut stmt: Option<SelectStmt> = None;
    let mut pending_op: Option<CompoundOperator> = None;
    let mut with_clause: Option<WithClause> = None;

    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::with_clause => {
                with_clause = Some(build_with_clause(part)?);
            }
            Rule::select_core => {
                let core = build_select_core(part)?;
                match stmt {
                    None => stmt = Some(core),
                    Some(ref mut base) => {
                        let op = pending_op
                            .take()
                            .expect("compound_op precedes a non-leading core");
                        base.compound.push((op, core));
                    }
                }
            }
            Rule::compound_op => pending_op = Some(build_compound_op(part)),
            // ORDER BY / LIMIT bind to the whole compound, so they live on the leading core.
            Rule::order_item => {
                stmt.as_mut().expect("order_item follows a core").order_by = build_order_item(part)
            }
            Rule::limit_item => {
                build_limit_item(part, stmt.as_mut().expect("limit follows a core"))
            }
            _ => {}
        }
    }
    let mut stmt = stmt.ok_or_else(|| ParseError::new("select_stmt has at least one select_core"))?;
    stmt.with_clause = with_clause;
    Ok(stmt)
}

fn build_with_clause(pair: Pair<'_, Rule>) -> Result<WithClause, ParseError> {
    let mut recursive = false;
    let mut ctes = Vec::new();
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::with_prefix => {
                for kw in part.into_inner() {
                    if kw.as_rule() == Rule::K_RECURSIVE {
                        recursive = true;
                    }
                }
            }
            Rule::cte_list => {
                ctes = part.into_inner().map(build_cte).collect::<Result<_, _>>()?;
            }
            _ => {}
        }
    }
    Ok(WithClause { recursive, ctes })
}

fn build_cte(pair: Pair<'_, Rule>) -> Result<Cte, ParseError> {
    let mut name = String::new();
    let mut columns = Vec::new();
    let mut query: Option<SelectStmt> = None;
    let mut materialized: Option<bool> = None;
    let mut after_as = false;
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::ident if name.is_empty() => name = part.as_str().to_string(),
            Rule::column_list => {
                columns = part
                    .into_inner()
                    .filter(|p| p.as_rule() == Rule::ident)
                    .map(|p| p.as_str().to_string())
                    .collect();
            }
            Rule::K_AS => after_as = true,
            Rule::K_MATERIALIZED if after_as => materialized = Some(true),
            Rule::K_NOT if after_as && materialized.is_none() => materialized = Some(false),
            Rule::select_stmt => query = Some(build_select(part)?),
            _ => {}
        }
    }
    Ok(Cte {
        name,
        columns,
        query: query.expect("cte has a select_stmt"),
        materialized,
    })
}

fn build_select_core(pair: Pair<'_, Rule>) -> Result<SelectStmt, ParseError> {
    let mut stmt = SelectStmt {
        distinct: false,
        columns: Vec::new(),
        from: Vec::new(),
        where_clause: None,
        group_by: Vec::new(),
        having: None,
        compound: Vec::new(),
        order_by: Vec::new(),
        limit: None,
        offset: None,
        with_clause: None,
    };

    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::K_DISTINCT => stmt.distinct = true,
            Rule::result_columns => stmt.columns = build_result_columns(part),
            Rule::from_item => stmt.from = build_from_item(part)?,
            Rule::where_item => stmt.where_clause = Some(build_expr_item(part)),
            Rule::group_item => stmt.group_by = build_group_item(part),
            Rule::having_item => stmt.having = Some(build_expr_item(part)),
            _ => {} // K_SELECT, K_ALL
        }
    }
    Ok(stmt)
}

fn build_compound_op(pair: Pair<'_, Rule>) -> CompoundOperator {
    // compound_op = { union_all | union | intersect | except }
    let inner = pair
        .into_inner()
        .next()
        .expect("compound_op wraps a specific operator");
    match inner.as_rule() {
        Rule::union_all => CompoundOperator::UnionAll,
        Rule::union => CompoundOperator::Union,
        Rule::intersect => CompoundOperator::Intersect,
        Rule::except => CompoundOperator::Except,
        other => unreachable!("unexpected compound_op child {other:?}"),
    }
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
    // table_alias = { table_as_alias | implicit_alias } where each contains an alias.
    fn find_alias(pair: Pair<'_, Rule>) -> Option<Pair<'_, Rule>> {
        if pair.as_rule() == Rule::alias {
            return Some(pair);
        }
        for child in pair.into_inner() {
            if let Some(found) = find_alias(child) {
                return Some(found);
            }
        }
        None
    }
    find_alias(pair)
        .expect("alias wrapper has an alias")
        .as_str()
        .to_string()
}

fn build_from_item(pair: Pair<'_, Rule>) -> Result<Vec<TableOrJoin>, ParseError> {
    // from_item = { K_FROM ~ from_clause }
    let from_clause = pair
        .into_inner()
        .find(|p| p.as_rule() == Rule::from_clause)
        .expect("from_item has from_clause");
    from_clause
        .into_inner()
        .filter(|p| p.as_rule() == Rule::table_ref_with_joins)
        .map(build_table_ref_with_joins)
        .collect()
}

/// A `table_ref_with_joins` parses as a leading `table_ref` followed by zero or more explicit
/// join suffixes. The grammar is right-recursive, so a chain `a JOIN b JOIN c` arrives as
/// `a, JOIN b, (JOIN c)` nested inside the last suffix. We fold it back into the left-deep
/// tree `(a JOIN b) JOIN c` that upstream's SrcList represents.
fn build_table_ref_with_joins(pair: Pair<'_, Rule>) -> Result<TableOrJoin, ParseError> {
    let mut items: Vec<Pair<'_, Rule>> = pair.into_inner().collect();
    assert!(
        !items.is_empty(),
        "table_ref_with_joins has a leading table_ref"
    );
    let mut acc = TableOrJoin::Table(build_table_ref(items.remove(0)));
    for suffix in items {
        let (op, right, constraint) = build_join_suffix(suffix)?;
        acc = TableOrJoin::Join(Join {
            op,
            left: Box::new(acc),
            right,
            constraint,
        });
    }
    Ok(acc)
}
fn build_join_suffix(
    pair: Pair<'_, Rule>,
) -> Result<(JoinOp, TableRef, Option<JoinConstraint>), ParseError> {
    let mut keywords: Vec<&str> = Vec::new();
    let mut table: Option<TableRef> = None;
    let mut constraint: Option<JoinConstraint> = None;

    fn descend_on_using(pair: Pair<'_, Rule>, constraint: &mut Option<JoinConstraint>) {
        match pair.as_rule() {
            Rule::on_clause => {
                *constraint = Some(JoinConstraint::On(build_expr_item(pair)));
            }
            Rule::using_clause => {
                let cols: Vec<String> = pair
                    .into_inner()
                    .filter(|p| p.as_rule() == Rule::ident)
                    .map(|p| p.as_str().to_string())
                    .collect();
                *constraint = Some(JoinConstraint::Using(cols));
            }
            _ => {}
        }
    }

    // Collect all join-modifier keywords that appear anywhere inside the `join_op` wrapper.
    fn collect_join_keywords<'a>(pair: Pair<'a, Rule>, out: &mut Vec<&'a str>) {
        if pair.as_rule() == Rule::join_modifier {
            out.push(pair.as_str());
        } else {
            for child in pair.into_inner() {
                collect_join_keywords(child, out);
            }
        }
    }

    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::join_op => collect_join_keywords(part, &mut keywords),
            Rule::table_ref => table = Some(build_table_ref(part)),
            Rule::on_using => {
                // `on_using` is a wrapper around exactly one `on_clause` or `using_clause`.
                if let Some(inner) = part.into_inner().next() {
                    descend_on_using(inner, &mut constraint);
                }
            }
            Rule::on_clause | Rule::using_clause => descend_on_using(part, &mut constraint),
            _ => {}
        }
    }
    let op = JoinOp::from_keywords(&keywords)
        .map_err(|bad| ParseError::new(format!("invalid join type: {bad}")))?;
    Ok((op, table.expect("join_suffix has a table_ref"), constraint))
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
            Rule::as_alias | Rule::table_alias | Rule::table_as_alias | Rule::implicit_alias => {
                alias = Some(build_as_alias(part))
            }
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

fn build_drop_table(pair: Pair<'_, Rule>) -> DropTableStmt {
    let mut stmt = DropTableStmt {
        if_exists: false,
        schema: None,
        name: String::new(),
    };
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::if_exists => stmt.if_exists = true,
            Rule::qualified_name => {
                let (s, n) = build_qualified_name(part);
                stmt.schema = s;
                stmt.name = n;
            }
            _ => {}
        }
    }
    stmt
}

fn build_update(pair: Pair<'_, Rule>) -> UpdateStmt {
    let mut stmt = UpdateStmt {
        or_action: None,
        schema: None,
        table: String::new(),
        assignments: Vec::new(),
        where_clause: None,
    };
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::or_action => stmt.or_action = Some(build_or_action(part)),
            Rule::qualified_name => {
                let (s, n) = build_qualified_name(part);
                stmt.schema = s;
                stmt.table = n;
            }
            Rule::assignment_list => stmt.assignments = build_assignment_list(part),
            Rule::where_item => stmt.where_clause = Some(build_expr_item(part)),
            _ => {}
        }
    }
    stmt
}

fn build_or_action(pair: Pair<'_, Rule>) -> ConflictAction {
    // or_action = { K_OR ~ (K_ROLLBACK | K_ABORT | K_FAIL | K_IGNORE | K_REPLACE) }
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::K_ROLLBACK => return ConflictAction::Rollback,
            Rule::K_ABORT => return ConflictAction::Abort,
            Rule::K_FAIL => return ConflictAction::Fail,
            Rule::K_IGNORE => return ConflictAction::Ignore,
            Rule::K_REPLACE => return ConflictAction::Replace,
            _ => {}
        }
    }
    ConflictAction::Abort
}

fn build_assignment_list(pair: Pair<'_, Rule>) -> Vec<Assignment> {
    pair.into_inner().map(build_assignment).collect()
}

fn build_assignment(pair: Pair<'_, Rule>) -> Assignment {
    let mut column = String::new();
    let mut value = None;
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::ident => column = part.as_str().to_string(),
            Rule::expr => value = Some(expr::build_expr(part)),
            _ => {}
        }
    }
    Assignment {
        column,
        value: value.expect("assignment has a value expr"),
    }
}

fn build_create_index(pair: Pair<'_, Rule>) -> CreateIndex {
    let mut ci = CreateIndex {
        unique: false,
        if_not_exists: false,
        schema: None,
        name: String::new(),
        table: String::new(),
        columns: Vec::new(),
        where_clause: None,
    };
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::K_UNIQUE => ci.unique = true,
            Rule::if_not_exists => ci.if_not_exists = true,
            // The first qualified_name is the index's own name (and optional schema).
            Rule::qualified_name if ci.name.is_empty() => {
                let (s, n) = build_qualified_name(part);
                ci.schema = s;
                ci.name = n;
            }
            // The bare ident after K_ON is the table being indexed.
            Rule::ident if ci.table.is_empty() => {
                ci.table = part.as_str().to_string();
            }
            Rule::indexed_column => ci.columns.push(build_indexed_column(part)),
            Rule::where_item => ci.where_clause = Some(build_expr_item(part)),
            _ => {}
        }
    }
    ci
}

fn build_indexed_column(pair: Pair<'_, Rule>) -> IndexedColumn {
    let mut name: Option<String> = None;
    let mut expr: Option<Expr> = None;
    let mut collation = None;
    let mut desc = false;
    // Walk children. The first child that is an `ident` or `expr` establishes the indexed
    // key. A bare identifier is stored as `name` and, if it is genuinely just a column name
    // (not an expression such as `a+1`), as an `Expr::Column` so that code that evaluates index
    // keys can treat plain columns and expressions uniformly. Real expression indexes keep
    // `name` empty so downstream code can tell them apart.
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::ident if name.is_none() && expr.is_none() => {
                let n = part.as_str().to_string();
                name = Some(n.clone());
                expr = Some(Expr::Column {
                    schema: None,
                    table: None,
                    name: n,
                });
            }
            Rule::expr if expr.is_none() => {
                let built = expr::build_expr(part);
                // If the expression is a bare column reference, treat it as a plain-column
                // index (preserves `name`). Anything more complex is a true expression index.
                if let Expr::Column { name: col, .. } = &built {
                    name = Some(col.clone());
                }
                expr = Some(built);
            }
            Rule::ident => collation = Some(part.as_str().to_string()),
            Rule::K_DESC => desc = true,
            Rule::K_ASC => desc = false,
            _ => {}
        }
    }
    IndexedColumn {
        name: name.unwrap_or_default(),
        expr,
        collation,
        desc,
    }
}

fn build_drop_index(pair: Pair<'_, Rule>) -> DropIndexStmt {
    let mut stmt = DropIndexStmt {
        if_exists: false,
        schema: None,
        name: String::new(),
    };
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::if_exists => stmt.if_exists = true,
            Rule::qualified_name => {
                let (s, n) = build_qualified_name(part);
                stmt.schema = s;
                stmt.name = n;
            }
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
        assert_eq!(s.from[0].table().unwrap().name, "t");
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
        assert_eq!(s.from[0].table().unwrap().alias.as_deref(), Some("alias"));
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
    fn drop_table_optional_if_exists() {
        let Stmt::DropTable(d) = &parse("DROP TABLE t;").unwrap()[0] else {
            panic!("expected DROP TABLE")
        };
        assert_eq!(d.name, "t");
        assert!(!d.if_exists);

        let Stmt::DropTable(d) = &parse("DROP TABLE IF EXISTS main.t;").unwrap()[0] else {
            panic!("expected DROP TABLE IF EXISTS")
        };
        assert!(d.if_exists);
        assert_eq!(d.schema.as_deref(), Some("main"));
        assert_eq!(d.name, "t");
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

    #[test]
    fn update_simple_and_or_action() {
        let Stmt::Update(u) = &parse("UPDATE t SET a = 1;").unwrap()[0] else {
            panic!("expected UPDATE")
        };
        assert!(u.or_action.is_none());
        assert_eq!(u.table, "t");
        assert_eq!(u.assignments.len(), 1);
        assert_eq!(u.assignments[0].column, "a");
        assert_eq!(u.assignments[0].value, Expr::Literal(Literal::Integer(1)));
        assert!(u.where_clause.is_none());

        let Stmt::Update(u) =
            &parse("UPDATE OR REPLACE main.t SET a = a + 1, b = 'x' WHERE a > 0;").unwrap()[0]
        else {
            panic!("expected UPDATE")
        };
        assert_eq!(u.or_action, Some(ConflictAction::Replace));
        assert_eq!(u.schema.as_deref(), Some("main"));
        assert_eq!(u.table, "t");
        assert_eq!(u.assignments.len(), 2);
        assert_eq!(u.assignments[0].column, "a");
        assert_eq!(u.assignments[1].column, "b");
        assert!(u.where_clause.is_some());
    }

    #[test]
    fn update_rejects_bad_syntax() {
        // Missing SET clause.
        assert!(parse("UPDATE t WHERE a = 1;").is_err());
        // Missing value on assignment.
        assert!(parse("UPDATE t SET a;").is_err());
        // Trailing comma.
        assert!(parse("UPDATE t SET a = 1,;").is_err());
    }

    #[test]
    fn bare_min_i64_literal_is_integer_not_real() {
        // SQLite parses the literal `-9223372036854775808` as INTEGER (the minimum
        // signed 64-bit value). Anything beyond that, including `-9223372036854775809`,
        // overflows and becomes REAL.
        let cases = [
            ("SELECT -9223372036854775808;", Literal::Integer(i64::MIN)),
            (
                "SELECT -9223372036854775809;",
                Literal::Real(-9223372036854775809.0),
            ),
            ("SELECT +9223372036854775807;", Literal::Integer(i64::MAX)),
            (
                "SELECT 9223372036854775808;",
                Literal::Real(9223372036854775808.0),
            ),
        ];
        for (sql, expected) in cases {
            let Stmt::Select(s) = &parse(sql).unwrap()[0] else {
                panic!("expected SELECT for {sql}")
            };
            let ResultColumn::Expr { expr, .. } = &s.columns[0] else {
                panic!("expected expression result for {sql}")
            };
            assert_eq!(expr, &Expr::Literal(expected), "{sql}");
        }
    }

    #[test]
    fn between_and_not_between() {
        let Stmt::Select(s) = &parse("SELECT 1 WHERE 5 BETWEEN 1 AND 10;").unwrap()[0] else {
            panic!()
        };
        assert!(matches!(
            s.where_clause,
            Some(Expr::Between { ref expr, ref low, ref high, negated }) if matches!(expr.as_ref(), Expr::Literal(Literal::Integer(5))) && matches!(low.as_ref(), Expr::Literal(Literal::Integer(1))) && matches!(high.as_ref(), Expr::Literal(Literal::Integer(10))) && !negated
        ));
        let Stmt::Select(s) = &parse("SELECT 1 WHERE 5 NOT BETWEEN 1 AND 10;").unwrap()[0] else {
            panic!()
        };
        assert!(matches!(
            s.where_clause,
            Some(Expr::Between { negated: true, .. })
        ));
    }

    #[test]
    fn in_and_not_in_value_list() {
        let Stmt::Select(s) = &parse("SELECT 1 WHERE 5 IN (1, 2, 3);").unwrap()[0] else {
            panic!()
        };
        assert!(matches!(
            s.where_clause,
            Some(Expr::In { ref expr, ref values, negated }) if values.len() == 3 && !negated && matches!(expr.as_ref(), Expr::Literal(Literal::Integer(5)))
        ));
        let Stmt::Select(s) = &parse("SELECT 1 WHERE 5 NOT IN (1, 2);").unwrap()[0] else {
            panic!()
        };
        assert!(matches!(
            s.where_clause,
            Some(Expr::In { negated: true, .. })
        ));
    }

    #[test]
    fn exists_subquery() {
        let Stmt::Select(s) = &parse("SELECT 1 WHERE EXISTS (SELECT 1);").unwrap()[0] else {
            panic!()
        };
        assert!(matches!(s.where_clause, Some(Expr::Exists(_))));
    }

    #[test]
    fn scalar_subquery_in_expression() {
        // Subquery as a result column.
        let Stmt::Select(s) = &parse("SELECT (SELECT 1);").unwrap()[0] else {
            panic!()
        };
        let ResultColumn::Expr { expr, .. } = &s.columns[0] else {
            panic!()
        };
        assert!(matches!(expr, Expr::Subquery(_)));

        // Subquery on the right of a comparison, in a WHERE clause.
        let Stmt::Select(s) = &parse("SELECT 1 WHERE x = (SELECT max(y) FROM t);").unwrap()[0]
        else {
            panic!()
        };
        let Some(Expr::Binary { right, .. }) = &s.where_clause else {
            panic!()
        };
        assert!(matches!(**right, Expr::Subquery(_)));

        // A parenthesised ordinary expression must NOT be parsed as a subquery (backtracking).
        let Stmt::Select(s) = &parse("SELECT (1 + 2);").unwrap()[0] else {
            panic!()
        };
        let ResultColumn::Expr { expr, .. } = &s.columns[0] else {
            panic!()
        };
        assert!(matches!(expr, Expr::Binary { .. }));
    }

    #[test]
    fn cast_expression() {
        let Stmt::Select(s) = &parse("SELECT CAST('123' AS INTEGER);").unwrap()[0] else {
            panic!()
        };
        assert!(matches!(
            s.columns[0],
            ResultColumn::Expr { ref expr, .. } if matches!(expr, Expr::Cast { type_name, .. } if type_name == "INTEGER")
        ));
    }

    #[test]
    fn case_expression() {
        let Stmt::Select(s) =
            &parse("SELECT CASE 1 WHEN 1 THEN 'one' ELSE 'other' END;").unwrap()[0]
        else {
            panic!()
        };
        let ResultColumn::Expr { expr, .. } = &s.columns[0] else {
            panic!()
        };
        let Expr::Case {
            base,
            when_then,
            else_expr,
        } = expr
        else {
            panic!()
        };
        assert!(base.is_some());
        assert_eq!(when_then.len(), 1);
        assert!(else_expr.is_some());

        let Stmt::Select(s) = &parse("SELECT CASE WHEN 1=1 THEN 'yes' ELSE 'no' END;").unwrap()[0]
        else {
            panic!()
        };
        let ResultColumn::Expr { expr, .. } = &s.columns[0] else {
            panic!()
        };
        let Expr::Case {
            base,
            when_then,
            else_expr,
        } = expr
        else {
            panic!()
        };
        assert!(base.is_none());
        assert_eq!(when_then.len(), 1);
        assert!(else_expr.is_some());
    }

    #[test]
    fn collate_expression() {
        let Stmt::Select(s) = &parse("SELECT 1 COLLATE NOCASE;").unwrap()[0] else {
            panic!()
        };
        let ResultColumn::Expr { expr, .. } = &s.columns[0] else {
            panic!()
        };
        assert!(matches!(expr, Expr::Collate { collation, .. } if collation == "NOCASE"));
    }

    #[test]
    fn is_distinct_from() {
        let Stmt::Select(s) = &parse("SELECT 1 IS DISTINCT FROM 2;").unwrap()[0] else {
            panic!()
        };
        let ResultColumn::Expr { expr, .. } = &s.columns[0] else {
            panic!()
        };
        assert!(matches!(expr, Expr::IsDistinctFrom { negated: false, .. }));
        let Stmt::Select(s) = &parse("SELECT 1 IS NOT DISTINCT FROM 1;").unwrap()[0] else {
            panic!()
        };
        let ResultColumn::Expr { expr, .. } = &s.columns[0] else {
            panic!()
        };
        assert!(matches!(expr, Expr::IsDistinctFrom { negated: true, .. }));
    }

    #[test]
    fn bitwise_operators_parse() {
        // Each operator is parsed at the precedence level expected by SQLite.
        let Stmt::Select(s) = &parse("SELECT 5 & 3;").unwrap()[0] else {
            panic!()
        };
        let ResultColumn::Expr { expr, .. } = &s.columns[0] else {
            panic!()
        };
        assert!(matches!(
            expr,
            Expr::Binary {
                op: BinaryOp::BitAnd,
                ..
            }
        ));

        let Stmt::Select(s) = &parse("SELECT 5 | 3;").unwrap()[0] else {
            panic!()
        };
        let ResultColumn::Expr { expr, .. } = &s.columns[0] else {
            panic!()
        };
        assert!(matches!(
            expr,
            Expr::Binary {
                op: BinaryOp::BitOr,
                ..
            }
        ));

        let Stmt::Select(s) = &parse("SELECT 5 << 1;").unwrap()[0] else {
            panic!()
        };
        let ResultColumn::Expr { expr, .. } = &s.columns[0] else {
            panic!()
        };
        assert!(matches!(
            expr,
            Expr::Binary {
                op: BinaryOp::ShiftLeft,
                ..
            }
        ));

        let Stmt::Select(s) = &parse("SELECT 5 >> 1;").unwrap()[0] else {
            panic!()
        };
        let ResultColumn::Expr { expr, .. } = &s.columns[0] else {
            panic!()
        };
        assert!(matches!(
            expr,
            Expr::Binary {
                op: BinaryOp::ShiftRight,
                ..
            }
        ));

        let Stmt::Select(s) = &parse("SELECT ~5;").unwrap()[0] else {
            panic!()
        };
        let ResultColumn::Expr { expr, .. } = &s.columns[0] else {
            panic!()
        };
        assert!(matches!(
            expr,
            Expr::Unary {
                op: UnaryOp::BitNot,
                ..
            }
        ));
    }

    #[test]
    fn bitwise_shift_not_confused_with_comparison() {
        // Regression: `<<` and `>>` must not be tokenised as two `<`/`>` comparisons.
        assert!(parse("SELECT 5 << 1;").is_ok());
        assert!(parse("SELECT 5 >> 1;").is_ok());
        assert!(parse("SELECT 5 <> 1;").is_ok());
    }

    #[test]
    fn json_extract_operators_parse() {
        // `->` extracts JSON; `->>` extracts a SQL value.  `->>` must win the longest match.
        let Stmt::Select(s) = &parse("SELECT a -> 'b';").unwrap()[0] else {
            panic!()
        };
        let ResultColumn::Expr { expr, .. } = &s.columns[0] else {
            panic!()
        };
        assert!(matches!(
            expr,
            Expr::Binary {
                op: BinaryOp::JsonExtract,
                ..
            }
        ));

        let Stmt::Select(s) = &parse("SELECT a ->> 'b';").unwrap()[0] else {
            panic!()
        };
        let ResultColumn::Expr { expr, .. } = &s.columns[0] else {
            panic!()
        };
        assert!(matches!(
            expr,
            Expr::Binary {
                op: BinaryOp::JsonExtractText,
                ..
            }
        ));

        // Left-associative chaining: `a -> 'b' ->> 'c'` => ((a -> 'b') ->> 'c').
        let Stmt::Select(s) = &parse("SELECT a -> 'b' ->> 'c';").unwrap()[0] else {
            panic!()
        };
        let ResultColumn::Expr { expr, .. } = &s.columns[0] else {
            panic!()
        };
        let Expr::Binary {
            op: BinaryOp::JsonExtractText,
            left,
            ..
        } = expr
        else {
            panic!("outermost op should be ->>")
        };
        assert!(matches!(
            **left,
            Expr::Binary {
                op: BinaryOp::JsonExtract,
                ..
            }
        ));
    }

    #[test]
    fn joins_parse_and_are_left_associative() {
        // Plain JOIN is INNER.
        let Stmt::Select(s) = &parse("SELECT * FROM t1 JOIN t2 ON t1.a = t2.b;").unwrap()[0] else {
            panic!()
        };
        let TableOrJoin::Join(j) = &s.from[0] else {
            panic!("expected a single Join node")
        };
        assert_eq!(j.op, JoinOp::Inner);
        assert!(matches!(j.constraint, Some(JoinConstraint::On(_))));
        assert_eq!(j.right.name, "t2");
        let TableOrJoin::Table(left) = j.left.as_ref() else {
            panic!("left side should be plain table")
        };
        assert_eq!(left.name, "t1");

        // Chained joins are left-deep.
        let Stmt::Select(s) =
            &parse("SELECT * FROM t1 JOIN t2 USING(a) LEFT JOIN t3 ON t1.b = t3.c;").unwrap()[0]
        else {
            panic!()
        };
        let TableOrJoin::Join(outer) = &s.from[0] else {
            panic!()
        };
        assert_eq!(outer.op, JoinOp::Left);
        assert_eq!(outer.right.name, "t3");
        let TableOrJoin::Join(inner) = outer.left.as_ref() else {
            panic!("expected nested join on the left")
        };
        assert_eq!(inner.op, JoinOp::Inner);
        assert_eq!(inner.right.name, "t2");
        assert!(matches!(inner.constraint, Some(JoinConstraint::Using(_))));
    }

    #[test]
    fn join_keyword_order_variants() {
        // Oracle: `LEFT NATURAL OUTER JOIN` is legal and means the same as
        // `NATURAL LEFT OUTER JOIN`. The keyword order `OUTER LEFT NATURAL` is accepted
        // by the upstream lexer but the final permutation check rejects it because
        // `OUTER` appears without an adjacent LEFT/RIGHT/FULL. We model the parser faithfully
        // enough to accept the same legal orderings.
        let Stmt::Select(s) = &parse("SELECT * FROM t1 NATURAL LEFT OUTER JOIN t2;").unwrap()[0]
        else {
            panic!()
        };
        let TableOrJoin::Join(j) = &s.from[0] else {
            panic!()
        };
        assert_eq!(j.op, JoinOp::Natural);
        assert!(j.constraint.is_none());

        // RIGHT and FULL OUTER are parsed (execution is later).
        let Stmt::Select(s) =
            &parse("SELECT * FROM t1 RIGHT OUTER JOIN t2 ON t1.a = t2.a;").unwrap()[0]
        else {
            panic!()
        };
        let TableOrJoin::Join(j) = &s.from[0] else {
            panic!()
        };
        assert_eq!(j.op, JoinOp::RightOuter);

        let Stmt::Select(s) =
            &parse("SELECT * FROM t1 FULL OUTER JOIN t2 ON t1.a = t2.a;").unwrap()[0]
        else {
            panic!()
        };
        let TableOrJoin::Join(j) = &s.from[0] else {
            panic!()
        };
        assert_eq!(j.op, JoinOp::FullOuter);
    }

    #[test]
    fn join_keywords_allowed_as_identifiers() {
        // SQLite allows join-related words as identifiers when not in a join context.
        let cases = [
            "SELECT 1 AS inner, 2 AS outer, 3 AS left, 4 AS right, 5 AS cross, 6 AS natural, 7 AS full;",
            "CREATE TABLE outer (a);",
            "CREATE TABLE naturalx (a);",
        ];
        for sql in cases {
            assert!(parse(sql).is_ok(), "{sql}");
        }
    }

    #[test]
    fn invalid_join_types_rejected() {
        assert!(parse("SELECT * FROM t1 INNER OUTER JOIN t2;").is_err());
        assert!(parse("SELECT * FROM t1 OUTER JOIN t2;").is_err());
        assert!(parse("SELECT * FROM t1 LEFT BOGUS JOIN t2;").is_err());
    }

    #[test]
    fn natural_join_may_not_have_on_or_using() {
        // These are still syntactically valid in the grammar; upstream rejects them at
        // semantic analysis. For the parser slice we just verify they parse.
        assert!(parse("SELECT * FROM t1 NATURAL JOIN t2 ON t1.a = t2.a;").is_ok());
        assert!(parse("SELECT * FROM t1 NATURAL JOIN t2 USING(a);").is_ok());
    }

    #[test]
    fn cte_with_recursive_parses() {
        let Stmt::Select(s) = &parse(
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM t WHERE n<3) SELECT * FROM t;",
        )
        .unwrap()[0]
        else {
            panic!("expected SELECT")
        };
        let wc = s.with_clause.as_ref().expect("WITH clause present");
        assert!(wc.recursive);
        assert_eq!(wc.ctes.len(), 1);
        assert_eq!(wc.ctes[0].name, "t");
        assert_eq!(wc.ctes[0].columns, vec!["n".to_string()]);
        assert_eq!(wc.ctes[0].query.compound.len(), 1);

        let Stmt::Select(s) = &parse(
            "WITH a AS (SELECT 1), b AS (SELECT 2) SELECT * FROM a, b;",
        )
        .unwrap()[0]
        else {
            panic!("expected SELECT")
        };
        let wc = s.with_clause.as_ref().expect("WITH clause present");
        assert!(!wc.recursive);
        assert_eq!(wc.ctes.len(), 2);
        assert_eq!(wc.ctes[0].name, "a");
        assert_eq!(wc.ctes[1].name, "b");
    }

    #[test]
    fn compound_select_operators_parse() {
        // UNION ALL must beat the plain UNION alternative.
        let Stmt::Select(s) = &parse("SELECT 1 UNION ALL SELECT 2;").unwrap()[0] else {
            panic!()
        };
        assert_eq!(s.compound.len(), 1);
        assert_eq!(s.compound[0].0, CompoundOperator::UnionAll);

        let Stmt::Select(s) = &parse("SELECT 1 UNION SELECT 2;").unwrap()[0] else {
            panic!()
        };
        assert_eq!(s.compound[0].0, CompoundOperator::Union);

        // Three cores chained with INTERSECT then EXCEPT; ORDER BY binds to the whole compound.
        let Stmt::Select(s) =
            &parse("SELECT a FROM t INTERSECT SELECT a FROM u EXCEPT SELECT a FROM v ORDER BY 1;")
                .unwrap()[0]
        else {
            panic!()
        };
        assert_eq!(s.compound.len(), 2);
        assert_eq!(s.compound[0].0, CompoundOperator::Intersect);
        assert_eq!(s.compound[1].0, CompoundOperator::Except);
        // The trailing ORDER BY lives on the leading core, not on any arm.
        assert_eq!(s.order_by.len(), 1);
        assert!(s.compound[0].1.order_by.is_empty());
        assert!(s.compound[1].1.order_by.is_empty());
        // Each arm carries its own FROM.
        assert_eq!(s.compound[1].1.from.len(), 1);

        // UNION/INTERSECT/EXCEPT are reserved: they cannot be used as bare identifiers.
        assert!(parse("SELECT 1 AS union;").is_err());
    }
}
