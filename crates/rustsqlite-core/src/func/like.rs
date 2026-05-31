//! `LIKE` and `GLOB` pattern matching (a faithful port of `patternCompare` and
//! `likeFunc`/`globFunc` in `func.c`).
//!
//! Both operators lower to function calls — `X LIKE Y` ⇒ `like(Y, X)` and `X GLOB Y` ⇒
//! `glob(Y, X)`, so the **pattern is the first argument**. The 3-argument `like(pattern, str,
//! escape)` form adds a user escape character. The matcher ([`pattern_compare`]) is a
//! line-for-line port of upstream's single `patternCompare` routine, parameterized by a
//! [`CompareInfo`] exactly as the C code is by its `compareInfo` table:
//!
//! * **LIKE** — `%` (matchAll) matches any run of characters (including none), `_` (matchOne)
//!   matches exactly one character. Case folding is **ASCII-only**: `A`–`Z` fold with `a`–`z`,
//!   but non-ASCII letters such as `À`/`à` compare case-*sensitively* (so `'À' LIKE 'à'` is 0).
//!   Matching is by Unicode code point. `matchSet` is disabled, so `[` is an ordinary character
//!   (verified: `'a' LIKE '[a]'` is 0). The optional escape character makes the following
//!   character literal; a *dangling* escape (end of pattern) yields no match.
//! * **GLOB** — `*` (matchAll) matches any run, `?` (matchOne) matches one character, and `[...]`
//!   (matchSet) is a character class supporting ranges (`a-z`), negation with a leading `^`, and
//!   a literal `]` when it is the first class character. GLOB is case-*sensitive* and uses `[`
//!   as its `matchOther` (there is no escape character).
//!
//! NULL handling: if the pattern or the string is NULL the result is NULL. The 3-argument escape
//! must be exactly one character — a NULL, empty, or multi-character escape is an error whose
//! message matches the oracle (`ESCAPE expression must be a single character`).
//!
//! Operand storage classes: the registered functions take the **text rendering** of each operand
//! (so `123 LIKE '1%'` and `like('ab', x'6162')` match on the rendered text), verified against
//! the `sqlite3` binary.
//!
//! KNOWN DIVERGENCE (BLOB operands of the *operator* form). Both the operator and the function
//! lower to the identical `Opcode::Function`/`call_scalar` dispatch in this engine, so they are
//! indistinguishable at runtime and a single behavior must be chosen. We implement the
//! `like()`/`glob()` *function* semantics uniformly: every operand is converted to its text
//! rendering and matched. Upstream's *operator* form differs only for BLOB operands: with the
//! default `SQLITE_LIKE_DOESNT_MATCH_BLOBS` build, `likeFunc`/`globFunc` return 0 if either
//! operand is a BLOB (e.g. `x'6162' LIKE 'ab'` → 0), whereas the function form converts the BLOB
//! to text and matches. We follow the function form. NUMBER operands agree with the oracle for
//! both forms (e.g. `12.5 GLOB '12.5'`, `123 LIKE '123'`, `123 LIKE '1%'` are all 1), because
//! SQLite renders a numeric operand to text before matching in every case. Plain TEXT operands —
//! the overwhelming common case — are byte-identical for both the operator and the function forms.

use crate::error::{Error, Result};
use crate::types::Value;

/// Mirrors upstream's `struct compareInfo`: the two wildcard characters, whether the compare is
/// case-insensitive (LIKE), and whether `[...]` sets are recognized (GLOB only).
struct CompareInfo {
    /// `%` for LIKE, `*` for GLOB.
    match_all: char,
    /// `_` for LIKE, `?` for GLOB.
    match_one: char,
    /// ASCII-only case folding (LIKE = true, GLOB = false).
    no_case: bool,
    /// Recognize `[...]` character classes (GLOB = true, LIKE = false).
    match_set: bool,
}

const LIKE_INFO: CompareInfo = CompareInfo {
    match_all: '%',
    match_one: '_',
    no_case: true,
    match_set: false,
};

const GLOB_INFO: CompareInfo = CompareInfo {
    match_all: '*',
    match_one: '?',
    no_case: false,
    match_set: true,
};

/// `like(pattern, str)` / `like(pattern, str, escape)` — registered scalar.
///
/// Returns `Int(1)`/`Int(0)` for match/no-match, `Null` if `pattern` or `text` is NULL. A supplied
/// escape that is empty or longer than one character is an error (`Result::Err`), matching the
/// oracle's `ESCAPE expression must be a single character`. A NULL escape yields `Null` (matching
/// upstream `likeFunc`, which returns the unset/NULL result), not an error.
pub fn like(pattern: &Value, text: &Value, escape: Option<&Value>) -> Result<Value> {
    if pattern.is_null() || text.is_null() {
        return Ok(Value::Null);
    }
    // A NULL escape argument makes the whole result NULL (not an error): upstream `likeFunc`
    // does `zEsc = sqlite3_value_text(argv[2]); if( zEsc==0 ) return;`, leaving the result unset
    // (i.e. NULL). Verified against the oracle: `'a' LIKE 'a' ESCAPE NULL` → NULL.
    if matches!(escape, Some(Value::Null)) {
        return Ok(Value::Null);
    }
    // The default LIKE escape is `\0` (no escape). `matchOther` is the escape char for LIKE.
    let esc = parse_escape(escape)?.unwrap_or('\0');
    let pat: Vec<char> = pattern.to_text().unwrap_or_default().chars().collect();
    let txt: Vec<char> = text.to_text().unwrap_or_default().chars().collect();
    Ok(bool_val(pattern_compare(&pat, &txt, &LIKE_INFO, esc)))
}

/// `glob(pattern, str)` — registered scalar. Returns `Int(1)`/`Int(0)`, or `Null` if `pattern`
/// or `text` is NULL. GLOB's `matchOther` is `[` (it has no escape character).
pub fn glob(pattern: &Value, text: &Value) -> Value {
    if pattern.is_null() || text.is_null() {
        return Value::Null;
    }
    let pat: Vec<char> = pattern.to_text().unwrap_or_default().chars().collect();
    let txt: Vec<char> = text.to_text().unwrap_or_default().chars().collect();
    bool_val(pattern_compare(&pat, &txt, &GLOB_INFO, '['))
}

/// Validate the optional 3rd `like()` argument: it must be exactly one Unicode character. A
/// missing argument yields `None`; an empty or multi-character escape is an error whose message
/// matches the oracle. A NULL escape is handled by the caller ([`like`]) — it makes the whole
/// result NULL — so it never reaches here; it is treated defensively as a bad escape.
fn parse_escape(escape: Option<&Value>) -> Result<Option<char>> {
    let ev = match escape {
        None => return Ok(None),
        Some(ev) => ev,
    };
    let s = ev.to_text().ok_or_else(escape_error)?;
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Ok(Some(c)),
        _ => Err(escape_error()),
    }
}

/// The error SQLite raises for a bad ESCAPE argument (NULL / empty / multi-character).
fn escape_error() -> Error {
    Error::msg("ESCAPE expression must be a single character")
}

fn bool_val(b: bool) -> Value {
    Value::Int(i64::from(b))
}

/// ASCII case fold (upper-case `A`–`Z` → lower-case), used by LIKE's `no_case` path. Mirrors
/// `sqlite3Tolower`, which only affects ASCII.
fn to_lower(c: char) -> char {
    if c.is_ascii_uppercase() {
        c.to_ascii_lowercase()
    } else {
        c
    }
}

/// The three outcomes `patternCompare` distinguishes (`SQLITE_MATCH` / `SQLITE_NOMATCH` /
/// `SQLITE_NOWILDCARDMATCH`). The "no wildcard match" value lets the `%`/`*` backtracking loop
/// abort early when even consuming the rest of the string cannot help.
#[derive(Clone, Copy, PartialEq)]
enum Outcome {
    Match,
    NoMatch,
    NoWildcardMatch,
}

/// Read the character at `*i` and advance the index, returning `'\0'` at end of input (mirrors
/// upstream's `Utf8Read`/`sqlite3Utf8Read`, which return 0 at the terminating NUL).
fn read(chars: &[char], i: &mut usize) -> char {
    if *i < chars.len() {
        let c = chars[*i];
        *i += 1;
        c
    } else {
        '\0'
    }
}

/// Faithful port of `patternCompare` (`func.c`). `match_other` is the escape character for LIKE
/// (or `'\0'` for none) and `'['` for GLOB. Operates over Unicode characters (the `Vec<char>`
/// stands in for the C UTF-8 cursor, with index arithmetic replacing pointer arithmetic).
fn pattern_compare(
    pattern: &[char],
    string: &[char],
    info: &CompareInfo,
    match_other: char,
) -> bool {
    compare(pattern, 0, string, 0, info, match_other) == Outcome::Match
}

/// The recursive worker. `pi`/`si` are the current pattern/string indices (`zPattern`/`zString`
/// in the C). Returns one of the three [`Outcome`]s.
fn compare(
    pattern: &[char],
    mut pi: usize,
    string: &[char],
    mut si: usize,
    info: &CompareInfo,
    match_other: char,
) -> Outcome {
    let match_all = info.match_all;
    let match_one = info.match_one;
    // NOTE: the C keeps a `zEscaped` cursor so the trailing `c==matchOne && zPattern!=zEscaped`
    // test can reject an *escaped* `_`/`?` from matching as a wildcard. We instead handle the
    // escape case inline (it `continue`s before reaching that test), so an escaped `_`/`?` can
    // never reach the wildcard branch and no separate `zEscaped` bookkeeping is required.

    loop {
        let c = read(pattern, &mut pi);
        if c == '\0' {
            break;
        }

        if c == match_all {
            // Skip runs of matchAll; each interleaved matchOne consumes one input char.
            let mut c = read(pattern, &mut pi);
            while c == match_all || (c == match_one && match_one != '\0') {
                if c == match_one && read(string, &mut si) == '\0' {
                    return Outcome::NoWildcardMatch;
                }
                c = read(pattern, &mut pi);
            }
            if c == '\0' {
                return Outcome::Match; // trailing matchAll matches the rest
            } else if c == match_other {
                if !info.match_set {
                    // LIKE: the escape after `%` makes the next char literal.
                    c = read(pattern, &mut pi);
                    if c == '\0' {
                        return Outcome::NoWildcardMatch;
                    }
                } else {
                    // GLOB: a `[...]` set immediately follows the `*`; leave `pi` pointing at it.
                }
            }
            // `c` is now the first post-`matchAll` pattern char. Back up `pi` by one so the
            // recursive call re-reads from `&zPattern[-1]`.
            pi -= 1;
            loop {
                let m = compare(pattern, pi, string, si, info, match_other);
                if m != Outcome::NoMatch {
                    return m;
                }
                // Advance the string one char and retry (the C `strcspn` fast-path is just an
                // optimization; a plain scan is behavior-identical).
                if read(string, &mut si) == '\0' {
                    break;
                }
            }
            return Outcome::NoWildcardMatch;
        }

        if c == match_other {
            if !info.match_set {
                // LIKE escape: the next pattern char is taken literally.
                let c2 = read(pattern, &mut pi);
                if c2 == '\0' {
                    return Outcome::NoMatch; // dangling escape → no match
                }
                // The escaped character `c2` is compared literally (an escaped `%`/`_`/escape is
                // an ordinary character, never a wildcard).
                let cc = read(string, &mut si);
                if c2 == cc {
                    continue;
                }
                if info.no_case && to_lower(c2) == to_lower(cc) && is_ascii(c2) && is_ascii(cc) {
                    continue;
                }
                return Outcome::NoMatch;
            } else {
                // GLOB `[...]` set.
                match match_set(pattern, &mut pi, string, &mut si) {
                    SetResult::Match => continue,
                    SetResult::NoMatch => return Outcome::NoMatch,
                }
            }
        }

        // Ordinary literal comparison (the C fall-through at `c2 = Utf8Read(zString)`).
        let c2 = read(string, &mut si);
        if c == c2 {
            continue;
        }
        if info.no_case && to_lower(c) == to_lower(c2) && is_ascii(c) && is_ascii(c2) {
            continue;
        }
        // The C `c==matchOne && zPattern!=zEscaped && c2!=0`. `matchOne` (`_`/`?`) matches any
        // single non-end character; an *escaped* `_`/`?` is handled inline above and never
        // reaches this point, so the `zEscaped` guard is automatically satisfied.
        if c == match_one && c2 != '\0' {
            continue;
        }
        return Outcome::NoMatch;
    }

    if si >= string.len() {
        Outcome::Match
    } else {
        Outcome::NoMatch
    }
}

fn is_ascii(c: char) -> bool {
    (c as u32) < 0x80
}

/// Result of a GLOB `[...]` set comparison against the current string character.
enum SetResult {
    Match,
    NoMatch,
}

/// Port of the `globInfo->matchSet` branch of `patternCompare`. On entry `*pi` points just past
/// the `[`; on a match it is left just past the closing `]` and the string index `*si` has
/// consumed one character. Mirrors the C exactly, including: a leading `^` inverts the set; a `]`
/// immediately after the (optional) `^` is a literal member; `a-z` forms a range only when there
/// is a non-zero `prior_c` and the `-` is not the last class character; after a range `prior_c`
/// is reset to 0 (so `[a-c-z]` = {a,b,c,-,z}); an unterminated set (`c2==0`) never matches.
fn match_set(pattern: &[char], pi: &mut usize, string: &[char], si: &mut usize) -> SetResult {
    let mut prior_c = '\0';
    let mut seen = false;
    let mut invert = false;

    let c = read(string, si);
    if c == '\0' {
        return SetResult::NoMatch;
    }
    let mut c2 = read(pattern, pi);
    if c2 == '^' {
        invert = true;
        c2 = read(pattern, pi);
    }
    if c2 == ']' {
        if c == ']' {
            seen = true;
        }
        c2 = read(pattern, pi);
    }
    while c2 != '\0' && c2 != ']' {
        // `zPattern[0]` in the C — the next unread pattern char (or `\0`).
        let next = if *pi < pattern.len() {
            pattern[*pi]
        } else {
            '\0'
        };
        if c2 == '-' && next != ']' && next != '\0' && prior_c != '\0' {
            c2 = read(pattern, pi);
            if c >= prior_c && c <= c2 {
                seen = true;
            }
            prior_c = '\0';
        } else {
            if c == c2 {
                seen = true;
            }
            prior_c = c2;
        }
        c2 = read(pattern, pi);
    }
    if c2 == '\0' || !(seen ^ invert) {
        return SetResult::NoMatch;
    }
    SetResult::Match
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Value {
        Value::Text(s.to_string())
    }

    fn liked(pat: &str, s: &str) -> bool {
        matches!(like(&t(pat), &t(s), None), Ok(Value::Int(1)))
    }
    fn globbed(pat: &str, s: &str) -> bool {
        matches!(glob(&t(pat), &t(s)), Value::Int(1))
    }

    #[test]
    fn like_wildcards_and_case() {
        assert!(liked("a%", "abc"));
        assert!(liked("A_C", "abc")); // ASCII case-insensitive
        assert!(liked("%c", "abc"));
        assert!(liked("%", ""));
        assert!(liked("%%%", "abc"));
        assert!(liked("a%d_", "abcde"));
        assert!(!liked("a_c", "ac")); // `_` needs exactly one char
        assert!(liked("a_c", "axc"));
        assert!(liked("a_c", "aÀc")); // `_` matches one *Unicode* char
        assert!(liked("_%", "a"));
        // ASCII folds; non-ASCII is case-sensitive.
        assert!(liked("abc", "ABC"));
        assert!(liked("xyz", "XYZ"));
        assert!(!liked("à", "À"));
        assert!(liked("À", "À"));
        // `[` is a literal in LIKE (no character classes).
        assert!(!liked("[a]", "a"));
        assert!(liked("[a]", "[a]"));
    }

    #[test]
    fn like_escape() {
        let bs = t("\\");
        let m = |pat: &str, s: &str| matches!(like(&t(pat), &t(s), Some(&bs)), Ok(Value::Int(1)));
        assert!(m("a\\%", "a%")); // pattern `a\%` == literal "a%" (escaped % is literal)
        assert!(!m("a\\%", "axc")); // the % is literal, so `axc` does not match
        assert!(m("a\\%c", "a%c"));
        assert!(!m("a\\%c", "axyzc"));
        assert!(m("a\\_c", "a_c")); // escaped _ is literal
        assert!(!m("a\\_c", "axc"));
        assert!(m("100\\%", "100%"));
        // A dangling escape (escape at end of pattern) never matches.
        assert!(!m("abc\\", "abc\\"));
        assert!(!m("abc\\", "abc"));
        // A non-single-character escape errors (the oracle's message).
        assert!(like(&t("a%"), &t("abc"), Some(&t("xy"))).is_err());
        assert!(like(&t("a%"), &t("abc"), Some(&t(""))).is_err());
        // A NULL escape makes the whole result NULL (not an error), matching the oracle:
        // `'abc' LIKE 'a%' ESCAPE NULL` → NULL.
        assert_eq!(
            like(&t("a%"), &t("abc"), Some(&Value::Null)).unwrap(),
            Value::Null
        );
        // A single non-ASCII escape character is accepted.
        assert!(like(&t("aÀ%"), &t("a%c"), Some(&t("À"))).is_ok());
    }

    #[test]
    fn glob_wildcards_and_classes() {
        assert!(globbed("a*", "abc"));
        assert!(!globbed("A*", "abc")); // case-sensitive
        assert!(globbed("a?c", "abc"));
        assert!(!globbed("a?c", "ac"));
        assert!(globbed("a?c", "aÀc")); // `?` matches one Unicode char
        assert!(globbed("[a-c]bc", "abc"));
        // `!` is NOT special in SQLite GLOB — `[!b]` is the literal set {!, b} (verified against
        // the 3.53.1 oracle: `'a' GLOB '[!b]'` is 0, `'!' GLOB '[!b]'` is 1).
        assert!(!globbed("[!b]", "a")); // `a` is neither `!` nor `b`
        assert!(globbed("[!b]", "!")); // `!` is a literal member
        assert!(globbed("[!a]", "a")); // `a` is a literal member of {!, a}
        assert!(globbed("[!a]", "!"));
        assert!(globbed("[^b]", "a")); // `^` negation
        assert!(!globbed("[^a]", "a"));
        assert!(globbed("[a-z]", "x"));
        assert!(globbed("[]]", "]")); // literal `]` as first class char
        assert!(globbed("[]a]", "a"));
        assert!(!globbed("[]]", "x"));
        assert!(!globbed("[c-a]", "b")); // reversed range matches nothing
        assert!(globbed("[*]", "*")); // special char literal inside class
        assert!(globbed("[?]", "?"));
        assert!(globbed("[a-]", "-")); // trailing `-` is literal
        assert!(globbed("[-a]", "-")); // leading `-` is literal
                                       // `[a-c-z]`: range a-c, then literal `-`, then literal `z`.
        assert!(globbed("[a-c-z]", "a"));
        assert!(globbed("[a-c-z]", "b"));
        assert!(globbed("[a-c-z]", "-"));
        assert!(globbed("[a-c-z]", "z"));
        assert!(!globbed("[a-c-z]", "y"));
        // Unterminated set: never matches (the C returns NOMATCH on `c2==0`).
        assert!(!globbed("[a", "a"));
        assert!(!globbed("[abc", "a"));
    }

    #[test]
    fn null_operands() {
        assert_eq!(like(&Value::Null, &t("a"), None).unwrap(), Value::Null);
        assert_eq!(like(&t("a"), &Value::Null, None).unwrap(), Value::Null);
        assert_eq!(glob(&Value::Null, &t("a")), Value::Null);
        assert_eq!(glob(&t("a"), &Value::Null), Value::Null);
    }

    #[test]
    fn non_text_operands() {
        // Numbers use their text rendering.
        assert!(matches!(
            like(&t("1%"), &Value::Int(123), None),
            Ok(Value::Int(1))
        ));
        assert!(matches!(
            like(&t("12%"), &Value::Real(12.5), None),
            Ok(Value::Int(1))
        ));
    }
}
