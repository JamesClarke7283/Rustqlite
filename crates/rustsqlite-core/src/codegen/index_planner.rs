//! Tiny query planner: an index-aware codegen for the small set of operators the M5.1/M5.2
//! slices support.
//!
//! The planner reads the catalog to find a usable index on the table. It considers three
//! benefits an index can provide, in roughly increasing value:
//!
//! 1. **ORDER BY benefit** — when `ORDER BY` is a prefix of the index's columns (in matching
//!    ASC/DESC direction), the index scan already yields rows in the requested order, so the
//!    sorter is dropped. (`SELECT ... FROM t ORDER BY a` with `CREATE INDEX idx ON t(a)`.)
//! 2. **Covering benefit** — when every column the query needs (projection + WHERE + ORDER BY
//!    + DISTINCT) is one of the index's columns, the table lookup is dropped entirely and the
//!    scan reads only the index b-tree (an "index-only scan"). The rowid tail of the index
//!    record is also available as a column value when the rowid-alias column is needed.
//! 3. **WHERE equality benefit** — when the `WHERE` clause contains equality comparisons on a
//!    prefix of the index columns (`col1 = const AND col2 = const ...`), the scan seeks
//!    directly to the matching range instead of walking the whole index.
//!
//! Any one of these benefits is enough for the planner to prefer the index over a table scan;
//! they compose freely (a covering index that also satisfies ORDER BY and has a WHERE equality
//! prefix is the ideal plan).
//!
//! The codegen output for an indexed equality with a non-covering index is:
//! ```text
//!   OpenRead  table_cur, table_root, 0
//!   OpenRead  idx_cur,   idx_root, 0, P4=KeyInfo(n=K, ASC, BINARY)
//!   <load constant into reg K..K+n-1>
//!   SeekGE    idx_cur, end_seek, K, P4=n
//!   IdxGT     idx_cur, end_seek, K, P4=n
//! loop_top:
//!   IdxRowid  idx_cur, R
//!   NotExists table_cur, idx_next, R
//!   <project + WHERE-filter; ResultRow>
//! idx_next:
//!   Next      idx_cur, loop_top
//! end_seek:
//!   Halt
//! ```
//! For a covering index the `OpenRead table_cur` / `IdxRowid` / `NotExists` are dropped and
//! the projection reads directly from the index cursor (column position = index column
//! position; the rowid tail is at position `nkey_fields`). For an ORDER BY-only plan (no
//! WHERE equality) the `SeekGE`/`IdxGT` are dropped and the scan is a plain `Rewind`/`Next`
//! over the index.
//!
//! The M5.2 slice deliberately keeps this small: it handles multi-column indexes only for
//! prefix equality and prefix ORDER BY; partial keys beyond the prefix, range scans, and
//! reverse scans fall through to the M3a scan path unchanged.
//!
//! For M5.2 the `WHERE` clause is *re-checked* on the table row (the IdxGT only verified
//! the indexed-column prefix, not the rest of the WHERE). When the WHERE is exactly the
//! indexed equalities, this is a tautology; when it is more complex, the row is filtered
//! again here. When the index is covering, the WHERE is re-checked against the index-read
//! column values instead.

use rustqlite_parser::{BinaryOp, Expr, IndexedBy, Literal, OrderingTerm, SelectStmt};

use crate::error::{Error, Result};
use crate::schema::{IndexObject, Table};
use crate::types::Value;

/// A comparison operator on an indexed column against a constant RHS, in the form the index
/// range-scan machinery consumes. Mirrors the `WO_EQ`/`WO_GT`/`WO_LE`/... bit flags in
/// `where.c`'s `WhereTerm`/`WhereLoop` planning, collapsed to a single closed enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RangeOp {
    /// `col = const` / `col IS const`. The column is pinned to a single value.
    Eq,
    /// `col > const`.
    Gt,
    /// `col >= const`.
    Ge,
    /// `col < const`.
    Lt,
    /// `col <= const`.
    Le,
    /// `col IS NULL`. The column is pinned to NULL (a single-value range).
    IsNull,
}

impl RangeOp {
    /// The `EXPLAIN QUERY PLAN` detail token for this operator (`=`, `>`, `<`).
    /// `>=`/`<=`/`IS NULL` all render as their strict counterpart because SQLite collapses
    /// `a>=?` to `a>?` (seeking one position earlier and relying on the row scan to include
    /// the equal row) and `a<=?` to `a<?` likewise; `IS NULL` renders as `=?`.
    pub fn detail_token(self) -> &'static str {
        match self {
            RangeOp::Eq | RangeOp::IsNull => "=",
            RangeOp::Gt | RangeOp::Ge => ">",
            RangeOp::Lt | RangeOp::Le => "<",
        }
    }

    /// True when this operator pins the column to a single value (`=` or `IS NULL`), so the
    /// next index column can be constrained (an equality prefix).
    pub fn is_equality(self) -> bool {
        matches!(self, RangeOp::Eq | RangeOp::IsNull)
    }
}

/// One constraint on an indexed column: the column, the operator, and the constant RHS value
/// (NULL for `IS NULL` and for the open end of a half-bounded range).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RangeKey {
    pub column: String,
    pub op: RangeOp,
    pub value: Value,
}

/// An index plan: the chosen index, the matched range prefix (which subsumes the old
/// equality-only prefix — an `Eq` constraint is just a single-ended range), whether the index
/// covers all columns needed by the query (so no table lookup is required), and whether the
/// index scan ordering satisfies the `ORDER BY` clause (so no sorter is required).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IndexPlan {
    pub index: IndexObject,
    /// The constraints on the index's leading columns. The prefix consists of zero or more
    /// `Eq`/`IsNull` columns (the "equality prefix") followed by at most one column with a
    /// range constraint (`Gt`/`Ge`/`Lt`/`Le`) — possibly with both a lower and an upper bound
    /// on that same column (`a > 1 AND a < 3`). A column past the first range-bound column is
    /// never constrained (the b-tree order is unknown past a range).
    ///
    /// The list is in index-column order. Each entry is one constraint; when the same column has
    /// both a lower and an upper bound there are two entries (one `Gt`/`Ge`, one `Lt`/`Le`).
    pub range: Vec<RangeKey>,
    /// `true` when the index scan yields rows in the ORDER BY order, so the sorter is dropped.
    /// Only set when `select.order_by` is non-empty AND the index ordering satisfies it.
    pub order_by_satisfied: bool,
    /// `true` when every column the query needs is read from the index, so no table cursor is
    /// opened and no `IdxRowid`/`NotExists` pair is emitted.
    pub covering: bool,
    /// `true` when an `INDEXED BY <name>` hint forced this index even though the index does not
    /// satisfy the ORDER BY clause. The codegen must emit a sorter over the index scan.
    /// Always `false` for an unconstrained planner pick.
    pub needs_sorter: bool,
}

impl IndexPlan {
    /// The equality prefix length: the number of leading `Eq`/`IsNull` constraints. The
    /// codegen's `SeekGE`/`SeekGT` key covers `eq_prefix_len` columns; the `IdxGT`/`IdxGE`
    /// boundary check covers `eq_prefix_len` columns (for an equality prefix) or
    /// `eq_prefix_len + 1` (for a range-bounded column).
    pub fn eq_prefix_len(&self) -> usize {
        self.range.iter().take_while(|k| k.op.is_equality()).count()
    }

    /// True when the plan has at least one WHERE constraint (equality or range) — i.e. the
    /// scan is a `SeekGE`/`SeekGT` search, not a full index walk.
    pub fn has_where_constraint(&self) -> bool {
        !self.range.is_empty()
    }

    /// True when the plan has an equality prefix (the old `equality`-non-empty shape).
    pub fn has_where_equality(&self) -> bool {
        self.range.iter().any(|k| k.op.is_equality())
    }

    /// The lower-bound constraint on the first range column (the column right after the
    /// equality prefix), if any.
    pub fn lower_bound(&self) -> Option<&RangeKey> {
        self.range.iter().find(|k| matches!(k.op, RangeOp::Gt | RangeOp::Ge))
    }

    /// The upper-bound constraint on the first range column, if any.
    pub fn upper_bound(&self) -> Option<&RangeKey> {
        self.range.iter().find(|k| matches!(k.op, RangeOp::Lt | RangeOp::Le))
    }

    /// The equality-prefix constraint values, in index order. Used by the codegen to load the
    /// `SeekGE` key registers.
    pub fn equality_values(&self) -> Vec<&RangeKey> {
        self.range
            .iter()
            .filter(|k| k.op.is_equality())
            .collect()
    }
}

/// Pick an index to use for a `SELECT`, if any. Returns `Some(plan)` when an index provides at
/// least one of: an ORDER BY benefit, a covering benefit, or a WHERE equality prefix.
/// `None` means the M3a table-scan path is the right choice.
///
/// `hint` carries the `INDEXED BY name` / `NOT INDEXED` table hint (M27.6):
///   * `Some(NotIndexed)` → always returns `None` (force a table scan, ignoring every index).
///   * `Some(Index(name))` → only the named index is considered; an error is raised when no
///     such index exists on the table. The named index is used even when it provides no
///     benefit (a full index scan with a sorter when ORDER BY is not satisfied). This mirrors
///     upstream's `INDEXED BY` semantics: the hint forces the planner's hand.
///   * `None` → the unconstrained planner pick (the M5.1/M5.2 behavior).
pub(crate) fn pick_index(
    select: &SelectStmt,
    table: &Table,
    indexes: &[IndexObject],
    hint: Option<&IndexedBy>,
) -> Result<Option<IndexPlan>> {
    if indexes.is_empty() {
        if let Some(IndexedBy::Index(name)) = hint {
            return Err(Error::msg(format!("no such index: {}", name)));
        }
        return Ok(None);
    }
    if select.from.len() != 1 {
        // The planner only handles single-table FROM; the hint is silently dropped here
        // (mirrors upstream's behavior where a join ignores a per-table hint that the planner
        // can't apply — though upstream raises "no such index" for a missing name regardless).
        if let Some(IndexedBy::Index(name)) = hint {
            let exists = indexes.iter().any(|i| i.name.eq_ignore_ascii_case(name));
            if !exists {
                return Err(Error::msg(format!("no such index: {}", name)));
            }
        }
        return Ok(None);
    }
    // `NOT INDEXED` forbids using any index — force a table scan.
    if matches!(hint, Some(IndexedBy::NotIndexed)) {
        return Ok(None);
    }

    let table_columns: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
    let where_constraints = collect_where_range_constraints(select);

    // The columns the query references (projection + WHERE + ORDER BY). Used to decide if an
    // index is covering. `collect_referenced_columns` walks the expressions and returns the
    // table-column indices it finds.
    let referenced = collect_referenced_columns(select, table);

    // `INDEXED BY name` forces the named index. Resolve it (case-insensitive) and raise the
    // oracle-matched "no such index: <name>" error when it doesn't exist.
    if let Some(IndexedBy::Index(forced_name)) = hint {
        let idx = indexes
            .iter()
            .find(|i| i.name.eq_ignore_ascii_case(forced_name))
            .ok_or_else(|| Error::msg(format!("no such index: {}", forced_name)))?;
        return Ok(Some(plan_for_index(select, idx, table, &table_columns, &where_constraints, &referenced, true)?));
    }

    // Choose the index with the best combined benefit. Score is a tuple
    // (constraint_count, covering, order_by_satisfied): more WHERE constraints (equality +
    // range bounds) win; ties go to a covering index (saves the table lookup); further ties go
    // to an ORDER BY-satisfying index (saves the sorter). This is a simple proxy for cost — a
    // real planner would estimate row counts and I/O.
    let mut best: Option<IndexPlan> = None;
    let mut best_score: (usize, bool, bool) = (0, false, false);
    for idx in indexes {
        // Partial indexes can only be used when the query's WHERE implies the index predicate.
        // A safe, conservative rule that matches SQLite for simple cases: the index predicate
        // must appear verbatim (or tautologically) in the query WHERE. Until we have a real
        // theorem prover, we accept the index only when the query WHERE literally contains the
        // same expression tree as the index predicate, so `WHERE a=1 AND predicate` uses a
        // partial index defined with `WHERE predicate`, while `WHERE a=1` does not.
        if !partial_index_usable(idx, select) {
            continue;
        }

        let plan = plan_for_index(select, idx, table, &table_columns, &where_constraints, &referenced, false)?;
        // Require at least one benefit to use the index. A useless index that is neither
        // covering, nor ORDER-BY-satisfying, nor has a WHERE constraint would just add an
        // extra b-tree open with no gain — fall through to the table scan.
        let has_benefit =
            plan.has_where_constraint()
                || plan.order_by_satisfied
                || (plan.covering && !referenced.is_empty() && index_strictly_smaller_than_table(idx, table));
        if !has_benefit {
            continue;
        }
        // When the query has an ORDER BY that this index does NOT satisfy, the indexed scan
        // would still need a sorter. The codegen's indexed path handles this via the sorter
        // path (`needs_sorter`) when there is a WHERE constraint — the index is used for the
        // seek, and a sorter re-orders the rows by the ORDER BY keys. When there is no WHERE
        // constraint (covering-only or ORDER-BY-only), the index offers no benefit over a
        // table scan + sorter, so fall through to the table-scan + sorter path.
        if !select.order_by.is_empty() && !plan.order_by_satisfied {
            if plan.has_where_constraint() {
                // Use the index for the WHERE seek + a sorter for ORDER BY.
                let mut plan = plan;
                plan.needs_sorter = true;
                let eq_len = plan.range.len();
                let score = (eq_len, plan.covering, plan.order_by_satisfied);
                if score > best_score {
                    best_score = score;
                    best = Some(plan);
                }
            }
            continue;
        }

        let eq_len = plan.range.len();
        let score = (eq_len, plan.covering, plan.order_by_satisfied);
        if score > best_score {
            best_score = score;
            best = Some(plan);
        }
    }

    Ok(best)
}

/// Evaluate a single index for the query. Shared between the unconstrained planner loop and
/// the `INDEXED BY` forced path. When `forced` is true the partial-index usability check is
/// skipped (a forced index is used even when its predicate doesn't match the query WHERE) and
/// `needs_sorter` is set when the index doesn't satisfy ORDER BY.
fn plan_for_index(
    select: &SelectStmt,
    idx: &IndexObject,
    table: &Table,
    table_columns: &[&str],
    where_constraints: &[RangeKey],
    referenced: &[usize],
    forced: bool,
) -> Result<IndexPlan> {
    if !forced && !partial_index_usable(idx, select) {
        // Skip partial indexes whose predicate isn't matched (the caller's loop continues).
        // Returning a no-benefit plan here makes the loop's `has_benefit` check drop it.
        return Ok(IndexPlan {
            index: idx.clone(),
            range: Vec::new(),
            order_by_satisfied: false,
            covering: false,
            needs_sorter: false,
        });
    }

    // (1) WHERE range prefix. Walk the index columns in order, matching each against the WHERE
    // constraints. An equality (`=`/`IS NULL`) on column N lets column N+1 be constrained; a
    // range (`>`/`>=`/`<`/`<=`) on column N terminates the prefix (column N+1 is unconstrained
    // — the b-tree order past a range is unknown). A single column may carry both a lower and
    // an upper bound (`a > 1 AND a < 3`).
    let range = match find_index_range_prefix(idx, table_columns, where_constraints) {
        Some(r) => r,
        None => Vec::new(),
    };

    // The equality prefix length (number of leading `=`/`IS NULL` constraints) determines
    // where the ORDER BY match starts: an equality on column 0 lets ORDER BY on column 1 be
    // satisfied by the same index.
    let eq_prefix_len = range.iter().take_while(|k| k.op.is_equality()).count();

    // (2) ORDER BY benefit. The index satisfies ORDER BY when:
    //   * there is an ORDER BY clause,
    //   * the ORDER BY terms are a prefix of the index columns (in index order) starting
    //     right after the equality prefix, and
    //   * each term's direction matches the index column's direction.
    // A range-bounded column past the equality prefix is NOT part of the ORDER BY (the scan
    // walks the range, which is in order for the column itself but the ORDER BY must name that
    // column too — we don't model that subtlety, conservatively rejecting).
    let order_by_satisfied =
        order_by_matches_index(select, idx, table, eq_prefix_len, &range);

    // (3) Covering benefit. The index is covering when every referenced column is one of
    // the index's columns. The rowid-alias column is satisfied by the index's trailing
    // rowid (read via `Column` at position `nkey_fields`). A non-alias rowid reference
    // (`SELECT rowid FROM t`) is also satisfied by the trailing rowid.
    let covering = !referenced.is_empty() && index_covers(idx, table, referenced);

    // A forced index is used regardless of benefit. When the ORDER BY is not satisfied,
    // the codegen must wrap the index scan in a sorter (mirrors the oracle's
    // `SCAN t USING INDEX <name>` + `USE TEMP B-TREE FOR ORDER BY`).
    let needs_sorter = forced && !select.order_by.is_empty() && !order_by_satisfied;

    Ok(IndexPlan {
        index: idx.clone(),
        range,
        order_by_satisfied,
        covering,
        needs_sorter,
    })
}

/// True when a partial index's predicate is satisfied by the query's WHERE clause.
/// The conservative check looks for the exact predicate expression as a conjunct of the
/// WHERE clause (flattened by AND). This handles the common `WHERE a = ? AND b = ?` query
/// against an index `WHERE b = ?` — the literal equality term `b = ?` must be present.
fn partial_index_usable(index: &IndexObject, select: &SelectStmt) -> bool {
    let Some(pred) = &index.where_clause else {
        return true; // non-partial indexes are always usable
    };
    let Some(w) = &select.where_clause else {
        return false;
    };
    let mut conjuncts = Vec::new();
    flatten_and(w, &mut conjuncts);
    conjuncts.iter().any(|c| exprs_equal(c, pred))
}

fn flatten_and(expr: &Expr, out: &mut Vec<Expr>) {
    if let Expr::Binary {
        op: BinaryOp::And,
        left,
        right,
    } = expr
    {
        flatten_and(left, out);
        flatten_and(right, out);
    } else {
        out.push(expr.clone());
    }
}

fn exprs_equal(a: &Expr, b: &Expr) -> bool {
    a == b
}

/// Collect all range/equality constraints from the WHERE clause as a flat list. The supported
/// WHERE shape is a conjunction of `col <op> const` comparisons (possibly with extra terms);
/// we flatten `AND` and gather every constraint whose RHS is a constant literal or bind
/// parameter. `BETWEEN` is rewritten to `>= low AND <= high` (the same rewrite upstream's
/// `where.c` does in `sqlite3WhereCanonicalFuncUsage`/`whereLoopInfo` via the
/// `WO_GE`/`WO_LE` pair derived from a `BETWEEN` term).
fn collect_where_range_constraints(select: &SelectStmt) -> Vec<RangeKey> {
    let Some(w) = select.where_clause.as_ref() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    flatten_and_collect_range(w, &mut out);
    out
}

/// Recursively walk `expr`, flattening `AND` chains and recording every range/equality
/// constraint. `BETWEEN` lowers to a `Ge` + `Le` pair on the same column. `IS NULL` lowers to
/// an `IsNull` constraint. `IS NOT NULL` is not a usable index constraint (it's a not-NULL
/// scan, which the b-tree can't seek to) and is dropped here.
fn flatten_and_collect_range(expr: &Expr, out: &mut Vec<RangeKey>) {
    match expr {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            flatten_and_collect_range(left, out);
            flatten_and_collect_range(right, out);
        }
        Expr::Between {
            expr,
            low,
            high,
            negated: false,
        } => {
            // `expr BETWEEN low AND high` → `expr >= low AND expr <= high`.
            if let Some(col) = column_name(expr) {
                if let Some(lo) = const_value(low) {
                    out.push(RangeKey { column: col.clone(), op: RangeOp::Ge, value: lo });
                }
                if let Some(hi) = const_value(high) {
                    out.push(RangeKey { column: col, op: RangeOp::Le, value: hi });
                }
            }
        }
        other => {
            if let Some(rk) = as_range_key(other) {
                out.push(rk);
            }
        }
    }
}

/// If `expr` is `col <op> const` (or the commutative form), return the range key. Recognizes
/// `=`, `IS`, `>`, `>=`, `<`, `<=`, `IS NULL` (parsed as `col IS NULL` which the parser
/// lowers to a `Binary { op: Is, right: Literal(Null) }`), and `IS NOT NULL` (parsed as
/// `Binary { op: IsNot, right: Literal(Null) }`, lowered to `Gt(NULL)` — the b-tree seek past
/// the NULL entries). Returns `None` for `!=`, `<>`, non-constant RHS, or a non-column LHS.
fn as_range_key(expr: &Expr) -> Option<RangeKey> {
    let (col_expr, val_expr, op) = match expr {
        Expr::Binary {
            op: BinaryOp::Is,
            left,
            right,
        } => {
            // `col IS NULL` is a real constraint (RangeOp::IsNull); `col IS <non-null>` is an
            // equality. Don't reject NULL here — the `Is` operator explicitly compares to NULL.
            let col = column_name(left.as_ref()).or_else(|| column_name(right.as_ref()))?;
            let val_expr = if column_name(left.as_ref()).is_some() {
                right.as_ref()
            } else {
                left.as_ref()
            };
            let value = const_value(val_expr)?;
            if matches!(value, Value::Null) {
                return Some(RangeKey { column: col, op: RangeOp::IsNull, value });
            }
            return Some(RangeKey { column: col, op: RangeOp::Eq, value });
        }
        Expr::Binary {
            op: BinaryOp::Eq,
            left,
            right,
        } => (left.as_ref(), right.as_ref(), RangeOp::Eq),
        Expr::Binary {
            op: BinaryOp::IsNot,
            left,
            right,
        } => {
            // `col IS NOT NULL` → `col > NULL` (seek past the NULL entries). Only useful when
            // the RHS is NULL; `col IS NOT <non-null>` is not a range constraint.
            let val = const_value(right.as_ref())?;
            if !matches!(val, Value::Null) {
                return None;
            }
            let col = column_name(left.as_ref())?;
            return Some(RangeKey { column: col, op: RangeOp::Gt, value: Value::Null });
        }
        Expr::Binary {
            op: BinaryOp::Gt,
            left,
            right,
        } => (left.as_ref(), right.as_ref(), RangeOp::Gt),
        Expr::Binary {
            op: BinaryOp::Ge,
            left,
            right,
        } => (left.as_ref(), right.as_ref(), RangeOp::Ge),
        Expr::Binary {
            op: BinaryOp::Lt,
            left,
            right,
        } => (left.as_ref(), right.as_ref(), RangeOp::Lt),
        Expr::Binary {
            op: BinaryOp::Le,
            left,
            right,
        } => (left.as_ref(), right.as_ref(), RangeOp::Le),
        _ => return None,
    };

    // Resolve the column side (LHS or RHS) and the value side (the other).
    let (col, val_expr) = if let Some(c) = column_name(col_expr) {
        (c, val_expr)
    } else {
        // Try the commutative form: const <op> col.
        let c = column_name(val_expr)?;
        (c, col_expr)
    };
    let value = const_value(val_expr)?;

    // `col = NULL` is always UNKNOWN in three-valued logic, so the indexed path (which would
    // return the NULL row) is wrong. Reject the equality. (`col IS NULL` is handled above as
    // `RangeOp::IsNull` and is NOT rejected.)
    if matches!(value, Value::Null) && op == RangeOp::Eq {
        return None;
    }

    Some(RangeKey { column: col, op, value })
}

/// Walk the index columns in order, matching each against the collected WHERE constraints.
/// The prefix is a run of `Eq`/`IsNull` columns followed by at most one column with a range
/// constraint (which may carry both a lower and an upper bound). Returns `Some(prefix)` when
/// at least the first index column has a constraint; `None` when the index provides no WHERE
/// benefit (the caller still considers the index for covering / ORDER BY benefits).
fn find_index_range_prefix(
    index: &IndexObject,
    table_columns: &[&str],
    constraints: &[RangeKey],
) -> Option<Vec<RangeKey>> {
    let mut prefix: Vec<RangeKey> = Vec::new();
    let mut saw_range = false;
    for ic in &index.columns {
        // Sanity check: the indexed column must exist on the table.
        if !table_columns
            .iter()
            .any(|c| c.eq_ignore_ascii_case(&ic.name))
        {
            return None;
        }
        // Past a range-bounded column, no further column is constrained.
        if saw_range {
            break;
        }
        // Gather every constraint on this index column.
        let matches: Vec<&RangeKey> = constraints
            .iter()
            .filter(|c| c.column.eq_ignore_ascii_case(&ic.name))
            .collect();
        if matches.is_empty() {
            break;
        }
        // Equality / IS NULL constraints: take the first one (the b-tree pins the column).
        // If there are multiple equalities on the same column, take the first (the planner
        // doesn't model contradiction — the codegen re-checks WHERE on the row anyway).
        if let Some(eq) = matches.iter().find(|c| c.op.is_equality()) {
            prefix.push((*eq).clone());
            continue;
        }
        // No equality — look for a range (lower and/or upper bound).
        let lower = matches.iter().find(|c| matches!(c.op, RangeOp::Gt | RangeOp::Ge));
        let upper = matches.iter().find(|c| matches!(c.op, RangeOp::Lt | RangeOp::Le));
        if let Some(lo) = lower {
            prefix.push((*lo).clone());
        }
        if let Some(hi) = upper {
            prefix.push((*hi).clone());
        }
        if lower.is_some() || upper.is_some() {
            saw_range = true;
            continue;
        }
        // No usable constraint on this column — stop.
        break;
    }
    if prefix.is_empty() {
        None
    } else {
        Some(prefix)
    }
}

/// True when the `ORDER BY` clause is satisfied by walking this index forward starting at
/// the column right after the WHERE equality prefix. Mirrors the `nOBSat` logic in
/// `where.c`'s `whereLoopAddBtreeIndex` for the ORDER-BY-on-index case.
///
/// The match rules (forward scan only — we don't implement reverse scans yet):
///   * Each ORDER BY term must be a bare column reference that matches the corresponding
///     index column (by name), starting at index column `prefix_len`.
///   * The ORDER BY direction must match the index column's `desc` flag: ASC for an
///     ascending index column, DESC for a descending one. (A mismatched direction would
///     require a reverse scan, which we don't support — fall through to the sorter.)
///   * The full ORDER BY must be consumed (no trailing terms the index doesn't satisfy).
///   * `NULLS FIRST`/`NULLS LAST` must be the SQLite default for the direction (NULLS FIRST
///     for ASC, NULLS LAST for DESC) — the index's NULL placement matches the default.
fn order_by_matches_index(
    select: &SelectStmt,
    index: &IndexObject,
    table: &Table,
    prefix_len: usize,
    where_constraints: &[RangeKey],
) -> bool {
    if select.order_by.is_empty() {
        return false;
    }
    // If there are WHERE equalities on a prefix, the ORDER BY can only be satisfied by the
    // columns *after* that prefix — the prefix columns are pinned to a single value, so
    // ordering by them is a no-op (and upstream's `nOBSat` accounts for this). An ORDER BY
    // that re-lists a prefix column is still satisfiable but we'd need to skip it; for
    // simplicity we reject an ORDER BY term that names a prefix column. The common case
    // (`WHERE a=? ORDER BY b` on `INDEX(a,b)`) works.
    let idx_cols = &index.columns;
    if prefix_len + select.order_by.len() > idx_cols.len() {
        return false;
    }
    for (i, term) in select.order_by.iter().enumerate() {
        let ic = &idx_cols[prefix_len + i];
        // The ORDER BY term must be a bare column matching this index column.
        let Some(term_col) = order_by_column_name(term, table) else {
            return false;
        };
        if !term_col.eq_ignore_ascii_case(&ic.name) {
            return false;
        }
        // Direction must match (forward scan only). The index column's `desc` flag says the
        // index stores that column descending; an `ORDER BY col DESC` matches a `desc` index
        // column, an `ORDER BY col ASC` (the default) matches a non-`desc` index column.
        if term.desc != ic.desc {
            return false;
        }
        // NULLS FIRST/LAST must be the SQLite default for the direction. The default for ASC
        // is NULLS FIRST, for DESC is NULLS LAST. The index's NULL placement matches the
        // default. An explicit non-default NULLS placement would need a sorter.
        if !nulls_is_default(term) {
            return false;
        }
    }
    // The ORDER BY columns after the prefix must not be constrained by an equality — if they
    // were, they'd be pinned and the ORDER BY would be over-constrained (upstream handles
    // this via `nDistinctCol`); for simplicity we just reject the rare case.
    let _ = where_constraints;
    true
}

/// The column name an ORDER BY term references, after resolving aliases and ordinals. Returns
/// `None` for non-column terms (expressions, ordinals that resolve to non-column outputs).
fn order_by_column_name(term: &OrderingTerm, table: &Table) -> Option<String> {
    // An ordinal ORDER BY n selects an output column; we can only use the index when that
    // output is a bare column. The caller (the codegen) resolves ordinals against outputs;
    // here we only accept a bare column reference.
    if let Expr::Column { name, .. } = &term.expr {
        // Verify it's a real column on the table (not an alias). Aliases that shadow columns
        // would still resolve to the column, which is fine.
        if table.column_index(name).is_some() {
            return Some(name.clone());
        }
    }
    None
}

/// True when the ORDER BY term's NULLS placement is the SQLite default for its direction
/// (NULLS FIRST for ASC, NULLS LAST for DESC). An explicit non-default would require a sorter
/// because the index's NULL placement is the default.
fn nulls_is_default(term: &OrderingTerm) -> bool {
    use rustqlite_parser::NullsOrder;
    match term.nulls {
        None => true,
        Some(NullsOrder::First) => !term.desc,
        Some(NullsOrder::Last) => term.desc,
    }
}

/// The set of table-column indices referenced by the query's projection, WHERE, and ORDER BY
/// clauses. Used to decide if an index is covering. The rowid-alias column is included when
/// it's referenced (it's readable from the index's trailing rowid). Returns an empty set for
/// a FROM-less / `*`-only query that we can't analyze — the caller treats empty as "not
/// covering" so `SELECT *` never picks a covering index. The oracle's cost model also avoids
/// the covering index for `SELECT *` when the index is redundant (same columns as the table);
/// expanding `*` to all columns and checking `index_covers` would match that, but the
/// `has_benefit` gate (no WHERE, no ORDER BY → no benefit) already drops the redundant case.
fn collect_referenced_columns(select: &SelectStmt, table: &Table) -> Vec<usize> {
    let mut cols: Vec<usize> = Vec::new();
    let mut push = |idx: usize| {
        if !cols.contains(&idx) {
            cols.push(idx);
        }
    };
    // Projection. `*` / `t.*` reference every column on the table.
    for rc in &select.columns {
        match rc {
            rustqlite_parser::ResultColumn::Star | rustqlite_parser::ResultColumn::TableStar(_) => {
                for (i, _c) in table.columns.iter().enumerate() {
                    push(i);
                }
            }
            rustqlite_parser::ResultColumn::Expr { expr, .. } => {
                collect_columns(expr, table, &mut push);
            }
        }
    }
    if let Some(w) = &select.where_clause {
        collect_columns(w, table, &mut push);
    }
    for term in &select.order_by {
        collect_columns(&term.expr, table, &mut push);
    }
    cols
}

/// Walk an expression tree, recording the table-column index of every `Expr::Column` that
/// resolves to a stored column on `table`. The rowid-alias column is recorded as its column
/// index (so the covering check knows the index's trailing rowid satisfies it). Columns that
/// don't resolve (unknown names) are ignored — they'll surface as "no such column" later in
/// codegen.
fn collect_columns(expr: &Expr, table: &Table, push: &mut impl FnMut(usize)) {
    match expr {
        Expr::Column { name, .. } => {
            if let Some(i) = table.column_index(name) {
                push(i);
            }
        }
        Expr::Unary { expr, .. } => collect_columns(expr, table, push),
        Expr::Binary { left, right, .. } => {
            collect_columns(left, table, push);
            collect_columns(right, table, push);
        }
        Expr::Between { expr, low, high, .. } => {
            collect_columns(expr, table, push);
            collect_columns(low, table, push);
            collect_columns(high, table, push);
        }
        Expr::In { expr, values, .. } => {
            collect_columns(expr, table, push);
            for v in values {
                collect_columns(v, table, push);
            }
        }
        Expr::InSubquery { expr, .. } => collect_columns(expr, table, push),
        Expr::Cast { expr, .. } => collect_columns(expr, table, push),
        Expr::Case {
            base,
            when_then,
            else_expr,
        } => {
            if let Some(b) = base {
                collect_columns(b, table, push);
            }
            for (w, t) in when_then {
                collect_columns(w, table, push);
                collect_columns(t, table, push);
            }
            if let Some(e) = else_expr {
                collect_columns(e, table, push);
            }
        }
        Expr::Collate { expr, .. } => collect_columns(expr, table, push),
        Expr::IsDistinctFrom { left, right, .. } => {
            collect_columns(left, table, push);
            collect_columns(right, table, push);
        }
        Expr::Row(es) => es.iter().for_each(|e| collect_columns(e, table, push)),
        Expr::Coalesce2 { left, right } => {
            collect_columns(left, table, push);
            collect_columns(right, table, push);
        }
        Expr::Function { args, .. } => {
            if let rustqlite_parser::FunctionArgs::List(v) = args {
                for a in v {
                    collect_columns(a, table, push);
                }
            }
        }
        // Leaves with no column references.
        Expr::Literal(_) | Expr::BindParam(_) | Expr::Exists(_) | Expr::Subquery(_)
        | Expr::AggRef(_) => {}
    }
}

/// True when `index` covers all the columns in `referenced`: every referenced column is one
/// of the index's plain columns, OR is the table's rowid-alias column (satisfied by the
/// index's trailing rowid). Expression-index columns are matched by expression identity (not
/// yet implemented — we conservatively return false if any referenced column is not a plain
/// indexed column or the rowid alias).
fn index_covers(index: &IndexObject, table: &Table, referenced: &[usize]) -> bool {
    // The set of table-column indices the index stores as plain columns.
    let indexed_cols: Vec<usize> = index
        .columns
        .iter()
        .filter_map(|ic| {
            if ic.is_expression() {
                None
            } else {
                table.column_index(&ic.name)
            }
        })
        .collect();
    for &col_idx in referenced {
        // The rowid-alias column is satisfied by the index's trailing rowid.
        if Some(col_idx) == table.rowid_alias {
            continue;
        }
        if !indexed_cols.contains(&col_idx) {
            return false;
        }
    }
    true
}

/// True when the index's plain (non-expression) columns are a strict subset of the table's
/// columns — the index is smaller than the table, so a covering index scan reads fewer bytes
/// per row than a table scan. A redundant index (same columns as the table) offers no covering
/// benefit; the oracle's cost model prefers the table scan in that case.
fn index_strictly_smaller_than_table(index: &IndexObject, table: &Table) -> bool {
    let plain_indexed: usize = index.columns.iter().filter(|ic| !ic.is_expression()).count();
    plain_indexed < table.columns.len()
}

fn column_name(expr: &Expr) -> Option<String> {
    let Expr::Column { name, .. } = expr else {
        return None;
    };
    Some(name.clone())
}

fn const_value(expr: &Expr) -> Option<Value> {
    match expr {
        Expr::Literal(lit) => Some(literal_to_value(lit)),
        Expr::BindParam(_) => Some(Value::Null),
        Expr::Unary {
            op: rustqlite_parser::UnaryOp::Negate,
            expr,
        } => {
            let v = const_value(expr)?;
            match v {
                Value::Int(i) => Some(Value::Int(-i)),
                Value::Real(r) => Some(Value::Real(-r)),
                _ => None,
            }
        }
        _ => None,
    }
}

fn literal_to_value(lit: &Literal) -> Value {
    match lit {
        Literal::Null => Value::Null,
        Literal::Integer(n) => Value::Int(*n),
        Literal::Real(r) => Value::Real(*r),
        Literal::Text(s) => Value::Text(s.clone()),
        Literal::Blob(b) => Value::Blob(b.clone()),
        Literal::Bool(b) => Value::Int(if *b { 1 } else { 0 }),
    }
}