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

/// The built-in aggregate kinds. The name matches the SQL function name (case-insensitively
/// resolved at codegen time and stored here verbatim for the executor's dispatch).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregateKind {
    Count,
    Sum,
    Total,
    Avg,
    Min,
    Max,
    GroupConcat,
}

impl AggregateKind {
    /// Resolve a function name (case-insensitive) and argument count to an aggregate kind, or
    /// `None` if `name` is not a built-in aggregate. `count(*)` is encoded as `count` with zero
    /// arguments at the AST layer; the codegen path passes `n_arg == 0` for the star form and
    /// `n_arg == 1` for the regular form, so both shapes map to [`AggregateKind::Count`].
    pub fn from_name(name: &str, n_arg: usize) -> Option<AggregateKind> {
        let lname = name.to_ascii_lowercase();
        match lname.as_str() {
            "count" if n_arg == 0 || n_arg == 1 => Some(AggregateKind::Count),
            "sum" if n_arg == 1 => Some(AggregateKind::Sum),
            "total" if n_arg == 1 => Some(AggregateKind::Total),
            "avg" if n_arg == 1 => Some(AggregateKind::Avg),
            "min" if n_arg == 1 => Some(AggregateKind::Min),
            "max" if n_arg == 1 => Some(AggregateKind::Max),
            "group_concat" | "string_agg" if n_arg == 1 || n_arg == 2 => {
                Some(AggregateKind::GroupConcat)
            }
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
        }
    }

    /// The fixed argument count used to disambiguate the aggregate's step function. `count(*)`
    /// is represented with `n_arg == 0`; the rest take 1 (or 2 for `group_concat`'s separator).
    pub fn n_arg(self) -> usize {
        match self {
            AggregateKind::Count => 1, // `count(expr)`; the star form passes a sentinel
            AggregateKind::Sum | AggregateKind::Total | AggregateKind::Avg | AggregateKind::Min
            | AggregateKind::Max => 1,
            AggregateKind::GroupConcat => 2,
        }
    }
}

/// `true` if `name` is a built-in aggregate at any arity (used by the codegen path to detect
/// aggregate calls in the projection list without yet knowing the argument count). The
/// per-arity check is done later by [`AggregateKind::from_name`].
pub fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "count" | "sum" | "total" | "avg" | "min" | "max" | "group_concat" | "string_agg"
    )
}

/// The per-group accumulator state for a built-in aggregate. Stored as `Value::Aggregate` in
/// the register file so a single `AggStep`/`AggFinal` pair can update it in place.
#[derive(Clone, Debug)]
pub struct Accumulator {
    pub kind: AggregateKind,
    /// Number of rows that contributed a non-NULL argument (or, for `count(*)`, every row).
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
}