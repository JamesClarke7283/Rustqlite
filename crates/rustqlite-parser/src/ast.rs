//! Abstract syntax tree for the SQL subset Rustqlite currently parses.
//!
//! Node names mirror SQLite's parse-tree structs (`Select`, `SrcList`, `Expr`, `ExprList`,
//! `Insert`, `CreateTable`, `ColumnDef`, …) so the codegen layer can be checked against
//! upstream `build.c`/`select.c`/`expr.c`. Source spans are not yet attached (tracked for
//! the full parse.y port); error locations currently come from the pest error.

/// A complete top-level statement.
// The variants differ in size (SELECT carries the most). Boxing the large variant is the
// usual fix, but the AST is reworked substantially in M2 (full parse.y port) where the node
// representation — including arena/boxing decisions — is revisited; allow it until then.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Select(SelectStmt),
    CreateTable(CreateTable),
    Insert(InsertStmt),
    Delete(DeleteStmt),
    DropTable(DropTableStmt),
    Update(UpdateStmt),
    /// `CREATE [UNIQUE] INDEX [IF NOT EXISTS] name ON tbl(col [COLLATE name] [ASC|DESC]) [WHERE …]`
    /// (the first M5.1 slice accepts a single indexed column, no `WHERE`, no `COLLATE`).
    CreateIndex(CreateIndex),
    /// `DROP INDEX [IF EXISTS] [schema.]name`.
    DropIndex(DropIndexStmt),
    /// `EXPLAIN <stmt>` / `EXPLAIN QUERY PLAN <stmt>`. The inner statement is boxed (it is the
    /// large variant). `kind` distinguishes the bytecode listing from the query-plan tree.
    Explain(Box<Stmt>, ExplainKind),
}

/// Which form of `EXPLAIN` prefixed the statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplainKind {
    /// Plain `EXPLAIN` — the VDBE bytecode listing.
    Bytecode,
    /// `EXPLAIN QUERY PLAN` — the high-level query plan tree.
    QueryPlan,
}

/// A compound-SELECT operator joining two query cores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompoundOperator {
    Union,
    UnionAll,
    Intersect,
    Except,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectStmt {
    pub distinct: bool,
    pub columns: Vec<ResultColumn>,
    pub from: Vec<TableRef>,
    pub where_clause: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    /// Additional compound arms: each `(op, core)` is appended to the leading core.  The arms
    /// carry only their own core clauses (distinct/columns/from/where/group/having); the trailing
    /// `order_by`/`limit`/`offset` on *this* struct bind to the whole compound.
    pub compound: Vec<(CompoundOperator, SelectStmt)>,
    pub order_by: Vec<OrderingTerm>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResultColumn {
    /// `*`
    Star,
    /// `table.*`
    TableStar(String),
    /// An expression, optionally aliased with `AS name`.
    Expr { expr: Expr, alias: Option<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderingTerm {
    pub expr: Expr,
    /// `true` for DESC, `false` for ASC (default).
    pub desc: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateTable {
    pub temporary: bool,
    pub if_not_exists: bool,
    pub schema: Option<String>,
    pub name: String,
    pub columns: Vec<ColumnDef>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    /// The declared type text (e.g. `"INTEGER"`, `"VARCHAR(10)"`), as written. Affinity is
    /// derived from this in the engine's `types` layer, faithfully to SQLite's rules.
    pub type_name: Option<String>,
    pub constraints: Vec<ColumnConstraint>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ColumnConstraint {
    PrimaryKey { desc: bool, autoincrement: bool },
    NotNull,
    Unique,
    Default(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub struct InsertStmt {
    pub or_action: Option<ConflictAction>,
    pub schema: Option<String>,
    pub table: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Expr>>,
}

/// `DELETE FROM [schema.]tbl [WHERE expr]`. The first M4.6 slice omits `ORDER BY`, `LIMIT`,
/// `RETURNING`, and the multi-table `DELETE t1, t2 FROM …` form (those are deferred).
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStmt {
    pub schema: Option<String>,
    pub table: String,
    pub where_clause: Option<Expr>,
}

/// `DROP TABLE [IF EXISTS] [schema.]tbl`. The first M4.6 slice omits `DROP INDEX/VIEW/TRIGGER`.
#[derive(Debug, Clone, PartialEq)]
pub struct DropTableStmt {
    pub if_exists: bool,
    pub schema: Option<String>,
    pub name: String,
}

/// `UPDATE [or_action] [schema.]tbl SET col = expr [, col = expr ...] [WHERE expr]`. The first
/// M5.0 slice: a single-table `UPDATE` with optional `WHERE`, no `ORDER BY`/`LIMIT`/`FROM`/
/// `RETURNING`, no UPSERT/triggers/FK/indexes.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateStmt {
    pub or_action: Option<ConflictAction>,
    pub schema: Option<String>,
    pub table: String,
    pub assignments: Vec<Assignment>,
    pub where_clause: Option<Expr>,
}

/// `CREATE [UNIQUE] INDEX [IF NOT EXISTS] [schema.]name ON tbl(col_or_expr [COLLATE name] [ASC|DESC] …)
/// [WHERE expr]`.
/// Multi-column and expression indexes are accepted from M5.2 onward; the `collation` and `desc`
/// fields are recorded in the AST. The optional `where_clause` is the partial-index predicate.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateIndex {
    pub unique: bool,
    pub if_not_exists: bool,
    pub schema: Option<String>,
    pub name: String,
    pub table: String,
    pub columns: Vec<IndexedColumn>,
    pub where_clause: Option<Expr>,
}

/// One entry in a `CREATE INDEX` column/expression list.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexedColumn {
    /// For a plain column index this is the column name and `expr` is `None`.
    /// For an expression index this is an empty string and `expr` holds the parsed expression.
    pub name: String,
    /// The expression indexed when this is an expression index (`None` for a plain column).
    pub expr: Option<Expr>,
    /// `COLLATE name` applied to this column/expression.
    pub collation: Option<String>,
    /// `true` for `DESC`, `false` for `ASC` (default).
    pub desc: bool,
}

/// `DROP INDEX [IF EXISTS] [schema.]name`. The first M5.1 slice: schema qualifier must be absent
/// (or the default `main`/`temp`).
#[derive(Debug, Clone, PartialEq)]
pub struct DropIndexStmt {
    pub if_exists: bool,
    pub schema: Option<String>,
    pub name: String,
}

/// One `col = expr` on the left of `UPDATE … SET`. Multi-column assignment (the
/// `(a, b) = (…)` row-value form) arrives in a later slice.
#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub column: String,
    pub value: Expr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictAction {
    Rollback,
    Abort,
    Fail,
    Ignore,
    Replace,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Literal),
    Column {
        schema: Option<String>,
        table: Option<String>,
        name: String,
    },
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Function {
        name: String,
        distinct: bool,
        args: FunctionArgs,
    },
    BindParam(String),
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },
    In {
        expr: Box<Expr>,
        values: Vec<Expr>,
        negated: bool,
    },
    Exists(Box<SelectStmt>),
    /// A scalar subquery used as an expression: `(SELECT …)`.  Evaluates to the first column of
    /// the first row (NULL if the subquery returns no rows).  Parsed here; execution is deferred.
    Subquery(Box<SelectStmt>),
    Cast {
        expr: Box<Expr>,
        type_name: String,
    },
    Case {
        base: Option<Box<Expr>>,
        when_then: Vec<(Expr, Expr)>,
        else_expr: Option<Box<Expr>>,
    },
    Collate {
        expr: Box<Expr>,
        collation: String,
    },
    IsDistinctFrom {
        left: Box<Expr>,
        right: Box<Expr>,
        negated: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum FunctionArgs {
    /// `count(*)`
    Star,
    List(Vec<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
    Bool(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Negate,
    Positive,
    Not,
    BitNot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Concat,
    Is,
    IsNot,
    Like,
    Glob,
    BitAnd,
    BitOr,
    ShiftLeft,
    ShiftRight,
    /// `->` — JSON extraction returning a JSON representation.
    JsonExtract,
    /// `->>` — JSON extraction returning a SQL text/numeric value.
    JsonExtractText,
}
