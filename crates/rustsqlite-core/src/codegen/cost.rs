//! SQLite-style LogEst cost model for the index planner (M27.1).
//!
//! This is a faithful port of the logarithmic-cost arithmetic and the
//! `whereLoopAddBtreeIndex` / `whereLoopAddBtree` cost equations from
//! upstream's `where.c` and `util.c` (SQLite 3.53.x). The planner ranks
//! candidate index plans by a `(rRun, nOut)` cost pair instead of the
//! simple `(eq_len, covering, order_by_satisfied)` score used in the M5.2
//! slice, so the engine's index choice matches the oracle's on the
//! common cases (covering vs non-covering, single-eq vs multi-eq, ties
//! broken in favor of the later-defined index — exactly what SQLite's
//! `whereLoopFindLesser` produces with its strict `>=` comparisons).
//!
//! `LogEst` is `10*log2(x)` (an i16/i32 in upstream; we use `i64` for
//! convenience). `log_est_add(a, b)` ≈ `a + b` in linear space. `est_log(n)`
//! is `log2(n)` in LogEst units.
//!
//! When `sqlite_stat1` data is unavailable (the current state — ANALYZE is
//! M22.4 [BLOCKED]), upstream uses default per-prefix row estimates:
//!   `aiRowLogEst[0] = 200` (≈ 1M rows on the table),
//!   `aiRowLogEst[1..=5] = [33, 32, 30, 28, 26]` (≈ 10 distinct values per
//!   column), and `aiRowLogEst[6..] = 23`. We mirror those defaults so the
//!   relative ranking of plans matches the oracle's until real statistics
//!   land.

/// A SQLite `LogEst` value: `10*log2(x)` as an integer (upstream uses `i16`).
/// We use `i64` for arithmetic convenience; the values stay well within `i16`
/// range for any plausible table size.
pub(crate) type LogEst = i64;

/// Convert an integer row count to a `LogEst` (mirrors `sqlite3LogEst` in
/// `util.c`). Returns `10*log2(x)` (clamped to 0 for `x < 2`).
pub(crate) fn log_est(x: u64) -> LogEst {
    // The upstream table is `{ 0, 2, 3, 5, 6, 7, 8, 9 }` for x&7, plus 40
    // per shift of 4 (or 10 per shift of 1). We implement the same constant
    // table; the result is `10*log2(x)` rounded to the nearest LogEst.
    const A: [LogEst; 8] = [0, 2, 3, 5, 6, 7, 8, 9];
    if x < 2 {
        return 0;
    }
    let mut y: LogEst = 40;
    let mut v = x;
    if v < 8 {
        while v < 8 {
            y -= 10;
            v <<= 1;
        }
    } else {
        // Shift down by 4 bits at a time (40 per shift), then 1 bit (10).
        while v > 255 {
            y += 40;
            v >>= 4;
        }
        while v > 15 {
            y += 10;
            v >>= 1;
        }
    }
    A[(v & 7) as usize] + y - 10
}

/// Add two LogEst values (mirrors `sqlite3LogEstAdd` in `util.c`). The sum is
/// `log(a+b) = max(a,b) + log(1 + 2^(-|a-b|))`, approximated by the upstream
/// lookup table.
pub(crate) fn log_est_add(a: LogEst, b: LogEst) -> LogEst {
    const X: [u8; 32] = [
        10, 10, // 0,1
        9, 9, // 2,3
        8, 8, // 4,5
        7, 7, 7, // 6,7,8
        6, 6, 6, // 9,10,11
        5, 5, 5, // 12-14
        4, 4, 4, 4, // 15-18
        3, 3, 3, 3, 3, 3, // 19-24
        2, 2, 2, 2, 2, 2, 2, // 25-31
    ];
    if a >= b {
        if a > b + 49 {
            return a;
        }
        if a > b + 31 {
            return a + 1;
        }
        return a + X[(a - b) as usize] as LogEst;
    } else {
        if b > a + 49 {
            return b;
        }
        if b > a + 31 {
            return b + 1;
        }
        return b + X[(b - a) as usize] as LogEst;
    }
}

/// Estimate `log2(N)` in LogEst units (mirrors `estLog` in `where.c`).
/// `estLog(N) = 0` for `N <= 10`, otherwise `log_est(N) - 33` (the `-33`
/// accounts for the `10*log2` scale: `log2(N) = (10*log2(N) - 33)/10 + 3.3`
/// ≈ for the small-N regime).
pub(crate) fn est_log(n: LogEst) -> LogEst {
    if n <= 10 {
        0
    } else {
        log_est(n as u64) - 33
    }
}

/// The default `aiRowLogEst[]` array for an index with no `sqlite_stat1` data.
/// Mirrors `sqlite3DefaultRowEst` in `build.c`: `a[0] = 200` (1M rows on the
/// table, the upstream default `nRowLogEst`), `a[1..=5] = [33, 32, 30, 28, 26]`
/// (≈ 10 distinct values per leading column), `a[6..] = 23`.
///
/// The returned vector has `n_key_cols + 1` entries: `a[0]` is the total row
/// count for the table, `a[k]` is the estimated number of rows matching a
/// particular value of the first `k` index columns.
pub(crate) fn default_ai_row_log_est(n_key_cols: usize) -> Vec<LogEst> {
    const A_VAL: [LogEst; 5] = [33, 32, 30, 28, 26];
    let mut a = Vec::with_capacity(n_key_cols + 1);
    a.push(200); // a[0] = table.nRowLogEst (1M rows default)
    let n_copy = A_VAL.len().min(n_key_cols);
    for i in 0..n_copy {
        a.push(A_VAL[i]);
    }
    for _ in n_copy + 1..=n_key_cols {
        a.push(23);
    }
    a
}

/// The estimated per-column byte size (`szEst` in upstream's `build.c`).
/// Mirrors `sqlite3AddColumn`: `1` for BLOB-affinity (no type or a BLOB
/// type), `5` for TEXT-affinity columns. The other affinities (INTEGER,
/// REAL, NUMERIC) keep the default `1`. This is the value upstream
/// accumulates into `wTable`/`wIndex` before taking `sqlite3LogEst(w*4)`.
pub(crate) fn column_sz_est(affinity: crate::types::Affinity) -> u64 {
    use crate::types::Affinity;
    match affinity {
        Affinity::Text => 5,
        Affinity::Integer | Affinity::Real | Affinity::Numeric | Affinity::Blob => 1,
    }
}

/// The estimated row width (in bytes, as a LogEst) for a table. Upstream's
/// `estimateTableWidth` sums per-column `szEst` values and adds 1 for the
/// rowid (when the table has a rowid), then takes `sqlite3LogEst(wTable*4)`.
/// We mirror that exactly so the `szIdxRow / szTabRow` ratio used in the
/// index-scan cost equation matches the oracle's.
pub(crate) fn estimated_table_width_log(table: &crate::schema::Table) -> LogEst {
    let mut w: u64 = 0;
    for c in &table.columns {
        w += column_sz_est(c.affinity);
    }
    if !table.without_rowid {
        w += 1; // the rowid
    }
    log_est(w * 4)
}

/// The estimated row width for an index (LogEst). Upstream's
/// `estimateIndexWidth` sums the per-column `szEst` of the indexed columns
/// and appends the rowid (1 byte) for a rowid table's index. We mirror that.
/// Expression-index columns are conservatively estimated at 5 bytes (the
/// TEXT default — most expression indexes produce text/numeric values).
pub(crate) fn estimated_index_width_log(
    index: &crate::schema::IndexObject,
    table: &crate::schema::Table,
) -> LogEst {
    let mut w: u64 = 0;
    for ic in &index.columns {
        if ic.is_expression() {
            w += 5;
            continue;
        }
        if let Some(i) = table.column_index(&ic.name) {
            w += column_sz_est(table.columns[i].affinity);
        } else {
            w += 1;
        }
    }
    if !table.without_rowid {
        w += 1; // the appended rowid column
    }
    log_est(w * 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_est_values_match_upstream() {
        // sqlite3LogEst(1048576) == 200 (the default table row estimate)
        assert_eq!(log_est(1_048_576), 200);
        // sqlite3LogEst(2) == 10
        assert_eq!(log_est(2), 10);
        // sqlite3LogEst(4) == 20
        assert_eq!(log_est(4), 20);
        // sqlite3LogEst(1000) == 99
        assert_eq!(log_est(1000), 99);
        // sqlite3LogEst(5) == 23 (used for the a[6..] default)
        assert_eq!(log_est(5), 23);
        // sqlite3LogEst(25) == 46
        assert_eq!(log_est(25), 46);
        // sqlite3LogEst(18) == 42
        assert_eq!(log_est(18), 42);
        // sqlite3LogEst(20) == 43
        assert_eq!(log_est(20), 43);
    }

    #[test]
    fn log_est_add_matches_upstream() {
        // Adding two equal LogEsts doubles the linear count (adds ~10 LogEst).
        // log_est_add(200, 200) ≈ 210.
        assert_eq!(log_est_add(200, 200), 210);
        // Adding a tiny value to a large one is the large one.
        assert_eq!(log_est_add(200, 0), 200);
        // Commutative.
        assert_eq!(log_est_add(0, 200), 200);
        // log_est_add(100, 100) == 110.
        assert_eq!(log_est_add(100, 100), 110);
    }

    #[test]
    fn est_log_matches_upstream() {
        // estLog(N<=10) == 0
        assert_eq!(est_log(0), 0);
        assert_eq!(est_log(10), 0);
        // estLog(200) == log_est(200) - 33 == 146 - 33 == 113
        assert_eq!(est_log(200), log_est(200) - 33);
    }

    #[test]
    fn default_ai_row_log_est_shape() {
        let a = default_ai_row_log_est(2);
        assert_eq!(a.len(), 3);
        assert_eq!(a[0], 200);
        assert_eq!(a[1], 33);
        assert_eq!(a[2], 32);

        let a = default_ai_row_log_est(7);
        assert_eq!(a.len(), 8);
        assert_eq!(a[0], 200);
        assert_eq!(a[1], 33);
        assert_eq!(a[2], 32);
        assert_eq!(a[3], 30);
        assert_eq!(a[4], 28);
        assert_eq!(a[5], 26);
        // a[6] and a[7] are the "beyond 5 columns" default of 23.
        assert_eq!(a[6], 23);
        assert_eq!(a[7], 23);
    }
}