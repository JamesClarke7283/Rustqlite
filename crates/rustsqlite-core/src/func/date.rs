//! Date and time functions (a faithful port of `date.c`).
//!
//! SQLite processes all dates and times as julian-day numbers stored as
//! `iJD = julian_day * 86400000` (milliday units). The parser accepts
//! `YYYY-MM-DD[ HH:MM:SS[.FFF]][Z|±HH:MM]`, `HH:MM:SS[.FFF]`, a bare julian
//! day number, `now`, and `subsec`/`subsecond`; modifiers (`+N days`,
//! `start of month`, `weekday N`, `unixepoch`, `auto`, `localtime`, `utc`,
//! `ceiling`, `floor`, `julianday`, `subsec`/`subsecond`, and
//! `±YYYY-MM-DD[ HH:MM]`) adjust the value before the formatting functions
//! render it. See `https://sqlite.org/lang_datefunc.html` and the upstream
//! `date.c` for the spec.
//!
//! `current_date`, `current_time`, and `current_timestamp` are the zero-arg
//! "now" shortcuts; `now`/`subsec`/`localtime`/`utc` and the `current_*`
//! functions are volatile (depend on wall-clock + locale), so the VDBE
//! executor intercepts them and reaches into a `DateCtx` for the current
//! time. Everything else lives here as pure functions over an already-parsed
//! `DateTime`.

use crate::error::{Error, Result};
use crate::types::Value;

/// `INT_464269060799999` from `date.c` — the maximum legal `iJD`
/// (9999-12-31 23:59:59.999 in millidays).
const MAX_IJD_POS: i64 = 464_269_060_799_999;

/// Millidays per day (used everywhere).
const MS_PER_DAY: i64 = 86_400_000;

/// The julian day for the Unix epoch (1970-01-01 00:00:00 UTC) in millidays.
const UNIX_EPOCH_IJD: i64 = 2_108_667_600 * 100_000; // = 210866760000000

/// A parsed date/time value, mirroring `struct DateTime` in `date.c`.
///
/// All fields are in the same units/semantics as the C struct so the
/// algorithms port verbatim. `iJD` is the julian day * 86_400_000; the Y/M/D
/// and h/m/s fields are the civil-calendar decomposition (lazy: only valid
/// when the corresponding `valid*` flag is set).
#[derive(Clone, Debug)]
pub struct DateTime {
    pub i_jd: i64,
    pub y: i32,
    pub m: i32,
    pub d: i32,
    pub h: i32,
    pub min: i32,
    pub tz: i32,
    pub s: f64,
    pub valid_jd: bool,
    pub valid_ymd: bool,
    pub valid_hms: bool,
    pub n_floor: i32,
    pub raw_s: bool,
    pub is_error: bool,
    pub use_subsec: bool,
    pub is_utc: bool,
    pub is_local: bool,
}

impl Default for DateTime {
    fn default() -> Self {
        DateTime {
            i_jd: 0,
            y: 0,
            m: 0,
            d: 0,
            h: 0,
            min: 0,
            tz: 0,
            s: 0.0,
            valid_jd: false,
            valid_ymd: false,
            valid_hms: false,
            n_floor: 0,
            raw_s: false,
            is_error: false,
            use_subsec: false,
            is_utc: false,
            is_local: false,
        }
    }
}

/// The max-value of `aMx[]` in `date.c::getDigits`: a=12, b=14, c=24, d=31,
/// e=59, f=14712 (but we use 9999 to allow 4-digit years; the C `f` is
/// 14712 because of historical signed-range quirks — using 9999 matches the
/// upstream `getDigits` for years in 0..=9999).
const MAX_M: [i32; 6] = [12, 14, 24, 31, 59, 9999];

/// `getDigits` from `date.c`: parse a sequence of fixed-width integers out
/// of `s` according to `fmt`. `fmt` is a flat list of 4-byte groups:
/// (ndigits, min, max-code, separator) where max-code is `a..=f` mapping to
/// `MAX_M`. The last group is only 3 bytes (no separator) — the upstream C
/// reads `nextC = zFormat[3]` which is the implicit `\0` terminator.
/// Returns the number of successfully-parsed integers, written into `out`.
fn get_digits<'a>(s: &mut &'a str, fmt: &[u8], out: &mut [i32]) -> usize {
    let mut cnt = 0;
    let mut fi = 0;
    loop {
        if fi >= fmt.len() {
            break;
        }
        let ndig = (fmt[fi] - b'0') as usize;
        let min = (fmt[fi + 1] - b'0') as i32;
        let max = MAX_M[(fmt[fi + 2] - b'a') as usize];
        // The last group has only 3 bytes (separator is implicit \0).
        let next_c = if fi + 3 < fmt.len() { fmt[fi + 3] } else { 0 };
        let bytes = s.as_bytes();
        if bytes.len() < ndig {
            break;
        }
        let mut val: i32 = 0;
        let mut ok = true;
        for k in 0..ndig {
            let c = bytes[k];
            if !c.is_ascii_digit() {
                ok = false;
                break;
            }
            val = val * 10 + (c - b'0') as i32;
        }
        if !ok || val < min || val > max {
            break;
        }
        *s = &s[ndig..];
        out[cnt] = val;
        cnt += 1;
        if next_c != 0 {
            // separator must match
            if s.as_bytes().first().copied() != Some(next_c) {
                break;
            }
            *s = &s[1..];
            fi += 4;
        } else {
            // Last group is 3 bytes (no separator); we're done.
            break;
        }
    }
    cnt
}

/// Skip ASCII whitespace.
fn skip_ws(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    &s[i..]
}

/// `parseTimezone` from `date.c`: parse the optional trailing `Z` or
/// `±HH:MM`. Returns `Ok(())` on success (including "no timezone present")
/// and an error on a malformed timezone. Mutates `p.tz`/`p.is_utc`/`p.is_local`.
fn parse_timezone(s: &str, p: &mut DateTime) -> Result<()> {
    let s = skip_ws(s);
    if s.is_empty() {
        p.tz = 0;
        return Ok(());
    }
    let mut rest = s;
    let sgn;
    match rest.as_bytes()[0] {
        b'-' => sgn = -1,
        b'+' => sgn = 1,
        b'Z' | b'z' => {
            rest = &rest[1..];
            p.is_local = false;
            p.is_utc = true;
            let rest = skip_ws(rest);
            if !rest.is_empty() {
                return Err(Error::msg("bad timezone"));
            }
            return Ok(());
        }
        _ => {
            // Not a timezone extension — that's allowed (it's "missing").
            return Err(Error::msg("not a timezone"));
        }
    }
    rest = &rest[1..];
    let mut out = [0i32; 2];
    let got = get_digits(&mut rest, b"20b:20e", &mut out);
    if got != 2 {
        return Err(Error::msg("bad timezone"));
    }
    p.tz = sgn * (out[1] + out[0] * 60);
    if p.tz == 0 {
        p.is_local = false;
        p.is_utc = true;
    }
    let rest = skip_ws(rest);
    if !rest.is_empty() {
        return Err(Error::msg("trailing chars after timezone"));
    }
    Ok(())
}

/// `parseHhMmSs` from `date.c`: parse `HH:MM[:SS[.FFF]]` plus optional tz.
fn parse_hhmmss(s: &str, p: &mut DateTime) -> Result<()> {
    let mut rest = s;
    let mut out = [0i32; 2];
    if get_digits(&mut rest, b"20c:20e", &mut out) != 2 {
        return Err(Error::msg("bad HH:MM"));
    }
    let h = out[0];
    let m = out[1];
    let mut sec: f64 = 0.0;
    let rest_after_min = rest;
    if rest.as_bytes().first() == Some(&b':') {
        rest = &rest[1..];
        let mut s_out = [0i32; 1];
        if get_digits(&mut rest, b"20e", &mut s_out) != 1 {
            return Err(Error::msg("bad seconds"));
        }
        sec = s_out[0] as f64;
        if rest.as_bytes().first() == Some(&b'.')
            && rest.as_bytes().get(1).is_some_and(|c| c.is_ascii_digit())
        {
            rest = &rest[1..];
            let bytes = rest.as_bytes();
            let mut ms: f64 = 0.0;
            let mut scale: f64 = 1.0;
            let mut k = 0;
            while k < bytes.len() && bytes[k].is_ascii_digit() {
                ms = ms * 10.0 + (bytes[k] - b'0') as f64;
                scale *= 10.0;
                k += 1;
            }
            ms /= scale;
            if ms > 0.999 {
                ms = 0.999;
            }
            sec += ms;
            rest = &rest[k..];
        }
    }
    p.valid_jd = false;
    p.raw_s = false;
    p.valid_hms = true;
    p.h = h;
    p.min = m;
    p.s = sec;
    parse_timezone(rest, p).or_else(|e| {
        // "not a timezone" means no timezone present — that's OK.
        if e.message == "not a timezone" {
            p.tz = 0;
            Ok(())
        } else {
            // Trailing chars after HH:MM are an error for the caller.
            Err(e)
        }
    })?;
    let _ = rest_after_min;
    Ok(())
}

/// `datetimeError` from `date.c`: zero the struct and mark the error flag.
fn datetime_error(p: &mut DateTime) {
    *p = DateTime::default();
    p.is_error = true;
}

/// `computeJD` from `date.c`: convert Y/M/D + H/M/S into `iJD` (millidays).
fn compute_jd(p: &mut DateTime) {
    if p.valid_jd {
        return;
    }
    let (mut y, mut m, d);
    if p.valid_ymd {
        y = p.y;
        m = p.m;
        d = p.d;
    } else {
        y = 2000;
        m = 1;
        d = 1;
    }
    if y < -4713 || y > 9999 || p.raw_s {
        datetime_error(p);
        return;
    }
    if m <= 2 {
        y -= 1;
        m += 12;
    }
    let a = (y + 4800) / 100;
    let b = 38 - a + a / 4;
    let x1 = 36525 * (y + 4716) / 100;
    let x2 = 306001 * (m + 1) / 10000;
    p.i_jd = ((x1 as f64 + x2 as f64 + d as f64 + b as f64 - 1524.5) * MS_PER_DAY as f64) as i64;
    p.valid_jd = true;
    if p.valid_hms {
        p.i_jd += p.h as i64 * 3_600_000 + p.min as i64 * 60_000 + (p.s * 1000.0 + 0.5) as i64;
        if p.tz != 0 {
            p.i_jd -= p.tz as i64 * 60_000;
            p.valid_ymd = false;
            p.valid_hms = false;
            p.tz = 0;
            p.is_utc = true;
            p.is_local = false;
        }
    }
}

/// `computeFloor` from `date.c`: set `n_floor` to the day-slip needed to
/// roll an overflowed day-of-month back to the end of the previous month.
fn compute_floor(p: &mut DateTime) {
    debug_assert!(p.valid_ymd || p.is_error);
    debug_assert!(p.d >= 0 && p.d <= 31);
    debug_assert!(p.m >= 0 && p.m <= 12);
    if p.d <= 28 {
        p.n_floor = 0;
    } else if ((1u32 << p.m) & 0x15aa) != 0 {
        p.n_floor = 0;
    } else if p.m != 2 {
        p.n_floor = (p.d == 31) as i32;
    } else if p.y % 4 != 0 || (p.y % 100 == 0 && p.y % 400 != 0) {
        p.n_floor = p.d - 28;
    } else {
        p.n_floor = p.d - 29;
    }
}

/// `parseYyyyMmDd` from `date.c`.
fn parse_yyyy_mm_dd(s: &str, p: &mut DateTime) -> Result<()> {
    let mut rest = s;
    let neg;
    if rest.as_bytes().first() == Some(&b'-') {
        rest = &rest[1..];
        neg = true;
    } else {
        neg = false;
    }
    let mut out = [0i32; 3];
    if get_digits(&mut rest, b"40f-21a-21d", &mut out) != 3 {
        return Err(Error::msg("bad YYYY-MM-DD"));
    }
    let y = out[0];
    let m = out[1];
    let d = out[2];
    // Skip whitespace and optional 'T' separator.
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'T') {
        i += 1;
    }
    rest = &rest[i..];
    if parse_hhmmss(rest, p).is_ok() {
        // got the time
    } else if rest.is_empty() {
        p.valid_hms = false;
    } else {
        return Err(Error::msg("bad time after date"));
    }
    p.valid_jd = false;
    p.valid_ymd = true;
    p.y = if neg { -y } else { y };
    p.m = m;
    p.d = d;
    compute_floor(p);
    if p.tz != 0 {
        compute_jd(p);
    }
    Ok(())
}

/// `setRawDateNumber` from `date.c`: install `r` as either a julian day
/// (when in range) or a raw seconds value (`rawS = true`).
fn set_raw_date_number(p: &mut DateTime, r: f64) {
    p.s = r;
    p.raw_s = true;
    if r >= 0.0 && r < 5_373_484.5 {
        p.i_jd = (r * MS_PER_DAY as f64 + 0.5) as i64;
        p.valid_jd = true;
    }
}

/// `autoAdjustDate` from `date.c`: if `rawS` is set and the value is in the
/// unix-epoch seconds range, convert to a julian day.
fn auto_adjust_date(p: &mut DateTime) {
    if !p.raw_s || p.valid_jd {
        p.raw_s = false;
    } else if p.s >= -210_866_760_000.0 && p.s <= 25_340_230_799.0 {
        let r = p.s * 1000.0 + UNIX_EPOCH_IJD as f64;
        clear_ymd_hms_tz(p);
        p.i_jd = (r + 0.5) as i64;
        p.valid_jd = true;
        p.raw_s = false;
    }
}

/// `parseDateOrTime` from `date.c`: try every supported form. `now_ctx`
/// supplies the current milliday timestamp for `now`/`subsec`.
fn parse_date_or_time(s: &str, p: &mut DateTime, now_ctx: Option<&DateCtx>) -> Result<()> {
    if parse_yyyy_mm_dd(s, p).is_ok() {
        return Ok(());
    }
    *p = DateTime::default();
    if parse_hhmmss(s, p).is_ok() {
        return Ok(());
    }
    *p = DateTime::default();
    let lower = s.to_ascii_lowercase();
    if lower == "now" {
        let jd = now_ctx.ok_or_else(|| Error::msg("date functions need a runtime context"))?.now_jd;
        p.i_jd = jd;
        if p.i_jd > 0 {
            p.valid_jd = true;
            p.is_utc = true;
            p.is_local = false;
            clear_ymd_hms_tz(p);
            return Ok(());
        }
        return Err(Error::msg("no current time"));
    }
    // Try a numeric parse (julian day or unix seconds). Upstream's
    // `sqlite3AtoF` returns a negative value when there are leftover
    // non-whitespace characters, and `parseDateOrTime` only accepts the
    // numeric form when the *entire* string (modulo trailing whitespace) was
    // the number — so "2023-13-01" is NOT a julian day 2023, it's a bad date.
    let (val_opt, full) = crate::util::numeric_prefix(s);
    if let Some(val) = val_opt {
        if full {
            let r = val.as_f64();
            set_raw_date_number(p, r);
            return Ok(());
        }
    }
    if lower == "subsec" || lower == "subsecond" {
        let ctx = now_ctx.ok_or_else(|| Error::msg("date functions need a runtime context"))?;
        p.use_subsec = true;
        p.i_jd = ctx.now_jd;
        if p.i_jd > 0 {
            p.valid_jd = true;
            p.is_utc = true;
            p.is_local = false;
            clear_ymd_hms_tz(p);
            return Ok(());
        }
        return Err(Error::msg("no current time"));
    }
    Err(Error::msg("bad date/time"))
}

/// `validJulianDay` from `date.c`.
fn valid_julian_day(i_jd: i64) -> bool {
    i_jd >= 0 && i_jd <= MAX_IJD_POS
}

/// `computeYMD` from `date.c`.
fn compute_ymd(p: &mut DateTime) {
    if p.valid_ymd {
        return;
    }
    if !p.valid_jd {
        p.y = 2000;
        p.m = 1;
        p.d = 1;
    } else if !valid_julian_day(p.i_jd) {
        datetime_error(p);
        return;
    } else {
        let z = (p.i_jd + 43_200_000) / MS_PER_DAY;
        let alpha = ((z as f64 + 32044.75) / 36524.25) as i64 - 52;
        let a = z + 1 + alpha - (alpha + 100) / 4 + 25;
        let b = a + 1524;
        let c = ((b as f64 - 122.1) / 365.25) as i64;
        let d = 36525 * (c & 32767) / 100;
        let e = ((b - d) as f64 / 30.6001) as i64;
        let x1 = (30.6001 * e as f64) as i64;
        p.d = (b - d - x1) as i32;
        p.m = if e < 14 { (e - 1) as i32 } else { (e - 13) as i32 };
        p.y = if p.m > 2 { (c - 4716) as i32 } else { (c - 4715) as i32 };
    }
    p.valid_ymd = true;
}

/// `computeHMS` from `date.c`.
fn compute_hms(p: &mut DateTime) {
    if p.valid_hms {
        return;
    }
    compute_jd(p);
    let day_ms = ((p.i_jd + 43_200_000) % MS_PER_DAY) as i64;
    p.s = (day_ms % 60_000) as f64 / 1000.0;
    let day_min = day_ms / 60_000;
    p.min = (day_min % 60) as i32;
    p.h = (day_min / 60) as i32;
    p.raw_s = false;
    p.valid_hms = true;
}

fn compute_ymd_hms(p: &mut DateTime) {
    compute_ymd(p);
    compute_hms(p);
}

fn clear_ymd_hms_tz(p: &mut DateTime) {
    p.valid_ymd = false;
    p.valid_hms = false;
    p.tz = 0;
}

/// The `aXformType[]` table from `date.c`.
struct Xform {
    name: &'static str,
    r_limit: f64,
    r_xform: f64,
}

const A_XFORM: [Xform; 6] = [
    Xform { name: "second", r_limit: 4.6427e14, r_xform: 1.0 },
    Xform { name: "minute", r_limit: 7.7379e12, r_xform: 60.0 },
    Xform { name: "hour", r_limit: 1.2897e11, r_xform: 3600.0 },
    Xform { name: "day", r_limit: 5_373_485.0, r_xform: 86_400.0 },
    Xform { name: "month", r_limit: 176_546.0, r_xform: 2_592_000.0 },
    Xform { name: "year", r_limit: 14_713.0, r_xform: 31_536_000.0 },
];

/// `parseModifier` from `date.c`: apply a single modifier to `p`.
///
/// `idx` is the 1-based parameter index of the modifier (matches upstream's
/// `idx`); `now_ctx` is the runtime context for `localtime`/`utc`.
fn parse_modifier(s: &str, p: &mut DateTime, idx: usize, now_ctx: Option<&DateCtx>) -> Result<()> {
    let s_trim = s.trim();
    if s_trim.is_empty() {
        return Err(Error::msg("empty modifier"));
    }
    let lower_first = s_trim.chars().next().unwrap().to_ascii_lowercase();
    match lower_first {
        'a' => {
            if s_trim.eq_ignore_ascii_case("auto") {
                if idx > 1 {
                    return Err(Error::msg("auto out of place"));
                }
                auto_adjust_date(p);
                return Ok(());
            }
        }
        'c' => {
            if s_trim.eq_ignore_ascii_case("ceiling") {
                compute_jd(p);
                clear_ymd_hms_tz(p);
                p.n_floor = 0;
                return Ok(());
            }
        }
        'f' => {
            if s_trim.eq_ignore_ascii_case("floor") {
                compute_jd(p);
                p.i_jd -= p.n_floor as i64 * MS_PER_DAY;
                clear_ymd_hms_tz(p);
                return Ok(());
            }
        }
        'j' => {
            if s_trim.eq_ignore_ascii_case("julianday") {
                if idx > 1 {
                    return Err(Error::msg("julianday out of place"));
                }
                if p.valid_jd && p.raw_s {
                    p.raw_s = false;
                    return Ok(());
                }
            }
        }
        'l' => {
            if s_trim.eq_ignore_ascii_case("localtime") {
                // Without OS localtime, treat as a no-op that flips the flag.
                // (Faithful port needs osLocaltime; deferred — we still need
                // to compute JD and clear flags so subsequent renders work.)
                compute_jd(p);
                if !p.is_local {
                    // Approximate: no TZ shift available without libc localtime.
                }
                p.is_utc = false;
                p.is_local = true;
                return Ok(());
            }
        }
        'u' => {
            if s_trim.eq_ignore_ascii_case("unixepoch") && p.raw_s {
                if idx > 1 {
                    return Err(Error::msg("unixepoch out of place"));
                }
                let r = p.s * 1000.0 + UNIX_EPOCH_IJD as f64;
                if r >= 0.0 && r < 464_269_060_800_000.0 {
                    clear_ymd_hms_tz(p);
                    p.i_jd = (r + 0.5) as i64;
                    p.valid_jd = true;
                    p.raw_s = false;
                    return Ok(());
                }
            } else if s_trim.eq_ignore_ascii_case("utc") {
                if !p.is_utc {
                    // Without libc localtime, utc is the inverse of localtime;
                    // we treat both as no-ops here.
                    compute_jd(p);
                }
                p.is_utc = true;
                p.is_local = false;
                return Ok(());
            }
        }
        'w' => {
            if let Some(rest) = s_trim.strip_prefix("weekday ") {
                let rest = rest.trim();
                let (val_opt, full) = crate::util::numeric_prefix(rest);
                if let Some(val) = val_opt {
                    if full {
                        let r = val.as_f64();
                        if r >= 0.0 && r < 7.0 && (r as i64) as f64 == r {
                            let n = r as i64;
                            compute_ymd_hms(p);
                            p.tz = 0;
                            p.valid_jd = false;
                            compute_jd(p);
                            let z = ((p.i_jd + 129_600_000) / MS_PER_DAY) % 7;
                            let mut z = z;
                            if z > n {
                                z -= 7;
                            }
                            p.i_jd += (n - z) * MS_PER_DAY;
                            clear_ymd_hms_tz(p);
                            return Ok(());
                        }
                    }
                }
            }
        }
        's' => {
            if let Some(rest) = s_trim.strip_prefix("start of ") {
                if !p.valid_jd && !p.valid_ymd && !p.valid_hms {
                    return Err(Error::msg("start of on empty date"));
                }
                let rest = rest.trim();
                compute_ymd(p);
                p.valid_hms = true;
                p.h = 0;
                p.min = 0;
                p.s = 0.0;
                p.raw_s = false;
                p.tz = 0;
                p.valid_jd = false;
                if rest.eq_ignore_ascii_case("month") {
                    p.d = 1;
                    return Ok(());
                } else if rest.eq_ignore_ascii_case("year") {
                    p.m = 1;
                    p.d = 1;
                    return Ok(());
                } else if rest.eq_ignore_ascii_case("day") {
                    return Ok(());
                }
                return Err(Error::msg("bad start of"));
            }
            if s_trim.eq_ignore_ascii_case("subsec") || s_trim.eq_ignore_ascii_case("subsecond") {
                p.use_subsec = true;
                return Ok(());
            }
        }
        '+' | '-' | '0'..='9' => {
            return parse_numeric_modifier(s_trim, p, now_ctx);
        }
        _ => {}
    }
    Err(Error::msg("unrecognized modifier"))
}

/// The numeric modifier branch of `parseModifier` (the `'+','-','0'..'9'` arm
/// in `date.c`). Handles `+NNN days`, `±YYYY-MM-DD HH:MM`, and `±HH:MM:SS`.
fn parse_numeric_modifier(s: &str, p: &mut DateTime, _now_ctx: Option<&DateCtx>) -> Result<()> {
    let bytes = s.as_bytes();
    let z0 = bytes[0];
    // Find the end of the leading number (or the YYYY part of ±YYYY-MM-DD).
    let mut n = 1usize;
    while n < bytes.len() {
        let c = bytes[n];
        if c == b':' || c.is_ascii_whitespace() {
            break;
        }
        if c == b'-' {
            // A `±YYYY-MM-DD` modifier: 4 or 5 digit year then `-MM`.
            if n == 5 {
                // -YYYY or +YYYY followed by -MM-DD
                let mut out = [0i32; 3];
                let mut probe = &s[1..];
                if get_digits(&mut probe, b"40f-20a-20d", &mut out) == 3 {
                    return apply_year_month_day_modifier(p, z0, out[0], out[1], out[2], s);
                }
            }
            if n == 6 {
                let mut out = [0i32; 3];
                let mut probe = &s[1..];
                if get_digits(&mut probe, b"50f-20a-20d", &mut out) == 3 {
                    return apply_year_month_day_modifier(p, z0, out[0], out[1], out[2], s);
                }
            }
            break;
        }
        n += 1;
    }
    // Parse the leading number.
    let num_str = &s[..n];
    let (val_opt, _full) = crate::util::numeric_prefix(num_str);
    let r = match val_opt {
        Some(v) => v.as_f64(),
        None => return Err(Error::msg("bad numeric modifier")),
    };
    if n < bytes.len() && bytes[n] == b'-' {
        // ±YYYY-MM-DD form (the 4- or 5-digit-year case already handled above;
        // if we get here, the structure didn't match).
        return Err(Error::msg("bad ±YYYY-MM-DD modifier"));
    }
    if n < bytes.len() && bytes[n] == b':' {
        // ±HH:MM:SS[.FFF] modifier.
        let z2 = if bytes[0].is_ascii_digit() { &s[0..] } else { &s[1..] };
        let mut tx = DateTime::default();
        parse_hhmmss(z2, &mut tx)?;
        compute_jd(&mut tx);
        tx.i_jd -= 43_200_000;
        let day = tx.i_jd / MS_PER_DAY;
        tx.i_jd -= day * MS_PER_DAY;
        if z0 == b'-' {
            tx.i_jd = -tx.i_jd;
        }
        compute_jd(p);
        clear_ymd_hms_tz(p);
        p.i_jd += tx.i_jd;
        return Ok(());
    }
    // ±NNN <unit> form.
    let mut s_rest = &s[n..];
    s_rest = skip_ws(s_rest);
    let mut unit = s_rest;
    // trim trailing 's'
    let mut len = unit.len();
    if len < 3 || len > 10 {
        return Err(Error::msg("bad unit length"));
    }
    if unit.as_bytes().get(len - 1) == Some(&b's') {
        len -= 1;
        unit = &unit[..len];
    }
    compute_jd(p);
    let r_rounder = if r < 0.0 { -0.5 } else { 0.5 };
    p.n_floor = 0;
    for (i, xf) in A_XFORM.iter().enumerate() {
        if xf.name.len() == len
            && unit.eq_ignore_ascii_case(xf.name)
            && r > -xf.r_limit
            && r < xf.r_limit
        {
            match i {
                4 => {
                    // month
                    compute_ymd_hms(p);
                    p.m += r as i32;
                    let x = if p.m > 0 { (p.m - 1) / 12 } else { (p.m - 12) / 12 };
                    p.y += x;
                    p.m -= x * 12;
                    compute_floor(p);
                    p.valid_jd = false;
                    let r2 = r - (r as i32) as f64;
                    compute_jd(p);
                    p.i_jd += (r2 * 1000.0 * xf.r_xform + r_rounder) as i64;
                    clear_ymd_hms_tz(p);
                    return Ok(());
                }
                5 => {
                    // year
                    let y = r as i32;
                    compute_ymd_hms(p);
                    debug_assert!(p.m >= 0 && p.m <= 12);
                    p.y += y;
                    compute_floor(p);
                    p.valid_jd = false;
                    let r2 = r - (r as i32) as f64;
                    compute_jd(p);
                    p.i_jd += (r2 * 1000.0 * xf.r_xform + r_rounder) as i64;
                    clear_ymd_hms_tz(p);
                    return Ok(());
                }
                _ => {
                    compute_jd(p);
                    p.i_jd += (r * 1000.0 * xf.r_xform + r_rounder) as i64;
                    clear_ymd_hms_tz(p);
                    return Ok(());
                }
            }
        }
    }
    let _ = s_rest;
    Err(Error::msg("unknown unit"))
}

/// Apply a `±YYYY-MM-DD [HH:MM]` modifier to `p` (the `z[n]=='-'` branch of
/// `parseModifier`).
fn apply_year_month_day_modifier(
    p: &mut DateTime,
    z0: u8,
    y: i32,
    m: i32,
    d: i32,
    s_full: &str,
) -> Result<()> {
    if m >= 12 {
        return Err(Error::msg("bad month in ±YYYY-MM-DD modifier"));
    }
    if d >= 31 {
        return Err(Error::msg("bad day in ±YYYY-MM-DD modifier"));
    }
    compute_ymd_hms(p);
    p.valid_jd = false;
    let mut d_eff = d;
    if z0 == b'-' {
        p.y -= y;
        p.m -= m;
        d_eff = -d;
    } else {
        p.y += y;
        p.m += m;
    }
    let x = if p.m > 0 { (p.m - 1) / 12 } else { (p.m - 12) / 12 };
    p.y += x;
    p.m -= x * 12;
    compute_floor(p);
    compute_jd(p);
    p.valid_hms = false;
    p.valid_ymd = false;
    p.i_jd += d_eff as i64 * MS_PER_DAY;
    // Optional `HH:MM` follows after a space.
    let after = &s_full[12..];
    let after = after.trim_start();
    if after.is_empty() {
        return Ok(());
    }
    let bytes = after.as_bytes();
    if bytes.len() >= 5 {
        let mut out = [0i32; 2];
        let mut probe = after;
        if get_digits(&mut probe, b"20c:20e", &mut out) == 2 {
            let h = out[0] as i64;
            let m = out[1] as i64;
            p.i_jd += h * 3_600_000 + m * 60_000;
            return Ok(());
        }
    }
    Err(Error::msg("bad HH:MM in ±YYYY-MM-DD modifier"))
}

/// `isDate` from `date.c`: parse `args[0]` as a time value and apply each
/// subsequent argument as a modifier. `argc == 0` means `now`. Returns
/// `Ok(())` on success and leaves the result in `p`; returns an error
/// (leaving `p` in an unspecified state) on a parse failure.
pub fn is_date(args: &[Value], p: &mut DateTime, now_ctx: Option<&DateCtx>) -> Result<()> {
    *p = DateTime::default();
    if args.is_empty() {
        let ctx = now_ctx.ok_or_else(|| Error::msg("no 'now' context"))?;
        p.i_jd = ctx.now_jd;
        if p.i_jd > 0 {
            p.valid_jd = true;
            p.is_utc = true;
            p.is_local = false;
            clear_ymd_hms_tz(p);
        } else {
            return Err(Error::msg("no current time"));
        }
        return Ok(());
    }
    match &args[0] {
        Value::Int(_) | Value::Real(_) => set_raw_date_number(p, args[0].as_f64()),
        v => {
            let s = match v.to_text() {
                Some(t) => t,
                None => return Err(Error::msg("NULL date")),
            };
            parse_date_or_time(&s, p, now_ctx)?;
        }
    }
    for (i, arg) in args.iter().enumerate().skip(1) {
        let s = match arg.to_text() {
            Some(t) => t,
            None => return Err(Error::msg("NULL modifier")),
        };
        parse_modifier(&s, p, i, now_ctx)?;
    }
    compute_jd(p);
    if p.is_error || !valid_julian_day(p.i_jd) {
        return Err(Error::msg("bad date/time"));
    }
    if args.len() == 1 && p.valid_ymd && p.d > 28 {
        // Normalize overflowed dates like 2023-02-31 → 2023-03-03.
        p.valid_ymd = false;
    }
    Ok(())
}

/// Format helpers (the `zBuf` rendering in each `*Func`).

/// `dateFunc` from `date.c` → `YYYY-MM-DD`.
pub fn date_fn(args: &[Value], now_ctx: Option<&DateCtx>) -> Result<Value> {
    let mut x = DateTime::default();
    if is_date(args, &mut x, now_ctx).is_ok() {
        compute_ymd(&mut x);
        let mut y = x.y;
        if y < 0 {
            y = -y;
        }
        let s = format!(
            "{}{:04}-{:02}-{:02}",
            if x.y < 0 { "-" } else { "" },
            y,
            x.m,
            x.d
        );
        Ok(Value::Text(s))
    } else {
        Ok(Value::Null)
    }
}

/// `timeFunc` from `date.c` → `HH:MM:SS` (or `HH:MM:SS.SSS` with `subsec`).
pub fn time_fn(args: &[Value], now_ctx: Option<&DateCtx>) -> Result<Value> {
    let mut x = DateTime::default();
    if is_date(args, &mut x, now_ctx).is_ok() {
        compute_hms(&mut x);
        let s = if x.use_subsec {
            let si = (1000.0 * x.s + 0.5) as i64;
            format!(
                "{:02}:{:02}:{:02}.{:03}",
                x.h,
                x.min,
                si / 1000,
                si % 1000
            )
        } else {
            format!("{:02}:{:02}:{:02}", x.h, x.min, x.s as i64)
        };
        Ok(Value::Text(s))
    } else {
        Ok(Value::Null)
    }
}

/// `datetimeFunc` from `date.c` → `YYYY-MM-DD HH:MM:SS[.SSS]`.
pub fn datetime_fn(args: &[Value], now_ctx: Option<&DateCtx>) -> Result<Value> {
    let mut x = DateTime::default();
    if is_date(args, &mut x, now_ctx).is_ok() {
        compute_ymd_hms(&mut x);
        let mut y = x.y;
        if y < 0 {
            y = -y;
        }
        let s = if x.use_subsec {
            let si = (1000.0 * x.s + 0.5) as i64;
            format!(
                "{}{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
                if x.y < 0 { "-" } else { "" },
                y,
                x.m,
                x.d,
                x.h,
                x.min,
                si / 1000,
                si % 1000
            )
        } else {
            format!(
                "{}{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                if x.y < 0 { "-" } else { "" },
                y,
                x.m,
                x.d,
                x.h,
                x.min,
                x.s as i64
            )
        };
        Ok(Value::Text(s))
    } else {
        Ok(Value::Null)
    }
}

/// `juliandayFunc` from `date.c` → the julian day number as REAL.
pub fn julianday_fn(args: &[Value], now_ctx: Option<&DateCtx>) -> Result<Value> {
    let mut x = DateTime::default();
    if is_date(args, &mut x, now_ctx).is_ok() {
        compute_jd(&mut x);
        Ok(Value::Real(x.i_jd as f64 / MS_PER_DAY as f64))
    } else {
        Ok(Value::Null)
    }
}

/// `unixepochFunc` from `date.c` → seconds (or fractional seconds with
/// `subsec`) since 1970-01-01 00:00:00 UTC.
pub fn unixepoch_fn(args: &[Value], now_ctx: Option<&DateCtx>) -> Result<Value> {
    let mut x = DateTime::default();
    if is_date(args, &mut x, now_ctx).is_ok() {
        compute_jd(&mut x);
        if x.use_subsec {
            Ok(Value::Real((x.i_jd - UNIX_EPOCH_IJD) as f64 / 1000.0))
        } else {
            Ok(Value::Int(x.i_jd / 1000 - UNIX_EPOCH_IJD / 1000))
        }
    } else {
        Ok(Value::Null)
    }
}

/// `timediffFunc` from `date.c` → `+YYYY-MM-DD HH:MM:SS.SSS` (the time that
/// must be added to DATE2 to get DATE1).
pub fn timediff_fn(args: &[Value], now_ctx: Option<&DateCtx>) -> Result<Value> {
    if args.len() != 2 {
        return Ok(Value::Null);
    }
    let mut d1 = DateTime::default();
    if is_date(&[args[0].clone()], &mut d1, now_ctx).is_err() {
        return Ok(Value::Null);
    }
    let mut d2 = DateTime::default();
    if is_date(&[args[1].clone()], &mut d2, now_ctx).is_err() {
        return Ok(Value::Null);
    }
    compute_ymd_hms(&mut d1);
    compute_ymd_hms(&mut d2);
    let (sign, mut y, mut m);
    if d1.i_jd >= d2.i_jd {
        sign = '+';
        y = d1.y - d2.y;
        if y != 0 {
            d2.y = d1.y;
            d2.valid_jd = false;
            compute_jd(&mut d2);
        }
        m = d1.m - d2.m;
        if m < 0 {
            y -= 1;
            m += 12;
        }
        if m != 0 {
            d2.m = d1.m;
            d2.valid_jd = false;
            compute_jd(&mut d2);
        }
        while d1.i_jd < d2.i_jd {
            m -= 1;
            if m < 0 {
                m = 11;
                y -= 1;
            }
            d2.m -= 1;
            if d2.m < 1 {
                d2.m = 12;
                d2.y -= 1;
            }
            d2.valid_jd = false;
            compute_jd(&mut d2);
        }
        d1.i_jd -= d2.i_jd;
        d1.i_jd += 1_486_995_408 * 100_000;
    } else {
        sign = '-';
        y = d2.y - d1.y;
        if y != 0 {
            d2.y = d1.y;
            d2.valid_jd = false;
            compute_jd(&mut d2);
        }
        m = d2.m - d1.m;
        if m < 0 {
            y -= 1;
            m += 12;
        }
        if m != 0 {
            d2.m = d1.m;
            d2.valid_jd = false;
            compute_jd(&mut d2);
        }
        while d1.i_jd > d2.i_jd {
            m -= 1;
            if m < 0 {
                m = 11;
                y -= 1;
            }
            d2.m += 1;
            if d2.m > 12 {
                d2.m = 1;
                d2.y += 1;
            }
            d2.valid_jd = false;
            compute_jd(&mut d2);
        }
        d1.i_jd = d2.i_jd - d1.i_jd;
        d1.i_jd += 1_486_995_408 * 100_000;
    }
    clear_ymd_hms_tz(&mut d1);
    compute_ymd_hms(&mut d1);
    let s = format!(
        "{}{:04}-{:02}-{:02} {:02}:{:02}:{:06.3}",
        sign, y, m, d1.d - 1, d1.h, d1.min, d1.s
    );
    Ok(Value::Text(s))
}

/// Day-of-week helpers used by `strftime` (0=Sunday..6=Saturday).
fn days_after_sunday(p: &DateTime) -> i64 {
    ((p.i_jd + 129_600_000) / MS_PER_DAY) % 7
}

/// Day-of-week (0=Monday..6=Sunday).
fn days_after_monday(p: &DateTime) -> i64 {
    ((p.i_jd + 43_200_000) / MS_PER_DAY) % 7
}

/// Day-of-year (Jan01 = 0).
fn days_after_jan01(p: &DateTime) -> i64 {
    let mut jan01 = p.clone();
    debug_assert!(jan01.valid_ymd);
    debug_assert!(jan01.valid_hms);
    debug_assert!(p.valid_jd);
    jan01.valid_jd = false;
    jan01.m = 1;
    jan01.d = 1;
    compute_jd(&mut jan01);
    (p.i_jd - jan01.i_jd + 43_200_000) / MS_PER_DAY
}

/// `strftimeFunc` from `date.c`. The first argument is the format string;
/// the rest form the time value. Returns NULL on a bad format specifier.
pub fn strftime_fn(args: &[Value], now_ctx: Option<&DateCtx>) -> Result<Value> {
    if args.is_empty() {
        return Ok(Value::Null);
    }
    let fmt = match args[0].to_text() {
        Some(t) => t,
        None => return Ok(Value::Null),
    };
    let mut x = DateTime::default();
    if is_date(&args[1..], &mut x, now_ctx).is_err() {
        return Ok(Value::Null);
    }
    compute_jd(&mut x);
    compute_ymd_hms(&mut x);
    let mut out = String::new();
    let bytes = fmt.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        i += 1;
        if i >= bytes.len() {
            // Trailing % — SQLite resets the result and returns NULL.
            return Ok(Value::Null);
        }
        let cf = bytes[i] as char;
        i += 1;
        let mut h12 = x.h;
        if h12 > 12 {
            h12 -= 12;
        }
        if h12 == 0 {
            h12 = 12;
        }
        match cf {
            'd' => out.push_str(&format!("{:02}", x.d)),
            'e' => out.push_str(&format!("{:2}", x.d)),
            'f' => {
                let mut s = x.s;
                if s > 59.999 {
                    s = 59.999;
                }
                out.push_str(&format!("{:06.3}", s));
            }
            'F' => out.push_str(&format!("{:04}-{:02}-{:02}", x.y, x.m, x.d)),
            'G' | 'g' => {
                let mut y = x.clone();
                y.i_jd += (3 - days_after_monday(&x)) * MS_PER_DAY;
                y.valid_ymd = false;
                compute_ymd(&mut y);
                if cf == 'g' {
                    out.push_str(&format!("{:02}", y.y % 100));
                } else {
                    out.push_str(&format!("{:04}", y.y));
                }
            }
            'H' => out.push_str(&format!("{:02}", x.h)),
            'k' => out.push_str(&format!("{:2}", x.h)),
            'I' => out.push_str(&format!("{:02}", h12)),
            'l' => out.push_str(&format!("{:2}", h12)),
            'j' => out.push_str(&format!("{:03}", days_after_jan01(&x) + 1)),
            'J' => out.push_str(&format!("{:.16}", x.i_jd as f64 / MS_PER_DAY as f64)),
            'm' => out.push_str(&format!("{:02}", x.m)),
            'M' => out.push_str(&format!("{:02}", x.min)),
            'p' => out.push_str(if x.h >= 12 { "PM" } else { "AM" }),
            'P' => out.push_str(if x.h >= 12 { "pm" } else { "am" }),
            'R' => out.push_str(&format!("{:02}:{:02}", x.h, x.min)),
            's' => {
                if x.use_subsec {
                    out.push_str(&format!(
                        "{:.3}",
                        (x.i_jd - UNIX_EPOCH_IJD) as f64 / 1000.0
                    ));
                } else {
                    let is = x.i_jd / 1000 - UNIX_EPOCH_IJD / 1000;
                    out.push_str(&format!("{}", is));
                }
            }
            'S' => out.push_str(&format!("{:02}", x.s as i64)),
            'T' => out.push_str(&format!("{:02}:{:02}:{:02}", x.h, x.min, x.s as i64)),
            'u' | 'w' => {
                let mut c = (days_after_sunday(&x) as u8) + b'0';
                if c == b'0' && cf == 'u' {
                    c = b'7';
                }
                out.push(c as char);
            }
            'U' => out.push_str(&format!(
                "{:02}",
                (days_after_jan01(&x) - days_after_sunday(&x) + 7) / 7
            )),
            'V' => {
                let mut y = x.clone();
                y.i_jd += (3 - days_after_monday(&x)) * MS_PER_DAY;
                y.valid_ymd = false;
                compute_ymd(&mut y);
                out.push_str(&format!("{:02}", days_after_jan01(&y) / 7 + 1));
            }
            'W' => out.push_str(&format!(
                "{:02}",
                (days_after_jan01(&x) - days_after_monday(&x) + 7) / 7
            )),
            'Y' => out.push_str(&format!("{:04}", x.y)),
            '%' => out.push('%'),
            _ => {
                // Unknown specifier → SQLite resets the result and returns NULL.
                return Ok(Value::Null);
            }
        }
    }
    Ok(Value::Text(out))
}

/// The runtime context the VDBE executor supplies for `now`/`subsec`/`localtime`/`utc`
/// and the `current_*` zero-arg functions. Mirrors `sqlite3StmtCurrentTime` /
/// the `setDateTimeToCurrent` path in `date.c`.
#[derive(Clone, Copy)]
pub struct DateCtx {
    /// The current time as a julian-day number times `86_400_000`
    /// (millidays), UTC. Set once per statement from the wall clock.
    pub now_jd: i64,
}

impl DateCtx {
    /// Build a `DateCtx` from the system clock, returning `None` if the
    /// clock is unavailable. Mirrors `sqlite3StmtCurrentTime64`.
    pub fn now() -> Option<DateCtx> {
        use std::time::{SystemTime, UNIX_EPOCH};
        let dur = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
        let ms = dur.as_millis() as i64;
        let now_jd = UNIX_EPOCH_IJD + ms;
        Some(DateCtx { now_jd })
    }
}

/// `current_date()` → `date('now')`.
pub fn current_date_fn(now_ctx: Option<&DateCtx>) -> Result<Value> {
    date_fn(&[], now_ctx)
}

/// `current_time()` → `time('now')`.
pub fn current_time_fn(now_ctx: Option<&DateCtx>) -> Result<Value> {
    time_fn(&[], now_ctx)
}

/// `current_timestamp()` → `datetime('now')`.
pub fn current_timestamp_fn(now_ctx: Option<&DateCtx>) -> Result<Value> {
    datetime_fn(&[], now_ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Value {
        Value::Text(s.to_string())
    }

    #[test]
    fn date_basic() {
        assert_eq!(date_fn(&[t("2023-05-15")], None).unwrap(), t("2023-05-15"));
        assert_eq!(
            date_fn(&[t("2023-05-15 12:34:56")], None).unwrap(),
            t("2023-05-15")
        );
        // overflowed date normalizes
        assert_eq!(date_fn(&[t("2023-02-31")], None).unwrap(), t("2023-03-03"));
        // bad date → NULL
        assert_eq!(date_fn(&[t("not a date")], None).unwrap(), Value::Null);
    }

    #[test]
    fn time_basic() {
        assert_eq!(time_fn(&[t("2023-05-15 12:34:56")], None).unwrap(), t("12:34:56"));
        assert_eq!(time_fn(&[t("12:34:56")], None).unwrap(), t("12:34:56"));
        assert_eq!(time_fn(&[t("12:34")], None).unwrap(), t("12:34:00"));
    }

    #[test]
    fn datetime_basic() {
        assert_eq!(
            datetime_fn(&[t("2023-05-15 12:34:56")], None).unwrap(),
            t("2023-05-15 12:34:56")
        );
        assert_eq!(
            datetime_fn(&[t("2023-05-15")], None).unwrap(),
            t("2023-05-15 00:00:00")
        );
    }

    #[test]
    fn julianday_basic() {
        // 1970-01-01 00:00:00 → JD 2440587.5
        let v = julianday_fn(&[t("1970-01-01 00:00:00")], None).unwrap();
        assert!(matches!(v, Value::Real(_)));
        if let Value::Real(r) = v {
            assert!((r - 2440587.5).abs() < 1e-6, "got {r}");
        }
        // 2000-01-01 00:00:00 → JD 2451544.5
        let v = julianday_fn(&[t("2000-01-01 00:00:00")], None).unwrap();
        if let Value::Real(r) = v {
            assert!((r - 2451544.5).abs() < 1e-6, "got {r}");
        }
    }

    #[test]
    fn unixepoch_basic() {
        let v = unixepoch_fn(&[t("1970-01-01 00:00:00")], None).unwrap();
        assert_eq!(v, Value::Int(0));
        let v = unixepoch_fn(&[t("2000-01-01 00:00:00")], None).unwrap();
        assert_eq!(v, Value::Int(946_684_800));
    }

    #[test]
    fn date_with_modifiers() {
        assert_eq!(
            date_fn(&[t("2023-05-15"), t("+1 day")], None).unwrap(),
            t("2023-05-16")
        );
        assert_eq!(
            date_fn(&[t("2023-05-15"), t("-1 month")], None).unwrap(),
            t("2023-04-15")
        );
        assert_eq!(
            date_fn(&[t("2023-05-15"), t("start of month")], None).unwrap(),
            t("2023-05-01")
        );
        assert_eq!(
            date_fn(&[t("2023-05-15"), t("start of year")], None).unwrap(),
            t("2023-01-01")
        );
        assert_eq!(
            date_fn(&[t("2023-05-19"), t("weekday 0")], None).unwrap(),
            t("2023-05-21")
        );
    }

    #[test]
    fn strftime_basic() {
        assert_eq!(
            strftime_fn(&[t("%Y-%m-%d"), t("2023-05-15")], None).unwrap(),
            t("2023-05-15")
        );
        assert_eq!(
            strftime_fn(&[t("%H:%M:%S"), t("2023-05-15 12:34:56")], None).unwrap(),
            t("12:34:56")
        );
        assert_eq!(
            strftime_fn(&[t("%j"), t("2023-01-01")], None).unwrap(),
            t("001")
        );
        assert_eq!(
            strftime_fn(&[t("%j"), t("2023-12-31")], None).unwrap(),
            t("365")
        );
        assert_eq!(
            strftime_fn(&[t("%%"), t("2023-01-01")], None).unwrap(),
            t("%")
        );
    }

    #[test]
    fn timediff_basic() {
        let v = timediff_fn(&[t("2023-05-15"), t("2023-05-14")], None).unwrap();
        assert_eq!(v, t("+0000-00-01 00:00:00.000"));
        let v = timediff_fn(&[t("2023-05-14"), t("2023-05-15")], None).unwrap();
        assert_eq!(v, t("-0000-00-01 00:00:00.000"));
    }
}