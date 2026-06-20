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
    /// `ALTER TABLE [schema.]tbl RENAME TO new_name`. The first M2.25 slice: only the RENAME
    /// TABLE form; ADD/DROP/RENAME COLUMN arrive in M2.26–M2.28.
    AlterTable(AlterTableStmt),
    /// `CREATE [TEMP] VIEW [IF NOT EXISTS] [schema.]name [(cols)] AS SELECT ...`. M2.29
    /// covers the parser only; view expansion is M15.
    CreateView(CreateView),
    /// `DROP VIEW [IF EXISTS] [schema.]name`. M2.30 covers the parser only.
    DropView(DropViewStmt),
    /// `CREATE [TEMP] TRIGGER [IF NOT EXISTS] [schema.]name <timing> <event> ON tbl
    /// [FOR EACH ROW] [WHEN expr] BEGIN <body> END`. M2.31 covers the parser only.
    CreateTrigger(CreateTrigger),
    /// `DROP TRIGGER [IF EXISTS] [schema.]name`. M2.32 covers the parser only.
    DropTrigger(DropTriggerStmt),
    /// `PRAGMA [schema.]name [= value | (value)]`. M2.33 covers the parser only; codegen is M20.
    Pragma(PragmaStmt),
    /// Transaction control statements: `BEGIN`, `COMMIT`/`END`, `ROLLBACK`,
    /// `SAVEPOINT`, `RELEASE`. M2.34–M2.37 cover the parser only; codegen is M12.
    Transaction(TransactionStmt),
    /// `ATTACH [DATABASE] expr AS expr [KEY expr]`. M2.38 covers the parser only; codegen is M21.
    Attach(AttachStmt),
    /// `DETACH [DATABASE] expr`. M2.39 covers the parser only; codegen is M21.
    Detach(DetachStmt),
    /// `VACUUM [schema] [INTO expr]`. M2.40 covers the parser only; codegen is M22.
    Vacuum(VacuumStmt),
    /// `ANALYZE [schema.]name`. M2.41 covers the parser only; codegen is M22.
    Analyze(AnalyzeStmt),
    /// `REINDEX [schema.]name`. M2.42 covers the parser only; codegen is M22.
    Reindex(ReindexStmt),
    /// `CREATE VIRTUAL TABLE [IF NOT EXISTS] [schema.]name USING module [(args)]`.
    /// M2.43 covers the parser only; codegen is M31.
    CreateVirtualTable(CreateVirtualTable),
    /// `EXPLAIN <stmt>` / `EXPLAIN QUERY PLAN <stmt>`. The inner statement is boxed (it is the
    /// large variant). `kind` distinguishes the bytecode listing from the query-plan tree.
    Explain(Box<Stmt>, ExplainKind),
}

/// `CREATE VIRTUAL TABLE [IF NOT EXISTS] [schema.]name USING module [(args)]`. M2.43 covers
/// the parser only; codegen is M31. The module arguments are captured as the raw text between
/// the parentheses (matching upstream's permissive `vtabarglist` which accepts arbitrary
/// token sequences with balanced parens), so the module implementation can interpret them.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateVirtualTable {
    pub if_not_exists: bool,
    pub schema: Option<String>,
    pub name: String,
    /// The virtual table module name (e.g. `fts5`, `rtree`, `generate_series`).
    pub module: String,
    /// The raw text of the module arguments (the contents of the trailing `(...)`, without
    /// the surrounding parens). Empty when no argument list is present.
    pub args: String,
}

/// `ATTACH [DATABASE] expr AS expr [KEY expr]`. M2.38 covers the parser only; codegen is M21.
#[derive(Debug, Clone, PartialEq)]
pub struct AttachStmt {
    /// Whether the optional `DATABASE` keyword was present (informational only).
    pub database_kw: bool,
    pub filename: Expr,
    pub schema_name: Expr,
    pub key: Option<Expr>,
}

/// `DETACH [DATABASE] expr`. M2.39 covers the parser only; codegen is M21.
#[derive(Debug, Clone, PartialEq)]
pub struct DetachStmt {
    pub database_kw: bool,
    pub schema_name: Expr,
}

/// `VACUUM [schema] [INTO expr]`. M2.40 covers the parser only; codegen is M22.
#[derive(Debug, Clone, PartialEq)]
pub struct VacuumStmt {
    pub schema: Option<String>,
    pub into: Option<Expr>,
}

/// `ANALYZE [schema.]name`. M2.41 covers the parser only; codegen is M22.
#[derive(Debug, Clone, PartialEq)]
pub struct AnalyzeStmt {
    pub schema: Option<String>,
    pub name: Option<String>,
}

/// `REINDEX [schema.]name`. M2.42 covers the parser only; codegen is M22.
#[derive(Debug, Clone, PartialEq)]
pub struct ReindexStmt {
    pub schema: Option<String>,
    pub name: Option<String>,
}

/// One of the transaction-control statements. M2.34–M2.37 cover the parser only; codegen is M12.
#[derive(Debug, Clone, PartialEq)]
pub enum TransactionStmt {
    /// `BEGIN [DEFERRED|IMMEDIATE|EXCLUSIVE] [TRANSACTION [name]]`.
    Begin {
        transaction_type: TransactionType,
        name: Option<String>,
    },
    /// `COMMIT [TRANSACTION [name]]` / `END [TRANSACTION [name]]`. `END` is an alias for
    /// `COMMIT`; the `ended` flag records which keyword was used (purely informational).
    Commit {
        name: Option<String>,
        ended: bool,
    },
    /// `ROLLBACK [TRANSACTION [name]] [TO [SAVEPOINT] name]`. When the `to_savepoint` field is
    /// `Some`, this is a rollback to a named savepoint; otherwise it is a full rollback.
    Rollback {
        name: Option<String>,
        to_savepoint: Option<String>,
    },
    /// `SAVEPOINT name`.
    Savepoint(String),
    /// `RELEASE [SAVEPOINT] name`.
    Release(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionType {
    Deferred,
    Immediate,
    Exclusive,
}

/// `ALTER TABLE [schema.]tbl <action>`. M2.25 covers only the `RENAME TO new_name` action;
/// the remaining actions (ADD/DROP/RENAME COLUMN, ADD/DROP CONSTRAINT, ALTER COLUMN
/// SET/DROP NOT NULL) are tracked as separate M2 tasks.
#[derive(Debug, Clone, PartialEq)]
pub struct AlterTableStmt {
    pub schema: Option<String>,
    pub table: String,
    pub action: AlterTableAction,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AlterTableAction {
    /// `RENAME TO new_name` — rename the table.
    RenameTo(String),
    /// `ADD [COLUMN] col_def` — add a new column. The optional `COLUMN` keyword carries no
    /// semantic content; the column definition is identical in shape to a CREATE TABLE
    /// column definition.
    AddColumn(ColumnDef),
    /// `DROP [COLUMN] name` — drop an existing column. The optional `COLUMN` keyword carries
    /// no semantic content.
    DropColumn(String),
    /// `RENAME [COLUMN] old TO new` — rename an existing column. The optional `COLUMN`
    /// keyword carries no semantic content.
    RenameColumn { old: String, new: String },
    /// `ALTER [COLUMN] name DROP NOT NULL` (M2.69).
    AlterColumnDropNotNull(String),
    /// `ALTER [COLUMN] name SET NOT NULL` (M2.70).
    AlterColumnSetNotNull(String),
    /// `ADD [CONSTRAINT name] CHECK (expr)` (M2.71).
    AddCheckConstraint { name: Option<String>, expr: Expr },
    /// `DROP CONSTRAINT name` (M2.72).
    DropConstraint(String),
}

/// `CREATE [TEMP] VIEW [IF NOT EXISTS] [schema.]name [(cols)] AS SELECT ...`. The optional
/// column list gives explicit names to the view's columns; when absent, the column names
/// are inferred from the SELECT's result columns. M2.29 covers the parser only.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateView {
    pub temporary: bool,
    pub if_not_exists: bool,
    pub schema: Option<String>,
    pub name: String,
    /// Optional explicit column list `(col1, col2, ...)`.
    pub columns: Vec<String>,
    pub select: SelectStmt,
}

/// `DROP VIEW [IF EXISTS] [schema.]name`. M2.30 covers the parser only; codegen is M15.
#[derive(Debug, Clone, PartialEq)]
pub struct DropViewStmt {
    pub if_exists: bool,
    pub schema: Option<String>,
    pub name: String,
}

/// `CREATE [TEMP] TRIGGER [IF NOT EXISTS] [schema.]name <timing> <event> ON tbl
/// [FOR EACH ROW] [WHEN expr] BEGIN <body> END`. M2.31 covers the parser only; the trigger
/// body is a list of statements (INSERT/UPDATE/DELETE/SELECT). `RAISE(...)` inside trigger
/// bodies is parsed as part of M16 (the trigger milestone).
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTrigger {
    pub temporary: bool,
    pub if_not_exists: bool,
    pub schema: Option<String>,
    pub name: String,
    /// When the trigger fires relative to the event.
    pub timing: TriggerTime,
    /// The event that fires the trigger. `Update` carries an optional column list for the
    /// `UPDATE OF col1, col2` form.
    pub event: TriggerEvent,
    /// The table the trigger is attached to (schema-qualified).
    pub table_schema: Option<String>,
    pub table: String,
    /// `FOR EACH ROW` — currently the only form SQLite supports; parsed but not enforced.
    pub for_each_row: bool,
    /// Optional `WHEN expr` trigger condition.
    pub when_clause: Option<Expr>,
    /// The trigger body: a list of statements separated by `;` inside `BEGIN ... END`.
    pub body: Vec<TriggerStep>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerTime {
    Before,
    After,
    InsteadOf,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TriggerEvent {
    Delete,
    Insert,
    /// `UPDATE [OF col1, col2, ...]` — the column list is optional.
    Update { columns: Vec<String> },
}

/// One statement in a trigger body. These mirror the top-level statement structs but live
/// inside a trigger program. The `RAISE(...)` expression form is deferred to M16.
#[derive(Debug, Clone, PartialEq)]
pub enum TriggerStep {
    Insert(InsertStmt),
    Update(UpdateStmt),
    Delete(DeleteStmt),
    Select(SelectStmt),
}

/// `DROP TRIGGER [IF EXISTS] [schema.]name`. M2.32 covers the parser only; codegen is M16.
#[derive(Debug, Clone, PartialEq)]
pub struct DropTriggerStmt {
    pub if_exists: bool,
    pub schema: Option<String>,
    pub name: String,
}

/// `PRAGMA [schema.]name [= value | (value)]`. M2.33 covers the parser only; codegen is M20.
/// The value is optional (read form); when present it is a signed number, identifier, or
/// one of the keywords `ON`/`DELETE`/`DEFAULT` (matching upstream's `nmnum`/`minus_num`).
#[derive(Debug, Clone, PartialEq)]
pub struct PragmaStmt {
    pub schema: Option<String>,
    pub name: String,
    /// The value form: bare (read), `= value`, or `(value)`.
    pub value: Option<PragmaValue>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PragmaValue {
    /// `= value` or `(value)` — the value itself.
    Equal(PragmaValueKind),
    /// `(value)` — parenthesised form.
    Paren(PragmaValueKind),
}

/// The actual value of a pragma. Upstream's `nmnum`/`minus_num` accepts a signed number, an
/// identifier, or the keywords `ON`/`DELETE`/`DEFAULT`.
#[derive(Debug, Clone, PartialEq)]
pub enum PragmaValueKind {
    /// A signed number literal (e.g. `1`, `-1`, `+2`, `0x1F`).
    Number(Literal),
    /// An identifier (pragma name as value, e.g. `PRAGMA journal_mode = WAL`).
    Ident(String),
    /// The keyword `ON`.
    On,
    /// The keyword `DELETE`.
    Delete,
    /// The keyword `DEFAULT`.
    Default,
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

/// A single common table expression.
#[derive(Debug, Clone, PartialEq)]
pub struct Cte {
    pub name: String,
    pub columns: Vec<String>,
    pub query: SelectStmt,
    /// `AS MATERIALIZED` / `AS NOT MATERIALIZED` hint. None means no hint.
    pub materialized: Option<bool>,
}

/// A `WITH [RECURSIVE] …` clause: a list of CTEs and a recursion flag.
#[derive(Debug, Clone, PartialEq)]
pub struct WithClause {
    pub recursive: bool,
    pub ctes: Vec<Cte>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectStmt {
    pub distinct: bool,
    pub columns: Vec<ResultColumn>,
    /// `FROM` clause. A plain single-table/cross-join clause is a single `TableOrJoin::Table`
    /// element (or a comma-separated list of them). Explicit joins are represented as a
    /// left-associative tree through the `TableOrJoin::Join` variant.
    pub from: Vec<TableOrJoin>,
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
    /// Optional `WITH` clause attached to this SELECT statement.
    pub with_clause: Option<WithClause>,
    /// Optional trailing `WINDOW name AS (window_spec), ...` clause (M2.55). Codegen is M11.
    pub window_clause: Vec<NamedWindow>,
    /// When this select core is a `VALUES (expr_list) [, …]`, the rows of expressions are stored
    /// here and `columns`/`from` are empty. Each row has the same number of expressions.
    pub values: Vec<Vec<Expr>>,
}

/// A named window definition from the trailing `WINDOW` clause: `name AS (window_spec)`.
#[derive(Debug, Clone, PartialEq)]
pub struct NamedWindow {
    pub name: String,
    pub spec: Window,
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
    /// `INDEXED BY name` / `NOT INDEXED` table hint (M2.54). `None` means no hint.
    pub indexed_by: Option<IndexedBy>,
    /// Optional table-valued-function argument list `name(args)` (M2.68). `None` means a plain
    /// table reference, not a function call. Codegen is M31.
    pub args: Option<Vec<Expr>>,
}

/// The `INDEXED BY name` / `NOT INDEXED` table hint that may follow a table reference.
/// M2.54 covers the parser only; codegen is M27.6.
#[derive(Debug, Clone, PartialEq)]
pub enum IndexedBy {
    /// `INDEXED BY name` — force the use of the named index.
    Index(String),
    /// `NOT INDEXED` — forbid index usage (full table scan).
    NotIndexed,
}

/// A join operator connecting the left-hand table-or-join sequence to the next table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinOp {
    Inner,
    Cross,
    Natural,
    Left,
    LeftOuter,
    Right,
    RightOuter,
    Full,
    FullOuter,
}

impl TableOrJoin {
    /// If this node is a plain table reference, return it; otherwise `None`.
    pub fn table(&self) -> Option<&TableRef> {
        match self {
            TableOrJoin::Table(t) => Some(t),
            TableOrJoin::Subquery { .. } | TableOrJoin::Join(_) => None,
        }
    }

    /// If this node is a subquery, return it; otherwise `None`.
    pub fn subquery(&self) -> Option<(&SelectStmt, &str)> {
        match self {
            TableOrJoin::Subquery { query, alias } => Some((query, alias)),
            _ => None,
        }
    }
}

impl JoinOp {
    /// Combine join modifier keywords as they are parsed left-to-right, mirroring
    /// `sqlite3JoinType` in upstream `select.c`. Returns `Err` with the error keyword
    /// string if the combination is invalid (e.g. `INNER OUTER`, `OUTER` alone).
    pub fn from_keywords(keywords: &[&str]) -> std::result::Result<Self, String> {
        use JoinOp::*;
        let mut natural = false;
        let mut left = false;
        let mut right = false;
        let mut outer = false;
        let mut inner = false;
        let mut cross = false;
        for kw in keywords {
            match kw.to_ascii_lowercase().as_str() {
                "natural" => natural = true,
                "left" => left = true,
                "right" => right = true,
                "full" => {
                    left = true;
                    right = true;
                }
                "outer" => outer = true,
                "inner" => inner = true,
                "cross" => cross = true,
                _ => return Err((*kw).to_string()),
            }
        }

        // Invalid combinations per upstream sqlite3JoinType.
        let invalid = ((cross || inner) && outer)
            || (cross && (left || right))
            || (cross && natural && inner)
            || (outer && !(left || right))
            || (natural && left && right)
            || (inner && (left || right));
        if invalid {
            return Err(keywords.join(" "));
        }

        let op = if natural {
            Natural
        } else if left && right && outer {
            FullOuter
        } else if left && right {
            Full
        } else if left && outer {
            LeftOuter
        } else if left {
            Left
        } else if right && outer {
            RightOuter
        } else if right {
            Right
        } else if cross {
            Cross
        } else {
            Inner
        };
        Ok(op)
    }
}

/// The join constraint following a join operator: `ON expr` or `USING (cols)`.
#[derive(Debug, Clone, PartialEq)]
pub enum JoinConstraint {
    On(Expr),
    Using(Vec<String>),
}

/// A node in the `FROM` clause: either a plain table reference, a subquery (with required
/// alias), or a (possibly nested) join.
#[derive(Debug, Clone, PartialEq)]
pub enum TableOrJoin {
    Table(TableRef),
    Subquery {
        query: Box<SelectStmt>,
        alias: String,
    },
    Join(Join),
}

/// One link in a joined `FROM` clause. PEG naturally produces a left-associative parse, so
/// the left side can itself be a join chain (`(a JOIN b) JOIN c`). A plain `FROM t1, t2` is
/// modeled as an implicit `Inner` join with no constraint when cross-joined by a comma.
#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub op: JoinOp,
    pub left: Box<TableOrJoin>,
    pub right: TableRef,
    pub constraint: Option<JoinConstraint>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderingTerm {
    pub expr: Expr,
    /// `true` for DESC, `false` for ASC (default).
    pub desc: bool,
    /// `NULLS FIRST` / `NULLS LAST` (M2.56). `None` means the default NULL ordering (NULLS FIRST
    /// for ASC, NULLS LAST for DESC in SQLite). Codegen is M11.7.
    pub nulls: Option<NullsOrder>,
}

/// `NULLS FIRST` / `NULLS LAST` in ORDER BY (M2.56).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullsOrder {
    First,
    Last,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateTable {
    pub temporary: bool,
    pub if_not_exists: bool,
    pub schema: Option<String>,
    pub name: String,
    pub columns: Vec<ColumnDef>,
    /// Table-level constraints (PRIMARY KEY(cols), UNIQUE(cols), CHECK(expr), FOREIGN KEY).
    /// M2.44 adds parser support; codegen enforcement is M35/M53.
    pub constraints: Vec<TableConstraint>,
    /// `WITHOUT ROWID` flag (M2.48) — when true the table is an index-organized table with the
    /// primary key as the b-tree key, no rowid alias. Codegen is M5.3.6.
    pub without_rowid: bool,
    /// `STRICT` flag (M2.49) — when true the table enforces column type affinity strictly.
    /// Codegen is M33.
    pub strict: bool,
    /// `CREATE TABLE name AS SELECT ...` (CTAS, M2.59). When `Some`, the table's columns are
    /// derived from the SELECT's result columns and `columns`/`constraints` are empty.
    pub as_select: Option<SelectStmt>,
}

/// A table-level constraint (declared after the columns in CREATE TABLE). M2.44 covers the
/// parser only; codegen enforcement is M35/M53.
#[derive(Debug, Clone, PartialEq)]
pub struct TableConstraint {
    /// Optional constraint name (`CONSTRAINT name ...`).
    pub name: Option<String>,
    pub body: TableConstraintBody,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TableConstraintBody {
    /// `PRIMARY KEY (cols)` — composite primary key. The columns carry optional sort order.
    PrimaryKey { columns: Vec<PrimaryKeyColumn> },
    /// `UNIQUE (cols)`.
    Unique { columns: Vec<PrimaryKeyColumn> },
    /// `CHECK (expr)`.
    Check(Expr),
    /// `FOREIGN KEY (cols) REFERENCES parent [(parent_cols)] [refargs] [deferrable]`.
    ForeignKey {
        columns: Vec<String>,
        references: References,
    },
}

/// One column in a `PRIMARY KEY(cols)` / `UNIQUE(cols)` table constraint, with optional
/// sort order. (Upstream's `sortlist` also allows `COLLATE name`; that is deferred.)
#[derive(Debug, Clone, PartialEq)]
pub struct PrimaryKeyColumn {
    pub name: String,
    pub desc: bool,
}

/// The `REFERENCES parent [(cols)] [ON DELETE/UPDATE action] [deferrable]` clause of a
/// `FOREIGN KEY` constraint (also used on column-level `REFERENCES`).
#[derive(Debug, Clone, PartialEq)]
pub struct References {
    pub parent_table: String,
    /// Optional parent column list. `None` means "reference the parent's PK".
    pub parent_columns: Option<Vec<String>>,
    /// `ON DELETE action` (None means NO ACTION).
    pub on_delete: Option<ReferenceAction>,
    /// `ON UPDATE action` (None means NO ACTION).
    pub on_update: Option<ReferenceAction>,
    /// `DEFERRABLE [INITIALLY DEFERRED|IMMEDIATE]` / `NOT DEFERRABLE`.
    pub deferrable: Option<Deferrable>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceAction {
    SetNull,
    SetDefault,
    Cascade,
    Restrict,
    NoAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deferrable {
    /// `DEFERRABLE INITIALLY DEFERRED`.
    DeferrableInitiallyDeferred,
    /// `DEFERRABLE INITIALLY IMMEDIATE` (or just `DEFERRABLE`).
    DeferrableInitiallyImmediate,
    /// `NOT DEFERRABLE`.
    NotDeferrable,
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
    /// Column-level `REFERENCES parent [(cols)] [refargs] [deferrable]`. M2.44 parses this;
    /// enforcement is M17 (foreign keys).
    References(References),
    /// `GENERATED ALWAYS AS (expr) [STORED|VIRTUAL]` / `AS (expr) [STORED|VIRTUAL]`.
    /// M2.50 parses this; codegen is M34.
    Generated {
        expr: Expr,
        /// `true` for STORED, `false` for VIRTUAL (the default when neither keyword is present).
        stored: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct InsertStmt {
    pub or_action: Option<ConflictAction>,
    pub schema: Option<String>,
    pub table: String,
    pub columns: Vec<String>,
    /// The source data for the insert. For `INSERT ... VALUES` this carries the literal rows;
    /// for `INSERT ... SELECT` it carries the select body.
    pub source: InsertSource,
    /// Optional UPSERT clause(s) at the end of the INSERT.
    pub upsert: Vec<UpsertClause>,
    /// Optional `RETURNING` clause: expressions to evaluate and yield per inserted row.
    pub returning: Option<Vec<ResultColumn>>,
}

/// Data source for an `INSERT` statement.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum InsertSource {
    /// `VALUES (expr_list) [, ...]` — a list of literal/constant rows.
    Values(Vec<Vec<Expr>>),
    /// `SELECT ...` — a query whose result rows become the inserted rows.
    Select(SelectStmt),
    /// `DEFAULT VALUES` — insert a single row using each column's default value (or NULL).
    DefaultValues,
}

/// `DELETE FROM [schema.]tbl [WHERE expr] [ORDER BY ...] [LIMIT n [OFFSET m]] [RETURNING ...]`.
/// `RETURNING` is added in M2.24; `ORDER BY`/`LIMIT`/`OFFSET` in M2.51.
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStmt {
    pub schema: Option<String>,
    pub table: String,
    pub where_clause: Option<Expr>,
    /// `ORDER BY ...` — M2.51.
    pub order_by: Vec<OrderingTerm>,
    /// `LIMIT n` — M2.51.
    pub limit: Option<Expr>,
    /// `OFFSET m` — M2.51.
    pub offset: Option<Expr>,
    /// Optional `RETURNING` clause: expressions to evaluate and yield per deleted row.
    pub returning: Option<Vec<ResultColumn>>,
}

/// `DROP TABLE [IF EXISTS] [schema.]tbl`. The first M4.6 slice omits `DROP INDEX/VIEW/TRIGGER`.
#[derive(Debug, Clone, PartialEq)]
pub struct DropTableStmt {
    pub if_exists: bool,
    pub schema: Option<String>,
    pub name: String,
}

/// `UPDATE [or_action] [schema.]tbl SET col = expr [, col = expr ...] [FROM from_clause]
/// [WHERE expr] [ORDER BY ...] [LIMIT n [OFFSET m]] [RETURNING ...]`.
/// `RETURNING` is added in M2.24; `ORDER BY`/`LIMIT`/`OFFSET` in M2.52; `FROM` in M2.53.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateStmt {
    pub or_action: Option<ConflictAction>,
    pub schema: Option<String>,
    pub table: String,
    pub assignments: Vec<Assignment>,
    /// Optional `FROM from_clause` (SQLite 3.33+) — M2.53.
    pub from: Vec<TableOrJoin>,
    pub where_clause: Option<Expr>,
    /// `ORDER BY ...` — M2.52.
    pub order_by: Vec<OrderingTerm>,
    /// `LIMIT n` — M2.52.
    pub limit: Option<Expr>,
    /// `OFFSET m` — M2.52.
    pub offset: Option<Expr>,
    /// Optional `RETURNING` clause: expressions to evaluate and yield per updated row.
    pub returning: Option<Vec<ResultColumn>>,
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

/// A single `ON CONFLICT ... DO ...` clause from an UPSERT.
#[derive(Debug, Clone, PartialEq)]
pub struct UpsertClause {
    /// Optional conflict target: list of indexed columns/expressions, plus an
    /// optional WHERE predicate that narrows which unique index is selected.
    pub target: Option<UpsertTarget>,
    /// The action to take on conflict.
    pub action: UpsertAction,
}

/// Target of an UPSERT clause: the `(a, b, ...)` list plus optional `WHERE idx_predicate`.
#[derive(Debug, Clone, PartialEq)]
pub struct UpsertTarget {
    pub columns: Vec<UpsertTargetColumn>,
    pub where_clause: Option<Expr>,
}

/// A column (or expression) named in the UPSERT conflict target. Bare identifiers
/// carry optional collation and sort order to mirror upstream's `sortlist` form.
#[derive(Debug, Clone, PartialEq)]
pub enum UpsertTargetColumn {
    Column { name: String, collation: Option<String>, desc: bool },
    Expr(Expr),
}

/// Action side of an UPSERT clause.
#[derive(Debug, Clone, PartialEq)]
pub enum UpsertAction {
    /// `DO NOTHING`.
    Nothing,
    /// `DO UPDATE SET ... [WHERE ...]`.
    Update {
        assignments: Vec<Assignment>,
        where_clause: Option<Expr>,
    },
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
        /// `FILTER (WHERE expr)` on an aggregate/window function call (M2.55). None when absent.
        filter: Option<Box<Expr>>,
        /// `OVER (window_spec)` / `OVER name` on a window function call (M2.55). None when absent.
        over: Option<Window>,
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
    /// `X [NOT] IN (SELECT …)` — the right-hand side is a subquery rather than a literal value
    /// list. Kept separate from [`Expr::In`] so the codegen path can distinguish a row-set
    /// membership test (materialise the subquery into an ephemeral index) from a small inline
    /// value list. Parsed in M2.60 (row-value expressions) along with the row-value `IN` form.
    InSubquery {
        expr: Box<Expr>,
        subquery: Box<SelectStmt>,
        negated: bool,
    },
    Exists(Box<SelectStmt>),
    /// A scalar subquery used as an expression: `(SELECT …)`.  Evaluates to the first column of
    /// the first row (NULL if the subquery returns no rows).  Parsed here; execution is deferred.
    Subquery(Box<SelectStmt>),
    /// A row value `(e0, e1, …)` with two or more elements, matching upstream's `TK_VECTOR`
    /// node. A parenthesised single expression `(e)` is *not* a row value — it collapses to `e`
    /// itself (see upstream's `LP nexprlist COMMA expr RP` rule, which requires at least one
    /// comma). Row values support comparison (`(a,b) = (1,2)`, with element-wise lexicographic
    /// semantics) and `IN` against a row-set (`(a,b) IN ((1,2),(3,4))` or `(a,b) IN (SELECT …)`).
    /// Execution is deferred; the parser builds the AST node here.
    Row(Vec<Expr>),
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
    /// A codegen-only synthetic reference to an aggregate accumulator's result register,
    /// emitted by the aggregate codegen path after it assigns each aggregate call a register.
    /// Never produced by the parser; used to rewrite projection/HAVING expressions so a
    /// `count(*)` in `SELECT g, count(*) FROM t GROUP BY g` reads from the accumulator register
    /// instead of trying to evaluate the function call during the per-group output pass.
    AggRef(i32),
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
    /// `X REGEXP Y` (M2.61) — calls the user-registered `regexp` function as `regexp(Y, X)`.
    Regexp,
    /// `X MATCH Y` (M2.62) — used by FTS; calls the user-registered `match` function.
    Match,
    BitAnd,
    BitOr,
    ShiftLeft,
    ShiftRight,
    /// `->` — JSON extraction returning a JSON representation.
    JsonExtract,
    /// `->>` — JSON extraction returning a SQL text/numeric value.
    JsonExtractText,
}

/// A window specification for `OVER (...)` / `OVER name` (M2.55). Codegen is M11.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    /// Optional named-window reference (`OVER name`). When `Some`, this references a window
    /// defined in the trailing `WINDOW` clause; the other fields are empty.
    pub name: Option<String>,
    /// `PARTITION BY expr, ...`.
    pub partition_by: Vec<Expr>,
    /// `ORDER BY ...`.
    pub order_by: Vec<OrderingTerm>,
    /// Frame specification (`ROWS/RANGE/GROUPS BETWEEN ... AND ...`). `None` means the default
    /// frame (`RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW`).
    pub frame: Option<Frame>,
}

/// A window frame specification (M2.55 / M11.8).
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    /// `ROWS` / `RANGE` / `GROUPS`.
    pub mode: FrameMode,
    /// The bounds. `BETWEEN start AND end` when `end` is `Some`; a single bound otherwise.
    pub start: FrameBound,
    pub end: Option<FrameBound>,
    /// `EXCLUDE NO OTHERS` / `CURRENT ROW` / `GROUP` / `TIES`. `None` means `NO OTHERS` (default).
    pub exclude: Option<FrameExclude>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameMode {
    Rows,
    Range,
    Groups,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FrameBound {
    /// `UNBOUNDED PRECEDING` / `UNBOUNDED FOLLOWING`.
    UnboundedPreceding,
    UnboundedFollowing,
    /// `CURRENT ROW`.
    CurrentRow,
    /// `expr PRECEDING` / `expr FOLLOWING`.
    Preceding(Box<Expr>),
    Following(Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameExclude {
    /// `NO OTHERS` (the default).
    NoOthers,
    /// `CURRENT ROW`.
    CurrentRow,
    /// `GROUP`.
    Group,
    /// `TIES`.
    Ties,
}
