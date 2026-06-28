//! `soundex(X)` — SOUNDEX encoding (mirrors `soundexFunc` in `func.c`, enabled by
//! `SQLITE_SOUNDEX`).
//!
//! The algorithm is the classic American Soundex with the H/W adjacency rule: H and W
//! are skipped (coded as 0) but do *not* break the adjacency collapse — so two letters
//! with the same code separated only by H/W collapse to one code. A,E,I,O,U,Y are
//! skipped and DO break adjacency (they reset the "previous code" to 0). The first
//! letter is retained as-is (uppercase) and its code is dropped from the digit stream
//! (so two adjacent same-code letters at the start collapse).

use crate::types::Value;

/// `soundex(X)` — 4-char Soundex encoding (`LDDD`). Returns `?000` for a NULL or
/// non-alphabetic input (mirrors the oracle).
pub fn soundex(v: &Value) -> Value {
    let s = match v.to_text() {
        Some(t) => t,
        None => return Value::Text("?000".to_string()),
    };
    let chars: Vec<char> = s
        .chars()
        .map(|c| c.to_ascii_uppercase())
        .filter(|c| c.is_ascii_alphabetic())
        .collect();
    if chars.is_empty() {
        return Value::Text("?000".to_string());
    }

    let mut result = String::with_capacity(4);
    result.push(chars[0]);
    let mut last_code = code(chars[0]);
    // For the first letter, we set last_code to its code so an adjacent same-code
    // letter collapses (standard Soundex behavior). But the H/W rule says H/W don't
    // reset last_code; A/E/I/O/U/Y do reset it to 0.

    for &c in &chars[1..] {
        if result.len() >= 4 {
            break;
        }
        match c {
            'A' | 'E' | 'I' | 'O' | 'U' | 'Y' | 'H' | 'W' => {
                // Vowels (and H/W) are skipped and reset last_code so the next coded
                // letter emits. This matches SQLite's `soundexFunc`, which does NOT
                // apply the H/W adjacency rule (a simpler American Soundex).
                last_code = 0;
                continue;
            }
            _ => {}
        }
        let code_c = code(c);
        if code_c != last_code {
            result.push(std::char::from_digit(code_c as u32, 10).unwrap());
            last_code = code_c;
        }
    }

    while result.len() < 4 {
        result.push('0');
    }
    Value::Text(result)
}

fn code(c: char) -> u8 {
    match c {
        'B' | 'F' | 'P' | 'V' => 1,
        'C' | 'G' | 'J' | 'K' | 'Q' | 'S' | 'X' | 'Z' => 2,
        'D' | 'T' => 3,
        'L' => 4,
        'M' | 'N' => 5,
        'R' => 6,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soundex_known() {
        let check = |input: &str, expected: &str| {
            assert_eq!(soundex(&Value::Text(input.into())), Value::Text(expected.into()));
        };
        check("Robert", "R163");
        check("Rupert", "R163");
        check("Rubin", "R150");
        check("Ashcraft", "A226");
        check("Tymczak", "T522");
        check("Pfister", "P236");
        check("Honeyman", "H555");
        check("Smith", "S530");
        check("Schmidt", "S530");
        check("Washington", "W252");
        check("Lee", "L000");
        check("Gutierrez", "G362");
        check("a", "A000");
        check("123abc", "A120");
        // Empty / NULL / whitespace → ?000
        assert_eq!(soundex(&Value::Text("".into())), Value::Text("?000".into()));
        assert_eq!(soundex(&Value::Null), Value::Text("?000".into()));
        assert_eq!(soundex(&Value::Int(123)), Value::Text("?000".into()));
    }
}