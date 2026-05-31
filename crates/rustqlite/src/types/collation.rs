//! Built-in collating sequences: `BINARY`, `NOCASE`, `RTRIM` (mirrors `vdbe.c`/`func.c`).
//!
//! Collations compare TEXT values. SQLite's three built-ins:
//! * `BINARY` — `memcmp` of the raw bytes (the default).
//! * `NOCASE` — like BINARY but the 26 ASCII upper-case letters fold to lower-case first.
//! * `RTRIM` — like BINARY but trailing spaces are ignored.

use std::cmp::Ordering;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Collation {
    Binary,
    NoCase,
    RTrim,
}

impl Collation {
    /// Resolve a collation by name (case-insensitive), as in `CREATE TABLE ... COLLATE`.
    pub fn from_name(name: &str) -> Option<Collation> {
        match name.to_ascii_uppercase().as_str() {
            "BINARY" => Some(Collation::Binary),
            "NOCASE" => Some(Collation::NoCase),
            "RTRIM" => Some(Collation::RTrim),
            _ => None,
        }
    }

    /// Compare two text values under this collation.
    pub fn compare(self, a: &str, b: &str) -> Ordering {
        match self {
            Collation::Binary => a.as_bytes().cmp(b.as_bytes()),
            Collation::NoCase => {
                let a = a.bytes().map(ascii_lower);
                let b = b.bytes().map(ascii_lower);
                a.cmp(b)
            }
            Collation::RTrim => {
                let a = rtrim_spaces(a.as_bytes());
                let b = rtrim_spaces(b.as_bytes());
                a.cmp(b)
            }
        }
    }
}

fn ascii_lower(b: u8) -> u8 {
    if b.is_ascii_uppercase() {
        b + 32
    } else {
        b
    }
}

fn rtrim_spaces(mut s: &[u8]) -> &[u8] {
    while let [rest @ .., b' '] = s {
        s = rest;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    #[test]
    fn binary_is_bytewise() {
        assert_eq!(Collation::Binary.compare("abc", "abd"), Less);
        assert_eq!(Collation::Binary.compare("ABC", "abc"), Less); // 'A'(65) < 'a'(97)
        assert_eq!(Collation::Binary.compare("abc", "abc"), Equal);
    }

    #[test]
    fn nocase_folds_ascii() {
        assert_eq!(Collation::NoCase.compare("ABC", "abc"), Equal);
        assert_eq!(Collation::NoCase.compare("Hello", "hELLO"), Equal);
        assert_eq!(Collation::NoCase.compare("abc", "abd"), Less);
    }

    #[test]
    fn rtrim_ignores_trailing_spaces() {
        assert_eq!(Collation::RTrim.compare("abc", "abc   "), Equal);
        assert_eq!(Collation::RTrim.compare("abc ", "abc"), Equal);
        assert_eq!(Collation::RTrim.compare("ab ", "abc"), Less);
    }

    #[test]
    fn resolve_by_name() {
        assert_eq!(Collation::from_name("nocase"), Some(Collation::NoCase));
        assert_eq!(Collation::from_name("RTRIM"), Some(Collation::RTrim));
        assert_eq!(Collation::from_name("bogus"), None);
    }
}
