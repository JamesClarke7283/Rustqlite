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
pub mod walker;

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
        Rule::create_table_stmt => Ok(Stmt::CreateTable(build_create_table(pair)?)),
        Rule::insert_stmt => Ok(Stmt::Insert(build_insert(pair))),
        Rule::delete_stmt => Ok(Stmt::Delete(build_delete(pair))),
        Rule::drop_table_stmt => Ok(Stmt::DropTable(build_drop_table(pair))),
        Rule::update_stmt => Ok(Stmt::Update(build_update(pair)?)),
        Rule::create_index_stmt => Ok(Stmt::CreateIndex(build_create_index(pair))),
        Rule::drop_index_stmt => Ok(Stmt::DropIndex(build_drop_index(pair))),
        Rule::alter_table_stmt => Ok(Stmt::AlterTable(build_alter_table(pair))),
        Rule::create_view_stmt => Ok(Stmt::CreateView(build_create_view(pair)?)),
        Rule::drop_view_stmt => Ok(Stmt::DropView(build_drop_view(pair))),
        Rule::create_trigger_stmt => Ok(Stmt::CreateTrigger(build_create_trigger(pair)?)),
        Rule::drop_trigger_stmt => Ok(Stmt::DropTrigger(build_drop_trigger(pair))),
        Rule::pragma_stmt => Ok(Stmt::Pragma(build_pragma(pair))),
        Rule::transaction_stmt => Ok(Stmt::Transaction(build_transaction(pair))),
        Rule::attach_stmt => Ok(Stmt::Attach(build_attach(pair)?)),
        Rule::detach_stmt => Ok(Stmt::Detach(build_detach(pair)?)),
        Rule::vacuum_stmt => Ok(Stmt::Vacuum(build_vacuum(pair)?)),
        Rule::analyze_stmt => Ok(Stmt::Analyze(build_analyze(pair))),
        Rule::reindex_stmt => Ok(Stmt::Reindex(build_reindex(pair))),
        Rule::create_vtab_stmt => Ok(Stmt::CreateVirtualTable(build_create_vtab(pair))),
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
            Rule::window_clause => {
                stmt.as_mut().expect("window_clause follows a core").window_clause =
                    build_window_clause(part);
            }
            _ => {}
        }
    }
    let mut stmt =
        stmt.ok_or_else(|| ParseError::new("select_stmt has at least one select_core"))?;
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

fn build_window_clause(pair: Pair<'_, Rule>) -> Vec<NamedWindow> {
    let windowdefn_list = pair
        .into_inner()
        .find(|p| p.as_rule() == Rule::windowdefn_list)
        .expect("window_clause has windowdefn_list");
    windowdefn_list
        .into_inner()
        .filter(|p| p.as_rule() == Rule::windowdefn)
        .map(|wd| {
            let mut name = String::new();
            let mut spec: Option<Window> = None;
            for part in wd.into_inner() {
                match part.as_rule() {
                    Rule::ident => name = part.as_str().to_string(),
                    Rule::window => spec = Some(expr::build_window_spec(part)),
                    _ => {}
                }
            }
            NamedWindow {
                name,
                spec: spec.expect("windowdefn has a window"),
            }
        })
        .collect()
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
        window_clause: Vec::new(),
        values: Vec::new(),
    };

    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::values_core => stmt.values = build_values_core(part),
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

fn build_values_core(pair: Pair<'_, Rule>) -> Vec<Vec<Expr>> {
    pair.into_inner()
        .filter(|p| p.as_rule() == Rule::value_row)
        .map(|row| row.into_inner().map(expr::build_expr).collect())
        .collect()
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
        "table_ref_with_joins has a leading table_ref, subquery, or parenthesised join"
    );
    let mut acc = build_table_or_join_source(items.remove(0))?;
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

/// Build the leading element of a `table_ref_with_joins`: a plain table reference, a subquery
/// with alias, or a parenthesised join.
fn build_table_or_join_source(pair: Pair<'_, Rule>) -> Result<TableOrJoin, ParseError> {
    match pair.as_rule() {
        Rule::table_ref => Ok(TableOrJoin::Table(build_table_ref(pair))),
        Rule::table_subquery => Ok(build_table_subquery(pair)?),
        Rule::parenthesised_join => build_parenthesised_join(pair),
        other => unreachable!("unexpected table_ref_with_joins leading child {other:?}"),
    }
}

fn build_table_subquery(pair: Pair<'_, Rule>) -> Result<TableOrJoin, ParseError> {
    let mut query: Option<SelectStmt> = None;
    let mut alias: Option<String> = None;
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::select_stmt => query = Some(build_select(part)?),
            Rule::values_core => query = Some(build_values_select(part)),
            Rule::table_alias => alias = Some(build_as_alias(part)),
            _ => {}
        }
    }
    Ok(TableOrJoin::Subquery {
        query: Box::new(query.expect("table_subquery has a select_stmt or values_core")),
        alias: alias.expect("table_subquery has an alias"),
    })
}

/// Build a synthetic `SelectStmt` for a parenthesised `VALUES` used as a subquery in FROM.
/// The values rows are stored in `values`; `columns` is left empty so the codegen emits the
/// standard column1, column2, ... names.
fn build_values_select(pair: Pair<'_, Rule>) -> SelectStmt {
    SelectStmt {
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
        window_clause: Vec::new(),
        values: build_values_core(pair),
    }
}

fn build_parenthesised_join(pair: Pair<'_, Rule>) -> Result<TableOrJoin, ParseError> {
    let from_clause = pair
        .into_inner()
        .find(|p| p.as_rule() == Rule::from_clause)
        .expect("parenthesised_join has a from_clause");
    let mut refs: Vec<TableOrJoin> = from_clause
        .into_inner()
        .filter(|p| p.as_rule() == Rule::table_ref_with_joins)
        .map(build_table_ref_with_joins)
        .collect::<Result<_, _>>()?;
    if refs.len() == 1 {
        Ok(refs.pop().unwrap())
    } else {
        // Comma-separated list inside parentheses: model as a left-deep chain of implicit
        // Inner joins with no constraint, matching the outer from_clause treatment.
        let mut acc = refs.remove(0);
        for right in refs {
            let right_table = right.table().ok_or_else(|| {
                ParseError::new(
                    "expected plain table reference in parenthesised join list".to_string(),
                )
            })?;
            acc = TableOrJoin::Join(Join {
                op: JoinOp::Inner,
                left: Box::new(acc),
                right: right_table.clone(),
                constraint: None,
            });
        }
        Ok(acc)
    }
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
    let mut indexed_by = None;
    let mut args = None;
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::qualified_name => {
                let (s, n) = build_qualified_name(part);
                schema = s;
                name = n;
            }
            Rule::expr_list_opt => {
                args = Some(part.into_inner().map(expr::build_expr).collect());
            }
            Rule::as_alias | Rule::table_alias | Rule::table_as_alias | Rule::implicit_alias => {
                alias = Some(build_as_alias(part))
            }
            Rule::indexed_opt => indexed_by = Some(build_indexed_opt(part)),
            _ => {}
        }
    }
    TableRef {
        schema,
        name,
        alias,
        indexed_by,
        args,
    }
}

fn build_indexed_opt(pair: Pair<'_, Rule>) -> IndexedBy {
    let inner = pair.into_inner().next().expect("indexed_opt has one child");
    match inner.as_rule() {
        Rule::indexed_by => {
            let name = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::ident)
                .map(|p| p.as_str().to_string())
                .expect("indexed_by has an ident");
            IndexedBy::Index(name)
        }
        Rule::not_indexed => IndexedBy::NotIndexed,
        other => unreachable!("unexpected indexed_opt child {other:?}"),
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
        .map(|term| build_ordering_term(term))
        .collect()
}

/// Build a single `ordering_term` (`expr [ASC|DESC] [NULLS FIRST|LAST]`).
pub(crate) fn build_ordering_term(term: Pair<'_, Rule>) -> OrderingTerm {
    let mut desc = false;
    let mut expr = None;
    let mut nulls = None;
    for part in term.into_inner() {
        match part.as_rule() {
            Rule::expr => expr = Some(expr::build_expr(part)),
            Rule::K_DESC => desc = true,
            Rule::K_ASC => desc = false,
            Rule::nulls_opt => {
                nulls = Some(build_nulls_opt(part));
            }
            _ => {}
        }
    }
    OrderingTerm {
        expr: expr.expect("ordering_term has an expr"),
        desc,
        nulls,
    }
}

/// Build a `nulls_opt` (`NULLS FIRST` / `NULLS LAST`).
fn build_nulls_opt(pair: Pair<'_, Rule>) -> NullsOrder {
    let kind = pair
        .into_inner()
        .find(|p| p.as_rule() == Rule::K_FIRST || p.as_rule() == Rule::K_LAST)
        .expect("nulls_opt has a direction");
    match kind.as_rule() {
        Rule::K_FIRST => NullsOrder::First,
        Rule::K_LAST => NullsOrder::Last,
        other => unreachable!("unexpected nulls_opt child {other:?}"),
    }
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

/// Build a `limit_item` pair into `(limit, offset)` exprs (for DELETE/UPDATE which don't use
/// `SelectStmt`). The first expr is the LIMIT; an `offset_item`/`limit_comma` provides OFFSET.
fn build_limit_offset(pair: Pair<'_, Rule>) -> (Option<Expr>, Option<Expr>) {
    let mut limit = None;
    let mut offset = None;
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::expr if limit.is_none() => limit = Some(expr::build_expr(part)),
            Rule::offset_item | Rule::limit_comma => {
                let e = part
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::expr)
                    .expect("offset clause has an expr");
                offset = Some(expr::build_expr(e));
            }
            _ => {}
        }
    }
    (limit, offset)
}

fn build_create_table(pair: Pair<'_, Rule>) -> Result<CreateTable, ParseError> {
    let mut ct = CreateTable {
        temporary: false,
        if_not_exists: false,
        schema: None,
        name: String::new(),
        columns: Vec::new(),
        constraints: Vec::new(),
        without_rowid: false,
        strict: false,
        as_select: None,
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
            Rule::table_element => {
                // `table_element = { column_def | table_constraint }`; unwrap the inner.
                let inner = part.into_inner().next().expect("table_element has one child");
                match inner.as_rule() {
                    Rule::column_def => ct.columns.push(build_column_def(inner)),
                    Rule::table_constraint => ct.constraints.push(build_table_constraint(inner)),
                    other => unreachable!("unexpected table_element child {other:?}"),
                }
            }
            Rule::table_options => {
                for opt in part.into_inner().filter(|p| p.as_rule() == Rule::table_option) {
                    let inner = opt.into_inner().next().expect("table_option has one child");
                    match inner.as_rule() {
                        Rule::without_rowid_opt => {
                            // `K_WITHOUT ~ ident` where ident must be "rowid".
                            let name = inner
                                .into_inner()
                                .find(|p| p.as_rule() == Rule::ident)
                                .map(|p| p.as_str().to_string())
                                .unwrap_or_default();
                            if name.eq_ignore_ascii_case("rowid") {
                                ct.without_rowid = true;
                            }
                            // Non-"rowid" names are a parse-time error upstream; we leave the
                            // flag false (the engine should reject). For strict faithfulness a
                            // parse error would be raised; that is deferred.
                        }
                        Rule::strict_opt => ct.strict = true,
                        other => unreachable!("unexpected table_option child {other:?}"),
                    }
                }
            }
            Rule::select_stmt => ct.as_select = Some(build_select(part)?),
            _ => {}
        }
    }
    Ok(ct)
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
            let mut on_conflict = None;
            for part in inner.into_inner() {
                match part.as_rule() {
                    Rule::K_DESC => desc = true,
                    Rule::K_AUTOINCREMENT => autoincrement = true,
                    Rule::onconf => on_conflict = Some(build_onconf(part)),
                    _ => {}
                }
            }
            ColumnConstraint::PrimaryKey {
                desc,
                autoincrement,
                on_conflict,
            }
        }
        Rule::c_not_null => {
            let on_conflict = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::onconf)
                .map(build_onconf);
            ColumnConstraint::NotNull { on_conflict }
        }
        Rule::c_unique => {
            let on_conflict = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::onconf)
                .map(build_onconf);
            ColumnConstraint::Unique { on_conflict }
        }
        Rule::c_default => {
            let mut children = inner.into_inner();
            let kind = children.next().expect("c_default has K_DEFAULT");
            assert_eq!(kind.as_rule(), Rule::K_DEFAULT);
            let value = children.next().expect("c_default has a value child");
            let e = match value.as_rule() {
                Rule::expr => expr::build_expr(value),
                Rule::literal => expr::build_literal_expr(value),
                Rule::signed_number => Expr::Literal(expr::build_number(value.as_str())),
                other => unreachable!("unexpected c_default child {other:?}"),
            };
            ColumnConstraint::Default(e)
        }
        Rule::c_references => {
            let mut parent_table = String::new();
            let mut parent_columns: Option<Vec<String>> = None;
            let mut on_delete: Option<ReferenceAction> = None;
            let mut on_update: Option<ReferenceAction> = None;
            let mut deferrable: Option<Deferrable> = None;
            for part in inner.into_inner() {
                match part.as_rule() {
                    Rule::ident if parent_table.is_empty() => {
                        parent_table = part.as_str().to_string();
                    }
                    Rule::column_list => {
                        parent_columns = Some(
                            part.into_inner()
                                .filter(|p| p.as_rule() == Rule::ident)
                                .map(|p| p.as_str().to_string())
                                .collect(),
                        );
                    }
                    Rule::refargs => {
                        let mut tmp = References {
                            parent_table: String::new(),
                            parent_columns: None,
                            on_delete: None,
                            on_update: None,
                            deferrable: None,
                        };
                        apply_refargs(part, &mut tmp);
                        on_delete = tmp.on_delete;
                        on_update = tmp.on_update;
                    }
                    Rule::defer_subclause => {
                        deferrable = Some(build_defer_subclause(part));
                    }
                    _ => {}
                }
            }
            ColumnConstraint::References(References {
                parent_table,
                parent_columns,
                on_delete,
                on_update,
                deferrable,
            })
        }
        Rule::c_generated => {
            let mut expr: Option<Expr> = None;
            let mut stored = false;
            for part in inner.into_inner() {
                match part.as_rule() {
                    Rule::expr => expr = Some(expr::build_expr(part)),
                    Rule::K_STORED => stored = true,
                    Rule::K_VIRTUAL => stored = false,
                    _ => {}
                }
            }
            ColumnConstraint::Generated {
                expr: expr.expect("c_generated has an expr"),
                stored,
            }
        }
        other => unreachable!("unexpected constraint {other:?}"),
    }
}

fn build_table_constraint(pair: Pair<'_, Rule>) -> TableConstraint {
    let mut name: Option<String> = None;
    let mut body: Option<TableConstraintBody> = None;
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::constraint_name => {
                name = Some(
                    part.into_inner()
                        .find(|p| p.as_rule() == Rule::ident)
                        .map(|p| p.as_str().to_string())
                        .expect("constraint_name has an ident"),
                );
            }
            Rule::constraint_body => {
                body = Some(build_constraint_body(part));
            }
            _ => {}
        }
    }
    TableConstraint {
        name,
        body: body.expect("table_constraint has a constraint_body"),
    }
}

fn build_constraint_body(pair: Pair<'_, Rule>) -> TableConstraintBody {
    let inner = pair.into_inner().next().expect("constraint_body has one child");
    match inner.as_rule() {
        Rule::tc_primary_key => {
            let columns = build_sortlist(&inner);
            let on_conflict = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::onconf)
                .map(build_onconf);
            TableConstraintBody::PrimaryKey { columns, on_conflict }
        }
        Rule::tc_unique => {
            let columns = build_sortlist(&inner);
            let on_conflict = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::onconf)
                .map(build_onconf);
            TableConstraintBody::Unique { columns, on_conflict }
        }
        Rule::tc_check => {
            let mut on_conflict = None;
            let mut expr: Option<Expr> = None;
            for part in inner.into_inner() {
                match part.as_rule() {
                    Rule::expr => expr = Some(expr::build_expr(part)),
                    Rule::onconf => on_conflict = Some(build_onconf(part)),
                    _ => {}
                }
            }
            TableConstraintBody::Check {
                expr: expr.expect("tc_check has an expr"),
                on_conflict,
            }
        }
        Rule::tc_foreign_key => {
            let mut columns: Vec<String> = Vec::new();
            let mut references: Option<References> = None;
            for part in inner.into_inner() {
                match part.as_rule() {
                    Rule::idlist => {
                        columns = part
                            .into_inner()
                            .filter(|p| p.as_rule() == Rule::ident)
                            .map(|p| p.as_str().to_string())
                            .collect();
                    }
                    Rule::ident => {
                        // The parent table name.
                        if references.is_none() {
                            references = Some(References {
                                parent_table: part.as_str().to_string(),
                                parent_columns: None,
                                on_delete: None,
                                on_update: None,
                                deferrable: None,
                            });
                        }
                    }
                    Rule::column_list => {
                        if let Some(ref mut r) = references {
                            r.parent_columns = Some(
                                part.into_inner()
                                    .filter(|p| p.as_rule() == Rule::ident)
                                    .map(|p| p.as_str().to_string())
                                    .collect(),
                            );
                        }
                    }
                    Rule::refargs => {
                        if let Some(ref mut r) = references {
                            apply_refargs(part, r);
                        }
                    }
                    Rule::defer_subclause => {
                        if let Some(ref mut r) = references {
                            r.deferrable = Some(build_defer_subclause(part));
                        }
                    }
                    _ => {}
                }
            }
            TableConstraintBody::ForeignKey {
                columns,
                references: references.expect("tc_foreign_key has a REFERENCES clause"),
            }
        }
        other => unreachable!("unexpected constraint_body child {other:?}"),
    }
}

/// Build the `sortlist` for a PRIMARY KEY / UNIQUE table constraint: a comma-separated list
/// of column names with optional ASC/DESC.
fn build_sortlist(pair: &Pair<'_, Rule>) -> Vec<PrimaryKeyColumn> {
    pair.clone()
        .into_inner()
        .find(|p| p.as_rule() == Rule::sortlist)
        .map(|sl| {
            sl.into_inner()
                .filter(|p| p.as_rule() == Rule::sortlist_item)
                .map(|item| {
                    let mut name = String::new();
                    let mut desc = false;
                    for part in item.into_inner() {
                        match part.as_rule() {
                            Rule::ident => name = part.as_str().to_string(),
                            Rule::K_DESC => desc = true,
                            _ => {}
                        }
                    }
                    PrimaryKeyColumn { name, desc }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Apply `ON DELETE/UPDATE action` clauses from a `refargs` pair to a `References` struct.
fn apply_refargs(pair: Pair<'_, Rule>, r: &mut References) {
    for refarg in pair.into_inner().filter(|p| p.as_rule() == Rule::refarg) {
        let mut inner = refarg.into_inner();
        let _on_kw = inner.next().expect("refarg starts with K_ON");
        let on_target = inner.next().expect("refarg has K_DELETE/K_UPDATE");
        let action = build_refact(inner.next().expect("refarg has a refact"));
        match on_target.as_rule() {
            Rule::K_DELETE => r.on_delete = Some(action),
            Rule::K_UPDATE => r.on_update = Some(action),
            other => unreachable!("unexpected refarg target {other:?}"),
        }
    }
}

/// Build a `refact` (the action of `ON DELETE/UPDATE`): `SET NULL`, `SET DEFAULT`,
/// `CASCADE`, `RESTRICT`, or `NO ACTION`.
fn build_refact(pair: Pair<'_, Rule>) -> ReferenceAction {
    let mut inner = pair.into_inner();
    let first = inner.next().expect("refact has a keyword");
    match first.as_rule() {
        Rule::K_SET => {
            let kind = inner.next().expect("refact SET has a target");
            match kind.as_rule() {
                Rule::K_NULL => ReferenceAction::SetNull,
                Rule::K_DEFAULT => ReferenceAction::SetDefault,
                other => unreachable!("unexpected refact SET child {other:?}"),
            }
        }
        Rule::K_CASCADE => ReferenceAction::Cascade,
        Rule::K_RESTRICT => ReferenceAction::Restrict,
        Rule::K_NO => ReferenceAction::NoAction,
        other => unreachable!("unexpected refact child {other:?}"),
    }
}

/// Build a `defer_subclause`: `DEFERRABLE [INITIALLY DEFERRED|IMMEDIATE]` /
/// `NOT DEFERRABLE [INITIALLY DEFERRED|IMMEDIATE]`.
fn build_defer_subclause(pair: Pair<'_, Rule>) -> Deferrable {
    let mut inner = pair.into_inner();
    let first = inner.next().expect("defer_subclause has a keyword");
    match first.as_rule() {
        Rule::K_NOT => Deferrable::NotDeferrable,
        Rule::K_DEFERRABLE => {
            // Optional `INITIALLY DEFERRED|IMMEDIATE`.
            match inner.next() {
                Some(p) if p.as_rule() == Rule::init_deferred => {
                    // init_deferred = { K_INITIALLY ~ (K_DEFERRED | K_IMMEDIATE) }
                    let mut id_inner = p.into_inner();
                    let _ = id_inner.next(); // skip K_INITIALLY
                    let kind = id_inner.next().expect("init_deferred has a keyword");
                    match kind.as_rule() {
                        Rule::K_DEFERRED => Deferrable::DeferrableInitiallyDeferred,
                        Rule::K_IMMEDIATE => Deferrable::DeferrableInitiallyImmediate,
                        other => unreachable!("unexpected init_deferred child {other:?}"),
                    }
                }
                _ => Deferrable::DeferrableInitiallyImmediate,
            }
        }
        other => unreachable!("unexpected defer_subclause child {other:?}"),
    }
}

fn build_insert(pair: Pair<'_, Rule>) -> InsertStmt {
    let mut stmt = InsertStmt {
        or_action: None,
        schema: None,
        table: String::new(),
        columns: Vec::new(),
        source: InsertSource::Values(Vec::new()),
        upsert: Vec::new(),
        returning: None,
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
            Rule::insert_source => {
                stmt.source = build_insert_source(part).unwrap_or_else(|e| {
                    panic!("insert source parse error: {e}");
                });
            }
            Rule::upsert_clause => {
                let (upsert, returning) = build_upsert_clause(part);
                stmt.upsert.push(upsert);
                if returning.is_some() {
                    stmt.returning = returning;
                }
            }
            Rule::returning_clause => stmt.returning = Some(build_returning(part)),
            _ => {}
        }
    }
    stmt
}

fn build_upsert_clause(pair: Pair<'_, Rule>) -> (UpsertClause, Option<Vec<ResultColumn>>) {
    let mut target = None;
    let mut action = None;
    let mut returning = None;
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::upsert_target => {
                target = Some(build_upsert_target(part));
            }
            Rule::upsert_action => {
                action = Some(build_upsert_action(part));
            }
            Rule::returning_clause => returning = Some(build_returning(part)),
            _ => {}
        }
    }
    (
        UpsertClause {
            target,
            action: action.expect("upsert action present"),
        },
        returning,
    )
}

fn build_upsert_target(pair: Pair<'_, Rule>) -> UpsertTarget {
    let mut columns = Vec::new();
    let mut where_clause = None;
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::upsert_target_column => columns.push(build_upsert_target_column(part)),
            Rule::where_item => where_clause = Some(build_expr_item(part)),
            _ => {}
        }
    }
    UpsertTarget {
        columns,
        where_clause,
    }
}

fn build_upsert_target_column(pair: Pair<'_, Rule>) -> UpsertTargetColumn {
    // The grammar rule accepts either a bare `ident` with optional COLLATE/ASC/DESC,
    // or an arbitrary expression. Disambiguate by walking the children: if the
    // first meaningful child is an ident and the remaining children are only
    // COLLATE/ASC/DESC, treat it as a named column; otherwise treat it as an
    // expression.
    let mut name: Option<String> = None;
    let mut expr: Option<Expr> = None;
    let mut collation = None;
    let mut desc = false;
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
    if let Some(expr) = expr {
        if name.is_some() && matches!(&expr, Expr::Column { .. }) {
            UpsertTargetColumn::Column {
                name: name.unwrap_or_default(),
                collation,
                desc,
            }
        } else {
            UpsertTargetColumn::Expr(expr)
        }
    } else {
        UpsertTargetColumn::Column {
            name: name.unwrap_or_default(),
            collation,
            desc,
        }
    }
}

fn build_upsert_action(pair: Pair<'_, Rule>) -> UpsertAction {
    let mut assignments = Vec::new();
    let mut where_clause = None;
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::K_NOTHING => return UpsertAction::Nothing,
            Rule::assignment_list => assignments = build_assignment_list(part),
            Rule::where_item => where_clause = Some(build_expr_item(part)),
            _ => {}
        }
    }
    if assignments.is_empty() {
        // Fallback: if neither NOTHING nor UPDATE matched, treat as DO NOTHING so the
        // grammar stays resilient (this path should not be reached with a valid parse).
        UpsertAction::Nothing
    } else {
        UpsertAction::Update {
            assignments,
            where_clause,
        }
    }
}

fn build_insert_source(pair: Pair<'_, Rule>) -> Result<InsertSource, ParseError> {
    // The grammar rule is `insert_source = { select_stmt | values_clause | default_values_clause }`.
    // Because `select_stmt` itself includes a `values_core` alternative, a VALUES source may parse
    // as a `select_stmt` (specifically a select_core containing values). Distinguish them by
    // inspecting the parsed children directly: if there is a `values_clause` child, use it; if
    // there is a `default_values_clause` child, use it; otherwise treat the whole thing as SELECT.
    for part in pair.clone().into_inner() {
        match part.as_rule() {
            Rule::default_values_clause => {
                return Ok(InsertSource::DefaultValues);
            }
            Rule::values_clause => {
                let rows = part
                    .into_inner()
                    .filter(|p| p.as_rule() == Rule::value_row)
                    .map(|row| row.into_inner().map(expr::build_expr).collect())
                    .collect();
                return Ok(InsertSource::Values(rows));
            }
            Rule::select_stmt => {
                let mut select = build_select(part)?;
                // If the parsed "select" is actually just a VALUES core (because the grammar's
                // select_stmt alternative swallowed the VALUES clause), normalise it back to a
                // VALUES insert source. A real SELECT will have `from`, `where_clause`, etc.
                if !select.values.is_empty()
                    && select.columns.is_empty()
                    && select.from.is_empty()
                    && select.where_clause.is_none()
                    && select.group_by.is_empty()
                    && select.having.is_none()
                    && select.compound.is_empty()
                    && select.order_by.is_empty()
                    && select.limit.is_none()
                    && select.offset.is_none()
                    && select.with_clause.is_none()
                {
                    return Ok(InsertSource::Values(std::mem::take(&mut select.values)));
                }
                return Ok(InsertSource::Select(select));
            }
            _ => {}
        }
    }
    Err(ParseError::new("INSERT source must be VALUES or SELECT"))
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
        order_by: Vec::new(),
        limit: None,
        offset: None,
        returning: None,
    };
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::qualified_name => {
                let (s, n) = build_qualified_name(part);
                stmt.schema = s;
                stmt.table = n;
            }
            Rule::where_item => stmt.where_clause = Some(build_expr_item(part)),
            Rule::order_item => stmt.order_by = build_order_item(part),
            Rule::limit_item => {
                let (l, o) = build_limit_offset(part);
                stmt.limit = l;
                stmt.offset = o;
            }
            Rule::returning_clause => stmt.returning = Some(build_returning(part)),
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

fn build_update(pair: Pair<'_, Rule>) -> Result<UpdateStmt, ParseError> {
    let mut stmt = UpdateStmt {
        or_action: None,
        schema: None,
        table: String::new(),
        assignments: Vec::new(),
        from: Vec::new(),
        where_clause: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        returning: None,
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
            Rule::from_item => stmt.from = build_from_item(part)?,
            Rule::where_item => stmt.where_clause = Some(build_expr_item(part)),
            Rule::order_item => stmt.order_by = build_order_item(part),
            Rule::limit_item => {
                let (l, o) = build_limit_offset(part);
                stmt.limit = l;
                stmt.offset = o;
            }
            Rule::returning_clause => stmt.returning = Some(build_returning(part)),
            _ => {}
        }
    }
    Ok(stmt)
}

/// Build the result-column list inside a `RETURNING` clause.
fn build_returning(pair: Pair<'_, Rule>) -> Vec<ResultColumn> {
    pair.into_inner()
        .find(|p| p.as_rule() == Rule::result_columns)
        .map(build_result_columns)
        .unwrap_or_default()
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

/// Build the `onconf` rule: `ON CONFLICT (K_ROLLBACK | K_ABORT | K_FAIL | K_IGNORE | K_REPLACE)`.
/// Returns the `ConflictAction` for the constraint's `ON CONFLICT` clause.
fn build_onconf(pair: Pair<'_, Rule>) -> ConflictAction {
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

fn build_alter_table(pair: Pair<'_, Rule>) -> AlterTableStmt {
    let mut schema: Option<String> = None;
    let mut table = String::new();
    let mut action: Option<AlterTableAction> = None;
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::qualified_name => {
                let (s, n) = build_qualified_name(part);
                schema = s;
                table = n;
            }
            Rule::alter_action => {
                action = Some(build_alter_action(part));
            }
            _ => {}
        }
    }
    AlterTableStmt {
        schema,
        table,
        action: action.expect("alter_table_stmt has an alter_action"),
    }
}

fn build_alter_action(pair: Pair<'_, Rule>) -> AlterTableAction {
    let inner = pair
        .into_inner()
        .next()
        .expect("alter_action has one child");
    match inner.as_rule() {
        Rule::rename_to => {
            let new_name = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::ident)
                .map(|p| p.as_str().to_string())
                .expect("rename_to has an ident");
            AlterTableAction::RenameTo(new_name)
        }
        Rule::add_column => {
            let col_def = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::column_def)
                .map(build_column_def)
                .expect("add_column has a column_def");
            AlterTableAction::AddColumn(col_def)
        }
        Rule::drop_column => {
            let name = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::ident)
                .map(|p| p.as_str().to_string())
                .expect("drop_column has an ident");
            AlterTableAction::DropColumn(name)
        }
        Rule::rename_column => {
            let mut idents = inner.into_inner().filter(|p| p.as_rule() == Rule::ident);
            let old = idents
                .next()
                .map(|p| p.as_str().to_string())
                .expect("rename_column has an old name");
            let new = idents
                .next()
                .map(|p| p.as_str().to_string())
                .expect("rename_column has a new name");
            AlterTableAction::RenameColumn { old, new }
        }
        Rule::alter_column => {
            let mut name: Option<String> = None;
            let mut has_drop = false;
            for part in inner.into_inner() {
                match part.as_rule() {
                    Rule::ident => name = Some(part.as_str().to_string()),
                    Rule::K_DROP => has_drop = true,
                    _ => {}
                }
            }
            let name = name.expect("alter_column has a name");
            if has_drop {
                AlterTableAction::AlterColumnDropNotNull(name)
            } else {
                AlterTableAction::AlterColumnSetNotNull(name)
            }
        }
        Rule::add_constraint => {
            let mut name: Option<String> = None;
            let mut expr: Option<Expr> = None;
            for part in inner.into_inner() {
                match part.as_rule() {
                    Rule::ident => name = Some(part.as_str().to_string()),
                    Rule::expr => expr = Some(expr::build_expr(part)),
                    _ => {}
                }
            }
            AlterTableAction::AddCheckConstraint {
                name,
                expr: expr.expect("add_constraint has an expr"),
            }
        }
        Rule::drop_constraint => {
            let name = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::ident)
                .map(|p| p.as_str().to_string())
                .expect("drop_constraint has a name");
            AlterTableAction::DropConstraint(name)
        }
        other => unreachable!("unexpected alter_action child {other:?}"),
    }
}

fn build_create_view(pair: Pair<'_, Rule>) -> Result<CreateView, ParseError> {
    let mut temporary = false;
    let mut if_not_exists = false;
    let mut schema: Option<String> = None;
    let mut name = String::new();
    let mut columns: Vec<String> = Vec::new();
    let mut select: Option<SelectStmt> = None;
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::K_TEMPORARY | Rule::K_TEMP => temporary = true,
            Rule::if_not_exists => if_not_exists = true,
            Rule::qualified_name => {
                let (s, n) = build_qualified_name(part);
                schema = s;
                name = n;
            }
            Rule::column_list => {
                columns = part
                    .into_inner()
                    .filter(|p| p.as_rule() == Rule::ident)
                    .map(|p| p.as_str().to_string())
                    .collect();
            }
            Rule::select_stmt => select = Some(build_select(part)?),
            _ => {}
        }
    }
    Ok(CreateView {
        temporary,
        if_not_exists,
        schema,
        name,
        columns,
        select: select.expect("create_view_stmt has a select_stmt"),
    })
}

fn build_drop_view(pair: Pair<'_, Rule>) -> DropViewStmt {
    let mut stmt = DropViewStmt {
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

fn build_create_trigger(pair: Pair<'_, Rule>) -> Result<CreateTrigger, ParseError> {
    let mut temporary = false;
    let mut if_not_exists = false;
    let mut schema: Option<String> = None;
    let mut name = String::new();
    let mut timing = TriggerTime::Before;
    let mut event = TriggerEvent::Insert;
    let mut table_schema: Option<String> = None;
    let mut table = String::new();
    let mut for_each_row = false;
    let mut when_clause: Option<Expr> = None;
    let mut body: Vec<TriggerStep> = Vec::new();

    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::K_TEMPORARY | Rule::K_TEMP => temporary = true,
            Rule::if_not_exists => if_not_exists = true,
            Rule::qualified_name => {
                let (s, n) = build_qualified_name(part);
                if name.is_empty() {
                    schema = s;
                    name = n;
                } else {
                    table_schema = s;
                    table = n;
                }
            }
            Rule::trigger_time => timing = build_trigger_time(part),
            Rule::trigger_event => event = build_trigger_event(part),
            Rule::foreach_clause => for_each_row = true,
            Rule::when_clause => when_clause = Some(build_expr_item(part)),
            Rule::trigger_cmd_list => {
                body = build_trigger_cmd_list(part)?;
            }
            _ => {}
        }
    }

    Ok(CreateTrigger {
        temporary,
        if_not_exists,
        schema,
        name,
        timing,
        event,
        table_schema,
        table,
        for_each_row,
        when_clause,
        body,
    })
}

fn build_trigger_time(pair: Pair<'_, Rule>) -> TriggerTime {
    let inner = pair.into_inner().next().expect("trigger_time has a child");
    match inner.as_rule() {
        Rule::K_BEFORE => TriggerTime::Before,
        Rule::K_AFTER => TriggerTime::After,
        // `K_INSTEAD ~ K_OF` is one alternative; the inner pair is K_INSTEAD.
        Rule::K_INSTEAD => TriggerTime::InsteadOf,
        other => unreachable!("unexpected trigger_time child {other:?}"),
    }
}

fn build_trigger_event(pair: Pair<'_, Rule>) -> TriggerEvent {
    let mut inner = pair.into_inner();
    let first = inner.next().expect("trigger_event has a keyword");
    match first.as_rule() {
        Rule::K_DELETE => TriggerEvent::Delete,
        Rule::K_INSERT => TriggerEvent::Insert,
        Rule::K_UPDATE => {
            // Optional `OF col1, col2, ...` (bare idlist, no parens). Skip the K_OF keyword
            // and find the idlist pair if present.
            let columns: Vec<String> = inner
                .by_ref()
                .find(|p| p.as_rule() == Rule::idlist)
                .map(|cl| {
                    cl.into_inner()
                        .filter(|p| p.as_rule() == Rule::ident)
                        .map(|p| p.as_str().to_string())
                        .collect()
                })
                .unwrap_or_default();
            TriggerEvent::Update { columns }
        }
        other => unreachable!("unexpected trigger_event child {other:?}"),
    }
}

fn build_trigger_cmd_list(pair: Pair<'_, Rule>) -> Result<Vec<TriggerStep>, ParseError> {
    let mut steps = Vec::new();
    for part in pair.into_inner() {
        if part.as_rule() == Rule::trigger_cmd {
            steps.push(build_trigger_cmd(part)?);
        }
    }
    Ok(steps)
}

fn build_trigger_cmd(pair: Pair<'_, Rule>) -> Result<TriggerStep, ParseError> {
    let inner = pair.into_inner().next().expect("trigger_cmd has one child");
    match inner.as_rule() {
        Rule::insert_stmt => Ok(TriggerStep::Insert(build_insert(inner))),
        Rule::update_stmt => Ok(TriggerStep::Update(build_update(inner)?)),
        Rule::delete_stmt => Ok(TriggerStep::Delete(build_delete(inner))),
        Rule::select_stmt => Ok(TriggerStep::Select(build_select(inner)?)),
        other => unreachable!("unexpected trigger_cmd child {other:?}"),
    }
}

fn build_drop_trigger(pair: Pair<'_, Rule>) -> DropTriggerStmt {
    let mut stmt = DropTriggerStmt {
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

fn build_pragma(pair: Pair<'_, Rule>) -> PragmaStmt {
    let mut schema: Option<String> = None;
    let mut name = String::new();
    let mut value: Option<PragmaValue> = None;
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::qualified_name => {
                let (s, n) = build_qualified_name(part);
                schema = s;
                name = n;
            }
            Rule::pragma_value => value = Some(build_pragma_value(part)),
            _ => {}
        }
    }
    PragmaStmt {
        schema,
        name,
        value,
    }
}

fn build_pragma_value(pair: Pair<'_, Rule>) -> PragmaValue {
    let inner = pair.into_inner().next().expect("pragma_value has one child");
    match inner.as_rule() {
        Rule::pragma_eq => {
            let kind = build_pragma_value_kind(
                inner.into_inner().next().expect("pragma_eq has a value kind"),
            );
            PragmaValue::Equal(kind)
        }
        Rule::pragma_paren => {
            let kind = build_pragma_value_kind(
                inner.into_inner().next().expect("pragma_paren has a value kind"),
            );
            PragmaValue::Paren(kind)
        }
        other => unreachable!("unexpected pragma_value child {other:?}"),
    }
}

fn build_pragma_value_kind(pair: Pair<'_, Rule>) -> PragmaValueKind {
    // `pragma_value_kind` is a non-atomic rule wrapping one of `signed_number`,
    // `pragma_kw_value`, or `ident`. When the inner alternative is itself a non-atomic rule
    // (e.g. `pragma_kw_value`), pest nests it; for atomic rules (`signed_number`, `ident`)
    // the pair's rule is the alternative directly. Unwrap one level if needed.
    let pair = if pair.as_rule() == Rule::pragma_value_kind {
        pair.into_inner().next().expect("pragma_value_kind has a child")
    } else {
        pair
    };
    match pair.as_rule() {
        Rule::signed_number => PragmaValueKind::Number(expr::build_number(pair.as_str())),
        Rule::ident => PragmaValueKind::Ident(pair.as_str().to_string()),
        Rule::pragma_kw_value => {
            let kw = pair.into_inner().next().expect("pragma_kw_value has a keyword");
            match kw.as_rule() {
                Rule::K_ON => PragmaValueKind::On,
                Rule::K_DELETE => PragmaValueKind::Delete,
                Rule::K_DEFAULT => PragmaValueKind::Default,
                other => unreachable!("unexpected pragma_kw_value child {other:?}"),
            }
        }
        other => unreachable!("unexpected pragma_value_kind child {other:?}"),
    }
}

fn build_transaction(pair: Pair<'_, Rule>) -> TransactionStmt {
    let inner = pair.into_inner().next().expect("transaction_stmt has one child");
    match inner.as_rule() {
        Rule::begin_stmt => {
            let mut transaction_type = TransactionType::Deferred;
            let mut name: Option<String> = None;
            for part in inner.into_inner() {
                match part.as_rule() {
                    Rule::transtype => {
                        transaction_type = match part.into_inner().next().expect("transtype has a kind").as_rule() {
                            Rule::K_DEFERRED => TransactionType::Deferred,
                            Rule::K_IMMEDIATE => TransactionType::Immediate,
                            Rule::K_EXCLUSIVE => TransactionType::Exclusive,
                            other => unreachable!("unexpected transtype child {other:?}"),
                        };
                    }
                    Rule::trans_opt => {
                        name = build_trans_opt(part);
                    }
                    _ => {}
                }
            }
            TransactionStmt::Begin {
                transaction_type,
                name,
            }
        }
        Rule::commit_stmt => {
            let mut ended = false;
            let mut name: Option<String> = None;
            for part in inner.into_inner() {
                match part.as_rule() {
                    Rule::K_END => ended = true,
                    Rule::trans_opt => name = build_trans_opt(part),
                    _ => {}
                }
            }
            TransactionStmt::Commit { name, ended }
        }
        Rule::rollback_stmt => {
            let mut name: Option<String> = None;
            let mut to_savepoint: Option<String> = None;
            for part in inner.into_inner() {
                match part.as_rule() {
                    Rule::trans_opt => name = build_trans_opt(part),
                    Rule::ident => to_savepoint = Some(part.as_str().to_string()),
                    _ => {}
                }
            }
            TransactionStmt::Rollback {
                name,
                to_savepoint,
            }
        }
        Rule::savepoint_stmt => {
            let name = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::ident)
                .map(|p| p.as_str().to_string())
                .expect("savepoint_stmt has a name");
            TransactionStmt::Savepoint(name)
        }
        Rule::release_stmt => {
            let name = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::ident)
                .map(|p| p.as_str().to_string())
                .expect("release_stmt has a name");
            TransactionStmt::Release(name)
        }
        other => unreachable!("unexpected transaction_stmt child {other:?}"),
    }
}

/// Extract the optional transaction name from a `trans_opt` pair
/// (`TRANSACTION [name]` or empty).
fn build_trans_opt(pair: Pair<'_, Rule>) -> Option<String> {
    pair.into_inner()
        .find(|p| p.as_rule() == Rule::ident)
        .map(|p| p.as_str().to_string())
}

fn build_attach(pair: Pair<'_, Rule>) -> Result<AttachStmt, ParseError> {
    let mut database_kw = false;
    let mut filename: Option<Expr> = None;
    let mut schema_name: Option<Expr> = None;
    let mut key: Option<Expr> = None;
    let mut after_as = false;
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::database_kw_opt => database_kw = true,
            Rule::K_AS => after_as = true,
            Rule::expr => {
                if !after_as {
                    filename = Some(expr::build_expr(part));
                } else if schema_name.is_none() {
                    schema_name = Some(expr::build_expr(part));
                } else {
                    key = Some(expr::build_expr(part));
                }
            }
            Rule::key_opt => {
                key = Some(expr::build_expr(
                    part.into_inner()
                        .find(|p| p.as_rule() == Rule::expr)
                        .expect("key_opt has an expr"),
                ));
            }
            _ => {}
        }
    }
    Ok(AttachStmt {
        database_kw,
        filename: filename.expect("attach_stmt has a filename expr"),
        schema_name: schema_name.expect("attach_stmt has a schema_name expr"),
        key,
    })
}

fn build_detach(pair: Pair<'_, Rule>) -> Result<DetachStmt, ParseError> {
    let mut database_kw = false;
    let mut schema_name: Option<Expr> = None;
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::database_kw_opt => database_kw = true,
            Rule::expr => schema_name = Some(expr::build_expr(part)),
            _ => {}
        }
    }
    Ok(DetachStmt {
        database_kw,
        schema_name: schema_name.expect("detach_stmt has a schema_name expr"),
    })
}

fn build_vacuum(pair: Pair<'_, Rule>) -> Result<VacuumStmt, ParseError> {
    let mut schema: Option<String> = None;
    let mut into: Option<Expr> = None;
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::ident => schema = Some(part.as_str().to_string()),
            Rule::vacuum_into => {
                into = Some(expr::build_expr(
                    part.into_inner()
                        .find(|p| p.as_rule() == Rule::expr)
                        .expect("vacuum_into has an expr"),
                ));
            }
            _ => {}
        }
    }
    Ok(VacuumStmt { schema, into })
}

fn build_analyze(pair: Pair<'_, Rule>) -> AnalyzeStmt {
    let mut schema: Option<String> = None;
    let mut name: Option<String> = None;
    for part in pair.into_inner() {
        if part.as_rule() == Rule::qualified_name {
            let (s, n) = build_qualified_name(part);
            schema = s;
            name = Some(n);
        }
    }
    AnalyzeStmt { schema, name }
}

fn build_reindex(pair: Pair<'_, Rule>) -> ReindexStmt {
    let mut schema: Option<String> = None;
    let mut name: Option<String> = None;
    for part in pair.into_inner() {
        if part.as_rule() == Rule::qualified_name {
            let (s, n) = build_qualified_name(part);
            schema = s;
            name = Some(n);
        }
    }
    ReindexStmt { schema, name }
}

fn build_create_vtab(pair: Pair<'_, Rule>) -> CreateVirtualTable {
    let mut if_not_exists = false;
    let mut schema: Option<String> = None;
    let mut name = String::new();
    let mut module = String::new();
    let mut args = String::new();
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::if_not_exists => if_not_exists = true,
            Rule::qualified_name => {
                let (s, n) = build_qualified_name(part);
                schema = s;
                name = n;
            }
            Rule::ident => module = part.as_str().to_string(),
            Rule::vtab_args => {
                // The vtab_args rule wraps `"(" ~ vtab_inner ~ ")"`. The raw text of the
                // pair includes the surrounding parens; strip them to get the arg text.
                let raw = part.as_str();
                if raw.len() >= 2 && raw.starts_with('(') && raw.ends_with(')') {
                    args = raw[1..raw.len() - 1].to_string();
                }
            }
            _ => {}
        }
    }
    CreateVirtualTable {
        if_not_exists,
        schema,
        name,
        module,
        args,
    }
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
            ColumnConstraint::NotNull { .. }
        ));
        assert_eq!(
            ct.columns[2].constraints[0],
            ColumnConstraint::Default(Expr::Literal(Literal::Integer(0)))
        );
    }

    #[test]
    fn create_table_with_table_constraints() {
        // PRIMARY KEY (cols)
        let Stmt::CreateTable(ct) =
            &parse("CREATE TABLE t (a INTEGER, b INTEGER, PRIMARY KEY (a, b DESC));").unwrap()[0]
        else {
            panic!()
        };
        assert_eq!(ct.columns.len(), 2);
        assert_eq!(ct.constraints.len(), 1);
        match &ct.constraints[0].body {
            TableConstraintBody::PrimaryKey { columns, .. } => {
                assert_eq!(columns.len(), 2);
                assert_eq!(columns[0].name, "a");
                assert!(!columns[0].desc);
                assert_eq!(columns[1].name, "b");
                assert!(columns[1].desc);
            }
            other => panic!("expected PrimaryKey, got {other:?}"),
        }

        // CONSTRAINT name UNIQUE (cols)
        let Stmt::CreateTable(ct) =
            &parse("CREATE TABLE t (a INTEGER, b TEXT, CONSTRAINT u_ab UNIQUE (a, b));").unwrap()[0]
        else {
            panic!()
        };
        assert_eq!(ct.constraints[0].name.as_deref(), Some("u_ab"));
        assert!(matches!(ct.constraints[0].body, TableConstraintBody::Unique { .. }));

        // CHECK (expr)
        let Stmt::CreateTable(ct) =
            &parse("CREATE TABLE t (a INTEGER, CHECK (a > 0));").unwrap()[0]
        else {
            panic!()
        };
        assert!(matches!(ct.constraints[0].body, TableConstraintBody::Check { .. }));

        // FOREIGN KEY (cols) REFERENCES parent (cols) ON DELETE CASCADE ON UPDATE SET NULL DEFERRABLE INITIALLY DEFERRED
        let Stmt::CreateTable(ct) = &parse(
            "CREATE TABLE child (pid INTEGER, FOREIGN KEY (pid) REFERENCES parent(id) ON DELETE CASCADE ON UPDATE SET NULL DEFERRABLE INITIALLY DEFERRED);",
        )
        .unwrap()[0]
        else {
            panic!()
        };
        match &ct.constraints[0].body {
            TableConstraintBody::ForeignKey { columns, references } => {
                assert_eq!(columns, &["pid"]);
                assert_eq!(references.parent_table, "parent");
                assert_eq!(references.parent_columns.as_deref().map(|v| v.len()), Some(1));
                assert_eq!(references.parent_columns.as_deref().unwrap()[0], "id");
                assert_eq!(references.on_delete, Some(ReferenceAction::Cascade));
                assert_eq!(references.on_update, Some(ReferenceAction::SetNull));
                assert_eq!(references.deferrable, Some(Deferrable::DeferrableInitiallyDeferred));
            }
            other => panic!("expected ForeignKey, got {other:?}"),
        }

        // Column-level REFERENCES
        let Stmt::CreateTable(ct) =
            &parse("CREATE TABLE child (pid INTEGER REFERENCES parent(id));").unwrap()[0]
        else {
            panic!()
        };
        assert_eq!(ct.columns[0].constraints.len(), 1);
        match &ct.columns[0].constraints[0] {
            ColumnConstraint::References(r) => {
                assert_eq!(r.parent_table, "parent");
                assert_eq!(r.parent_columns.as_deref().map(|v| v.len()), Some(1));
                assert_eq!(r.parent_columns.as_deref().unwrap()[0], "id");
            }
            other => panic!("expected References, got {other:?}"),
        }

        // ON CONFLICT clauses on table constraints.
        let Stmt::CreateTable(ct) =
            &parse("CREATE TABLE t (a INTEGER, PRIMARY KEY (a) ON CONFLICT REPLACE);").unwrap()[0]
        else {
            panic!()
        };
        assert!(matches!(ct.constraints[0].body, TableConstraintBody::PrimaryKey { .. }));

        // Bad syntax: missing REFERENCES parent.
        assert!(parse("CREATE TABLE t (a INTEGER, FOREIGN KEY (a));").is_err());
        // `check`, `foreign`, `references` are reserved.
        assert!(parse("CREATE TABLE t (a INTEGER, CHECK check);").is_err());
    }

    #[test]
    fn create_table_without_rowid_and_strict() {
        // WITHOUT ROWID
        let Stmt::CreateTable(ct) =
            &parse("CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT) WITHOUT ROWID;").unwrap()[0]
        else {
            panic!()
        };
        assert!(ct.without_rowid);
        assert!(!ct.strict);

        // STRICT
        let Stmt::CreateTable(ct) =
            &parse("CREATE TABLE t (a INTEGER, b TEXT) STRICT;").unwrap()[0]
        else {
            panic!()
        };
        assert!(!ct.without_rowid);
        assert!(ct.strict);

        // Both, in either order (comma-separated).
        let Stmt::CreateTable(ct) =
            &parse("CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT) WITHOUT ROWID, STRICT;")
                .unwrap()[0]
        else {
            panic!()
        };
        assert!(ct.without_rowid);
        assert!(ct.strict);
        let Stmt::CreateTable(ct) =
            &parse("CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT) STRICT, WITHOUT ROWID;")
                .unwrap()[0]
        else {
            panic!()
        };
        assert!(ct.without_rowid);
        assert!(ct.strict);

        // Case-insensitive "rowid".
        let Stmt::CreateTable(ct) =
            &parse("CREATE TABLE t (a INTEGER PRIMARY KEY) WITHOUT rowid;").unwrap()[0]
        else {
            panic!()
        };
        assert!(ct.without_rowid);

        // `without`, `strict` are reserved.
        assert!(parse("CREATE TABLE t (a INTEGER, without INTEGER);").is_err());
        assert!(parse("CREATE TABLE t (a INTEGER, strict INTEGER);").is_err());
    }

    #[test]
    fn create_table_generated_columns() {
        // GENERATED ALWAYS AS (expr) — defaults to VIRTUAL.
        let Stmt::CreateTable(ct) =
            &parse("CREATE TABLE t (a INTEGER, b INTEGER GENERATED ALWAYS AS (a + 1));").unwrap()
                [0]
        else {
            panic!()
        };
        match &ct.columns[1].constraints[0] {
            ColumnConstraint::Generated { stored, .. } => assert!(!stored),
            other => panic!("expected Generated, got {other:?}"),
        }

        // GENERATED ALWAYS AS (expr) STORED
        let Stmt::CreateTable(ct) =
            &parse("CREATE TABLE t (a INTEGER, b INTEGER GENERATED ALWAYS AS (a + 1) STORED);")
                .unwrap()[0]
        else {
            panic!()
        };
        match &ct.columns[1].constraints[0] {
            ColumnConstraint::Generated { stored, .. } => assert!(*stored),
            other => panic!("expected Generated, got {other:?}"),
        }

        // GENERATED ALWAYS AS (expr) VIRTUAL
        let Stmt::CreateTable(ct) =
            &parse("CREATE TABLE t (a INTEGER, b INTEGER GENERATED ALWAYS AS (a + 1) VIRTUAL);")
                .unwrap()[0]
        else {
            panic!()
        };
        match &ct.columns[1].constraints[0] {
            ColumnConstraint::Generated { stored, .. } => assert!(!stored),
            other => panic!("expected Generated, got {other:?}"),
        }

        // Bare `AS (expr)` form (without GENERATED ALWAYS).
        let Stmt::CreateTable(ct) =
            &parse("CREATE TABLE t (a INTEGER, b INTEGER AS (a * 2) STORED);").unwrap()[0]
        else {
            panic!()
        };
        match &ct.columns[1].constraints[0] {
            ColumnConstraint::Generated { stored, .. } => assert!(*stored),
            other => panic!("expected Generated, got {other:?}"),
        }

        // `generated`, `always`, `stored`, `virtual` are reserved.
        assert!(parse("CREATE TABLE t (a INTEGER, generated INTEGER);").is_err());
        assert!(parse("CREATE TABLE t (a INTEGER, always INTEGER);").is_err());
        assert!(parse("CREATE TABLE t (a INTEGER, stored INTEGER);").is_err());
        // Bad syntax: missing expr.
        assert!(parse("CREATE TABLE t (a INTEGER, b INTEGER GENERATED ALWAYS AS);").is_err());
    }

    #[test]
    fn delete_with_order_by_limit() {
        let Stmt::Delete(d) = &parse("DELETE FROM t ORDER BY a LIMIT 10;").unwrap()[0] else {
            panic!("expected DELETE")
        };
        assert_eq!(d.order_by.len(), 1);
        assert!(d.limit.is_some());
        assert!(d.offset.is_none());

        // With OFFSET.
        let Stmt::Delete(d) = &parse("DELETE FROM t ORDER BY a LIMIT 10 OFFSET 5;").unwrap()[0]
        else {
            panic!("expected DELETE")
        };
        assert!(d.limit.is_some());
        assert!(d.offset.is_some());

        // With WHERE.
        let Stmt::Delete(d) =
            &parse("DELETE FROM t WHERE a > 0 ORDER BY a DESC LIMIT 5;").unwrap()[0]
        else {
            panic!("expected DELETE")
        };
        assert!(d.where_clause.is_some());
        assert_eq!(d.order_by.len(), 1);
        assert!(d.order_by[0].desc);
        assert!(d.limit.is_some());
    }

    #[test]
    fn update_with_order_by_limit_and_from() {
        // UPDATE ... ORDER BY ... LIMIT ...
        let Stmt::Update(u) =
            &parse("UPDATE t SET a = 1 ORDER BY a LIMIT 10;").unwrap()[0]
        else {
            panic!("expected UPDATE")
        };
        assert_eq!(u.order_by.len(), 1);
        assert!(u.limit.is_some());

        // UPDATE ... FROM ...
        let Stmt::Update(u) =
            &parse("UPDATE t SET a = s.b FROM s WHERE t.id = s.id;").unwrap()[0]
        else {
            panic!("expected UPDATE")
        };
        assert_eq!(u.from.len(), 1);
        assert!(u.where_clause.is_some());

        // UPDATE ... FROM join ...
        let Stmt::Update(u) =
            &parse("UPDATE t SET a = s.b FROM s INNER JOIN r ON s.id = r.id;").unwrap()[0]
        else {
            panic!("expected UPDATE")
        };
        assert_eq!(u.from.len(), 1);

        // All combined.
        let Stmt::Update(u) = &parse(
            "UPDATE t SET a = 1 FROM s WHERE t.id = s.id ORDER BY t.a LIMIT 5 OFFSET 2 RETURNING a;",
        )
        .unwrap()[0]
        else {
            panic!("expected UPDATE")
        };
        assert_eq!(u.from.len(), 1);
        assert!(u.where_clause.is_some());
        assert_eq!(u.order_by.len(), 1);
        assert!(u.limit.is_some());
        assert!(u.offset.is_some());
        assert!(u.returning.is_some());
    }

    #[test]
    fn indexed_by_table_hint() {
        // SELECT FROM ... INDEXED BY name
        let Stmt::Select(s) = &parse("SELECT * FROM t INDEXED BY i WHERE a > 0;").unwrap()[0]
        else {
            panic!("expected SELECT")
        };
        assert_eq!(s.from.len(), 1);
        match &s.from[0] {
            TableOrJoin::Table(t) => {
                assert_eq!(t.name, "t");
                match &t.indexed_by {
                    Some(IndexedBy::Index(name)) => assert_eq!(name, "i"),
                    other => panic!("expected Index, got {other:?}"),
                }
            }
            other => panic!("expected Table, got {other:?}"),
        }

        // NOT INDEXED
        let Stmt::Select(s) = &parse("SELECT * FROM t NOT INDEXED;").unwrap()[0] else {
            panic!("expected SELECT")
        };
        match &s.from[0] {
            TableOrJoin::Table(t) => {
                assert_eq!(t.indexed_by, Some(IndexedBy::NotIndexed));
            }
            other => panic!("expected Table, got {other:?}"),
        }

        // With alias: INDEXED BY comes before the alias.
        let Stmt::Select(s) = &parse("SELECT * FROM t INDEXED BY i AS x;").unwrap()[0] else {
            panic!("expected SELECT")
        };
        match &s.from[0] {
            TableOrJoin::Table(t) => {
                assert_eq!(t.name, "t");
                assert_eq!(t.alias.as_deref(), Some("x"));
                assert!(matches!(t.indexed_by, Some(IndexedBy::Index(_))));
            }
            other => panic!("expected Table, got {other:?}"),
        }

        // `indexed` is reserved.
        assert!(parse("SELECT * FROM t indexed AS x;").is_err());
        // Bad syntax: INDEXED BY without name.
        assert!(parse("SELECT * FROM t INDEXED BY;").is_err());
    }

    #[test]
    fn window_functions_parses() {
        // count(*) OVER ()
        let Stmt::Select(s) = &parse("SELECT count(*) OVER () FROM t;").unwrap()[0] else {
            panic!("expected SELECT")
        };
        match &s.columns[0] {
            ResultColumn::Expr { expr, .. } => match expr {
                Expr::Function { over, .. } => assert!(over.is_some()),
                other => panic!("expected Function, got {other:?}"),
            },
            other => panic!("expected Expr, got {other:?}"),
        }

        // count(*) FILTER (WHERE a > 0) OVER (PARTITION BY b ORDER BY c)
        let Stmt::Select(s) =
            &parse("SELECT count(*) FILTER (WHERE a > 0) OVER (PARTITION BY b ORDER BY c) FROM t;")
                .unwrap()[0]
        else {
            panic!("expected SELECT")
        };
        match &s.columns[0] {
            ResultColumn::Expr { expr, .. } => match expr {
                Expr::Function { filter, over, .. } => {
                    assert!(filter.is_some());
                    let w = over.as_ref().expect("over is Some");
                    assert_eq!(w.partition_by.len(), 1);
                    assert_eq!(w.order_by.len(), 1);
                }
                other => panic!("expected Function, got {other:?}"),
            },
            other => panic!("expected Expr, got {other:?}"),
        }

        // OVER named window
        let Stmt::Select(s) =
            &parse("SELECT count(*) OVER w FROM t WINDOW w AS (PARTITION BY b);").unwrap()[0]
        else {
            panic!("expected SELECT")
        };
        match &s.columns[0] {
            ResultColumn::Expr { expr, .. } => match expr {
                Expr::Function { over, .. } => {
                    let w = over.as_ref().expect("over is Some");
                    assert_eq!(w.name.as_deref(), Some("w"));
                }
                other => panic!("expected Function, got {other:?}"),
            },
            other => panic!("expected Expr, got {other:?}"),
        }
        assert_eq!(s.window_clause.len(), 1);
        assert_eq!(s.window_clause[0].name, "w");
        assert_eq!(s.window_clause[0].spec.partition_by.len(), 1);

        // Frame: ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE NO OTHERS
        let Stmt::Select(s) = &parse(
            "SELECT sum(a) OVER (ORDER BY b ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE NO OTHERS) FROM t;",
        ).unwrap()[0] else { panic!("expected SELECT") };
        match &s.columns[0] {
            ResultColumn::Expr { expr, .. } => match expr {
                Expr::Function { over, .. } => {
                    let w = over.as_ref().expect("over is Some");
                    let f = w.frame.as_ref().expect("frame is Some");
                    assert_eq!(f.mode, FrameMode::Rows);
                    assert!(matches!(f.start, FrameBound::Preceding(_)));
                    assert!(matches!(f.end, Some(FrameBound::Following(_))));
                    assert_eq!(f.exclude, Some(FrameExclude::NoOthers));
                }
                other => panic!("expected Function, got {other:?}"),
            },
            other => panic!("expected Expr, got {other:?}"),
        }

        // Just FILTER without OVER (aggregate with filter).
        let Stmt::Select(s) =
            &parse("SELECT count(*) FILTER (WHERE a > 0) FROM t;").unwrap()[0]
        else {
            panic!("expected SELECT")
        };
        match &s.columns[0] {
            ResultColumn::Expr { expr, .. } => match expr {
                Expr::Function { filter, over, .. } => {
                    assert!(filter.is_some());
                    assert!(over.is_none());
                }
                other => panic!("expected Function, got {other:?}"),
            },
            other => panic!("expected Expr, got {other:?}"),
        }

        // Bad syntax: FILTER without paren.
        assert!(parse("SELECT count(*) FILTER WHERE a > 0 FROM t;").is_err());
    }

    #[test]
    fn order_by_nulls_first_last() {
        let Stmt::Select(s) = &parse("SELECT * FROM t ORDER BY a NULLS FIRST, b NULLS LAST;").unwrap()[0] else {
            panic!("expected SELECT")
        };
        assert_eq!(s.order_by.len(), 2);
        assert_eq!(s.order_by[0].nulls, Some(NullsOrder::First));
        assert_eq!(s.order_by[1].nulls, Some(NullsOrder::Last));

        // With ASC/DESC + NULLS.
        let Stmt::Select(s) = &parse("SELECT * FROM t ORDER BY a DESC NULLS FIRST;").unwrap()[0] else {
            panic!("expected SELECT")
        };
        assert!(s.order_by[0].desc);
        assert_eq!(s.order_by[0].nulls, Some(NullsOrder::First));

        // No NULLS clause: null.
        let Stmt::Select(s) = &parse("SELECT * FROM t ORDER BY a;").unwrap()[0] else {
            panic!("expected SELECT")
        };
        assert!(s.order_by[0].nulls.is_none());

        // In window OVER (ORDER BY ... NULLS ...).
        let Stmt::Select(s) = &parse("SELECT count(*) OVER (ORDER BY a NULLS LAST) FROM t;").unwrap()[0] else {
            panic!("expected SELECT")
        };
        match &s.columns[0] {
            ResultColumn::Expr { expr: Expr::Function { over, .. }, .. } => {
                let w = over.as_ref().expect("over is Some");
                assert_eq!(w.order_by[0].nulls, Some(NullsOrder::Last));
            }
            other => panic!("expected Function, got {other:?}"),
        }

        // `nulls` is reserved, so it must be quoted to use as a column name.
        assert!(parse("SELECT nulls FROM t;").is_err());
        // Bad syntax: NULLS without FIRST/LAST.
        assert!(parse("SELECT * FROM t ORDER BY a NULLS;").is_err());
    }

    #[test]
    fn create_table_as_select() {
        let Stmt::CreateTable(ct) = &parse("CREATE TABLE t AS SELECT 1 AS a, 2 AS b;").unwrap()[0]
        else {
            panic!("expected CREATE TABLE")
        };
        assert!(ct.columns.is_empty());
        assert!(ct.constraints.is_empty());
        assert!(ct.as_select.is_some());

        // With IF NOT EXISTS + schema.
        let Stmt::CreateTable(ct) =
            &parse("CREATE TABLE IF NOT EXISTS main.t AS SELECT * FROM s;").unwrap()[0]
        else {
            panic!("expected CREATE TABLE")
        };
        assert!(ct.if_not_exists);
        assert_eq!(ct.schema.as_deref(), Some("main"));
        assert!(ct.as_select.is_some());

        // TEMP TABLE AS SELECT.
        let Stmt::CreateTable(ct) =
            &parse("CREATE TEMP TABLE t AS SELECT 1;").unwrap()[0]
        else {
            panic!("expected CREATE TABLE")
        };
        assert!(ct.temporary);
        assert!(ct.as_select.is_some());
    }

    #[test]
    fn regexp_and_match_operators() {
        // REGEXP
        let Stmt::Select(s) = &parse("SELECT a REGEXP b FROM t;").unwrap()[0] else {
            panic!("expected SELECT")
        };
        match &s.columns[0] {
            ResultColumn::Expr { expr: Expr::Binary { op, .. }, .. } => {
                assert_eq!(*op, BinaryOp::Regexp);
            }
            other => panic!("expected Binary, got {other:?}"),
        }

        // NOT REGEXP
        let Stmt::Select(s) = &parse("SELECT a NOT REGEXP b FROM t;").unwrap()[0] else {
            panic!("expected SELECT")
        };
        match &s.columns[0] {
            ResultColumn::Expr { expr: Expr::Unary { op: UnaryOp::Not, .. }, .. } => {}
            other => panic!("expected Unary Not, got {other:?}"),
        }

        // MATCH
        let Stmt::Select(s) = &parse("SELECT a MATCH b FROM t;").unwrap()[0] else {
            panic!("expected SELECT")
        };
        match &s.columns[0] {
            ResultColumn::Expr { expr: Expr::Binary { op, .. }, .. } => {
                assert_eq!(*op, BinaryOp::Match);
            }
            other => panic!("expected Binary, got {other:?}"),
        }

        // `regexp` and `match` are reserved.
        assert!(parse("SELECT regexp FROM t;").is_err());
        assert!(parse("SELECT match FROM t;").is_err());
    }

    #[test]
    fn standalone_values_statement() {
        // VALUES as a top-level statement (not just in INSERT).
        let stmts = parse("VALUES (1), (2);").unwrap();
        assert_eq!(stmts.len(), 1);
        let Stmt::Select(s) = &stmts[0] else {
            panic!("expected Select with VALUES body")
        };
        assert_eq!(s.values.len(), 2);
        assert_eq!(s.values[0].len(), 1);
    }

    #[test]
    fn column_constraint_on_conflict() {
        use crate::ConflictAction;
        // Column-level ON CONFLICT clauses.
        let Stmt::CreateTable(ct) = &parse(
            "CREATE TABLE t(a INTEGER PRIMARY KEY ON CONFLICT REPLACE, b TEXT NOT NULL ON CONFLICT IGNORE, c UNIQUE ON CONFLICT ABORT);",
        )
        .unwrap()[0]
        else {
            panic!("expected CREATE TABLE")
        };
        // Verify it parses without error (the onconf is accepted).
        assert_eq!(ct.columns.len(), 3);
        // Verify the per-constraint OE is captured (M12.9).
        assert!(matches!(
            &ct.columns[0].constraints[0],
            ColumnConstraint::PrimaryKey { on_conflict: Some(ConflictAction::Replace), .. }
        ));
        assert!(matches!(
            &ct.columns[1].constraints[0],
            ColumnConstraint::NotNull { on_conflict: Some(ConflictAction::Ignore) }
        ));
        assert!(matches!(
            &ct.columns[2].constraints[0],
            ColumnConstraint::Unique { on_conflict: Some(ConflictAction::Abort) }
        ));
        // Bad syntax: invalid conflict action.
        assert!(parse("CREATE TABLE t(a INTEGER PRIMARY KEY ON CONFLICT BOGUS);").is_err());
    }

    #[test]
    fn table_constraint_on_conflict() {
        use crate::{ConflictAction, TableConstraintBody};
        // Table-level ON CONFLICT clauses on PRIMARY KEY / UNIQUE / CHECK.
        let Stmt::CreateTable(ct) = &parse(
            "CREATE TABLE t(a, b, c, PRIMARY KEY(a) ON CONFLICT IGNORE, UNIQUE(b) ON CONFLICT FAIL, CHECK(c > 0) ON CONFLICT ROLLBACK);",
        )
        .unwrap()[0]
        else {
            panic!("expected CREATE TABLE")
        };
        assert_eq!(ct.constraints.len(), 3);
        assert!(matches!(
            &ct.constraints[0].body,
            TableConstraintBody::PrimaryKey { on_conflict: Some(ConflictAction::Ignore), .. }
        ));
        assert!(matches!(
            &ct.constraints[1].body,
            TableConstraintBody::Unique { on_conflict: Some(ConflictAction::Fail), .. }
        ));
        assert!(matches!(
            &ct.constraints[2].body,
            TableConstraintBody::Check { on_conflict: Some(ConflictAction::Rollback), .. }
        ));
    }

    #[test]
    fn table_valued_function_in_from() {
        // FROM func(args)
        let Stmt::Select(s) = &parse("SELECT * FROM generate_series(1, 5);").unwrap()[0] else {
            panic!("expected SELECT")
        };
        match &s.from[0] {
            TableOrJoin::Table(t) => {
                assert_eq!(t.name, "generate_series");
                assert_eq!(t.args.as_ref().map(|a| a.len()), Some(2));
            }
            other => panic!("expected Table, got {other:?}"),
        }

        // With alias.
        let Stmt::Select(s) =
            &parse("SELECT * FROM generate_series(1, 5) AS g;").unwrap()[0]
        else {
            panic!("expected SELECT")
        };
        match &s.from[0] {
            TableOrJoin::Table(t) => {
                assert_eq!(t.name, "generate_series");
                assert!(t.args.is_some());
                assert_eq!(t.alias.as_deref(), Some("g"));
            }
            other => panic!("expected Table, got {other:?}"),
        }

        // Schema-qualified.
        let Stmt::Select(s) =
            &parse("SELECT * FROM main.generate_series(1, 5);").unwrap()[0]
        else {
            panic!("expected SELECT")
        };
        match &s.from[0] {
            TableOrJoin::Table(t) => {
                assert_eq!(t.schema.as_deref(), Some("main"));
                assert_eq!(t.name, "generate_series");
            }
            other => panic!("expected Table, got {other:?}"),
        }

        // Plain table reference (no args).
        let Stmt::Select(s) = &parse("SELECT * FROM t;").unwrap()[0] else {
            panic!("expected SELECT")
        };
        match &s.from[0] {
            TableOrJoin::Table(t) => assert!(t.args.is_none()),
            other => panic!("expected Table, got {other:?}"),
        }
    }

    #[test]
    fn alter_table_advanced_actions() {
        // ALTER COLUMN name DROP NOT NULL
        let Stmt::AlterTable(a) =
            &parse("ALTER TABLE t ALTER COLUMN b DROP NOT NULL;").unwrap()[0]
        else {
            panic!("expected ALTER TABLE")
        };
        assert_eq!(a.action, AlterTableAction::AlterColumnDropNotNull("b".to_string()));

        // ALTER COLUMN name SET NOT NULL (without COLUMN keyword)
        let Stmt::AlterTable(a) =
            &parse("ALTER TABLE t ALTER b SET NOT NULL;").unwrap()[0]
        else {
            panic!("expected ALTER TABLE")
        };
        assert_eq!(a.action, AlterTableAction::AlterColumnSetNotNull("b".to_string()));

        // ADD CONSTRAINT name CHECK (expr)
        let Stmt::AlterTable(a) =
            &parse("ALTER TABLE t ADD CONSTRAINT chk CHECK (a > 0);").unwrap()[0]
        else {
            panic!("expected ALTER TABLE")
        };
        match &a.action {
            AlterTableAction::AddCheckConstraint { name, .. } => {
                assert_eq!(name.as_deref(), Some("chk"));
            }
            other => panic!("expected AddCheckConstraint, got {other:?}"),
        }

        // ADD CHECK (expr) (without CONSTRAINT name)
        let Stmt::AlterTable(a) =
            &parse("ALTER TABLE t ADD CHECK (a > 0);").unwrap()[0]
        else {
            panic!("expected ALTER TABLE")
        };
        assert!(matches!(a.action, AlterTableAction::AddCheckConstraint { name: None, .. }));

        // DROP CONSTRAINT name
        let Stmt::AlterTable(a) =
            &parse("ALTER TABLE t DROP CONSTRAINT chk;").unwrap()[0]
        else {
            panic!("expected ALTER TABLE")
        };
        assert_eq!(a.action, AlterTableAction::DropConstraint("chk".to_string()));

        // `alter` is reserved, so it must be quoted as a column name.
        assert!(parse("ALTER TABLE t ALTER alter DROP NOT NULL;").is_err());
        // Bad syntax: ALTER COLUMN without name.
        assert!(parse("ALTER TABLE t ALTER DROP NOT NULL;").is_err());
        // Bad syntax: DROP CONSTRAINT without name.
        assert!(parse("ALTER TABLE t DROP CONSTRAINT;").is_err());
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
        let InsertSource::Values(rows) = &ins.source else {
            panic!("expected VALUES source")
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Expr::Literal(Literal::Integer(1)));

        let Stmt::Insert(ins) = &parse("INSERT OR IGNORE INTO t VALUES (1);").unwrap()[0] else {
            panic!()
        };
        assert_eq!(ins.or_action, Some(ConflictAction::Ignore));
    }

    #[test]
    fn insert_select_parses() {
        let Stmt::Insert(ins) =
            &parse("INSERT INTO t (a) SELECT x FROM s WHERE x > 0;").unwrap()[0]
        else {
            panic!()
        };
        assert_eq!(ins.table, "t");
        assert_eq!(ins.columns, vec!["a"]);
        assert!(matches!(ins.source, InsertSource::Select(_)));
    }

    #[test]
    fn insert_default_values_parses() {
        let Stmt::Insert(ins) = &parse("INSERT INTO t DEFAULT VALUES;").unwrap()[0] else {
            panic!()
        };
        assert_eq!(ins.table, "t");
        assert!(ins.columns.is_empty());
        assert_eq!(ins.source, InsertSource::DefaultValues);

        // An explicit column list is syntactically accepted and ignored by DEFAULT VALUES.
        let Stmt::Insert(ins) = &parse("INSERT INTO t (a, b) DEFAULT VALUES;").unwrap()[0] else {
            panic!()
        };
        assert_eq!(ins.table, "t");
        assert_eq!(ins.columns, vec!["a", "b"]);
        assert_eq!(ins.source, InsertSource::DefaultValues);
    }

    #[test]
    fn upsert_parses() {
        let Stmt::Insert(ins) =
            &parse("INSERT INTO t (a) VALUES (1) ON CONFLICT DO NOTHING;").unwrap()[0]
        else {
            panic!()
        };
        assert_eq!(ins.upsert.len(), 1);
        assert!(ins.upsert[0].target.is_none());
        assert_eq!(ins.upsert[0].action, UpsertAction::Nothing);
        assert!(ins.returning.is_none());

        let Stmt::Insert(ins) =
            &parse("INSERT INTO t (a) VALUES (1) ON CONFLICT(a) DO UPDATE SET a = excluded.a;").unwrap()[0]
        else {
            panic!()
        };
        assert_eq!(ins.upsert.len(), 1);
        let target = ins.upsert[0].target.as_ref().expect("target present");
        assert_eq!(target.columns.len(), 1);
        assert!(matches!(
            &target.columns[0],
            UpsertTargetColumn::Column { name, .. } if name == "a"
        ));
        let UpsertAction::Update { assignments, .. } = &ins.upsert[0].action else {
            panic!("expected DO UPDATE")
        };
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].column, "a");

        let Stmt::Insert(ins) =
            &parse("INSERT INTO t VALUES (1) ON CONFLICT(a, b) WHERE a > 0 DO UPDATE SET c = 2 WHERE c > 0;").unwrap()[0]
        else {
            panic!()
        };
        assert_eq!(ins.upsert.len(), 1);
        let target = ins.upsert[0].target.as_ref().expect("target present");
        assert_eq!(target.columns.len(), 2);
        assert!(target.where_clause.is_some());
        let UpsertAction::Update { where_clause, .. } = &ins.upsert[0].action else {
            panic!("expected DO UPDATE")
        };
        assert!(where_clause.is_some());

        // Multiple ON CONFLICT clauses may be chained (upstream supports multi-clause upsert).
        let Stmt::Insert(ins) =
            &parse("INSERT INTO t VALUES (1) ON CONFLICT(a) DO NOTHING ON CONFLICT(b) DO UPDATE SET c = 1;").unwrap()[0]
        else {
            panic!()
        };
        assert_eq!(ins.upsert.len(), 2);

        // `DO NOTHING` may omit the target entirely.
        let Stmt::Insert(ins) =
            &parse("INSERT INTO t VALUES (1) ON CONFLICT DO NOTHING;").unwrap()[0]
        else {
            panic!()
        };
        assert_eq!(ins.upsert.len(), 1);
        assert!(ins.upsert[0].target.is_none());
        assert_eq!(ins.upsert[0].action, UpsertAction::Nothing);

        // `DO UPDATE` without a target is allowed by upstream.
        let Stmt::Insert(ins) =
            &parse("INSERT INTO t VALUES (1) ON CONFLICT DO UPDATE SET a = 1;").unwrap()[0]
        else {
            panic!()
        };
        assert!(ins.upsert[0].target.is_none());
        let UpsertAction::Update { assignments, .. } = &ins.upsert[0].action else {
            panic!("expected DO UPDATE")
        };
        assert_eq!(assignments.len(), 1);

        // RETURNING after an upsert clause.
        let Stmt::Insert(ins) =
            &parse("INSERT INTO t VALUES (1) ON CONFLICT DO NOTHING RETURNING a;").unwrap()[0]
        else {
            panic!()
        };
        assert_eq!(ins.upsert.len(), 1);
        let ret = ins.returning.as_ref().expect("returning present");
        assert_eq!(ret.len(), 1);
        assert!(matches!(
            &ret[0], ResultColumn::Expr { expr: Expr::Column { name, .. }, .. } if name == "a"
        ));
    }

    #[test]
    fn insert_returning_parses() {
        let Stmt::Insert(ins) =
            &parse("INSERT INTO t (a) VALUES (1) RETURNING a, rowid;").unwrap()[0]
        else {
            panic!()
        };
        assert_eq!(ins.table, "t");
        let ret = ins.returning.as_ref().expect("returning present");
        assert_eq!(ret.len(), 2);
        assert!(matches!(
            &ret[0], ResultColumn::Expr { expr: Expr::Column { name, .. }, .. } if name == "a"
        ));
        assert!(matches!(
            &ret[1], ResultColumn::Expr { expr: Expr::Column { name, .. }, .. } if name == "rowid"
        ));
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
        assert!(d.returning.is_none());

        let Stmt::Delete(d) = &parse("DELETE FROM main.t WHERE x > 1;").unwrap()[0] else {
            panic!("expected DELETE")
        };
        assert_eq!(d.schema.as_deref(), Some("main"));
        assert_eq!(d.table, "t");
        assert!(d.where_clause.is_some());
        assert!(d.returning.is_none());

        let Stmt::Delete(d) = &parse("DELETE FROM t WHERE a = 1 RETURNING b;").unwrap()[0] else {
            panic!("expected DELETE")
        };
        assert_eq!(d.table, "t");
        assert!(d.where_clause.is_some());
        let ret = d.returning.as_ref().expect("returning present");
        assert_eq!(ret.len(), 1);
        assert!(matches!(&ret[0], ResultColumn::Expr { expr: Expr::Column { name, .. }, .. } if name == "b"));
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
    fn alter_table_rename_to() {
        let Stmt::AlterTable(a) = &parse("ALTER TABLE t RENAME TO t2;").unwrap()[0] else {
            panic!("expected ALTER TABLE")
        };
        assert_eq!(a.table, "t");
        assert!(a.schema.is_none());
        assert_eq!(a.action, AlterTableAction::RenameTo("t2".to_string()));

        // Schema-qualified form.
        let Stmt::AlterTable(a) = &parse("ALTER TABLE main.t RENAME TO new_t;").unwrap()[0]
        else {
            panic!("expected ALTER TABLE")
        };
        assert_eq!(a.schema.as_deref(), Some("main"));
        assert_eq!(a.table, "t");
        assert_eq!(a.action, AlterTableAction::RenameTo("new_t".to_string()));

        // Backtick-quoted identifier for the table name (current build_qualified_name
        // behaviour preserves the quotes, matching the rest of the parser; unquoting is
        // tracked separately for the full parse.y port).
        let Stmt::AlterTable(a) = &parse("ALTER TABLE `x` RENAME TO y;").unwrap()[0] else {
            panic!("expected ALTER TABLE")
        };
        assert_eq!(a.table, "`x`");
        assert_eq!(a.action, AlterTableAction::RenameTo("y".to_string()));

        // Bad syntax: missing TO.
        assert!(parse("ALTER TABLE t RENAME t2;").is_err());
        // Bad syntax: missing RENAME action.
        assert!(parse("ALTER TABLE t;").is_err());
        // `rename` is reserved, so it must be quoted to use as a table name.
        assert!(parse("ALTER TABLE rename RENAME TO t;").is_err());
    }

    #[test]
    fn alter_table_add_column() {
        // Bare ADD with column definition (no COLUMN keyword).
        let Stmt::AlterTable(a) = &parse("ALTER TABLE t ADD b INTEGER NOT NULL;").unwrap()[0]
        else {
            panic!("expected ALTER TABLE")
        };
        assert_eq!(a.table, "t");
        match &a.action {
            AlterTableAction::AddColumn(c) => {
                assert_eq!(c.name, "b");
                assert_eq!(c.type_name.as_deref(), Some("INTEGER"));
                assert_eq!(c.constraints.len(), 1);
                assert!(matches!(c.constraints[0], ColumnConstraint::NotNull { .. }));
            }
            other => panic!("expected AddColumn, got {other:?}"),
        }

        // Optional COLUMN keyword.
        let Stmt::AlterTable(a) = &parse("ALTER TABLE t ADD COLUMN b TEXT DEFAULT 'x';")
            .unwrap()[0]
        else {
            panic!("expected ALTER TABLE")
        };
        match &a.action {
            AlterTableAction::AddColumn(c) => {
                assert_eq!(c.name, "b");
                assert_eq!(c.type_name.as_deref(), Some("TEXT"));
                assert_eq!(c.constraints.len(), 1);
                assert!(matches!(c.constraints[0], ColumnConstraint::Default(_)));
            }
            other => panic!("expected AddColumn, got {other:?}"),
        }

        // ADD with no type and no constraints.
        let Stmt::AlterTable(a) = &parse("ALTER TABLE main.t ADD c;").unwrap()[0] else {
            panic!("expected ALTER TABLE")
        };
        assert_eq!(a.schema.as_deref(), Some("main"));
        match &a.action {
            AlterTableAction::AddColumn(c) => {
                assert_eq!(c.name, "c");
                assert!(c.type_name.is_none());
                assert!(c.constraints.is_empty());
            }
            other => panic!("expected AddColumn, got {other:?}"),
        }

        // Bad syntax: missing column name.
        assert!(parse("ALTER TABLE t ADD COLUMN;").is_err());
        // `column` is reserved, so it must be quoted to use as a column name. Without
        // quotes it is consumed as the optional COLUMN keyword, and the following token
        // becomes the column name (matching upstream sqlite3 behaviour).
        assert!(parse("ALTER TABLE t ADD \"column\" INTEGER;").is_ok());
        let Stmt::AlterTable(a) = &parse("ALTER TABLE t ADD \"column\" INTEGER;").unwrap()[0]
        else {
            panic!("expected ALTER TABLE")
        };
        match &a.action {
            AlterTableAction::AddColumn(c) => assert_eq!(c.name, "\"column\""),
            other => panic!("expected AddColumn, got {other:?}"),
        }
    }

    #[test]
    fn alter_table_drop_column() {
        // DROP COLUMN name
        let Stmt::AlterTable(a) = &parse("ALTER TABLE t DROP COLUMN b;").unwrap()[0] else {
            panic!("expected ALTER TABLE")
        };
        assert_eq!(a.table, "t");
        assert_eq!(a.action, AlterTableAction::DropColumn("b".to_string()));

        // DROP name (without COLUMN keyword)
        let Stmt::AlterTable(a) = &parse("ALTER TABLE main.t DROP b;").unwrap()[0] else {
            panic!("expected ALTER TABLE")
        };
        assert_eq!(a.schema.as_deref(), Some("main"));
        assert_eq!(a.action, AlterTableAction::DropColumn("b".to_string()));

        // `column` is reserved, so `DROP column` parses as `DROP COLUMN <missing>` — error.
        assert!(parse("ALTER TABLE t DROP column;").is_err());
        // Quoted "column" works as the column name.
        let Stmt::AlterTable(a) = &parse("ALTER TABLE t DROP \"column\";").unwrap()[0] else {
            panic!("expected ALTER TABLE")
        };
        assert_eq!(a.action, AlterTableAction::DropColumn("\"column\"".to_string()));

        // Bad syntax: missing column name.
        assert!(parse("ALTER TABLE t DROP COLUMN;").is_err());
        // Bad syntax: DROP without column name.
        assert!(parse("ALTER TABLE t DROP;").is_err());
    }

    #[test]
    fn alter_table_rename_column() {
        // RENAME COLUMN old TO new
        let Stmt::AlterTable(a) = &parse("ALTER TABLE t RENAME COLUMN b TO c;").unwrap()[0] else {
            panic!("expected ALTER TABLE")
        };
        assert_eq!(a.table, "t");
        assert_eq!(
            a.action,
            AlterTableAction::RenameColumn {
                old: "b".to_string(),
                new: "c".to_string()
            }
        );

        // RENAME old TO new (without COLUMN keyword)
        let Stmt::AlterTable(a) = &parse("ALTER TABLE main.t RENAME b TO c;").unwrap()[0] else {
            panic!("expected ALTER TABLE")
        };
        assert_eq!(a.schema.as_deref(), Some("main"));
        assert_eq!(
            a.action,
            AlterTableAction::RenameColumn {
                old: "b".to_string(),
                new: "c".to_string()
            }
        );

        // `column` is reserved; `RENAME column TO c` parses as `RENAME COLUMN <missing> TO c`
        // (column is the optional COLUMN keyword, then no old name follows) — error.
        assert!(parse("ALTER TABLE t RENAME column TO c;").is_err());
        // Quoted "column" works as the old name.
        let Stmt::AlterTable(a) = &parse("ALTER TABLE t RENAME \"column\" TO c;").unwrap()[0]
        else {
            panic!("expected ALTER TABLE")
        };
        assert_eq!(
            a.action,
            AlterTableAction::RenameColumn {
                old: "\"column\"".to_string(),
                new: "c".to_string()
            }
        );

        // Bad syntax: missing TO.
        assert!(parse("ALTER TABLE t RENAME COLUMN b c;").is_err());
        // Bad syntax: missing new name.
        assert!(parse("ALTER TABLE t RENAME COLUMN b TO;").is_err());
        // Bad syntax: missing old name.
        assert!(parse("ALTER TABLE t RENAME COLUMN TO c;").is_err());
    }

    #[test]
    fn create_view_parses() {
        // Minimal: CREATE VIEW name AS SELECT ...
        let Stmt::CreateView(v) = &parse("CREATE VIEW v AS SELECT 1;").unwrap()[0] else {
            panic!("expected CREATE VIEW")
        };
        assert!(!v.temporary);
        assert!(!v.if_not_exists);
        assert!(v.schema.is_none());
        assert_eq!(v.name, "v");
        assert!(v.columns.is_empty());
        assert!(v.select.columns.len() == 1);

        // TEMP + IF NOT EXISTS + schema + column list.
        let Stmt::CreateView(v) =
            &parse("CREATE TEMP VIEW IF NOT EXISTS main.v (a, b) AS SELECT x, y FROM t;").unwrap()[0]
        else {
            panic!("expected CREATE VIEW")
        };
        assert!(v.temporary);
        assert!(v.if_not_exists);
        assert_eq!(v.schema.as_deref(), Some("main"));
        assert_eq!(v.name, "v");
        assert_eq!(v.columns, vec!["a".to_string(), "b".to_string()]);
        assert!(v.select.from.len() == 1);

        // Bad syntax: missing AS.
        assert!(parse("CREATE VIEW v SELECT 1;").is_err());
        // Bad syntax: missing SELECT.
        assert!(parse("CREATE VIEW v AS ;").is_err());
        // `view` is reserved, so it must be quoted to use as a name.
        assert!(parse("CREATE VIEW view AS SELECT 1;").is_err());
        // Bad syntax: missing view name.
        assert!(parse("CREATE VIEW AS SELECT 1;").is_err());
    }

    #[test]
    fn drop_view_parses() {
        let Stmt::DropView(d) = &parse("DROP VIEW v;").unwrap()[0] else {
            panic!("expected DROP VIEW")
        };
        assert!(!d.if_exists);
        assert!(d.schema.is_none());
        assert_eq!(d.name, "v");

        let Stmt::DropView(d) = &parse("DROP VIEW IF EXISTS main.v;").unwrap()[0] else {
            panic!("expected DROP VIEW IF EXISTS")
        };
        assert!(d.if_exists);
        assert_eq!(d.schema.as_deref(), Some("main"));
        assert_eq!(d.name, "v");

        // `view` is reserved, so it must be quoted to use as a view name.
        assert!(parse("DROP VIEW view;").is_err());
        // Bad syntax: missing view name.
        assert!(parse("DROP VIEW;").is_err());
    }

    #[test]
    fn create_trigger_parses() {
        // Minimal: CREATE TRIGGER name BEFORE INSERT ON tbl BEGIN ... END
        let Stmt::CreateTrigger(t) =
            &parse("CREATE TRIGGER trg BEFORE INSERT ON t BEGIN INSERT INTO log VALUES (1); END;")
                .unwrap()[0]
        else {
            panic!("expected CREATE TRIGGER")
        };
        assert!(!t.temporary);
        assert!(!t.if_not_exists);
        assert!(t.schema.is_none());
        assert_eq!(t.name, "trg");
        assert_eq!(t.timing, TriggerTime::Before);
        assert_eq!(t.event, TriggerEvent::Insert);
        assert!(t.table_schema.is_none());
        assert_eq!(t.table, "t");
        assert!(!t.for_each_row);
        assert!(t.when_clause.is_none());
        assert_eq!(t.body.len(), 1);
        assert!(matches!(t.body[0], TriggerStep::Insert(_)));

        // AFTER DELETE with FOR EACH ROW
        let Stmt::CreateTrigger(t) = &parse(
            "CREATE TRIGGER trg AFTER DELETE ON t FOR EACH ROW BEGIN DELETE FROM log WHERE id = 1; END;",
        )
        .unwrap()[0]
        else {
            panic!("expected CREATE TRIGGER")
        };
        assert_eq!(t.timing, TriggerTime::After);
        assert_eq!(t.event, TriggerEvent::Delete);
        assert!(t.for_each_row);
        assert_eq!(t.body.len(), 1);
        assert!(matches!(t.body[0], TriggerStep::Delete(_)));

        // INSTEAD OF UPDATE OF cols, with WHEN, multiple body statements
        let Stmt::CreateTrigger(t) = &parse(
            "CREATE TEMP TRIGGER IF NOT EXISTS main.trg INSTEAD OF UPDATE OF a, b ON v \
             FOR EACH ROW WHEN new.x > 0 \
             BEGIN UPDATE log SET x = 1; SELECT 1; DELETE FROM log WHERE id = old.id; END;",
        )
        .unwrap()[0]
        else {
            panic!("expected CREATE TRIGGER")
        };
        assert!(t.temporary);
        assert!(t.if_not_exists);
        assert_eq!(t.schema.as_deref(), Some("main"));
        assert_eq!(t.name, "trg");
        assert_eq!(t.timing, TriggerTime::InsteadOf);
        match &t.event {
            TriggerEvent::Update { columns } => assert_eq!(columns, &["a", "b"]),
            other => panic!("expected Update event, got {other:?}"),
        }
        assert_eq!(t.table, "v");
        assert!(t.for_each_row);
        assert!(t.when_clause.is_some());
        assert_eq!(t.body.len(), 3);
        assert!(matches!(t.body[0], TriggerStep::Update(_)));
        assert!(matches!(t.body[1], TriggerStep::Select(_)));
        assert!(matches!(t.body[2], TriggerStep::Delete(_)));

        // Body with trailing semicolon before END is allowed.
        let Stmt::CreateTrigger(t) =
            &parse("CREATE TRIGGER trg BEFORE INSERT ON t BEGIN INSERT INTO log VALUES (1);; END;")
                .unwrap()[0]
        else {
            panic!("expected CREATE TRIGGER")
        };
        assert_eq!(t.body.len(), 1);

        // Bad syntax: missing BEGIN/END.
        assert!(parse("CREATE TRIGGER trg BEFORE INSERT ON t INSERT INTO log VALUES (1);").is_err());
        // Bad syntax: missing event.
        assert!(parse("CREATE TRIGGER trg BEFORE ON t BEGIN SELECT 1; END;").is_err());
        // Bad syntax: empty body.
        assert!(parse("CREATE TRIGGER trg BEFORE INSERT ON t BEGIN END;").is_err());
        // `trigger` is reserved.
        assert!(parse("CREATE TRIGGER trigger BEFORE INSERT ON t BEGIN SELECT 1; END;").is_err());
    }

    #[test]
    fn drop_trigger_parses() {
        let Stmt::DropTrigger(d) = &parse("DROP TRIGGER trg;").unwrap()[0] else {
            panic!("expected DROP TRIGGER")
        };
        assert!(!d.if_exists);
        assert!(d.schema.is_none());
        assert_eq!(d.name, "trg");

        let Stmt::DropTrigger(d) = &parse("DROP TRIGGER IF EXISTS main.trg;").unwrap()[0] else {
            panic!("expected DROP TRIGGER IF EXISTS")
        };
        assert!(d.if_exists);
        assert_eq!(d.schema.as_deref(), Some("main"));
        assert_eq!(d.name, "trg");

        // `trigger` is reserved, so it must be quoted to use as a trigger name.
        assert!(parse("DROP TRIGGER trigger;").is_err());
        // Bad syntax: missing trigger name.
        assert!(parse("DROP TRIGGER;").is_err());
    }

    #[test]
    fn pragma_parses() {
        // Read form: PRAGMA name
        let Stmt::Pragma(p) = &parse("PRAGMA foo;").unwrap()[0] else {
            panic!("expected PRAGMA")
        };
        assert!(p.schema.is_none());
        assert_eq!(p.name, "foo");
        assert!(p.value.is_none());

        // Schema-qualified.
        let Stmt::Pragma(p) = &parse("PRAGMA main.foo;").unwrap()[0] else {
            panic!("expected PRAGMA")
        };
        assert_eq!(p.schema.as_deref(), Some("main"));
        assert_eq!(p.name, "foo");
        assert!(p.value.is_none());

        // Equal form with identifier value.
        let Stmt::Pragma(p) = &parse("PRAGMA journal_mode = WAL;").unwrap()[0] else {
            panic!("expected PRAGMA")
        };
        assert_eq!(p.name, "journal_mode");
        match &p.value {
            Some(PragmaValue::Equal(PragmaValueKind::Ident(s))) => assert_eq!(s, "WAL"),
            other => panic!("expected Equal(Ident), got {other:?}"),
        }

        // Equal form with numeric value (signed).
        let Stmt::Pragma(p) = &parse("PRAGMA foo = -1;").unwrap()[0] else {
            panic!("expected PRAGMA")
        };
        match &p.value {
            Some(PragmaValue::Equal(PragmaValueKind::Number(Literal::Integer(n)))) => {
                assert_eq!(*n, -1);
            }
            other => panic!("expected Equal(Number), got {other:?}"),
        }

        // Paren form with numeric value.
        let Stmt::Pragma(p) = &parse("PRAGMA foo(1);").unwrap()[0] else {
            panic!("expected PRAGMA")
        };
        match &p.value {
            Some(PragmaValue::Paren(PragmaValueKind::Number(Literal::Integer(n)))) => {
                assert_eq!(*n, 1);
            }
            other => panic!("expected Paren(Number), got {other:?}"),
        }

        // Keyword values: ON / DELETE / DEFAULT.
        let Stmt::Pragma(p) = &parse("PRAGMA foo = ON;").unwrap()[0] else {
            panic!("expected PRAGMA")
        };
        match &p.value {
            Some(PragmaValue::Equal(PragmaValueKind::On)) => {}
            other => panic!("expected Equal(On), got {other:?}"),
        }
        let Stmt::Pragma(p) = &parse("PRAGMA foo(DELETE);").unwrap()[0] else {
            panic!("expected PRAGMA")
        };
        match &p.value {
            Some(PragmaValue::Paren(PragmaValueKind::Delete)) => {}
            other => panic!("expected Paren(Delete), got {other:?}"),
        }
        let Stmt::Pragma(p) = &parse("PRAGMA foo = DEFAULT;").unwrap()[0] else {
            panic!("expected PRAGMA")
        };
        match &p.value {
            Some(PragmaValue::Equal(PragmaValueKind::Default)) => {}
            other => panic!("expected Equal(Default), got {other:?}"),
        }

        // `pragma` is reserved, so it must be quoted to use as a pragma name.
        assert!(parse("PRAGMA pragma;").is_err());
        // Bad syntax: missing pragma name.
        assert!(parse("PRAGMA = 1;").is_err());
    }

    #[test]
    fn transaction_parses() {
        // BEGIN (defaults to DEFERRED)
        let Stmt::Transaction(t) = &parse("BEGIN;").unwrap()[0] else {
            panic!("expected transaction")
        };
        assert_eq!(t, &TransactionStmt::Begin { transaction_type: TransactionType::Deferred, name: None });

        // BEGIN IMMEDIATE TRANSACTION
        let Stmt::Transaction(t) = &parse("BEGIN IMMEDIATE TRANSACTION;").unwrap()[0] else {
            panic!("expected transaction")
        };
        assert_eq!(t, &TransactionStmt::Begin { transaction_type: TransactionType::Immediate, name: None });

        // BEGIN EXCLUSIVE TRANSACTION my_txn
        let Stmt::Transaction(t) = &parse("BEGIN EXCLUSIVE TRANSACTION my_txn;").unwrap()[0] else {
            panic!("expected transaction")
        };
        assert_eq!(t, &TransactionStmt::Begin { transaction_type: TransactionType::Exclusive, name: Some("my_txn".to_string()) });

        // BEGIN DEFERRED (without TRANSACTION keyword, just the type)
        let Stmt::Transaction(t) = &parse("BEGIN DEFERRED;").unwrap()[0] else {
            panic!("expected transaction")
        };
        assert_eq!(t, &TransactionStmt::Begin { transaction_type: TransactionType::Deferred, name: None });

        // COMMIT
        let Stmt::Transaction(t) = &parse("COMMIT;").unwrap()[0] else {
            panic!("expected transaction")
        };
        assert_eq!(t, &TransactionStmt::Commit { name: None, ended: false });

        // END TRANSACTION
        let Stmt::Transaction(t) = &parse("END TRANSACTION;").unwrap()[0] else {
            panic!("expected transaction")
        };
        assert_eq!(t, &TransactionStmt::Commit { name: None, ended: true });

        // COMMIT TRANSACTION my_txn
        let Stmt::Transaction(t) = &parse("COMMIT TRANSACTION my_txn;").unwrap()[0] else {
            panic!("expected transaction")
        };
        assert_eq!(t, &TransactionStmt::Commit { name: Some("my_txn".to_string()), ended: false });

        // ROLLBACK (full)
        let Stmt::Transaction(t) = &parse("ROLLBACK;").unwrap()[0] else {
            panic!("expected transaction")
        };
        assert_eq!(t, &TransactionStmt::Rollback { name: None, to_savepoint: None });

        // ROLLBACK TO sp
        let Stmt::Transaction(t) = &parse("ROLLBACK TO sp;").unwrap()[0] else {
            panic!("expected transaction")
        };
        assert_eq!(t, &TransactionStmt::Rollback { name: None, to_savepoint: Some("sp".to_string()) });

        // ROLLBACK TRANSACTION TO SAVEPOINT sp
        let Stmt::Transaction(t) = &parse("ROLLBACK TRANSACTION TO SAVEPOINT sp;").unwrap()[0] else {
            panic!("expected transaction")
        };
        assert_eq!(t, &TransactionStmt::Rollback { name: None, to_savepoint: Some("sp".to_string()) });

        // SAVEPOINT name
        let Stmt::Transaction(t) = &parse("SAVEPOINT sp;").unwrap()[0] else {
            panic!("expected transaction")
        };
        assert_eq!(t, &TransactionStmt::Savepoint("sp".to_string()));

        // RELEASE [SAVEPOINT] name
        let Stmt::Transaction(t) = &parse("RELEASE sp;").unwrap()[0] else {
            panic!("expected transaction")
        };
        assert_eq!(t, &TransactionStmt::Release("sp".to_string()));
        let Stmt::Transaction(t) = &parse("RELEASE SAVEPOINT sp;").unwrap()[0] else {
            panic!("expected transaction")
        };
        assert_eq!(t, &TransactionStmt::Release("sp".to_string()));

        // `begin`, `transaction`, `commit`, etc. are reserved.
        assert!(parse("BEGIN begin;").is_err());
        assert!(parse("SAVEPOINT savepoint;").is_err());
        // Bad syntax: SAVEPOINT without name.
        assert!(parse("SAVEPOINT;").is_err());
        assert!(parse("RELEASE;").is_err());
        // Bad syntax: ROLLBACK TO without name.
        assert!(parse("ROLLBACK TO;").is_err());
    }

    #[test]
    fn attach_detach_parses() {
        // ATTACH expr AS expr
        let Stmt::Attach(a) = &parse("ATTACH 'file.db' AS aux;").unwrap()[0] else {
            panic!("expected ATTACH")
        };
        assert!(!a.database_kw);
        assert!(matches!(&a.filename, Expr::Literal(Literal::Text(_))));
        // `aux` is an identifier expression (Expr::Column).
        assert!(matches!(&a.schema_name, Expr::Column { name, .. } if name == "aux"));
        assert!(a.key.is_none());

        // ATTACH DATABASE expr AS expr KEY expr
        let Stmt::Attach(a) =
            &parse("ATTACH DATABASE 'file.db' AS aux KEY 'secret';").unwrap()[0]
        else {
            panic!("expected ATTACH")
        };
        assert!(a.database_kw);
        assert!(a.key.is_some());

        // DETACH expr
        let Stmt::Detach(d) = &parse("DETACH aux;").unwrap()[0] else {
            panic!("expected DETACH")
        };
        assert!(!d.database_kw);

        // DETACH DATABASE expr
        let Stmt::Detach(d) = &parse("DETACH DATABASE aux;").unwrap()[0] else {
            panic!("expected DETACH")
        };
        assert!(d.database_kw);

        // Bad syntax: ATTACH without AS.
        assert!(parse("ATTACH 'file.db' aux;").is_err());
        // Bad syntax: DETACH without name.
        assert!(parse("DETACH;").is_err());
    }

    #[test]
    fn vacuum_analyze_reindex_parses() {
        // VACUUM (bare)
        let Stmt::Vacuum(v) = &parse("VACUUM;").unwrap()[0] else {
            panic!("expected VACUUM")
        };
        assert!(v.schema.is_none());
        assert!(v.into.is_none());

        // VACUUM schema
        let Stmt::Vacuum(v) = &parse("VACUUM main;").unwrap()[0] else {
            panic!("expected VACUUM")
        };
        assert_eq!(v.schema.as_deref(), Some("main"));
        assert!(v.into.is_none());

        // VACUUM INTO expr
        let Stmt::Vacuum(v) = &parse("VACUUM INTO '/tmp/x.db';").unwrap()[0] else {
            panic!("expected VACUUM")
        };
        assert!(v.schema.is_none());
        assert!(v.into.is_some());

        // VACUUM schema INTO expr
        let Stmt::Vacuum(v) = &parse("VACUUM main INTO '/tmp/x.db';").unwrap()[0] else {
            panic!("expected VACUUM")
        };
        assert_eq!(v.schema.as_deref(), Some("main"));
        assert!(v.into.is_some());

        // ANALYZE (bare)
        let Stmt::Analyze(a) = &parse("ANALYZE;").unwrap()[0] else {
            panic!("expected ANALYZE")
        };
        assert!(a.schema.is_none());
        assert!(a.name.is_none());

        // ANALYZE schema.name
        let Stmt::Analyze(a) = &parse("ANALYZE main.t;").unwrap()[0] else {
            panic!("expected ANALYZE")
        };
        assert_eq!(a.schema.as_deref(), Some("main"));
        assert_eq!(a.name.as_deref(), Some("t"));

        // ANALYZE name (no schema)
        let Stmt::Analyze(a) = &parse("ANALYZE t;").unwrap()[0] else {
            panic!("expected ANALYZE")
        };
        assert!(a.schema.is_none());
        assert_eq!(a.name.as_deref(), Some("t"));

        // REINDEX (bare)
        let Stmt::Reindex(r) = &parse("REINDEX;").unwrap()[0] else {
            panic!("expected REINDEX")
        };
        assert!(r.schema.is_none());
        assert!(r.name.is_none());

        // REINDEX schema.name
        let Stmt::Reindex(r) = &parse("REINDEX main.idx;").unwrap()[0] else {
            panic!("expected REINDEX")
        };
        assert_eq!(r.schema.as_deref(), Some("main"));
        assert_eq!(r.name.as_deref(), Some("idx"));

        // Reserved keywords: `vacuum`, `analyze`, `reindex`, `attach`, `detach` are reserved.
        assert!(parse("VACUUM vacuum;").is_err());
        assert!(parse("ANALYZE analyze;").is_err());
        assert!(parse("REINDEX reindex;").is_err());
        assert!(parse("ATTACH attach AS x;").is_err());
    }

    #[test]
    fn create_virtual_table_parses() {
        // CREATE VIRTUAL TABLE name USING module
        let Stmt::CreateVirtualTable(v) = &parse("CREATE VIRTUAL TABLE t USING fts5;").unwrap()[0]
        else {
            panic!("expected CREATE VIRTUAL TABLE")
        };
        assert!(!v.if_not_exists);
        assert!(v.schema.is_none());
        assert_eq!(v.name, "t");
        assert_eq!(v.module, "fts5");
        assert_eq!(v.args, "");

        // CREATE VIRTUAL TABLE IF NOT EXISTS schema.name USING module (args)
        let Stmt::CreateVirtualTable(v) =
            &parse("CREATE VIRTUAL TABLE IF NOT EXISTS main.t USING fts5(a, b, tokenize=porter);")
                .unwrap()[0]
        else {
            panic!("expected CREATE VIRTUAL TABLE")
        };
        assert!(v.if_not_exists);
        assert_eq!(v.schema.as_deref(), Some("main"));
        assert_eq!(v.name, "t");
        assert_eq!(v.module, "fts5");
        assert_eq!(v.args, "a, b, tokenize=porter");

        // Module args with nested parens (balanced).
        let Stmt::CreateVirtualTable(v) =
            &parse("CREATE VIRTUAL TABLE t USING m(a, (b, c), d);").unwrap()[0]
        else {
            panic!("expected CREATE VIRTUAL TABLE")
        };
        assert_eq!(v.module, "m");
        assert_eq!(v.args, "a, (b, c), d");

        // Bad syntax: missing USING.
        assert!(parse("CREATE VIRTUAL TABLE t fts5;").is_err());
        // Bad syntax: missing module name.
        assert!(parse("CREATE VIRTUAL TABLE t USING;").is_err());
        // Bad syntax: missing table name.
        assert!(parse("CREATE VIRTUAL TABLE USING fts5;").is_err());
        // `virtual` is reserved, so it must be quoted to use as a table name.
        assert!(parse("CREATE VIRTUAL TABLE virtual USING fts5;").is_err());
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
        assert!(u.returning.is_none());

        let Stmt::Update(u) =
            &parse("UPDATE OR REPLACE main.t SET a = a + 1, b = 'x' WHERE a > 0 RETURNING rowid, b;").unwrap()[0]
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
        let ret = u.returning.as_ref().expect("returning present");
        assert_eq!(ret.len(), 2);
        assert!(matches!(&ret[0], ResultColumn::Expr { expr: Expr::Column { name, .. }, .. } if name == "rowid"));
        assert!(matches!(&ret[1], ResultColumn::Expr { expr: Expr::Column { name, .. }, .. } if name == "b"));
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
    fn in_subquery_parses() {
        // `X IN (SELECT …)` — the RHS is a subquery, not a value list.
        let Stmt::Select(s) = &parse("SELECT 1 WHERE 5 IN (SELECT x FROM t);").unwrap()[0] else {
            panic!()
        };
        assert!(matches!(
            s.where_clause,
            Some(Expr::InSubquery { negated: false, .. })
        ));
        // `X NOT IN (SELECT …)`.
        let Stmt::Select(s) = &parse("SELECT 1 WHERE 5 NOT IN (SELECT x FROM t);").unwrap()[0]
        else {
            panic!()
        };
        assert!(matches!(
            s.where_clause,
            Some(Expr::InSubquery { negated: true, .. })
        ));
        // `X IN (VALUES (1),(2))` — the RHS is a VALUES select body (a subquery shape).
        let Stmt::Select(s) = &parse("SELECT 1 WHERE 5 IN (VALUES (1), (2));").unwrap()[0] else {
            panic!()
        };
        assert!(matches!(
            s.where_clause,
            Some(Expr::InSubquery { negated: false, .. })
        ));
    }

    #[test]
    fn row_value_expression_parses() {
        // A row value `(e0, e1, …)` with ≥2 entries becomes `Expr::Row`. A parenthesised single
        // expression `(e)` is *not* a row value (it collapses to `e` itself, mirroring
        // upstream's `LP nexprlist COMMA expr RP` rule that requires a comma).
        let Stmt::Select(s) = &parse("SELECT (1, 2);").unwrap()[0] else { panic!() };
        let ResultColumn::Expr { expr, .. } = &s.columns[0] else { panic!() };
        let Expr::Row(els) = expr else { panic!("not Row: {expr:?}") };
        assert_eq!(els.len(), 2);

        // Three-element row value.
        let Stmt::Select(s) = &parse("SELECT (1, 2, 3);").unwrap()[0] else { panic!() };
        let ResultColumn::Expr { expr, .. } = &s.columns[0] else { panic!() };
        let Expr::Row(els) = expr else { panic!("not Row: {expr:?}") };
        assert_eq!(els.len(), 3);

        // A parenthesised single expression is *not* a row value.
        let Stmt::Select(s) = &parse("SELECT (1);").unwrap()[0] else { panic!() };
        let ResultColumn::Expr { expr, .. } = &s.columns[0] else { panic!() };
        assert!(matches!(expr, Expr::Literal(Literal::Integer(1))));

        // Row-value comparison: `(a, b) = (1, 2)` keeps both sides as Row nodes.
        let Stmt::Select(s) = &parse("SELECT 1 WHERE (a, b) = (1, 2);").unwrap()[0] else {
            panic!()
        };
        let Some(Expr::Binary { op, left, right }) = &s.where_clause else { panic!() };
        assert_eq!(*op, BinaryOp::Eq);
        assert!(matches!(left.as_ref(), Expr::Row(v) if v.len() == 2));
        assert!(matches!(right.as_ref(), Expr::Row(v) if v.len() == 2));

        // Row-value IN with parenthesised row literals: `(a,b) IN ((1,2),(3,4))`.
        let Stmt::Select(s) = &parse("SELECT 1 WHERE (a, b) IN ((1, 2), (3, 4));").unwrap()[0]
        else {
            panic!()
        };
        let Some(Expr::In { values, negated, .. }) = &s.where_clause else { panic!() };
        assert!(!negated);
        assert_eq!(values.len(), 2);
        assert!(matches!(values[0], Expr::Row(_)));
        assert!(matches!(values[1], Expr::Row(_)));

        // Row-value IN with a subquery: `(a, b) IN (SELECT x, y FROM t)`.
        let Stmt::Select(s) = &parse("SELECT 1 WHERE (a, b) IN (SELECT x, y FROM t);").unwrap()[0]
        else {
            panic!()
        };
        assert!(matches!(
            s.where_clause,
            Some(Expr::InSubquery { negated: false, .. })
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
        assert_eq!(j.op, JoinOp::NaturalLeft);
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
    fn subquery_in_from_clause_parses() {
        let Stmt::Select(s) = &parse("SELECT * FROM (SELECT 1 AS x, 2 AS y) AS sq;").unwrap()[0]
        else {
            panic!("expected SELECT")
        };
        let TableOrJoin::Subquery { query, alias } = &s.from[0] else {
            panic!("expected subquery in FROM")
        };
        assert_eq!(alias, "sq");
        assert_eq!(query.columns.len(), 2);
        assert!(query.from.is_empty());
        assert!(query.values.is_empty());

        // Parenthesised joins can be mixed with subqueries.
        let Stmt::Select(s) =
            &parse("SELECT * FROM (t1 JOIN t2 ON t1.a = t2.b) JOIN t3 ON t2.c = t3.c;").unwrap()[0]
        else {
            panic!("expected SELECT")
        };
        let TableOrJoin::Join(outer) = &s.from[0] else {
            panic!("expected join")
        };
        assert_eq!(outer.right.name, "t3");
        let TableOrJoin::Join(inner) = outer.left.as_ref() else {
            panic!("expected nested join")
        };
        assert_eq!(inner.right.name, "t2");
        assert_eq!(inner.left.table().unwrap().name, "t1");

        // VALUES as a parenthesised subquery in FROM.
        let Stmt::Select(s) = &parse("SELECT * FROM (VALUES (1, 2)) AS sq;").unwrap()[0] else {
            panic!("expected SELECT")
        };
        let TableOrJoin::Subquery { query, alias } = &s.from[0] else {
            panic!("expected subquery in FROM")
        };
        assert_eq!(alias, "sq");
        assert!(query.columns.is_empty());
        assert_eq!(query.values.len(), 1);
    }

    #[test]
    fn natural_join_may_not_have_on_or_using() {
        // These are still syntactically valid in the grammar; upstream rejects them at
        // semantic analysis. For the parser slice we just verify they parse.
        assert!(parse("SELECT * FROM t1 NATURAL JOIN t2 ON t1.a = t2.a;").is_ok());
        assert!(parse("SELECT * FROM t1 NATURAL JOIN t2 USING(a);").is_ok());
    }

    #[test]
    fn values_core_parses() {
        let Stmt::Select(s) = &parse("VALUES (1, 'a'), (2, 'b');").unwrap()[0] else {
            panic!("expected SELECT")
        };
        assert!(s.columns.is_empty());
        assert!(s.from.is_empty());
        assert!(s.values.len() == 2);
        assert_eq!(s.values[0].len(), 2);
        assert_eq!(s.values[1].len(), 2);

        // VALUES can appear as the left side of a compound.
        let Stmt::Select(s) = &parse("VALUES (1, 2) UNION ALL SELECT 3, 4;").unwrap()[0] else {
            panic!("expected SELECT")
        };
        assert!(!s.values.is_empty());
        assert_eq!(s.compound.len(), 1);
    }

    #[test]
    fn values_core_rejects_bad_shape() {
        // Different arity across rows is a syntax error in SQLite (semantic check in upstream).
        // Our grammar currently accepts it; codegen later checks for consistent arity. Keep the
        // test as documentation of the oracle's behavior rather than asserting a parse failure.
        let Stmt::Select(s) = &parse("VALUES (1, 2), (3);").unwrap()[0] else {
            panic!("expected SELECT")
        };
        assert_eq!(s.values.len(), 2);
        assert_eq!(s.values[0].len(), 2);
        assert_eq!(s.values[1].len(), 1);
        // Empty values row is invalid.
        assert!(parse("VALUES ();").is_err());
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

        let Stmt::Select(s) =
            &parse("WITH a AS (SELECT 1), b AS (SELECT 2) SELECT * FROM a, b;").unwrap()[0]
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
