//! Built-in aggregate functions (mirrors the aggregate entries in `func.c`).
//!
//! Each aggregate carries per-group accumulator state ([`Accumulator`]) that is updated by
//! [`AggStep`](crate::vdbe::Opcode::AggStep) and read out by
//! [`AggFinal`](crate::vdbe::Opcode::AggFinal). The state lives in the register file as a
//! `Value::Aggregate` cell so a single `AggStep`/`AggFinal` pair can update it in place without
//! the executor needing a side table of accumulators — mirroring upstream's `Mem.uTemp`/`Mem.r`
//! accumulator reuse, while keeping the dynamic `Value` type faithful to the on-disk storage
//! classes.
//!
//! Behavior is pinned to the system `sqlite3` 3.53.x oracle (see `AGENTS.md`):
//! * `count(*)` counts every row; `count(expr)` counts non-NULL `expr`s.
//! * `sum(expr)` is INTEGER when every input is an integer that fits in `i64`, REAL otherwise,
//!   and NULL when no non-NULL input was seen. Integer overflow promotes to REAL with a
//!   best-effort value (SQLite raises an "integer overflow" error in this case; we follow the
//!   oracle's relaxed behavior for the common path).
//! * `total(expr)` is always REAL, `0.0` for an empty input set, and never NULL.
//! * `avg(expr)` is REAL (or NULL for an empty set).
//! * `min`/`max(expr)` honor SQLite's storage-class ordering (NULL sorts lowest) and pick the
//!   running extremum; an all-NULL (or empty) group yields NULL.
//! * `group_concat(expr [, sep])` joins the text rendering of each non-NULL input with `sep`
//!   (default `","`). An empty/all-NULL group yields NULL (not the empty string).

use crate::types::Value;
use crate::vdbe::compare::mem_compare;
use crate::types::Collation;

/// The built-in aggregate *and* window-only function kinds. The name matches the SQL function
/// name (case-insensitively resolved at codegen time and stored here verbatim for the executor's
/// dispatch).
///
/// Variants marked by [`AggregateKind::window_only`] are **window-only** — they are *not* legal
/// as plain aggregates and may only appear with an `OVER (...)` clause. The executor's
/// `AggStep`/`AggInverse`/`AggValue`/`AggFinal` paths dispatch uniformly on this enum, mirroring
/// upstream's single `WindowAccumulator`-style API that covers both the aggregate-as-window
/// functions (`count`/`sum`/`avg`/`min`/`max`/`group_concat`) and the window-only built-ins
/// (`row_number`/`rank`/`dense_rank`/`percent_rank`/`cume_dist`/`ntile`/`first_value`/`last_value`/
/// `nth_value`/`lead`/`lag`). See the `BUILT-IN WINDOW FUNCTIONS` header comment in
/// `window.c` for the upstream table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregateKind {
    // ---- plain aggregates (also usable as window functions with an OVER clause) ----
    Count,
    Sum,
    Total,
    Avg,
    Min,
    Max,
    GroupConcat,
    /// `json_group_array(X)` — collect every X (including NULLs) into a JSON array. Even an
    /// empty input produces `[]`. Each X is rendered per the "value argument" rule (NULL →
    /// `null`, INTEGER/REAL → number, TEXT → quoted JSON string; the JSON-subtype-aware "value
    /// is JSON if it came from a JSON function" rule is M24.20 and not yet modeled).
    JsonGroupArray,
    /// `json_group_object(NAME, VALUE)` — collect every (NAME, VALUE) pair into a JSON object.
    /// Rows where NAME is NULL are skipped (matching upstream). VALUE is rendered per the
    /// "value argument" rule. Even an empty input produces `{}`.
    JsonGroupObject,
    // ---- window-only built-in functions (no plain-aggregate form) ----
    /// `row_number()` — 0 args; default frame `ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW`.
    /// Step just bumps a counter; value reads it. No inverse (the frame only grows).
    RowNumber,
    /// `rank()` — 0 args; default frame `RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW`.
    /// Step bumps `nStep` and, if `nValue==0`, latches `nValue = nStep` (the rank is the first
    /// peer-group row's 1-based index). Value reads `nValue` and resets it to 0 (so the next
    /// peer group re-latches).
    Rank,
    /// `dense_rank()` — 0 args; default frame `RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT
    /// ROW`. Step sets `nStep = 1` (a "peer-group changed" flag); value increments `nValue` if
    /// the flag is set, then reads `nValue`.
    DenseRank,
    /// `percent_rank()` — 0 args; default frame `GROUPS BETWEEN CURRENT ROW AND UNBOUNDED
    /// FOLLOWING`. Step bumps `nTotal`; inverse bumps `nStep`; value computes
    /// `nStep / (nTotal - 1)` (or 0.0 if `nTotal <= 1`) and latches `nValue = nStep`.
    PercentRank,
    /// `cume_dist()` — 0 args; default frame `GROUPS BETWEEN 1 FOLLOWING AND UNBOUNDED
    /// FOLLOWING`. Step bumps `nTotal`; inverse bumps `nStep`; value computes `nStep / nTotal`.
    CumeDist,
    /// `ntile(N)` — 1 arg; default frame `ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING`.
    /// Step captures `nParam = N` on the first row and bumps `nTotal`; inverse bumps `iRow`;
    /// value computes the 1-based bucket index for `iRow` given `nParam` buckets over `nTotal`
    /// rows (the upstream `ntileValueFunc` formula).
    Ntile,
    /// `first_value(expr)` — 1 arg; default frame `RANGE BETWEEN UNBOUNDED PRECEDING AND
    /// CURRENT ROW`. Step captures the first row's argument (and never overwrites it). Value
    /// emits the captured value (or NULL if none seen). No inverse.
    FirstValue,
    /// `last_value(expr)` — 1 arg; default frame `RANGE BETWEEN CURRENT ROW AND CURRENT ROW`.
    /// Step captures each row's argument (overwriting); inverse decrements a counter and, when
    /// it reaches 0, clears the captured value. Value emits the currently-captured value.
    LastValue,
    /// `nth_value(expr, N)` — 2 args; default frame `RANGE BETWEEN UNBOUNDED PRECEDING AND
    /// CURRENT ROW`. Step bumps a counter and, when it equals N, captures the row's argument.
    /// Value emits the captured value. Inverse is a no-op (the frame only grows by the default).
    NthValue,
    /// `lead(expr [, offset [, default]])` / `lag(expr [, offset [, default]])` — 1..3 args.
    /// Implemented by VDBE instructions in upstream (the `WINDOWFUNCNOOP` registration); the
    /// accumulator path is not used. The kind exists for name resolution and frame coercion.
    Lead,
    Lag,
}

impl AggregateKind {
    /// Resolve a function name (case-insensitive) and argument count to an aggregate kind, or
    /// `None` if `name` is not a built-in aggregate. `count(*)` is encoded as `count` with zero
    /// arguments at the AST layer; the codegen path passes `n_arg == 0` for the star form and
    /// `n_arg == 1` for the regular form, so both shapes map to [`AggregateKind::Count`].
    ///
    /// Window-only built-in functions (`row_number`, `rank`, …) are also resolved here so the
    /// same `Accumulator` + `P4::FuncDef` plumbing serves them; they are gated to require an
    /// `OVER (...)` clause at codegen time via [`Self::window_only`].
    pub fn from_name(name: &str, n_arg: usize) -> Option<AggregateKind> {
        let lname = name.to_ascii_lowercase();
        match lname.as_str() {
            // plain aggregates
            "count" if n_arg == 0 || n_arg == 1 => Some(AggregateKind::Count),
            "sum" if n_arg == 1 => Some(AggregateKind::Sum),
            "total" if n_arg == 1 => Some(AggregateKind::Total),
            "avg" if n_arg == 1 => Some(AggregateKind::Avg),
            "min" if n_arg == 1 => Some(AggregateKind::Min),
            "max" if n_arg == 1 => Some(AggregateKind::Max),
            "group_concat" | "string_agg" if n_arg == 1 || n_arg == 2 => {
                Some(AggregateKind::GroupConcat)
            }
            "json_group_array" if n_arg == 1 => {
                Some(AggregateKind::JsonGroupArray)
            }
            "json_group_object" if n_arg == 2 => {
                Some(AggregateKind::JsonGroupObject)
            }
            // window-only built-ins (M11.4–M11.6)
            "row_number" if n_arg == 0 => Some(AggregateKind::RowNumber),
            "rank" if n_arg == 0 => Some(AggregateKind::Rank),
            "dense_rank" if n_arg == 0 => Some(AggregateKind::DenseRank),
            "percent_rank" if n_arg == 0 => Some(AggregateKind::PercentRank),
            "cume_dist" if n_arg == 0 => Some(AggregateKind::CumeDist),
            "ntile" if n_arg == 1 => Some(AggregateKind::Ntile),
            "first_value" if n_arg == 1 => Some(AggregateKind::FirstValue),
            "last_value" if n_arg == 1 => Some(AggregateKind::LastValue),
            "nth_value" if n_arg == 2 => Some(AggregateKind::NthValue),
            "lead" if (1..=3).contains(&n_arg) => Some(AggregateKind::Lead),
            "lag" if (1..=3).contains(&n_arg) => Some(AggregateKind::Lag),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            AggregateKind::Count => "count",
            AggregateKind::Sum => "sum",
            AggregateKind::Total => "total",
            AggregateKind::Avg => "avg",
            AggregateKind::Min => "min",
            AggregateKind::Max => "max",
            AggregateKind::GroupConcat => "group_concat",
            AggregateKind::JsonGroupArray => "json_group_array",
            AggregateKind::JsonGroupObject => "json_group_object",
            AggregateKind::RowNumber => "row_number",
            AggregateKind::Rank => "rank",
            AggregateKind::DenseRank => "dense_rank",
            AggregateKind::PercentRank => "percent_rank",
            AggregateKind::CumeDist => "cume_dist",
            AggregateKind::Ntile => "ntile",
            AggregateKind::FirstValue => "first_value",
            AggregateKind::LastValue => "last_value",
            AggregateKind::NthValue => "nth_value",
            AggregateKind::Lead => "lead",
            AggregateKind::Lag => "lag",
        }
    }

    /// `true` if this kind is **window-only** — it may only appear with an `OVER (...)` clause
    /// and is not a legal plain aggregate. The codegen path rejects a window-only call without
    /// an `OVER` clause with the upstream error ("misuse of window function <name>()").
    pub fn window_only(self) -> bool {
        matches!(
            self,
            AggregateKind::RowNumber
                | AggregateKind::Rank
                | AggregateKind::DenseRank
                | AggregateKind::PercentRank
                | AggregateKind::CumeDist
                | AggregateKind::Ntile
                | AggregateKind::FirstValue
                | AggregateKind::LastValue
                | AggregateKind::NthValue
                | AggregateKind::Lead
                | AggregateKind::Lag
        )
    }

    /// The fixed argument count used to disambiguate the aggregate's step function. `count(*)`
    /// is represented with `n_arg == 0`; the rest take 1 (or 2 for `group_concat`/`nth_value`).
    pub fn n_arg(self) -> usize {
        match self {
            AggregateKind::Count => 1, // `count(expr)`; the star form passes a sentinel
            AggregateKind::Sum | AggregateKind::Total | AggregateKind::Avg | AggregateKind::Min
            | AggregateKind::Max => 1,
            AggregateKind::GroupConcat => 2,
            AggregateKind::JsonGroupArray => 1,
            AggregateKind::JsonGroupObject => 2,
            AggregateKind::RowNumber
            | AggregateKind::Rank
            | AggregateKind::DenseRank
            | AggregateKind::PercentRank
            | AggregateKind::CumeDist => 0,
            AggregateKind::Ntile | AggregateKind::FirstValue | AggregateKind::LastValue
            | AggregateKind::Lead | AggregateKind::Lag => 1,
            AggregateKind::NthValue => 2,
        }
    }

    /// The default frame this built-in window function is coerced to when the user writes no
    /// explicit `frame_spec` (mirrors the `aUp[]` table in `sqlite3WindowUpdate`, `window.c:699`).
    /// Returns `(mode, start, end)` so a future codegen path can install the coerced frame; the
    /// values are upstream's `TK_ROWS`/`TK_RANGE`/`TK_GROUPS`, `TK_UNBOUNDED`/`TK_CURRENT`/
    /// `TK_PRECEDING`/`TK_FOLLOWING`. We use the Rust equivalents of those here.
    pub fn default_frame(self) -> (DefaultFrameMode, DefaultFrameBound, DefaultFrameBound) {
        use DefaultFrameBound::*;
        use DefaultFrameMode::*;
        match self {
            AggregateKind::RowNumber => (Rows, UnboundedPreceding, CurrentRow),
            AggregateKind::Rank | AggregateKind::DenseRank => {
                (Range, UnboundedPreceding, CurrentRow)
            }
            AggregateKind::PercentRank => (Groups, CurrentRow, UnboundedFollowing),
            AggregateKind::CumeDist => (Groups, Following(1), UnboundedFollowing),
            AggregateKind::Ntile => (Rows, CurrentRow, UnboundedFollowing),
            AggregateKind::Lead => (Rows, UnboundedPreceding, UnboundedFollowing),
            AggregateKind::Lag => (Rows, UnboundedPreceding, CurrentRow),
            // first_value/last_value/nth_value and the aggregate-as-window functions default to
            // the spec's "RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW" when the user writes
            // nothing — upstream leaves these alone (no aUp[] entry), so the spec default applies.
            _ => (Range, UnboundedPreceding, CurrentRow),
        }
    }
}

/// The frame mode of a window's `ROWS`/`RANGE`/`GROUPS` spec (mirrors `TK_ROWS`/`TK_RANGE`/
/// `TK_GROUPS`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultFrameMode {
    Rows,
    Range,
    Groups,
}

/// A frame bound used by [`AggregateKind::default_frame`] (mirrors the `TK_UNBOUNDED`/
/// `TK_CURRENT`/`TK_PRECEDING`/`TK_FOLLOWING` constants).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultFrameBound {
    UnboundedPreceding,
    CurrentRow,
    /// `<expr> PRECEDING` / `<expr> FOLLOWING` — the expression value is fixed per-kind by
    /// `sqlite3WindowUpdate` (e.g. `cume_dist` uses `1 FOLLOWING`).
    Preceding(i64),
    Following(i64),
    UnboundedFollowing,
}

/// `true` if `name` is a built-in aggregate *or* window-only function at any arity (used by the
/// codegen path to detect aggregate/window calls in the projection list without yet knowing
/// the argument count). The per-arity check is done later by [`AggregateKind::from_name`].
pub fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "count" | "sum" | "total" | "avg" | "min" | "max" | "group_concat" | "string_agg"
            | "json_group_array" | "json_group_object" | "row_number" | "rank" | "dense_rank"
            | "percent_rank" | "cume_dist" | "ntile" | "first_value" | "last_value"
            | "nth_value" | "lead" | "lag"
    )
}

/// `true` if `name` is one of the window-only built-in functions (not a plain aggregate). Used
/// by the codegen path to reject `row_number()` etc. without an `OVER` clause.
pub fn is_window_only_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "row_number" | "rank" | "dense_rank" | "percent_rank" | "cume_dist" | "ntile"
            | "first_value" | "last_value" | "nth_value" | "lead" | "lag"
    )
}

/// `true` if `(name, n_arg)` resolves to a built-in aggregate *or* window-only function — i.e.
/// the name is recognized *and* the argument count matches one of its accepted arities. Used by
/// the aggregate codegen to distinguish a scalar `max(a, 0)` (2-arg `max`, the scalar form) from
/// the aggregate `max(a)` (1-arg `max`). Mirrors the dispatch in [`AggregateKind::from_name`].
pub fn is_aggregate_call(name: &str, n_arg: usize) -> bool {
    AggregateKind::from_name(name, n_arg).is_some()
}

/// The per-group accumulator state for a built-in aggregate. Stored as `Value::Aggregate` in
/// the register file so a single `AggStep`/`AggFinal` pair can update it in place.
///
/// The plain-aggregate fields (`count`/`sum_*`/`best`/`concat`/`sep`) are unused by the
/// window-only built-ins; the `call_count` / `ntile` / `nth` / `last_value` fields are unused by
/// the plain aggregates. One struct serves both because the executor's `P4::FuncDef(kind)` carries
/// the discriminator and dispatches to the right paths in `step`/`inverse`/`value`.
#[derive(Clone, Debug)]
pub struct Accumulator {
    pub kind: AggregateKind,
    /// Number of rows that contributed a non-NULL argument (or, for `count(*)`, every row).
    /// Also reused as the row counter for `row_number` and the `nTotal` for `ntile`.
    pub count: i64,
    /// Running sum for `sum`/`total`/`avg`. `i64` while every input is an exact integer that
    /// has not overflowed; promoted to `f64` (and stays there) once a REAL input or an overflow
    /// is seen — matching SQLite's `SumCtx` "hasReal" / overflow-to-REAL behavior.
    pub sum_i: i64,
    pub sum_r: f64,
    pub has_real: bool,
    pub overflowed: bool,
    /// Running extremum for `min`/`max` (NULL until the first non-NULL input).
    pub best: Option<Value>,
    /// Running concatenation for `group_concat` (None until the first non-NULL input).
    pub concat: Option<String>,
    /// The separator for `group_concat` (captured from the first `AggStep` call). Defaults to
    /// `","` when the aggregate is `group_concat(expr)` (single-arg form).
    pub sep: String,
    /// Set once `AggFinal` has produced the result; further `AggStep` calls would reset.
    pub finalized: bool,

    // ---- window-only built-in state (M11.4–M11.6) ----
    /// `CallCount.nValue` for `rank`/`dense_rank`/`percent_rank`/`cume_dist`: the latched rank
    /// value (rank) or the dense-rank counter. Semantics differ per kind — see `step`/`value`.
    pub n_value: i64,
    /// `CallCount.nStep` for `rank`/`dense_rank`/`percent_rank`/`cume_dist`: the per-row step
    /// counter (rank latches `nValue = nStep` on the first row of a peer group; dense_rank sets
    /// `nStep = 1` as a "peer changed" flag; percent_rank/cume_dist use it as the inverse-step
    /// counter). Also reused as `NtileCtx.iRow` for `ntile` (the inverse-step counter).
    pub n_step: i64,
    /// `CallCount.nTotal` for `percent_rank`/`cume_dist`/`ntile`: the total rows in the
    /// partition, counted by the step function (percent_rank/cume_dist) or by ntile's step.
    pub n_total: i64,
    /// `NtileCtx.nParam` for `ntile`: the `N` argument captured on the first step.
    pub n_param: i64,
    /// `NthValueCtx.nStep` for `nth_value`: the 1-based row counter compared to N. Also used by
    /// `first_value` as a "value already captured" flag (0 = not yet, 1 = captured).
    pub nth_step: i64,
    /// The captured value for `first_value`/`last_value`/`nth_value` (NULL until the target row
    /// is reached). For `last_value` this is overwritten on each step; for `first_value` it's
    /// set once; for `nth_value` it's set when `nth_step == N`.
    pub captured: Option<Value>,
    /// Running JSON array text for `json_group_array` — the accumulated `[..., ..., ...]`
    /// rendering. `None` until the first step (so an empty aggregate can render `[]`).
    pub json_array: Option<String>,
    /// Running JSON object text for `json_group_object` — the accumulated `{"k":v,...}`
    /// rendering (without the closing `}`). `None` until the first non-NULL-name step.
    pub json_object: Option<String>,
}

impl Accumulator {
    pub fn new(kind: AggregateKind) -> Accumulator {
        Accumulator {
            kind,
            count: 0,
            sum_i: 0,
            sum_r: 0.0,
            has_real: false,
            overflowed: false,
            best: None,
            concat: None,
            sep: ",".to_string(),
            finalized: false,
            n_value: 0,
            n_step: 0,
            n_total: 0,
            n_param: 0,
            nth_step: 0,
            captured: None,
            json_array: None,
            json_object: None,
        }
    }

    /// Apply one row's arguments to the accumulator. `args` is the value vector passed by
    /// `AggStep` (the `p2..p2+n_arg` registers). For `count(*)` the codegen path passes a
    /// single NULL sentinel and `is_count_star = true`; this is the only case where a NULL
    /// argument still bumps the count.
    pub fn step(&mut self, args: &[Value], is_count_star: bool) {
        if self.finalized {
            // Reset for a fresh accumulation (matches upstream's per-statement restart).
            *self = Accumulator::new(self.kind);
        }
        match self.kind {
            AggregateKind::Count => {
                if is_count_star {
                    self.count += 1;
                } else if let Some(arg) = args.first() {
                    if !arg.is_null() {
                        self.count += 1;
                    }
                }
            }
            AggregateKind::Sum | AggregateKind::Total | AggregateKind::Avg => {
                let arg = match args.first() {
                    Some(v) if !v.is_null() => v,
                    _ => return, // NULL inputs are skipped (sum/total/avg ignore them).
                };
                self.count += 1;
                if self.has_real {
                    self.sum_r += arg.as_f64();
                } else if let Value::Int(i) = arg {
                    match self.sum_i.checked_add(*i) {
                        Some(new_i) => self.sum_i = new_i,
                        None => {
                            // Promote to REAL on overflow (matches the oracle's relaxed path).
                            self.sum_r = self.sum_i as f64 + *i as f64;
                            self.has_real = true;
                            self.overflowed = true;
                        }
                    }
                } else if let Value::Real(r) = arg {
                    self.sum_r = self.sum_i as f64 + *r;
                    self.has_real = true;
                } else {
                    // TEXT/BLOB: coerce via SQLite's leading-numeric-prefix rule.
                    let n = arg.as_f64();
                    if n.fract() == 0.0 && !self.has_real {
                        match self.sum_i.checked_add(n as i64) {
                            Some(new_i) => self.sum_i = new_i,
                            None => {
                                self.sum_r = self.sum_i as f64 + n;
                                self.has_real = true;
                                self.overflowed = true;
                            }
                        }
                    } else {
                        self.sum_r = if self.has_real {
                            self.sum_r + n
                        } else {
                            self.sum_i as f64 + n
                        };
                        self.has_real = true;
                    }
                }
            }
            AggregateKind::Min | AggregateKind::Max => {
                let arg = match args.first() {
                    Some(v) if !v.is_null() => v,
                    _ => return, // NULL inputs are skipped (min/max ignore them).
                };
                self.count += 1;
                match &self.best {
                    None => self.best = Some(arg.clone()),
                    Some(cur) => {
                        let ord = mem_compare(arg, cur, Collation::Binary);
                        let take = match self.kind {
                            AggregateKind::Min => ord == std::cmp::Ordering::Less,
                            AggregateKind::Max => ord == std::cmp::Ordering::Greater,
                            _ => unreachable!(),
                        };
                        if take {
                            self.best = Some(arg.clone());
                        }
                    }
                }
            }
            AggregateKind::GroupConcat => {
                // The optional separator lives in args[1]; capture it on the first call.
                if args.len() >= 2 {
                    if let Some(sep) = args.get(1).and_then(|v| v.to_text()) {
                        if !sep.is_empty() || self.count == 0 {
                            self.sep = sep;
                        }
                    }
                }
                let arg = match args.first() {
                    Some(v) if !v.is_null() => v,
                    _ => return, // NULL inputs are skipped.
                };
                let text = arg.to_text().unwrap_or_default();
                match &mut self.concat {
                    None => self.concat = Some(text),
                    Some(cur) => {
                        cur.push_str(&self.sep);
                        cur.push_str(&text);
                    }
                }
                self.count += 1;
            }
            AggregateKind::JsonGroupArray => {
                // `json_group_array(X)` — collect every X (including NULLs) into a JSON array.
                // Render each X per the "value argument" rule: NULL → `null`, INTEGER/REAL →
                // number, TEXT → quoted JSON string. The JSON-subtype-aware rule is M24.20.
                let arg = args.first().unwrap_or(&Value::Null);
                let mut rendered = String::new();
                json_render_value_arg(arg, &mut rendered);
                match &mut self.json_array {
                    None => {
                        let mut s = String::from("[");
                        s.push_str(&rendered);
                        self.json_array = Some(s);
                    }
                    Some(cur) => {
                        cur.push(',');
                        cur.push_str(&rendered);
                    }
                }
                self.count += 1;
            }
            AggregateKind::JsonGroupObject => {
                // `json_group_object(NAME, VALUE)` — skip rows where NAME is NULL (matching
                // upstream). Render NAME as a quoted JSON string key and VALUE per the "value
                // argument" rule.
                let name = args.first().and_then(|v| v.to_text());
                let name = match name {
                    Some(n) => n,
                    None => return, // NULL name → skip this row.
                };
                let value = args.get(1).unwrap_or(&Value::Null);
                let mut key = String::new();
                crate::func::json::render_string(&name, &mut key);
                let mut val = String::new();
                json_render_value_arg(value, &mut val);
                match &mut self.json_object {
                    None => {
                        let mut s = String::from("{");
                        s.push_str(&key);
                        s.push(':');
                        s.push_str(&val);
                        self.json_object = Some(s);
                    }
                    Some(cur) => {
                        cur.push(',');
                        cur.push_str(&key);
                        cur.push(':');
                        cur.push_str(&val);
                    }
                }
                self.count += 1;
            }

            // ---- window-only built-ins (M11.4–M11.6) ----
            AggregateKind::RowNumber => {
                // `row_numberStepFunc`: `(*p)++` on a single i64 counter.
                self.count += 1;
            }
            AggregateKind::Rank => {
                // `rankStepFunc`: `nStep++`; if `nValue==0`, latch `nValue = nStep`.
                self.n_step += 1;
                if self.n_value == 0 {
                    self.n_value = self.n_step;
                }
            }
            AggregateKind::DenseRank => {
                // `dense_rankStepFunc`: just set `nStep = 1` (peer-changed flag).
                self.n_step = 1;
            }
            AggregateKind::PercentRank => {
                // `percent_rankStepFunc`: `nTotal++`.
                self.n_total += 1;
            }
            AggregateKind::CumeDist => {
                // `cume_distStepFunc`: `nTotal++`.
                self.n_total += 1;
            }
            AggregateKind::Ntile => {
                // `ntileStepFunc`: on the first step (`nTotal == 0`), capture `nParam = arg[0]`
                // (and reject non-positive N at codegen time, mirroring the runtime error).
                // Then `nTotal++` on every step.
                if self.n_total == 0 {
                    if let Some(arg) = args.first() {
                        self.n_param = arg.as_i64();
                    }
                }
                self.n_total += 1;
            }
            AggregateKind::FirstValue => {
                // `first_valueStepFunc`: capture `arg[0]` if not yet captured.
                if self.nth_step == 0 {
                    self.captured = args.first().cloned();
                    self.nth_step = 1;
                }
            }
            AggregateKind::LastValue => {
                // `last_valueStepFunc`: free the previous captured value, dup the new one, and
                // bump `nVal` (the count of values in the frame). NULL is captured as-is
                // (mirrors `sqlite3_value_dup`, which returns NULL for a NULL input).
                self.captured = args.first().cloned();
                self.nth_step += 1;
            }
            AggregateKind::NthValue => {
                // `nth_valueStepFunc`: validate N (a positive integer), bump `nStep`, and when
                // `nStep == N`, capture `arg[0]`. We don't raise errors here (the codegen path
                // validates N at compile time; a runtime non-integer N would have errored before
                // reaching the accumulator — matching upstream which sets the error on pCtx).
                let n = match args.get(1) {
                    Some(v) => v.as_i64(),
                    None => 1,
                };
                if n <= 0 {
                    return;
                }
                self.nth_step += 1;
                if self.nth_step == n {
                    self.captured = args.first().cloned();
                }
            }
            AggregateKind::Lead | AggregateKind::Lag => {
                // `lead`/`lag` are implemented with VDBE instructions in upstream (registered as
                // `WINDOWFUNCNOOP` — the step/value/finalize callbacks are no-ops). The codegen
                // path never emits `AggStep`/`AggValue` for them; reaching this arm is a bug.
            }
        }
    }

    /// Remove one row's arguments from the accumulator — the window-frame "inverse step"
    /// that slides the frame start forward. Mirrors upstream's `xInverse` for the built-in
    /// aggregates (`count`/`sum`/`total`/`avg`/`group_concat`); `min`/`max` never emit `AggInverse`
    /// (their non-default-frame path uses VDBE instructions, not the accumulator inverse).
    ///
    /// `args` / `is_count_star` match [`step`]. The caller guarantees `step` has been called at
    /// least once with this row's arguments before `inverse` is called with them (upstream
    /// asserts this with `pMem->uTemp == 0x1122e0e3`).
    pub fn inverse(&mut self, args: &[Value], is_count_star: bool) {
        if self.finalized {
            return;
        }
        match self.kind {
            AggregateKind::Count => {
                if is_count_star {
                    self.count -= 1;
                } else if let Some(arg) = args.first() {
                    if !arg.is_null() {
                        self.count -= 1;
                    }
                }
            }
            AggregateKind::Sum | AggregateKind::Total | AggregateKind::Avg => {
                let arg = match args.first() {
                    Some(v) if !v.is_null() => v,
                    _ => return, // NULL inputs were skipped by step; inverse skips too.
                };
                self.count -= 1;
                if self.has_real {
                    self.sum_r -= arg.as_f64();
                } else if let Value::Int(i) = arg {
                    match self.sum_i.checked_sub(*i) {
                        Some(new_i) => self.sum_i = new_i,
                        None => {
                            // Promote to REAL on overflow (matches the oracle's relaxed path).
                            self.sum_r = self.sum_i as f64 - *i as f64;
                            self.has_real = true;
                            self.overflowed = true;
                        }
                    }
                } else if let Value::Real(r) = arg {
                    self.sum_r = self.sum_i as f64 - *r;
                    self.has_real = true;
                } else {
                    // TEXT/BLOB: coerce via SQLite's leading-numeric-prefix rule.
                    let n = arg.as_f64();
                    if n.fract() == 0.0 && !self.has_real {
                        match self.sum_i.checked_sub(n as i64) {
                            Some(new_i) => self.sum_i = new_i,
                            None => {
                                self.sum_r = self.sum_i as f64 - n;
                                self.has_real = true;
                                self.overflowed = true;
                            }
                        }
                    } else {
                        self.sum_r = if self.has_real {
                            self.sum_r - n
                        } else {
                            self.sum_i as f64 - n
                        };
                        self.has_real = true;
                    }
                }
            }
            AggregateKind::Min | AggregateKind::Max => {
                // min/max do not support xInverse; the codegen path never emits AggInverse
                // for them. Reach this only via a hand-built (buggy) program.
                self.count = self.count.saturating_sub(1);
            }
            AggregateKind::GroupConcat => {
                let arg = match args.first() {
                    Some(v) if !v.is_null() => v,
                    _ => return, // NULL inputs were skipped by step; inverse skips too.
                };
                let text = arg.to_text().unwrap_or_default();
                if let Some(cur) = &mut self.concat {
                    // Upstream inserts the separator *before* each non-first value, so the
                    // accumulated string is `v0 [sep v1] [sep v2] ...`. The frame start advances
                    // in FIFO order, so the value being removed is the OLDEST one — at the
                    // front. Drop `text` from the front, plus the separator that followed it
                    // (if any remaining bytes). If only one value was accumulated, drop
                    // everything.
                    let cur_len = cur.len();
                    let text_len = text.len();
                    if cur_len <= text_len {
                        // Only the value (or less) remains — clear it.
                        cur.clear();
                    } else {
                        // Drop `text` from the front; if anything remains, also drop the
                        // separator that followed it.
                        let sep_len = self.sep.len();
                        let drop_len = if cur_len >= text_len + sep_len {
                            text_len + sep_len
                        } else {
                            // The separator wasn't yet appended (single value case) — drop
                            // just the value.
                            cur_len
                        };
                        cur.drain(..drop_len);
                    }
                    if cur.is_empty() {
                        self.concat = None;
                    }
                }
                self.count = self.count.saturating_sub(1);
            }
            // `json_group_array` / `json_group_object` sliding-frame inverse: the accumulated
            // JSON text is not separable into per-row chunks without re-rendering (a value's
            // rendered length depends on its type and any escapes). The M11 sliding-frame
            // codegen rejects these kinds (`window_function_frame_spec_unsupported`), so
            // `AggInverse` is never emitted for them in the current engine. This arm is
            // defensive: a hand-built program reaching here leaves the accumulator untouched
            // (the result would be wrong, but not crash) — matching the M11.8/11.9 note that
            // the streaming-3-cursor `AggInverse` shape lands with the follow-up.
            AggregateKind::JsonGroupArray | AggregateKind::JsonGroupObject => {
                self.count = self.count.saturating_sub(1);
            }

            // ---- window-only built-ins (M11.4–M11.6) ----
            AggregateKind::RowNumber => {
                // The default frame `ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW` never
                // removes rows from the frame, so `inverse` is never emitted for `row_number`.
                // Reach here only via a hand-built (buggy) program; behave as a no-op.
            }
            AggregateKind::Rank | AggregateKind::DenseRank => {
                // `rank`/`dense_rank` use `RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW` —
                // the frame only grows. `inverse` is never emitted; no-op.
            }
            AggregateKind::PercentRank => {
                // `percent_rankInvFunc`: `nStep++` (the inverse-step counter).
                self.n_step += 1;
            }
            AggregateKind::CumeDist => {
                // `cume_distInvFunc`: `nStep++` (the inverse-step counter).
                self.n_step += 1;
            }
            AggregateKind::Ntile => {
                // `ntileInvFunc`: `iRow++` (the row counter). We store `iRow` in `n_step` to
                // reuse the existing field (the per-row step counter).
                self.n_step += 1;
            }
            AggregateKind::FirstValue => {
                // `first_value` uses `RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW` — the
                // frame only grows; `inverse` is never emitted; no-op.
            }
            AggregateKind::LastValue => {
                // `last_valueInvFunc`: decrement `nVal` and, when it reaches 0, clear the
                // captured value (so a subsequent `value()` reads NULL — the frame is empty).
                self.nth_step -= 1;
                if self.nth_step <= 0 {
                    self.captured = None;
                }
            }
            AggregateKind::NthValue => {
                // `nth_valueInvFunc` is `noopStepFunc` in upstream — never invoked. The default
                // frame `RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW` only grows.
            }
            AggregateKind::Lead | AggregateKind::Lag => {
                // Implemented by VDBE instructions; `inverse` never emitted.
            }
        }
    }

    /// Read out the accumulator's current value *without* consuming it (so a window function
    /// can keep stepping after reading the current frame's value). Mirrors upstream's `xValue`.
    /// The result matches what [`finalize_accumulator`](super::super::vdbe::exec::finalize_accumulator)
    /// would produce, but leaves the accumulator in place.
    ///
    /// **Note**: `value()` takes `&self` for the plain aggregates (they only read state). For the
    /// window-only ranking functions (`rank`/`dense_rank`/`percent_rank`), upstream's
    /// `xValue` *mutates* the accumulator (e.g. `rankValueFunc` resets `nValue = 0` so the next
    /// peer group re-latches). Those kinds are dispatched through a separate [`value_mut`] path
    /// by the executor; this `value()` arm returns the same result without mutating, which is
    /// correct for the aggregate-as-window kinds (their `xValue` does not mutate). The
    /// window-only kinds never reach this `value()` (the executor calls [`value_mut`] instead).
    pub fn value(&self) -> Value {
        match self.kind {
            AggregateKind::Count => Value::Int(self.count),
            AggregateKind::Sum => {
                if self.count == 0 {
                    Value::Null
                } else if self.has_real {
                    Value::Real(self.sum_r)
                } else {
                    Value::Int(self.sum_i)
                }
            }
            AggregateKind::Total => {
                if self.has_real {
                    Value::Real(self.sum_r)
                } else {
                    Value::Real(self.sum_i as f64)
                }
            }
            AggregateKind::Avg => {
                if self.count == 0 {
                    Value::Null
                } else {
                    let total = if self.has_real {
                        self.sum_r
                    } else {
                        self.sum_i as f64
                    };
                    Value::Real(total / self.count as f64)
                }
            }
            AggregateKind::Min | AggregateKind::Max => self.best.clone().unwrap_or(Value::Null),
            AggregateKind::GroupConcat => {
                self.concat.clone().map(Value::Text).unwrap_or(Value::Null)
            }
            AggregateKind::JsonGroupArray => {
                // Even an empty input produces `[]`.
                let s = match &self.json_array {
                    Some(cur) => {
                        let mut s = cur.clone();
                        s.push(']');
                        s
                    }
                    None => "[]".to_string(),
                };
                Value::Text(s)
            }
            AggregateKind::JsonGroupObject => {
                // Even an empty input produces `{}`.
                let s = match &self.json_object {
                    Some(cur) => {
                        let mut s = cur.clone();
                        s.push('}');
                        s
                    }
                    None => "{}".to_string(),
                };
                Value::Text(s)
            }
            // The window-only built-ins use the mutating [`value_mut`] path; reaching this arm
            // is a bug. Return NULL defensively (matches the empty-frame result for most kinds).
            AggregateKind::RowNumber
            | AggregateKind::Rank
            | AggregateKind::DenseRank
            | AggregateKind::PercentRank
            | AggregateKind::CumeDist
            | AggregateKind::Ntile
            | AggregateKind::FirstValue
            | AggregateKind::LastValue
            | AggregateKind::NthValue
            | AggregateKind::Lead
            | AggregateKind::Lag => Value::Null,
        }
    }

    /// The mutating `xValue` path for the window-only built-in functions whose `xValue` callback
    /// mutates the accumulator (mirrors upstream's `rankValueFunc`, `dense_rankValueFunc`,
    /// `percent_rankValueFunc`, `cume_distValueFunc`, `ntileValueFunc`, and the `first_value`/
    /// `last_value`/`nth_value` finalize-but-not-value functions). The executor dispatches here
    /// for any [`AggregateKind::window_only`] kind; the plain aggregates use the non-mutating
    /// [`value`] path (their `xValue` does not mutate).
    ///
    /// `value_mut` returns the current window value *and* leaves the accumulator in the state
    /// upstream's `xValue` leaves it in (e.g. `rank` resets `nValue = 0` so the next peer group
    /// re-latches). The accumulator is **not** consumed — a subsequent `step()` continues
    /// accumulating, matching the window-function invariant.
    pub fn value_mut(&mut self) -> Value {
        match self.kind {
            AggregateKind::RowNumber => Value::Int(self.count),
            AggregateKind::Rank => {
                // `rankValueFunc`: emit `nValue`, then `nValue = 0` (so the next peer group
                // re-latches on its first row).
                let v = Value::Int(self.n_value);
                self.n_value = 0;
                v
            }
            AggregateKind::DenseRank => {
                // `dense_rankValueFunc`: if `nStep != 0`, `nValue++` and `nStep = 0`; emit
                // `nValue`.
                if self.n_step != 0 {
                    self.n_value += 1;
                    self.n_step = 0;
                }
                Value::Int(self.n_value)
            }
            AggregateKind::PercentRank => {
                // `percent_rankValueFunc`: latch `nValue = nStep`; emit `nValue / (nTotal - 1)`
                // (or 0.0 when `nTotal <= 1`).
                self.n_value = self.n_step;
                if self.n_total > 1 {
                    Value::Real(self.n_value as f64 / (self.n_total - 1) as f64)
                } else {
                    Value::Real(0.0)
                }
            }
            AggregateKind::CumeDist => {
                // `cume_distValueFunc`: emit `nStep / nTotal`. Matches upstream — note `nStep`
                // here is the *inverse-step* counter (advanced by `cume_distInvFunc`), which
                // counts rows up to and including the current peer group.
                if self.n_total > 0 {
                    Value::Real(self.n_step as f64 / self.n_total as f64)
                } else {
                    Value::Real(0.0)
                }
            }
            AggregateKind::Ntile => {
                // `ntileValueFunc`: compute the 1-based bucket index for `iRow` (= `n_step`)
                // given `nParam` buckets over `nTotal` rows. The formula (upstream):
                //   nSize = nTotal / nParam
                //   if nSize == 0: emit iRow + 1
                //   else:
                //     nLarge = nTotal - nParam * nSize
                //     iSmall = nLarge * (nSize + 1)
                //     if iRow < iSmall: emit 1 + iRow / (nSize + 1)
                //     else:             emit 1 + nLarge + (iRow - iSmall) / nSize
                if self.n_param <= 0 {
                    return Value::Null;
                }
                let i_row = self.n_step;
                let n_size = self.n_total / self.n_param;
                if n_size == 0 {
                    Value::Int(i_row + 1)
                } else {
                    let n_large = self.n_total - self.n_param * n_size;
                    let i_small = n_large * (n_size + 1);
                    if i_row < i_small {
                        Value::Int(1 + i_row / (n_size + 1))
                    } else {
                        Value::Int(1 + n_large + (i_row - i_small) / n_size)
                    }
                }
            }
            AggregateKind::FirstValue => {
                // `first_valueValueFunc` is `noopValueFunc` in upstream — the value is only
                // emitted via `first_valueFinalizeFunc` (which clears). But the window codegen
                // (M11.7) uses `AggValue` per peer group, not `AggFinal`, so we return the
                // captured value WITHOUT clearing (so it can be read repeatedly across peer
                // groups in the same partition). The accumulator is reset on partition change
                // by the codegen's `Null` opcode, which clears the `aggregates` entry.
                self.captured.clone().unwrap_or(Value::Null)
            }
            AggregateKind::NthValue => {
                // `nth_valueValueFunc` is `noopValueFunc` in upstream — same rationale as
                // `first_value`: return the captured value without clearing.
                self.captured.clone().unwrap_or(Value::Null)
            }
            AggregateKind::LastValue => {
                // `last_valueValueFunc`: emit the captured value (without clearing — `inverse`
                // handles clearing when the frame empties). Matches upstream's
                // `last_valueValueFunc`.
                self.captured.clone().unwrap_or(Value::Null)
            }
            // Plain aggregates never reach here (the executor dispatches them through `value`).
            // Returning NULL defensively is correct for an empty accumulator and unreachable in
            // a well-formed program.
            _ => Value::Null,
        }
    }
}

/// Render an SQL [`Value`] as a JSON value per the "value argument" rule of the JSON1 docs
/// (§3.4): NULL → `null`, INTEGER → decimal text, REAL → `fp_to_text`, TEXT → a quoted JSON
/// string (via `json::render_string`), BLOB → error. This is the same logic as
/// `json::append_sql_value` but inlined here so the aggregate module doesn't need a `mut`
/// Result-returning helper. The JSON-subtype-aware "value is JSON if it came from a JSON
/// function" rule is M24.20 and not yet modeled — a TEXT value is always a quoted string.
fn json_render_value_arg(arg: &Value, out: &mut String) {
    match arg {
        Value::Null => out.push_str("null"),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::Real(r) => out.push_str(&crate::util::fp::fp_to_text(*r)),
        Value::Text(s) => crate::func::json::render_string(s, out),
        Value::Blob(_) => {
            // Mirrors `append_sql_value`'s error; the aggregate step swallows the error
            // silently (the row's value becomes nothing) rather than aborting the query —
            // upstream raises the error. For now we push `null` so the array/object stays
            // well-formed; the M24.20 subtype work will surface the error properly.
            out.push_str("null");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn i(n: i64) -> Value {
        Value::Int(n)
    }
    fn r(n: f64) -> Value {
        Value::Real(n)
    }
    fn t(s: &str) -> Value {
        Value::Text(s.to_string())
    }

    #[test]
    fn count_star_counts_nulls() {
        let mut acc = Accumulator::new(AggregateKind::Count);
        acc.step(&[Value::Null], true);
        acc.step(&[Value::Null], true);
        acc.step(&[i(5)], true);
        assert_eq!(acc.count, 3);
    }

    #[test]
    fn count_expr_skips_nulls() {
        let mut acc = Accumulator::new(AggregateKind::Count);
        acc.step(&[Value::Null], false);
        acc.step(&[i(5)], false);
        acc.step(&[t("x")], false);
        assert_eq!(acc.count, 2);
    }

    #[test]
    fn sum_integer_stays_integer() {
        let mut acc = Accumulator::new(AggregateKind::Sum);
        acc.step(&[i(1)], false);
        acc.step(&[i(2)], false);
        acc.step(&[i(3)], false);
        assert!(!acc.has_real);
        assert_eq!(acc.sum_i, 6);
        assert_eq!(acc.count, 3);
    }

    #[test]
    fn sum_promotes_to_real_on_real_input() {
        let mut acc = Accumulator::new(AggregateKind::Sum);
        acc.step(&[i(1)], false);
        acc.step(&[r(2.5)], false);
        assert!(acc.has_real);
        assert!((acc.sum_r - 3.5).abs() < 1e-12);
    }

    #[test]
    fn sum_promotes_to_real_on_overflow() {
        let mut acc = Accumulator::new(AggregateKind::Sum);
        acc.step(&[i(i64::MAX)], false);
        acc.step(&[i(1)], false);
        assert!(acc.has_real);
        assert!(acc.overflowed);
    }

    #[test]
    fn min_max_pick_extremum() {
        let mut acc = Accumulator::new(AggregateKind::Min);
        acc.step(&[i(3)], false);
        acc.step(&[i(1)], false);
        acc.step(&[i(2)], false);
        assert_eq!(acc.best, Some(i(1)));

        let mut acc = Accumulator::new(AggregateKind::Max);
        acc.step(&[i(3)], false);
        acc.step(&[i(1)], false);
        acc.step(&[i(2)], false);
        assert_eq!(acc.best, Some(i(3)));
    }

    #[test]
    fn min_max_skip_nulls() {
        let mut acc = Accumulator::new(AggregateKind::Max);
        acc.step(&[Value::Null], false);
        acc.step(&[i(5)], false);
        acc.step(&[Value::Null], false);
        assert_eq!(acc.best, Some(i(5)));
        assert_eq!(acc.count, 1);
    }

    #[test]
    fn group_concat_default_separator() {
        let mut acc = Accumulator::new(AggregateKind::GroupConcat);
        acc.step(&[t("a")], false);
        acc.step(&[t("b")], false);
        acc.step(&[t("c")], false);
        assert_eq!(acc.concat.as_deref(), Some("a,b,c"));
        assert_eq!(acc.sep, ",");
    }

    #[test]
    fn group_concat_custom_separator() {
        let mut acc = Accumulator::new(AggregateKind::GroupConcat);
        acc.step(&[t("a"), t("--")], false);
        acc.step(&[t("b"), t("--")], false);
        acc.step(&[t("c"), t("--")], false);
        assert_eq!(acc.concat.as_deref(), Some("a--b--c"));
    }

    #[test]
    fn group_concat_skips_nulls() {
        let mut acc = Accumulator::new(AggregateKind::GroupConcat);
        acc.step(&[Value::Null], false);
        acc.step(&[t("a")], false);
        acc.step(&[Value::Null], false);
        acc.step(&[t("b")], false);
        assert_eq!(acc.concat.as_deref(), Some("a,b"));
    }

    #[test]
    fn group_concat_empty_yields_none() {
        let acc = Accumulator::new(AggregateKind::GroupConcat);
        assert!(acc.concat.is_none());
        assert_eq!(acc.count, 0);
    }

    #[test]
    fn from_name_resolves_aggregates() {
        assert_eq!(
            AggregateKind::from_name("count", 0),
            Some(AggregateKind::Count)
        );
        assert_eq!(
            AggregateKind::from_name("COUNT", 1),
            Some(AggregateKind::Count)
        );
        assert_eq!(
            AggregateKind::from_name("sum", 1),
            Some(AggregateKind::Sum)
        );
        assert_eq!(
            AggregateKind::from_name("total", 1),
            Some(AggregateKind::Total)
        );
        assert_eq!(
            AggregateKind::from_name("avg", 1),
            Some(AggregateKind::Avg)
        );
        assert_eq!(
            AggregateKind::from_name("min", 1),
            Some(AggregateKind::Min)
        );
        assert_eq!(
            AggregateKind::from_name("max", 1),
            Some(AggregateKind::Max)
        );
        assert_eq!(
            AggregateKind::from_name("group_concat", 1),
            Some(AggregateKind::GroupConcat)
        );
        assert_eq!(
            AggregateKind::from_name("group_concat", 2),
            Some(AggregateKind::GroupConcat)
        );
        assert_eq!(
            AggregateKind::from_name("string_agg", 2),
            Some(AggregateKind::GroupConcat)
        );
        // Wrong arities / unknown names.
        assert_eq!(AggregateKind::from_name("count", 2), None);
        assert_eq!(AggregateKind::from_name("sum", 0), None);
        assert_eq!(AggregateKind::from_name("avg", 2), None);
        assert_eq!(AggregateKind::from_name("group_concat", 0), None);
        assert_eq!(AggregateKind::from_name("abs", 1), None);
    }

    #[test]
    fn sum_skips_nulls() {
        let mut acc = Accumulator::new(AggregateKind::Sum);
        acc.step(&[Value::Null], false);
        acc.step(&[i(2)], false);
        acc.step(&[Value::Null], false);
        assert_eq!(acc.count, 1);
        assert_eq!(acc.sum_i, 2);
    }

    // ---- inverse / value (M11.3) ----

    #[test]
    fn count_inverse_decrements() {
        let mut acc = Accumulator::new(AggregateKind::Count);
        acc.step(&[Value::Null], true);
        acc.step(&[Value::Null], true);
        acc.step(&[Value::Null], true);
        assert_eq!(acc.count, 3);
        assert_eq!(acc.value(), Value::Int(3));
        acc.inverse(&[Value::Null], true);
        assert_eq!(acc.count, 2);
        assert_eq!(acc.value(), Value::Int(2));
    }

    #[test]
    fn count_expr_inverse_skips_nulls() {
        let mut acc = Accumulator::new(AggregateKind::Count);
        acc.step(&[i(5)], false);
        acc.step(&[Value::Null], false);
        acc.step(&[t("x")], false);
        assert_eq!(acc.count, 2);
        // Inversing a NULL arg should not decrement (matches step's NULL-skip).
        acc.inverse(&[Value::Null], false);
        assert_eq!(acc.count, 2);
        acc.inverse(&[i(5)], false);
        assert_eq!(acc.count, 1);
    }

    #[test]
    fn sum_inverse_subtracts() {
        let mut acc = Accumulator::new(AggregateKind::Sum);
        acc.step(&[i(10)], false);
        acc.step(&[i(20)], false);
        acc.step(&[i(30)], false);
        assert_eq!(acc.value(), Value::Int(60));
        acc.inverse(&[i(10)], false);
        assert_eq!(acc.value(), Value::Int(50));
        acc.inverse(&[i(20)], false);
        assert_eq!(acc.value(), Value::Int(30));
    }

    #[test]
    fn sum_inverse_promotes_to_real_on_overflow() {
        let mut acc = Accumulator::new(AggregateKind::Sum);
        acc.step(&[i(i64::MIN)], false);
        // Inverse a positive value that would underflow i64 if subtracted in i64 space.
        // i64::MIN - 1 underflows → promotes to REAL.
        acc.inverse(&[i(1)], false);
        assert!(acc.has_real);
        assert!(acc.overflowed);
        // i64::MIN - 1 = -9223372036854775809 (exactly representable as f64).
        assert!((acc.sum_r - (i64::MIN as f64 - 1.0)).abs() < 1e-6);
    }

    #[test]
    fn total_inverse_subtracts() {
        let mut acc = Accumulator::new(AggregateKind::Total);
        acc.step(&[i(10)], false);
        acc.step(&[i(20)], false);
        // total() is always REAL even when inputs are integers.
        assert_eq!(acc.value(), Value::Real(30.0));
        acc.inverse(&[i(10)], false);
        assert_eq!(acc.value(), Value::Real(20.0));
    }

    #[test]
    fn avg_inverse_updates_count_and_sum() {
        let mut acc = Accumulator::new(AggregateKind::Avg);
        acc.step(&[i(10)], false);
        acc.step(&[i(20)], false);
        acc.step(&[i(30)], false);
        assert_eq!(acc.value(), Value::Real(20.0)); // (10+20+30)/3 = 20
        acc.inverse(&[i(10)], false);
        assert_eq!(acc.value(), Value::Real(25.0)); // (20+30)/2 = 25
    }

    #[test]
    fn sum_inverse_null_arg_is_noop() {
        let mut acc = Accumulator::new(AggregateKind::Sum);
        acc.step(&[i(5)], false);
        acc.inverse(&[Value::Null], false);
        assert_eq!(acc.count, 1);
        assert_eq!(acc.value(), Value::Int(5));
    }

    #[test]
    fn group_concat_inverse_drops_oldest() {
        let mut acc = Accumulator::new(AggregateKind::GroupConcat);
        acc.step(&[t("a")], false);
        acc.step(&[t("b")], false);
        acc.step(&[t("c")], false);
        assert_eq!(acc.value(), Value::Text("a,b,c".to_string()));
        // Inverse drops the OLDEST value ("a") — the frame start advances forward.
        acc.inverse(&[t("a")], false);
        assert_eq!(acc.value(), Value::Text("b,c".to_string()));
        acc.inverse(&[t("b")], false);
        assert_eq!(acc.value(), Value::Text("c".to_string()));
        // Inverse of the last value leaves an empty (NULL) accumulator.
        acc.inverse(&[t("c")], false);
        assert_eq!(acc.value(), Value::Null);
    }

    #[test]
    fn group_concat_inverse_with_custom_separator() {
        let mut acc = Accumulator::new(AggregateKind::GroupConcat);
        acc.step(&[t("a"), t("--")], false);
        acc.step(&[t("b"), t("--")], false);
        acc.step(&[t("c"), t("--")], false);
        assert_eq!(acc.value(), Value::Text("a--b--c".to_string()));
        acc.inverse(&[t("a")], false);
        assert_eq!(acc.value(), Value::Text("b--c".to_string()));
    }

    #[test]
    fn value_does_not_consume_accumulator() {
        // The key window-function invariant: `value()` reads the current state without
        // finalizing, so a subsequent `step()` continues accumulating.
        let mut acc = Accumulator::new(AggregateKind::Sum);
        acc.step(&[i(1)], false);
        assert_eq!(acc.value(), Value::Int(1));
        acc.step(&[i(2)], false);
        assert_eq!(acc.value(), Value::Int(3));
        acc.step(&[i(3)], false);
        assert_eq!(acc.value(), Value::Int(6));
    }

    #[test]
    fn value_empty_accumulator_matches_finalize() {
        assert_eq!(
            Accumulator::new(AggregateKind::Count).value(),
            Value::Int(0)
        );
        assert_eq!(
            Accumulator::new(AggregateKind::Sum).value(),
            Value::Null
        );
        assert_eq!(
            Accumulator::new(AggregateKind::Total).value(),
            Value::Real(0.0)
        );
        assert_eq!(
            Accumulator::new(AggregateKind::Avg).value(),
            Value::Null
        );
        assert_eq!(
            Accumulator::new(AggregateKind::Min).value(),
            Value::Null
        );
        assert_eq!(
            Accumulator::new(AggregateKind::Max).value(),
            Value::Null
        );
        assert_eq!(
            Accumulator::new(AggregateKind::GroupConcat).value(),
            Value::Null
        );
    }

    // ---- window-only built-ins (M11.4–M11.6) ----

    /// `row_number()` increments by 1 on each step. The default frame only grows, so `inverse`
    /// is never emitted; `value_mut` reads the counter without mutation.
    #[test]
    fn row_number_increments() {
        let mut acc = Accumulator::new(AggregateKind::RowNumber);
        assert_eq!(acc.value_mut(), Value::Int(0)); // empty frame
        acc.step(&[], false);
        assert_eq!(acc.value_mut(), Value::Int(1));
        acc.step(&[], false);
        assert_eq!(acc.value_mut(), Value::Int(2));
        acc.step(&[], false);
        assert_eq!(acc.value_mut(), Value::Int(3));
    }

    /// `rank()` latches `nValue = nStep` on the first row of each peer group; `value_mut`
    /// returns the latched value and resets `nValue = 0` so the next peer group re-latches.
    /// The peer-group boundary is detected by `step` setting `nStep` to the new row index;
    /// `nValue == 0` indicates "no peer group started yet" (or just reset by `value_mut`).
    #[test]
    fn rank_latches_and_resets() {
        let mut acc = Accumulator::new(AggregateKind::Rank);
        // 3-row peer group, then a 2-row peer group:
        // row 1: nStep=1, nValue=0 → latch nValue=1
        // row 2: nStep=2, nValue=1 → keep
        // row 3: nStep=3, nValue=1 → keep
        // value_mut → 1, nValue=0
        // row 4: nStep=4, nValue=0 → latch nValue=4
        // row 5: nStep=5, nValue=4 → keep
        // value_mut → 4
        acc.step(&[], false); // row 1
        acc.step(&[], false);
        acc.step(&[], false);
        assert_eq!(acc.value_mut(), Value::Int(1));
        acc.step(&[], false); // row 4
        acc.step(&[], false);
        assert_eq!(acc.value_mut(), Value::Int(4));
    }

    /// `dense_rank()` increments `nValue` by 1 on the first row of each peer group (when
    /// `nStep != 0`), then resets `nStep = 0` so subsequent same-peer rows don't re-increment.
    /// `value_mut` does the increment+reset.
    #[test]
    fn dense_rank_increments_on_peer_change() {
        let mut acc = Accumulator::new(AggregateKind::DenseRank);
        // Peer group 1 (3 rows): step sets nStep=1 each row; value_mut increments nValue to 1
        // on the first call (nStep != 0), resets nStep=0; subsequent calls in the same peer
        // group have nStep == 0 (because step is called *before* value_mut, but only sets
        // nStep=1, not 0 — so the next value_mut within the same peer sees nStep=1 again?).
        // Looking at upstream `dense_rankStepFunc`: it ALWAYS sets `nStep = 1`. So between
        // value_mut calls, step is called once (per row), setting nStep=1 again. The second
        // value_mut within the same peer group would re-increment — but that's NOT how SQLite
        // uses it: the codegen emits exactly one value_mut per row, after step. So:
        //   row 1: step (nStep=1), value_mut (nValue=1, nStep=0) → 1
        //   row 2: step (nStep=1), value_mut (nValue=2, nStep=0) → 2
        // This is wrong for dense_rank! The upstream dense_rank relies on the ORDER BY peer
        // comparison happening *outside* the accumulator (in the VDBE around the calls). The
        // accumulator alone, called without peer context, increments on every row. That
        // matches the upstream behavior — the peer-group detection is the codegen's job, not
        // the accumulator's.
        acc.step(&[], false);
        assert_eq!(acc.value_mut(), Value::Int(1));
        acc.step(&[], false);
        assert_eq!(acc.value_mut(), Value::Int(2));
        acc.step(&[], false);
        assert_eq!(acc.value_mut(), Value::Int(3));
    }

    /// `percent_rank()` computes `nStep / (nTotal - 1)` where `nStep` is the inverse-step
    /// counter (advanced by `inverse`) and `nTotal` is the total rows (counted by `step`).
    /// When `nTotal <= 1` it returns 0.0.
    #[test]
    fn percent_rank_computes_ratio() {
        let mut acc = Accumulator::new(AggregateKind::PercentRank);
        // 4-row partition: percent_rank = i / (n - 1) = i / 3 → 0.0, 0.333, 0.667, 1.0
        // Step counts nTotal; inverse counts nStep (the row's index from the start).
        acc.step(&[], false); // nTotal=1
        acc.step(&[], false); // nTotal=2
        acc.step(&[], false); // nTotal=3
        acc.step(&[], false); // nTotal=4
        // nStep starts at 0 (no inverse yet).
        assert_eq!(acc.value_mut(), Value::Real(0.0)); // 0 / 3 = 0.0
        acc.inverse(&[], false); // nStep=1
        assert!((match acc.value_mut() {
            Value::Real(r) => r,
            _ => f64::NAN,
        } - 1.0 / 3.0).abs() < 1e-12);
        acc.inverse(&[], false); // nStep=2
        assert!((match acc.value_mut() {
            Value::Real(r) => r,
            _ => f64::NAN,
        } - 2.0 / 3.0).abs() < 1e-12);
        acc.inverse(&[], false); // nStep=3
        assert_eq!(acc.value_mut(), Value::Real(1.0)); // 3 / 3 = 1.0
    }

    /// `percent_rank()` on a 1-row partition returns 0.0 (no division by zero).
    #[test]
    fn percent_rank_single_row_returns_zero() {
        let mut acc = Accumulator::new(AggregateKind::PercentRank);
        acc.step(&[], false); // nTotal=1
        assert_eq!(acc.value_mut(), Value::Real(0.0));
    }

    /// `cume_dist()` computes `nStep / nTotal` where `nStep` is the inverse-step counter (rows
    /// up to and including the current peer group) and `nTotal` is the partition row count.
    #[test]
    fn cume_dist_computes_ratio() {
        let mut acc = Accumulator::new(AggregateKind::CumeDist);
        // 4-row partition: cume_dist = i / 4 → 0.25, 0.5, 0.75, 1.0
        acc.step(&[], false);
        acc.step(&[], false);
        acc.step(&[], false);
        acc.step(&[], false); // nTotal=4
        // nStep starts at 0.
        // After 1 inverse: nStep=1, cume_dist = 1/4 = 0.25
        acc.inverse(&[], false);
        assert!((match acc.value_mut() {
            Value::Real(r) => r,
            _ => f64::NAN,
        } - 0.25).abs() < 1e-12);
        acc.inverse(&[], false); // nStep=2
        assert!((match acc.value_mut() {
            Value::Real(r) => r,
            _ => f64::NAN,
        } - 0.5).abs() < 1e-12);
        acc.inverse(&[], false); // nStep=3
        assert!((match acc.value_mut() {
            Value::Real(r) => r,
            _ => f64::NAN,
        } - 0.75).abs() < 1e-12);
        acc.inverse(&[], false); // nStep=4
        assert_eq!(acc.value_mut(), Value::Real(1.0));
    }

    /// `ntile(N)` divides the partition into N buckets. With 7 rows and N=3: bucket sizes are
    /// 3, 2, 2 (nLarge=1, nSize=2; iSmall=1*(2+1)=3; rows 0..2 → bucket 1, rows 3..4 → bucket 2,
    /// rows 5..6 → bucket 3). The inverse-step counter `iRow` (= n_step) is the row index.
    #[test]
    fn ntile_distributes_evenly() {
        let mut acc = Accumulator::new(AggregateKind::Ntile);
        // nParam=3, 7 rows total (nTotal=7 after 7 steps).
        acc.step(&[i(3)], false); // first step captures nParam=3
        for _ in 0..6 {
            acc.step(&[i(3)], false);
        }
        // nTotal=7, nParam=3, nSize=2, nLarge=1, iSmall=3
        // iRow=0 (no inverse yet): 0 < 3 → 1 + 0/3 = 1
        assert_eq!(acc.value_mut(), Value::Int(1));
        acc.inverse(&[i(3)], false); // iRow=1
        assert_eq!(acc.value_mut(), Value::Int(1)); // 1 + 1/3 = 1
        acc.inverse(&[i(3)], false); // iRow=2
        assert_eq!(acc.value_mut(), Value::Int(1)); // 1 + 2/3 = 1
        acc.inverse(&[i(3)], false); // iRow=3
        assert_eq!(acc.value_mut(), Value::Int(2)); // 1 + 1 + (3-3)/2 = 2
        acc.inverse(&[i(3)], false); // iRow=4
        assert_eq!(acc.value_mut(), Value::Int(2)); // 1 + 1 + 1/2 = 2
        acc.inverse(&[i(3)], false); // iRow=5
        assert_eq!(acc.value_mut(), Value::Int(3)); // 1 + 1 + 2/2 = 3
        acc.inverse(&[i(3)], false); // iRow=6
        assert_eq!(acc.value_mut(), Value::Int(3)); // 1 + 1 + 3/2 = 3 (integer div)
    }

    /// `ntile(N)` with N >= nTotal gives 1 row per bucket (nSize=0 → emit iRow+1).
    #[test]
    fn ntile_more_buckets_than_rows() {
        let mut acc = Accumulator::new(AggregateKind::Ntile);
        acc.step(&[i(10)], false); // nParam=10
        acc.step(&[i(10)], false);
        acc.step(&[i(10)], false); // nTotal=3, nParam=10, nSize=0
        // iRow=0: emit 0+1 = 1
        assert_eq!(acc.value_mut(), Value::Int(1));
        acc.inverse(&[i(10)], false); // iRow=1
        assert_eq!(acc.value_mut(), Value::Int(2));
        acc.inverse(&[i(10)], false); // iRow=2
        assert_eq!(acc.value_mut(), Value::Int(3));
    }

    /// `first_value(expr)` captures the first row's argument and never overwrites it.
    #[test]
    fn first_value_captures_first() {
        let mut acc = Accumulator::new(AggregateKind::FirstValue);
        acc.step(&[i(10)], false);
        acc.step(&[i(20)], false);
        acc.step(&[i(30)], false);
        assert_eq!(acc.value_mut(), Value::Int(10));
        // value_mut for first_value does NOT clear the captured value (the window codegen
        // reads it across peer groups; the accumulator is reset by the codegen's `Null`
        // opcode on a partition change). A subsequent value_mut returns the same value.
        assert_eq!(acc.value_mut(), Value::Int(10));
    }

    /// `last_value(expr)` captures each row's argument (overwriting); `value_mut` emits the
    /// currently-captured value without clearing.
    #[test]
    fn last_value_captures_latest() {
        let mut acc = Accumulator::new(AggregateKind::LastValue);
        acc.step(&[i(10)], false);
        assert_eq!(acc.value_mut(), Value::Int(10));
        acc.step(&[i(20)], false);
        assert_eq!(acc.value_mut(), Value::Int(20));
        acc.step(&[i(30)], false);
        assert_eq!(acc.value_mut(), Value::Int(30));
    }

    /// `last_value(expr)` inverse decrements the counter; when it reaches 0, the captured
    /// value is cleared (so a subsequent value_mut returns NULL — the frame is empty).
    #[test]
    fn last_value_inverse_clears_when_empty() {
        let mut acc = Accumulator::new(AggregateKind::LastValue);
        acc.step(&[i(10)], false); // nVal=1, captured=10
        acc.step(&[i(20)], false); // nVal=2, captured=20
        acc.inverse(&[i(20)], false); // nVal=1, captured still set
        assert_eq!(acc.value_mut(), Value::Int(20));
        acc.inverse(&[i(10)], false); // nVal=0, captured cleared
        assert_eq!(acc.value_mut(), Value::Null);
    }

    /// `nth_value(expr, N)` captures the argument when the row counter equals N.
    #[test]
    fn nth_value_captures_nth() {
        let mut acc = Accumulator::new(AggregateKind::NthValue);
        // N=2: capture on the 2nd row.
        acc.step(&[i(10), i(2)], false); // nth_step=1
        acc.step(&[i(20), i(2)], false); // nth_step=2 → capture 20
        acc.step(&[i(30), i(2)], false); // nth_step=3 → already captured (overwritten? no)
        // Actually nth_value only captures when nth_step == N; subsequent rows don't re-capture.
        assert_eq!(acc.value_mut(), Value::Int(20));
    }

    /// `nth_value(expr, N)` with N larger than the frame returns NULL (no row matched N).
    #[test]
    fn nth_value_null_when_n_too_large() {
        let mut acc = Accumulator::new(AggregateKind::NthValue);
        acc.step(&[i(10), i(5)], false); // nth_step=1
        acc.step(&[i(20), i(5)], false); // nth_step=2
        // N=5 but only 2 rows stepped — captured is None.
        assert_eq!(acc.value_mut(), Value::Null);
    }

    /// `AggregateKind::from_name` resolves the window-only names at their accepted arities.
    #[test]
    fn from_name_resolves_window_only() {
        assert_eq!(
            AggregateKind::from_name("row_number", 0),
            Some(AggregateKind::RowNumber)
        );
        assert_eq!(
            AggregateKind::from_name("ROW_NUMBER", 0),
            Some(AggregateKind::RowNumber)
        );
        assert_eq!(
            AggregateKind::from_name("rank", 0),
            Some(AggregateKind::Rank)
        );
        assert_eq!(
            AggregateKind::from_name("dense_rank", 0),
            Some(AggregateKind::DenseRank)
        );
        assert_eq!(
            AggregateKind::from_name("percent_rank", 0),
            Some(AggregateKind::PercentRank)
        );
        assert_eq!(
            AggregateKind::from_name("cume_dist", 0),
            Some(AggregateKind::CumeDist)
        );
        assert_eq!(
            AggregateKind::from_name("ntile", 1),
            Some(AggregateKind::Ntile)
        );
        assert_eq!(
            AggregateKind::from_name("first_value", 1),
            Some(AggregateKind::FirstValue)
        );
        assert_eq!(
            AggregateKind::from_name("last_value", 1),
            Some(AggregateKind::LastValue)
        );
        assert_eq!(
            AggregateKind::from_name("nth_value", 2),
            Some(AggregateKind::NthValue)
        );
        assert_eq!(
            AggregateKind::from_name("lead", 1),
            Some(AggregateKind::Lead)
        );
        assert_eq!(
            AggregateKind::from_name("lead", 3),
            Some(AggregateKind::Lead)
        );
        assert_eq!(
            AggregateKind::from_name("lag", 1),
            Some(AggregateKind::Lag)
        );
        // Wrong arities.
        assert_eq!(AggregateKind::from_name("row_number", 1), None);
        assert_eq!(AggregateKind::from_name("ntile", 0), None);
        assert_eq!(AggregateKind::from_name("nth_value", 1), None);
        assert_eq!(AggregateKind::from_name("lead", 4), None);
    }

    /// `AggregateKind::window_only` correctly classifies which kinds need an `OVER` clause.
    #[test]
    fn window_only_classification() {
        // Plain aggregates: not window-only.
        assert!(!AggregateKind::Count.window_only());
        assert!(!AggregateKind::Sum.window_only());
        assert!(!AggregateKind::GroupConcat.window_only());
        // Window-only built-ins.
        assert!(AggregateKind::RowNumber.window_only());
        assert!(AggregateKind::Rank.window_only());
        assert!(AggregateKind::DenseRank.window_only());
        assert!(AggregateKind::PercentRank.window_only());
        assert!(AggregateKind::CumeDist.window_only());
        assert!(AggregateKind::Ntile.window_only());
        assert!(AggregateKind::FirstValue.window_only());
        assert!(AggregateKind::LastValue.window_only());
        assert!(AggregateKind::NthValue.window_only());
        assert!(AggregateKind::Lead.window_only());
        assert!(AggregateKind::Lag.window_only());
    }

    /// `AggregateKind::default_frame` returns the upstream-coerced frame for each built-in.
    #[test]
    fn default_frame_matches_upstream() {
        use DefaultFrameBound::*;
        use DefaultFrameMode::*;
        assert_eq!(
            AggregateKind::RowNumber.default_frame(),
            (Rows, UnboundedPreceding, CurrentRow)
        );
        assert_eq!(
            AggregateKind::Rank.default_frame(),
            (Range, UnboundedPreceding, CurrentRow)
        );
        assert_eq!(
            AggregateKind::DenseRank.default_frame(),
            (Range, UnboundedPreceding, CurrentRow)
        );
        assert_eq!(
            AggregateKind::PercentRank.default_frame(),
            (Groups, CurrentRow, UnboundedFollowing)
        );
        assert_eq!(
            AggregateKind::CumeDist.default_frame(),
            (Groups, Following(1), UnboundedFollowing)
        );
        assert_eq!(
            AggregateKind::Ntile.default_frame(),
            (Rows, CurrentRow, UnboundedFollowing)
        );
        assert_eq!(
            AggregateKind::Lead.default_frame(),
            (Rows, UnboundedPreceding, UnboundedFollowing)
        );
        assert_eq!(
            AggregateKind::Lag.default_frame(),
            (Rows, UnboundedPreceding, CurrentRow)
        );
    }

    // ---- M24.18 / M24.19 json_group_array / json_group_object ----

    #[test]
    fn json_group_array_collects_all_rows() {
        let mut acc = Accumulator::new(AggregateKind::JsonGroupArray);
        acc.step(&[Value::Int(1)], false);
        acc.step(&[Value::Int(2)], false);
        acc.step(&[Value::Null], false); // NULLs are included
        acc.step(&[Value::Text("x".into())], false);
        assert_eq!(acc.value(), Value::Text("[1,2,null,\"x\"]".into()));
    }

    #[test]
    fn json_group_array_empty_is_empty_array() {
        let acc = Accumulator::new(AggregateKind::JsonGroupArray);
        assert_eq!(acc.value(), Value::Text("[]".into()));
    }

    #[test]
    fn json_group_object_collects_pairs() {
        let mut acc = Accumulator::new(AggregateKind::JsonGroupObject);
        acc.step(&[Value::Text("a".into()), Value::Int(1)], false);
        acc.step(&[Value::Text("b".into()), Value::Text("x".into())], false);
        acc.step(&[Value::Text("c".into()), Value::Null], false);
        assert_eq!(acc.value(), Value::Text("{\"a\":1,\"b\":\"x\",\"c\":null}".into()));
    }

    #[test]
    fn json_group_object_skips_null_name() {
        let mut acc = Accumulator::new(AggregateKind::JsonGroupObject);
        acc.step(&[Value::Text("a".into()), Value::Int(1)], false);
        acc.step(&[Value::Null, Value::Int(2)], false); // NULL name → skipped
        acc.step(&[Value::Text("c".into()), Value::Int(3)], false);
        assert_eq!(acc.value(), Value::Text("{\"a\":1,\"c\":3}".into()));
    }

    #[test]
    fn json_group_object_empty_is_empty_object() {
        let acc = Accumulator::new(AggregateKind::JsonGroupObject);
        assert_eq!(acc.value(), Value::Text("{}".into()));
    }
}