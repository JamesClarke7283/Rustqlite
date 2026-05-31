//! String built-in functions (mirrors the string entries in `func.c`).
//!
//! Each function is ported from the upstream C implementation and verified against the
//! `sqlite3` binary. Subtle points pinned by the differential tests: `instr` is 1-based and
//! counts characters for TEXT but bytes when both arguments are BLOBs; `replace` returns its
//! input unchanged for an empty pattern and NULL if any argument is NULL; the 2-argument
//! `trim`/`ltrim`/`rtrim` strip a *set* of characters; `char` uses SQLite's own UTF-8 writer
//! (out-of-range codepoints become U+FFFD); `hex` upper-cases; `concat` treats NULL as the
//! empty string while `concat_ws` skips NULL data arguments; `quote` renders an SQL literal.

use crate::types::Value;

/// `instr(haystack, needle)` — 1-based index of the first occurrence, 0 if absent. Counts
/// characters for TEXT (and any non-blob operand), bytes when *both* operands are BLOBs. A NULL
/// argument yields NULL. Ported from `instrFunc`.
pub fn instr(haystack: &Value, needle: &Value) -> Value {
    if haystack.is_null() || needle.is_null() {
        return Value::Null;
    }
    let both_blob = matches!(haystack, Value::Blob(_)) && matches!(needle, Value::Blob(_));
    if both_blob {
        let hay = match haystack {
            Value::Blob(b) => b.as_slice(),
            _ => unreachable!(),
        };
        let nee = match needle {
            Value::Blob(b) => b.as_slice(),
            _ => unreachable!(),
        };
        Value::Int(byte_instr(hay, nee))
    } else {
        // Compare on the UTF-8 text rendering, counting characters.
        let hay = haystack.to_text().unwrap_or_default();
        let nee = needle.to_text().unwrap_or_default();
        let hb = hay.as_bytes();
        let nb = nee.as_bytes();
        if nb.is_empty() {
            return Value::Int(1);
        }
        match byte_offset(hb, nb) {
            Some(off) => {
                // Number of UTF-8 *characters* before the match, plus 1.
                let chars = hay[..off].chars().count();
                Value::Int(chars as i64 + 1)
            }
            None => Value::Int(0),
        }
    }
}

/// 1-based byte index (BLOB path): one more than the number of bytes before the first match.
fn byte_instr(hay: &[u8], nee: &[u8]) -> i64 {
    if nee.is_empty() {
        return 1;
    }
    match byte_offset(hay, nee) {
        Some(off) => off as i64 + 1,
        None => 0,
    }
}

/// First byte offset at which `nee` occurs in `hay`, if any.
fn byte_offset(hay: &[u8], nee: &[u8]) -> Option<usize> {
    if nee.is_empty() {
        return Some(0);
    }
    if nee.len() > hay.len() {
        return None;
    }
    hay.windows(nee.len()).position(|w| w == nee)
}

/// `replace(X, Y, Z)` — replace every non-overlapping occurrence of `Y` in `X` with `Z`. An
/// empty `Y` returns `X` unchanged; any NULL argument yields NULL. Operates on the UTF-8 bytes
/// of the text rendering. Ported from `replaceFunc`.
pub fn replace(x: &Value, y: &Value, z: &Value) -> Value {
    if x.is_null() || y.is_null() || z.is_null() {
        return Value::Null;
    }
    let s = x.to_text().unwrap_or_default();
    let pat = y.to_text().unwrap_or_default();
    if pat.is_empty() {
        // SQLite returns the original argument value (preserving its storage class).
        return x.clone();
    }
    let rep = z.to_text().unwrap_or_default();
    Value::Text(s.replace(&pat, &rep))
}

/// `trim`/`ltrim`/`rtrim` direction selector.
#[derive(Clone, Copy)]
pub enum TrimSide {
    Both,
    Left,
    Right,
}

/// `trim(X)` / `trim(X, Y)` and the `l`/`r` variants. With one argument the character set is
/// ASCII space (0x20) only; with two it is the *set of characters* in `Y`. A NULL argument
/// yields NULL. Operates on whole UTF-8 characters. Ported from `trimFunc`.
pub fn trim(side: TrimSide, x: &Value, y: Option<&Value>) -> Value {
    let s = match x.to_text() {
        Some(s) => s,
        None => return Value::Null,
    };
    // Determine the character set to strip.
    let set: Vec<char> = match y {
        None => vec![' '],
        Some(yv) => match yv.to_text() {
            Some(t) => t.chars().collect(),
            None => return Value::Null,
        },
    };
    let in_set = |c: char| set.contains(&c);

    let chars: Vec<char> = s.chars().collect();
    let mut start = 0usize;
    let mut end = chars.len();
    if matches!(side, TrimSide::Both | TrimSide::Left) {
        while start < end && in_set(chars[start]) {
            start += 1;
        }
    }
    if matches!(side, TrimSide::Both | TrimSide::Right) {
        while end > start && in_set(chars[end - 1]) {
            end -= 1;
        }
    }
    Value::Text(chars[start..end].iter().collect())
}

/// `char(X1, X2, ...)` — build a string from Unicode codepoints, using SQLite's own UTF-8
/// writer (`charFunc`). Codepoints outside `0..=0x10FFFF` become U+FFFD; the low 21 bits are
/// then encoded. Zero arguments yields the empty string.
///
/// KNOWN LIMITATION: SQLite's writer also encodes lone surrogates (`0xD800..=0xDFFF`) to their
/// raw 3-byte form and stores them as TEXT even though that is not valid UTF-8. Our `Value::Text`
/// wraps a Rust `String`, which cannot hold those bytes, so such inputs currently yield `""`
/// instead of the surrogate bytes. Every codepoint that maps to valid UTF-8 (the overwhelming
/// common case) is byte-for-byte faithful. Lifting this needs `Value::Text` to carry raw bytes.
pub fn char_(args: &[Value]) -> Value {
    let mut out: Vec<u8> = Vec::with_capacity(args.len() * 4);
    for v in args {
        let mut x = v.as_i64();
        if !(0..=0x10ffff).contains(&x) {
            x = 0xfffd;
        }
        let c = (x & 0x1fffff) as u32;
        if c < 0x80 {
            out.push((c & 0xff) as u8);
        } else if c < 0x800 {
            out.push(0xc0 + ((c >> 6) & 0x1f) as u8);
            out.push(0x80 + (c & 0x3f) as u8);
        } else if c < 0x10000 {
            out.push(0xe0 + ((c >> 12) & 0x0f) as u8);
            out.push(0x80 + ((c >> 6) & 0x3f) as u8);
            out.push(0x80 + (c & 0x3f) as u8);
        } else {
            out.push(0xf0 + ((c >> 18) & 0x07) as u8);
            out.push(0x80 + ((c >> 12) & 0x3f) as u8);
            out.push(0x80 + ((c >> 6) & 0x3f) as u8);
            out.push(0x80 + (c & 0x3f) as u8);
        }
    }
    // SQLite labels the result UTF-8 TEXT; the bytes are always valid UTF-8 by construction.
    Value::Text(String::from_utf8(out).unwrap_or_default())
}

/// `unicode(X)` — the codepoint of the first character of `X` (its text rendering). NULL→NULL;
/// the empty string yields NULL (no first character). Ported from `unicodeFunc`.
pub fn unicode(x: &Value) -> Value {
    match x.to_text() {
        None => Value::Null,
        Some(s) => match s.chars().next() {
            Some(c) => Value::Int(c as i64),
            None => Value::Null,
        },
    }
}

/// `hex(X)` — UPPERCASE hexadecimal of the value's bytes: a BLOB's bytes directly, otherwise the
/// bytes of its UTF-8 text rendering. A NULL argument yields the **empty string**, NOT NULL —
/// `hexFunc` reads `sqlite3_value_blob(NULL)` as a zero-length buffer. Ported from `hexFunc`.
pub fn hex(x: &Value) -> Value {
    let bytes: Vec<u8> = match x {
        Value::Null => Vec::new(),
        Value::Blob(b) => b.clone(),
        other => other.to_text().unwrap_or_default().into_bytes(),
    };
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX_UP[(b >> 4) as usize] as char);
        s.push(HEX_UP[(b & 0xf) as usize] as char);
    }
    Value::Text(s)
}

const HEX_UP: &[u8; 16] = b"0123456789ABCDEF";

/// `unhex(X)` / `unhex(X, Y)` — inverse of `hex`. Decodes pairs of hex digits into a BLOB. With a
/// second argument, any byte appearing in `Y` is treated as a "pass" character and skipped, but
/// only while scanning for a *high* nibble: once a high nibble is read, the next byte must be a
/// hex digit, so a pass byte landing between the two nibbles of one byte fails. Invalid input
/// (any other non-hex byte, or an odd trailing nibble) yields **NULL** — `unhexFunc` does
/// `goto unhex_null`, which leaves the result unset (NULL), it does NOT raise an error. A NULL
/// argument also yields NULL.
pub fn unhex(x: &Value, y: Option<&Value>) -> Value {
    if x.is_null() {
        return Value::Null;
    }
    // The set of "pass" bytes. SQLite compares against the text of the second argument; NUL is
    // never matched because the scan terminates on NUL first.
    let pass: Vec<u8> = match y {
        None => Vec::new(),
        Some(yv) => {
            if yv.is_null() {
                return Value::Null;
            }
            yv.to_text().unwrap_or_default().into_bytes()
        }
    };
    let in_pass = |c: u8| c != 0 && pass.contains(&c);
    let input = match x {
        Value::Blob(b) => b.clone(),
        other => other.to_text().unwrap_or_default().into_bytes(),
    };

    let mut out = Vec::new();
    let mut i = 0usize;
    let n = input.len();
    while i < n {
        let mut c = input[i];
        // Skip leading pass bytes while looking for the high nibble (`while(!isxdigit(c))`).
        while hex_val(c).is_none() {
            if !in_pass(c) {
                return Value::Null;
            }
            i += 1;
            if i >= n {
                return Value::Blob(out); // trailing pass bytes → done
            }
            c = input[i];
        }
        // `c` is the high nibble; the immediately following byte must be a hex digit.
        let hi = hex_val(c).unwrap();
        i += 1;
        let lo = match input.get(i).copied().and_then(hex_val) {
            Some(lo) => lo,
            None => return Value::Null,
        };
        out.push((hi << 4) | lo);
        i += 1;
    }
    Value::Blob(out)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// `concat(...)` — concatenate the text rendering of every argument, treating NULL as the empty
/// string (unlike `||`). The result is always TEXT. Ported from `concatFunc`.
pub fn concat(args: &[Value]) -> Value {
    let mut s = String::new();
    for v in args {
        if let Some(t) = v.to_text() {
            s.push_str(&t);
        }
    }
    Value::Text(s)
}

/// `concat_ws(sep, ...)` — join the non-NULL data arguments with `sep` between them. A NULL
/// separator yields NULL; NULL data arguments are skipped (and produce no separator). Ported
/// from `concatwsFunc`.
pub fn concat_ws(args: &[Value]) -> Value {
    // args[0] is the separator; args[1..] are the data values.
    let sep = match args[0].to_text() {
        Some(s) => s,
        None => return Value::Null, // separator NULL → NULL
    };
    let mut s = String::new();
    let mut first = true;
    for v in &args[1..] {
        if let Some(t) = v.to_text() {
            if !first {
                s.push_str(&sep);
            }
            s.push_str(&t);
            first = false;
        }
    }
    Value::Text(s)
}

/// `quote(X)` — render `X` as a SQL literal: NULL→`NULL`, INTEGER bare, REAL via the faithful
/// rendering, TEXT single-quoted with `''` doubling, BLOB as `X'..'` (uppercase hex). Ported
/// from `quoteFunc`.
pub fn quote(x: &Value) -> Value {
    let s = match x {
        Value::Null => "NULL".to_string(),
        Value::Int(i) => i.to_string(),
        Value::Real(r) => quote_real(*r),
        Value::Text(t) => {
            let mut out = String::with_capacity(t.len() + 2);
            out.push('\'');
            for c in t.chars() {
                if c == '\'' {
                    out.push('\'');
                }
                out.push(c);
            }
            out.push('\'');
            out
        }
        Value::Blob(b) => {
            let mut out = String::with_capacity(b.len() * 2 + 3);
            out.push('X');
            out.push('\'');
            for byte in b {
                out.push(HEX_UP[(byte >> 4) as usize] as char);
                out.push(HEX_UP[(byte & 0xf) as usize] as char);
            }
            out.push('\'');
            out
        }
    };
    Value::Text(s)
}

/// Render a REAL for `quote`. SQLite's `sqlite3QuoteValue` formats a FLOAT with `%!0.15g` (note
/// the `0` **zero-pad** flag), unlike the bare `%!.15g` used by `sqlite3_column_text`. For finite
/// values the zero-pad flag has no effect with no field width, so the faithful
/// [`fp_to_text`](crate::util::fp::fp_to_text) renders them identically. The one place the flag
/// matters is `±∞`: printf.c's `isSpecial` branch, when zero-padding, emits `9.0e+999` /
/// `-9.0e+999` (via `s.z[0]='9'; s.iDP=1000`) instead of the bare `Inf` / `-Inf` the column path
/// produces — so `quote()` special-cases infinity. (A NaN REAL is stored as NULL upstream, so it
/// never reaches here.)
fn quote_real(r: f64) -> String {
    if r.is_infinite() {
        return if r < 0.0 {
            "-9.0e+999".to_string()
        } else {
            "9.0e+999".to_string()
        };
    }
    crate::util::fp::fp_to_text(r)
}

/// `octet_length(X)` — number of bytes in the UTF-8 (TEXT/number) or raw (BLOB) representation.
/// NULL→NULL. Mirrors `bytesFunc`.
pub fn octet_length(x: &Value) -> Value {
    match x {
        Value::Null => Value::Null,
        Value::Blob(b) => Value::Int(b.len() as i64),
        other => Value::Int(other.to_text().map_or(0, |t| t.len()) as i64),
    }
}
