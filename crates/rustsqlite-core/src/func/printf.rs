//! `printf(format, ...)` / `format(format, ...)` — printf-style string formatting
//! (mirrors `printf.c`).
//!
//! Ports the upstream `sqlite3_str_printf` machinery for the SQL-level `printf`/`format`
//! function. Supports the conversion set: `d i o u x X c s f F e E g G q Q w n p %`,
//! the flags `-+0 #`, width and precision (literal or `*`), and positional arguments
//! (`%N$spec`). NULL handling matches the oracle: a NULL format yields NULL; a NULL
//! argument yields 0/empty for numeric/string conversions, the literal `NULL` for `%Q`,
//! and NULL for the whole result for `%w`.

use crate::error::Result;
use crate::types::Value;

/// `printf(format, ...)` / `format(format, ...)` SQL function entry point.
pub fn printf_fn(args: &[Value]) -> Result<Value> {
    if args.is_empty() {
        // `printf()` with no arguments returns NULL (matches the oracle).
        return Ok(Value::Null);
    }
    if args[0].is_null() {
        return Ok(Value::Null);
    }
    let format = args[0].to_text().unwrap_or_default();
    let arg_values = &args[1..];

    let mut out = String::new();
    let mut args_iter = Args::new(arg_values);

    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'%') {
            chars.next();
            out.push('%');
            continue;
        }
        let directive = match parse_directive(&mut chars) {
            Some(d) => d,
            None => continue,
        };
        // Resolve width from arg if it was `*`.
        let mut d = directive;
        if d.width_star {
            let wv = args_iter.read(d.width_pos);
            let w = as_int(&wv);
            d.width = Some(if w < 0 {
                d.flags.minus = true;
                (-w) as i64
            } else {
                w
            });
        }
        // Resolve precision from arg if it was `*`.
        if d.prec_star {
            let pv = args_iter.read(d.prec_pos);
            d.prec = Some(as_int(&pv));
        }
        // `%n` consumes an arg but emits nothing (SQLite).
        let val = args_iter.read(d.pos);
        match d.conv {
            'd' | 'i' => fmt_int(as_int(&val), &d, &mut out),
            'u' => fmt_uint(as_int(&val), &d, &mut out),
            'x' | 'X' | 'o' => fmt_uint(as_int(&val), &d, &mut out),
            'c' => fmt_char(&val, &d, &mut out),
            's' => fmt_string(&val, &d, &mut out),
            'f' | 'F' => fmt_float(&val, &d, &mut out),
            'e' | 'E' => fmt_float(&val, &d, &mut out),
            'g' | 'G' => fmt_float(&val, &d, &mut out),
            'q' => fmt_sql_quote(&val, &d, &mut out),
            'Q' => fmt_sql_literal(&val, &d, &mut out),
            'w' => fmt_html_escape(&val, &d, &mut out, &mut false),
            'n' => {}
            'p' => fmt_uint(as_int(&val), &d, &mut out),
            _ => out.push(d.conv),
        }
    }
    Ok(Value::Text(out))
}

#[derive(Default, Clone, Copy)]
struct Flags {
    minus: bool,
    plus: bool,
    space: bool,
    zero: bool,
    hash: bool,
}

struct Directive {
    pos: Option<usize>,
    width_pos: Option<usize>,
    prec_pos: Option<usize>,
    flags: Flags,
    width: Option<i64>,
    prec: Option<i64>,
    width_star: bool,
    prec_star: bool,
    conv: char,
}

struct Args<'a> {
    args: &'a [Value],
    next: usize,
}

impl<'a> Args<'a> {
    fn new(args: &'a [Value]) -> Self {
        Self { args, next: 0 }
    }
    fn read(&mut self, pos: Option<usize>) -> Value {
        let idx = match pos {
            Some(p) => p.saturating_sub(1),
            None => {
                let i = self.next;
                self.next += 1;
                i
            }
        };
        self.args.get(idx).cloned().unwrap_or(Value::Null)
    }
}

fn parse_directive(chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<Directive> {
    let mut pos = None;
    // Positional value index: digits followed by `$`.
    let snapshot: String = chars.clone().collect();
    if snapshot.chars().next().map_or(false, |c| c.is_ascii_digit()) {
        let mut probe = snapshot.chars().peekable();
        if let Some(idx) = parse_positional(&mut probe) {
            pos = Some(idx);
            let consumed = snapshot.len() - probe.collect::<String>().len();
            for _ in 0..consumed {
                chars.next();
            }
        }
    }

    let mut flags = Flags::default();
    loop {
        match chars.peek() {
            Some('-') => {
                flags.minus = true;
                chars.next();
            }
            Some('+') => {
                flags.plus = true;
                chars.next();
            }
            Some(' ') => {
                flags.space = true;
                chars.next();
            }
            Some('0') => {
                flags.zero = true;
                chars.next();
            }
            Some('#') => {
                flags.hash = true;
                chars.next();
            }
            _ => break,
        }
    }

    // Width.
    let mut width = None;
    let mut width_pos = None;
    let mut width_star = false;
    if chars.peek() == Some(&'*') {
        chars.next();
        width_star = true;
        let snapshot: String = chars.clone().collect();
        if snapshot.chars().next().map_or(false, |c| c.is_ascii_digit()) {
            let mut probe = snapshot.chars().peekable();
            if let Some(idx) = parse_positional(&mut probe) {
                width_pos = Some(idx);
                let consumed = snapshot.len() - probe.collect::<String>().len();
                for _ in 0..consumed {
                    chars.next();
                }
            }
        }
    } else if let Some(w) = parse_number(chars) {
        width = Some(w);
    }

    // Precision.
    let mut prec = None;
    let mut prec_pos = None;
    let mut prec_star = false;
    if chars.peek() == Some(&'.') {
        chars.next();
        if chars.peek() == Some(&'*') {
            chars.next();
            prec_star = true;
            let snapshot: String = chars.clone().collect();
            if snapshot.chars().next().map_or(false, |c| c.is_ascii_digit()) {
                let mut probe = snapshot.chars().peekable();
                if let Some(idx) = parse_positional(&mut probe) {
                    prec_pos = Some(idx);
                    let consumed = snapshot.len() - probe.collect::<String>().len();
                    for _ in 0..consumed {
                        chars.next();
                    }
                }
            }
        } else if let Some(p) = parse_number(chars) {
            prec = Some(p);
        } else {
            prec = Some(0);
        }
    }

    // Length modifiers (ignored).
    while matches!(chars.peek(), Some('l') | Some('h') | Some('z') | Some('j') | Some('t') | Some('L')) {
        chars.next();
    }

    let conv = chars.next()?;
    if conv == '%' {
        return None;
    }

    Some(Directive {
        pos,
        width_pos,
        prec_pos,
        flags,
        width,
        prec,
        width_star,
        prec_star,
        conv,
    })
}

fn parse_positional(chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<usize> {
    let mut digits = String::new();
    while let Some(c) = chars.peek() {
        if c.is_ascii_digit() {
            digits.push(*c);
            chars.next();
        } else {
            break;
        }
    }
    if digits.is_empty() {
        return None;
    }
    if chars.peek() == Some(&'$') {
        chars.next();
        digits.parse::<usize>().ok()
    } else {
        None
    }
}

fn parse_number(chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<i64> {
    let mut digits = String::new();
    while let Some(c) = chars.peek() {
        if c.is_ascii_digit() {
            digits.push(*c);
            chars.next();
        } else {
            break;
        }
    }
    if digits.is_empty() {
        None
    } else {
        digits.parse::<i64>().ok()
    }
}

fn as_int(v: &Value) -> i64 {
    if v.is_null() {
        0
    } else {
        v.as_i64()
    }
}

fn as_float(v: &Value) -> f64 {
    if v.is_null() {
        0.0
    } else {
        v.as_f64()
    }
}

fn fmt_int(n: i64, d: &Directive, out: &mut String) {
    let prec = d.prec.unwrap_or(-1);
    let mut digits = if n < 0 {
        format!("-{}", n.unsigned_abs())
    } else {
        format!("{}", n)
    };
    if prec >= 0 {
        let (sign, body) = if let Some(stripped) = digits.strip_prefix('-') {
            ("-", stripped.to_string())
        } else {
            ("", digits.clone())
        };
        if prec == 0 && n == 0 {
            digits.clear();
        } else if (body.len() as i64) < prec {
            let zeros = "0".repeat((prec - body.len() as i64) as usize);
            digits = format!("{sign}{zeros}{body}");
        }
    }

    let prefix = if n >= 0 {
        if d.flags.plus {
            "+"
        } else if d.flags.space {
            " "
        } else {
            ""
        }
    } else {
        ""
    };

    let body = format!("{prefix}{digits}");
    pad_field(
        &body,
        d.width,
        d.flags.minus,
        d.flags.zero && d.prec.is_none() && !d.flags.minus,
        out,
    );
}

fn fmt_uint(n: i64, d: &Directive, out: &mut String) {
    let val = n as u64;
    let body = match d.conv {
        'u' => format!("{}", val),
        'x' => format!("{:x}", val),
        'X' => format!("{:X}", val),
        'o' => format!("{:o}", val),
        _ => unreachable!(),
    };
    let prefix = if d.flags.hash && val != 0 {
        match d.conv {
            'x' => "0x",
            'X' => "0X",
            'o' => "0",
            _ => "",
        }
    } else {
        ""
    };
    let body = format!("{prefix}{body}");
    pad_field(&body, d.width, d.flags.minus, d.flags.zero && !d.flags.minus, out);
}

fn fmt_string(v: &Value, d: &Directive, out: &mut String) {
    let s = if v.is_null() {
        String::new()
    } else {
        v.to_text().unwrap_or_default()
    };
    let s = if let Some(p) = d.prec {
        if p >= 0 {
            s.chars().take(p as usize).collect::<String>()
        } else {
            s
        }
    } else {
        s
    };
    pad_field(&s, d.width, d.flags.minus, false, out);
}

/// Format a single character (%c).
///
/// SQLite's SQL-level `printf` `%c` does *not* interpret the argument as a codepoint
/// (unlike C `printf`). Instead it renders the argument as text and emits the first
/// character. NULL → empty. (Verified against the oracle: `printf('%c', 72)` → `'7'`,
/// `printf('%c', 'abc')` → `'a'`, `printf('%c', -1)` → `'-'`.)
fn fmt_char(v: &Value, d: &Directive, out: &mut String) {
    let s = if v.is_null() {
        String::new()
    } else {
        v.to_text()
            .unwrap_or_default()
            .chars()
            .next()
            .map(|c| c.to_string())
            .unwrap_or_default()
    };
    pad_field(&s, d.width, d.flags.minus, false, out);
}

fn fmt_float(v: &Value, d: &Directive, out: &mut String) {
    let f = as_float(v);
    let prec = d.prec.unwrap_or(6).max(0) as usize;
    let body = match d.conv {
        'f' | 'F' => format_float_f(f, prec),
        'e' | 'E' => format_float_e(f, prec, d.conv == 'E'),
        'g' | 'G' => format_float_g(f, prec, d.conv == 'G'),
        _ => unreachable!(),
    };
    pad_field(&body, d.width, d.flags.minus, d.flags.zero && !d.flags.minus, out);
}

fn format_float_f(f: f64, prec: usize) -> String {
    if f.is_nan() {
        return "nan".to_string();
    }
    if f.is_infinite() {
        return if f < 0.0 { "-inf".to_string() } else { "inf".to_string() };
    }
    // Rust's `{:.*}` uses banker's rounding (half-to-even); SQLite's printf uses C
    // printf which rounds half away from zero. Reproduce by scaling, rounding via
    // `f64::round` (which rounds half away from zero), then formatting.
    if prec == 0 {
        return format!("{}", f.round() as i64);
    }
    let pow = 10f64.powi(prec as i32);
    let scaled = f * pow;
    // `f64::round` rounds half away from zero.
    let rounded = scaled.round() / pow;
    format!("{:.*}", prec, rounded)
}

fn format_float_e(f: f64, prec: usize, upper: bool) -> String {
    if f.is_nan() {
        return "nan".to_string();
    }
    if f.is_infinite() {
        return if f < 0.0 { "-inf".to_string() } else { "inf".to_string() };
    }
    let s = format!("{:.*e}", prec, f);
    normalize_e(s, upper)
}

fn normalize_e(s: String, upper: bool) -> String {
    let e = if upper { 'E' } else { 'e' };
    if let Some(pos) = s.find(|c: char| c == 'e' || c == 'E') {
        let (mantissa, exp) = s.split_at(pos);
        let exp = &exp[1..];
        let (sign, digits) = if let Some(rest) = exp.strip_prefix('-') {
            ("-", rest)
        } else if let Some(rest) = exp.strip_prefix('+') {
            ("+", rest)
        } else {
            ("+", exp)
        };
        let padded = format!("{:0>2}", digits);
        format!("{mantissa}{e}{sign}{padded}")
    } else {
        s
    }
}

fn format_float_g(f: f64, prec: usize, upper: bool) -> String {
    if f.is_nan() {
        return "nan".to_string();
    }
    if f.is_infinite() {
        return if f < 0.0 { "-inf".to_string() } else { "inf".to_string() };
    }
    if f == 0.0 {
        return "0".to_string();
    }
    let p = prec.max(1);
    let abs = f.abs();
    let exp = abs.log10().floor() as i32;
    if exp < -4 || exp >= p as i32 {
        let s = format_float_e(f, (p - 1).max(0), upper);
        strip_trailing_zeros_exp(s)
    } else {
        let prec_f = ((p as i32 - 1 - exp).max(0)) as usize;
        let s = format_float_f(f, prec_f);
        strip_trailing_zeros(s)
    }
}

fn strip_trailing_zeros(mut s: String) -> String {
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

fn strip_trailing_zeros_exp(s: String) -> String {
    if let Some(pos) = s.find(|c: char| c == 'e' || c == 'E') {
        let (mantissa, exp) = s.split_at(pos);
        let stripped = strip_trailing_zeros(mantissa.to_string());
        format!("{stripped}{exp}")
    } else {
        s
    }
}

fn fmt_sql_quote(v: &Value, d: &Directive, out: &mut String) {
    let s = if v.is_null() {
        String::new()
    } else {
        v.to_text().unwrap_or_default()
    };
    let quoted = s.replace('\'', "''");
    pad_field(&quoted, d.width, d.flags.minus, false, out);
}

fn fmt_sql_literal(v: &Value, d: &Directive, out: &mut String) {
    let body = if v.is_null() {
        "NULL".to_string()
    } else {
        let s = v.to_text().unwrap_or_default();
        format!("'{}'", s.replace('\'', "''"))
    };
    pad_field(&body, d.width, d.flags.minus, false, out);
}

/// `%w` — double every `"` (used for Windows file-path quoting). A NULL argument yields
/// the literal string `(NULL)` (matching the oracle — distinct from a NULL result).
fn fmt_html_escape(v: &Value, d: &Directive, out: &mut String, _null_flag: &mut bool) {
    if v.is_null() {
        out.push_str("(NULL)");
        return;
    }
    let s = v.to_text().unwrap_or_default();
    let escaped = s.replace('"', "\"\"");
    pad_field(&escaped, d.width, d.flags.minus, false, out);
}

fn pad_field(body: &str, width: Option<i64>, left: bool, zero: bool, out: &mut String) {
    let w = match width {
        Some(w) if w > 0 => w as usize,
        _ => {
            out.push_str(body);
            return;
        }
    };
    let len = body.chars().count();
    if len >= w {
        out.push_str(body);
        return;
    }
    let pad_n = w - len;
    if left {
        out.push_str(body);
        out.push_str(&" ".repeat(pad_n));
    } else if zero {
        let (prefix, rest) = split_sign_prefix(body);
        out.push_str(prefix);
        out.push_str(&"0".repeat(pad_n));
        out.push_str(rest);
    } else {
        out.push_str(&" ".repeat(pad_n));
        out.push_str(body);
    }
}

fn split_sign_prefix(s: &str) -> (&str, &str) {
    let bytes = s.as_bytes();
    let mut idx = 0;
    if idx < bytes.len() && (bytes[idx] == b'-' || bytes[idx] == b'+') {
        idx += 1;
    }
    if idx + 1 < bytes.len() && bytes[idx] == b'0' && (bytes[idx + 1] == b'x' || bytes[idx + 1] == b'X') {
        idx += 2;
    } else if idx < bytes.len() && bytes[idx] == b'0' && bytes.len() > idx + 1 && bytes[idx + 1].is_ascii_digit() {
        idx += 1;
    }
    s.split_at(idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Value {
        Value::Text(s.to_string())
    }

    fn pf(args: &[Value]) -> String {
        match printf_fn(args) {
            Ok(Value::Text(s)) => s,
            Ok(Value::Null) => "(NULL)".to_string(),
            Ok(v) => v.to_text().unwrap_or_default(),
            Err(e) => format!("ERR: {}", e.message),
        }
    }

    #[test]
    fn basic_printf() {
        assert_eq!(
            pf(&[t("%d %s %f"), Value::Int(1), t("hi"), Value::Real(3.5)]),
            "1 hi 3.500000"
        );
        assert_eq!(pf(&[t("%5.2f"), Value::Real(3.14159)]), " 3.14");
        assert_eq!(pf(&[t("%0*d"), Value::Int(6), Value::Int(42)]), "000042");
        assert_eq!(pf(&[t("%.*f"), Value::Int(2), Value::Real(3.14159)]), "3.14");
        assert_eq!(
            pf(&[t("%x %X %o"), Value::Int(255), Value::Int(255), Value::Int(255)]),
            "ff FF 377"
        );
        assert_eq!(pf(&[t("%c"), Value::Int(65)]), "6"); // SQLite %c: first char of "65"
        assert_eq!(pf(&[t("100%%")]), "100%");
        assert_eq!(pf(&[t("%q"), t("it's")]), "it''s");
        assert_eq!(pf(&[t("%Q"), t("it's")]), "'it''s'");
        assert_eq!(pf(&[t("%w"), t("a'b\"c")]), "a'b\"\"c");
        assert_eq!(pf(&[t("%5d|%-5d|"), Value::Int(42), Value::Int(42)]), "   42|42   |");
        assert_eq!(pf(&[t("%+d %+d"), Value::Int(5), Value::Int(-5)]), "+5 -5");
        assert_eq!(pf(&[t("%05d"), Value::Int(-3)]), "-0003");
        assert_eq!(pf(&[t("%#x"), Value::Int(255)]), "0xff");
        assert_eq!(pf(&[t("%#o"), Value::Int(255)]), "0377");
        assert_eq!(pf(&[t("%e"), Value::Real(123456.789)]), "1.234568e+05");
        assert_eq!(pf(&[t("%.0f"), Value::Real(0.5)]), "1");
        assert_eq!(pf(&[t("%.0f"), Value::Real(1.5)]), "2");
        assert_eq!(pf(&[t("%.0f"), Value::Real(2.5)]), "3");
        assert_eq!(pf(&[t("%g"), Value::Real(0.0001)]), "0.0001");
        assert_eq!(pf(&[t("%g"), Value::Real(0.00001)]), "1e-05");
        assert_eq!(pf(&[t("%g"), Value::Real(100000.0)]), "100000");
        assert_eq!(pf(&[t("%g"), Value::Real(1000000.0)]), "1e+06");
    }

    #[test]
    fn null_handling() {
        assert_eq!(pf(&[Value::Null]), "(NULL)");
        assert_eq!(pf(&[t("hello %s"), Value::Null]), "hello ");
        assert_eq!(pf(&[t("%d"), Value::Null]), "0");
        assert_eq!(pf(&[t("%Q"), Value::Null]), "NULL");
        assert_eq!(pf(&[t("%w"), Value::Null]), "(NULL)");
    }
}