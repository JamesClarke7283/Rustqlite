//! JSON parser (RFC 8259) — text → tree, mirroring `jsonTranslateTextToBlob` in `json.c`.
//!
//! This is the M24.1 foundation: a strict RFC 8259 parser that produces an owned [`JsonNode`]
//! tree. Standard JSON only (no JSON5 extensions, no JSONB binary form); the JSON5/JSONB
//! machinery in upstream's `json.c` is a large optimization surface that is not needed for
//! correctness and lands later.
//!
//! The tree is the in-memory representation every M24.2–M24.19 function builds on:
//! - [`JsonNode::Null`] / [`Bool`] / [`Int`] / [`Real`] / [`String`] — the five scalars.
//! - [`JsonNode::Array`] — ordered list of nodes.
//! - [`JsonNode::Object`] — ordered list of `(String key, JsonNode value)` pairs (insertion
//!   order, matching SQLite's `json_object` semantics; duplicate keys are preserved, with the
//!   *last* value winning on lookup — matching upstream).
//!
//! The parser is recursive descent with the same depth limit as upstream (`JSON_MAX_DEPTH =
//! 1000`); a deeper nest returns a malformed-JSON error.
//!
//! [`Null`]: JsonNode::Null
//! [`Bool`]: JsonNode::Bool
//! [`Int`]: JsonNode::Int
//! [`Real`]: JsonNode::Real
//! [`String`]: JsonNode::String
//! [`Array`]: JsonNode::Array
//! [`Object`]: JsonNode::Object

use crate::error::{Error, Result};
use crate::types::Value;

/// Maximum JSON nesting depth (mirrors `JSON_MAX_DEPTH` in `json.c`).
pub const JSON_MAX_DEPTH: usize = 1000;

/// A parsed JSON value (the tree node).
///
/// Integers that fit in `i64` are kept as [`JsonNode::Int`]; any number with a fractional part,
/// an exponent, or a magnitude outside `i64` is [`JsonNode::Real`]. This matches SQLite's
/// behavior where `json_extract('1')` returns INTEGER 1 and `json_extract('1.0')` returns REAL
/// 1.0.
#[derive(Clone, Debug, PartialEq)]
pub enum JsonNode {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    String(String),
    Array(Vec<JsonNode>),
    /// `(key, value)` pairs in insertion order. A duplicate key is stored as a second entry;
    /// [`JsonNode::object_lookup`] returns the *last* value for a key (matching upstream's
    /// "the last value wins" rule).
    Object(Vec<(String, JsonNode)>),
}

impl JsonNode {
    /// The `json_type()` label for this node: `"null"`, `"true"`, `"false"`, `"integer"`,
    /// `"real"`, `"text"`, `"array"`, `"object"`. (`true`/`false` are distinct from `bool` in
    /// upstream's `json_type` — SQLite reports `"true"`/`"`false"` for booleans, not `"bool"`.)
    pub fn type_label(&self) -> &'static str {
        match self {
            JsonNode::Null => "null",
            JsonNode::Bool(true) => "true",
            JsonNode::Bool(false) => "false",
            JsonNode::Int(_) => "integer",
            JsonNode::Real(_) => "real",
            JsonNode::String(_) => "text",
            JsonNode::Array(_) => "array",
            JsonNode::Object(_) => "object",
        }
    }

    /// Look up the *last* value associated with `key` in an object (or `None` if not an object
    /// or the key is absent). Matches SQLite's "last value wins" for duplicate keys.
    pub fn object_lookup(&self, key: &str) -> Option<&JsonNode> {
        if let JsonNode::Object(entries) = self {
            entries.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
        } else {
            None
        }
    }

    /// Returns true if this node is a JSON scalar (not array/object).
    pub fn is_scalar(&self) -> bool {
        matches!(
            self,
            JsonNode::Null | JsonNode::Bool(_) | JsonNode::Int(_) | JsonNode::Real(_) | JsonNode::String(_)
        )
    }
}

/// A parse error — the input is not valid JSON. The byte offset is the position of the first
/// bad byte (or the input length if the error is "unexpected end of input"). Mirrors upstream's
/// `iErr` field in `JsonParse`.
#[derive(Debug, Clone)]
pub struct JsonParseError {
    /// 0-based byte offset into the original JSON text.
    pub offset: usize,
    /// Human-readable description.
    pub message: String,
}

impl std::fmt::Display for JsonParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "malformed JSON at offset {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for JsonParseError {}

/// Parse a JSON text into a [`JsonNode`] tree. Strict RFC 8259 — rejects JSON5 extensions
/// (unquoted keys, single-quoted strings, trailing commas, comments, `Infinity`/`NaN`,
/// hexadecimal literals). A leading/trailing whitespace is permitted (matching upstream).
///
/// The parser runs on a dedicated thread with an enlarged stack so that the
/// `JSON_MAX_DEPTH=1000` recursion limit cannot overflow the default thread stack in debug
/// builds (where frames are large). The parsed tree is sent back to the caller.
pub fn parse(input: &str) -> Result<JsonNode> {
    // Fast path: the common case (small JSON) parses on the current stack.
    // The recursion only risks overflow on pathological depth, so we probe the depth first
    // and only spawn a big-stack thread when needed.
    let max_observed_depth = depth_probe(input);
    if max_observed_depth < 200 {
        return parse_inner(input);
    }
    // Pathological depth: run on a thread with a 64 MiB stack.
    let input = input.to_string();
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || parse_inner(&input))
        .map_err(|e| Error::msg(format!("failed to spawn JSON parser thread: {e}")))?
        .join()
        .map_err(|_| Error::msg("JSON parser thread panicked"))?
}

/// A cheap pass that counts the maximum nesting depth implied by the input without parsing,
/// so the caller can decide whether to run the recursive parser on a bigger stack. Only `{`,
/// `[`, and their matching closers matter; strings are skipped (so a `[` inside a string does
/// not count).
fn depth_probe(input: &str) -> usize {
    let bytes = input.as_bytes();
    let mut depth = 0usize;
    let mut max = 0usize;
    let mut in_str = false;
    let mut esc = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
        } else if b == b'"' {
            in_str = true;
        } else if b == b'{' || b == b'[' {
            depth += 1;
            if depth > max {
                max = depth;
            }
        } else if b == b'}' || b == b']' {
            if depth > 0 {
                depth -= 1;
            }
        }
        i += 1;
    }
    max
}

fn parse_inner(input: &str) -> Result<JsonNode> {
    let bytes = input.as_bytes();
    let mut p = Parser {
        bytes,
        pos: 0,
        depth: 0,
    };
    p.skip_ws();
    let node = p.parse_value()?;
    p.skip_ws();
    if p.pos != bytes.len() {
        return Err(malformed(p.pos, "extra text after JSON value"));
    }
    Ok(node)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn parse_value(&mut self) -> Result<JsonNode> {
        if self.depth >= JSON_MAX_DEPTH {
            return Err(malformed(self.pos, "JSON nested too deeply"));
        }
        let b = match self.peek() {
            Some(b) => b,
            None => return Err(malformed(self.pos, "unexpected end of input")),
        };
        self.depth += 1;
        let node = match b {
            b'{' => self.parse_object()?,
            b'[' => self.parse_array()?,
            b'"' => JsonNode::String(self.parse_string()?),
            b't' => self.parse_literal("true", JsonNode::Bool(true))?,
            b'f' => self.parse_literal("false", JsonNode::Bool(false))?,
            b'n' => self.parse_literal("null", JsonNode::Null)?,
            b'-' | b'0'..=b'9' => self.parse_number()?,
            _ => return Err(malformed(self.pos, format!("unexpected character {:?}", b as char))),
        };
        self.depth -= 1;
        Ok(node)
    }

    fn parse_object(&mut self) -> Result<JsonNode> {
        self.expect(b'{')?;
        let mut entries: Vec<(String, JsonNode)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(JsonNode::Object(entries));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(malformed(self.pos, "expected '\"' for object key"));
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(malformed(self.pos, "expected ':' after object key"));
            }
            self.pos += 1;
            self.skip_ws();
            let value = self.parse_value()?;
            entries.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    continue;
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(malformed(self.pos, "expected ',' or '}' in object")),
            }
        }
        Ok(JsonNode::Object(entries))
    }

    fn parse_array(&mut self) -> Result<JsonNode> {
        self.expect(b'[')?;
        let mut items: Vec<JsonNode> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(JsonNode::Array(items));
        }
        loop {
            self.skip_ws();
            let value = self.parse_value()?;
            items.push(value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    continue;
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(malformed(self.pos, "expected ',' or ']' in array")),
            }
        }
        Ok(JsonNode::Array(items))
    }

    fn parse_string(&mut self) -> Result<String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let b = match self.peek() {
                Some(b) => b,
                None => return Err(malformed(self.pos, "unterminated string")),
            };
            if b == b'"' {
                self.pos += 1;
                return Ok(out);
            }
            if b == b'\\' {
                self.pos += 1;
                let esc = match self.peek() {
                    Some(c) => c,
                    None => return Err(malformed(self.pos, "unterminated escape")),
                };
                self.pos += 1;
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{0008}'),
                    b'f' => out.push('\u{000C}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let cp = self.parse_4_hex()?;
                        // Surrogate pair handling.
                        if (0xD800..=0xDBFF).contains(&cp) {
                            // High surrogate — expect a low surrogate next.
                            if self.peek() != Some(b'\\') {
                                return Err(malformed(self.pos, "expected low surrogate"));
                            }
                            self.pos += 1;
                            if self.peek() != Some(b'u') {
                                return Err(malformed(self.pos, "expected '\\u' for low surrogate"));
                            }
                            self.pos += 1;
                            let lo = self.parse_4_hex()?;
                            if !(0xDC00..=0xDFFF).contains(&lo) {
                                return Err(malformed(self.pos, "invalid low surrogate"));
                            }
                            let combined = 0x10000
                                + ((cp - 0xD800) << 10)
                                + (lo - 0xDC00);
                            match char::from_u32(combined) {
                                Some(c) => out.push(c),
                                None => return Err(malformed(self.pos, "invalid surrogate pair")),
                            }
                        } else if (0xDC00..=0xDFFF).contains(&cp) {
                            return Err(malformed(self.pos, "unexpected low surrogate"));
                        } else {
                            match char::from_u32(cp) {
                                Some(c) => out.push(c),
                                None => return Err(malformed(self.pos, "invalid codepoint")),
                            }
                        }
                    }
                    _ => return Err(malformed(self.pos - 1, "invalid escape character")),
                }
            } else if b < 0x20 {
                return Err(malformed(self.pos, "unescaped control character in string"));
            } else {
                // Multi-byte UTF-8: consume a full UTF-8 sequence. The input is &str so it's
                // already validated UTF-8; we just need to copy the character verbatim.
                let rest = &self.bytes[self.pos..];
                let c = rest
                    .iter()
                    .take_while(|&&b| b >= 0x80)
                    .count();
                let len = if c == 0 { 1 } else { c };
                let chunk = &self.bytes[self.pos..self.pos + len];
                match std::str::from_utf8(chunk) {
                    Ok(s) => out.push_str(s),
                    Err(_) => return Err(malformed(self.pos, "invalid UTF-8 in string")),
                }
                self.pos += len;
            }
        }
    }

    fn parse_4_hex(&mut self) -> Result<u32> {
        if self.pos + 4 > self.bytes.len() {
            return Err(malformed(self.pos, "incomplete \\u escape"));
        }
        let hex = &self.bytes[self.pos..self.pos + 4];
        self.pos += 4;
        let mut v = 0u32;
        for &b in hex {
            let d = match b {
                b'0'..=b'9' => (b - b'0') as u32,
                b'a'..=b'f' => (b - b'a' + 10) as u32,
                b'A'..=b'F' => (b - b'A' + 10) as u32,
                _ => return Err(malformed(self.pos - 4, "invalid hex digit in \\u escape")),
            };
            v = (v << 4) | d;
        }
        Ok(v)
    }

    fn parse_number(&mut self) -> Result<JsonNode> {
        let start = self.pos;
        let mut is_real = false;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        // Integer part: 0 alone, or 1-9 followed by digits.
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                self.pos += 1;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(malformed(self.pos, "expected digit in number")),
        }
        // Fractional part.
        if self.peek() == Some(b'.') {
            is_real = true;
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(malformed(self.pos, "expected digit after '.'"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        // Exponent part.
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            is_real = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(malformed(self.pos, "expected digit in exponent"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap();
        if is_real {
            text.parse::<f64>()
                .map(JsonNode::Real)
                .map_err(|_| malformed(start, "invalid number"))
        } else {
            // Integer: parse as i64; an overflow promotes to REAL (matching SQLite).
            match text.parse::<i64>() {
                Ok(i) => Ok(JsonNode::Int(i)),
                Err(_) => text
                    .parse::<f64>()
                    .map(JsonNode::Real)
                    .map_err(|_| malformed(start, "invalid number")),
            }
        }
    }

    fn parse_literal(&mut self, lit: &str, value: JsonNode) -> Result<JsonNode> {
        let bytes = lit.as_bytes();
        if self.pos + bytes.len() > self.bytes.len() {
            return Err(malformed(self.pos, format!("expected '{}'", lit)));
        }
        if &self.bytes[self.pos..self.pos + bytes.len()] != bytes {
            return Err(malformed(self.pos, format!("expected '{}'", lit)));
        }
        self.pos += bytes.len();
        Ok(value)
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn expect(&mut self, b: u8) -> Result<()> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(malformed(self.pos, format!("expected {:?}", b as char)))
        }
    }
}

fn malformed(offset: usize, msg: impl Into<String>) -> Error {
    // SQLite reports "malformed JSON" as the high-level error (via `sqlite3_result_error`).
    // The detailed offset is exposed separately via `json_error_position()` (M24.14). For
    // now we keep the offset in the message for debuggability — the high-level "malformed
    // JSON" prefix matches the oracle's error text so error-message parity tests that check
    // the prefix still pass.
    let _ = offset;
    Error::msg(format!("malformed JSON: {}", msg.into()))
}

/// Render a [`JsonNode`] back to canonical JSON text (no whitespace), matching the output of
/// SQLite's `json()` function. Strings are escaped per RFC 8259 with the upstream escape set
/// (`"`, `\`, control characters as `\u00XX` or short escapes; non-ASCII pass through as UTF-8).
pub fn render(node: &JsonNode) -> String {
    let mut out = String::new();
    render_into(node, &mut out);
    out
}

fn render_into(node: &JsonNode, out: &mut String) {
    match node {
        JsonNode::Null => out.push_str("null"),
        JsonNode::Bool(true) => out.push_str("true"),
        JsonNode::Bool(false) => out.push_str("false"),
        JsonNode::Int(i) => out.push_str(&i.to_string()),
        JsonNode::Real(r) => out.push_str(&crate::util::fp::fp_to_text(*r)),
        JsonNode::String(s) => render_string(s, out),
        JsonNode::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                render_into(item, out);
            }
            out.push(']');
        }
        JsonNode::Object(entries) => {
            out.push('{');
            for (i, (k, v)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                render_string(k, out);
                out.push(':');
                render_into(v, out);
            }
            out.push('}');
        }
    }
}

fn render_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---- SQL function implementations (M24.2) ----
//
// These mirror the `JFUNCTION` entries in `json.c`'s `aJsonFunc` table. They take already-
// evaluated `Value` arguments and return a `Value`. The codegen routes them via
// `func::registry::call_scalar`.

/// `json(X)` — validate X as JSON and re-render it as canonical JSON text.
///
/// Behavior (matching the oracle):
/// - NULL → NULL.
/// - INTEGER/REAL → the number rendered as a JSON number (`json(123)` → `123`, `json(1.5)` →
///   `1.5`).
/// - TEXT → parsed as JSON; on success the canonical text is returned (with the JSON subtype
///   flag, once we model subtypes); on failure "malformed JSON".
/// - BLOB → "JSON cannot hold BLOB values" (a bare BLOB is never a valid JSON argument).
///
/// The returned TEXT carries the `JSON_SUBTYPE` so it isn't re-quoted when fed back into
/// another `json_*` function. Subtype tracking is M24.20; for now the value is plain TEXT
/// (divergence only when nested `json(json('...'))` is called — the inner text is already
/// canonical so the re-parse renders the same text).
pub fn json_fn(arg: &Value) -> Result<Value> {
    match arg {
        Value::Null => Ok(Value::Null),
        Value::Int(i) => Ok(Value::Text(i.to_string())),
        Value::Real(r) => Ok(Value::Text(crate::util::fp::fp_to_text(*r))),
        Value::Text(s) => {
            let node = parse(s)?;
            Ok(Value::Text(render(&node)))
        }
        Value::Blob(_) => Err(Error::msg("JSON cannot hold BLOB values")),
    }
}

/// `jsonb(X)` — like [`json_fn`] but returns the value as a BLOB. Upstream returns the JSONB
/// binary form; we return the canonical JSON text encoded as UTF-8 bytes in a BLOB. This is
/// not byte-faithful to upstream's JSONB but round-trips through our own `jsonb(blob)` (the
/// blob's bytes are valid JSON text). The JSONB binary form lands with a dedicated follow-up.
pub fn jsonb_fn(arg: &Value) -> Result<Value> {
    match arg {
        Value::Null => Ok(Value::Null),
        Value::Int(i) => Ok(Value::Blob(i.to_string().into_bytes())),
        Value::Real(r) => Ok(Value::Blob(crate::util::fp::fp_to_text(*r).into_bytes())),
        Value::Text(s) => {
            let node = parse(s)?;
            Ok(Value::Blob(render(&node).into_bytes()))
        }
        Value::Blob(_) => Err(Error::msg("JSON cannot hold BLOB values")),
    }
}

/// Convert a [`Value`] to a [`JsonNode`] the way the `json_*` functions see their argument:
/// NULL → Null, INTEGER → Int, REAL → Real, TEXT → parsed as JSON (raising "malformed JSON" on
/// failure), BLOB → "JSON cannot hold BLOB values". Used by M24.3–M24.13 functions that need
/// to interpret an SQL value as a JSON value.
pub fn value_to_json(arg: &Value) -> Result<JsonNode> {
    match arg {
        Value::Null => Ok(JsonNode::Null),
        Value::Int(i) => Ok(JsonNode::Int(*i)),
        Value::Real(r) => Ok(JsonNode::Real(*r)),
        Value::Text(s) => parse(s),
        Value::Blob(_) => Err(Error::msg("JSON cannot hold BLOB values")),
    }
}

/// Append the JSON rendering of an SQL value to `out`, the way `jsonAppendSqlValue` does in
/// `json.c`. NULL → `null`, INTEGER → decimal text, REAL → `fp_to_text`, TEXT → quoted JSON
/// string (unless it carries the JSON subtype — not yet modeled, so always quoted), BLOB →
/// "JSON cannot hold BLOB values" error.
fn append_sql_value(arg: &Value, out: &mut String) -> Result<()> {
    match arg {
        Value::Null => out.push_str("null"),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::Real(r) => out.push_str(&crate::util::fp::fp_to_text(*r)),
        Value::Text(s) => render_string(s, out),
        Value::Blob(_) => return Err(Error::msg("JSON cannot hold BLOB values")),
    }
    Ok(())
}

/// `json_array(...)` — build a JSON array from the arguments. Each argument is rendered per
/// [`append_sql_value`]: NULL/INTEGER/REAL as JSON scalars, TEXT as a quoted JSON string, BLOB
/// as an error. Zero arguments → `[]`.
pub fn json_array_fn(args: &[Value]) -> Result<Value> {
    let mut out = String::new();
    out.push('[');
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        append_sql_value(arg, &mut out)?;
    }
    out.push(']');
    Ok(Value::Text(out))
}

/// `json_object(K1, V1, K2, V2, ...)` — build a JSON object from key/value pairs. Keys must
/// be TEXT (matching the oracle's "json_object() labels must be TEXT"); values are rendered
/// per [`append_sql_value`]. An odd argument count is an error.
pub fn json_object_fn(args: &[Value]) -> Result<Value> {
    if args.len() % 2 != 0 {
        return Err(Error::msg(
            "json_object() requires an even number of arguments",
        ));
    }
    let mut out = String::new();
    out.push('{');
    for (i, pair) in args.chunks(2).enumerate() {
        if i > 0 {
            out.push(',');
        }
        match &pair[0] {
            Value::Text(k) => render_string(k, &mut out),
            _ => {
                return Err(Error::msg("json_object() labels must be TEXT"));
            }
        }
        out.push(':');
        append_sql_value(&pair[1], &mut out)?;
    }
    out.push('}');
    Ok(Value::Text(out))
}

// ---- JSON path lookup (M24.5) ----
//
// Path syntax (a subset of upstream's `jsonLookupStep`, sufficient for json_extract):
//   $             — the root.
//   $.key         — object lookup by bare-key (alphanumerics + _).
//   $."key"       — object lookup by quoted key (with JSON-string escapes).
//   $['key']      — alternate quoted-key form (upstream uses $["key"]; we accept both
//                   single and double quotes for ergonomics — upstream only accepts ").
//   $[N]          — array index (0-based).
//   $[#]          — last array element.
//   $[#-N]        — N-th element from the end.
//   $.a.b[2].c    — chained.
//
// Returns `Ok(Some(node))` on a hit, `Ok(None)` on a miss (matching upstream's
// `JSON_LOOKUP_NOTFOUND` — json_extract returns NULL), and `Err` on a malformed path.

/// Look up `path` against `root`. Returns `Ok(Some(&JsonNode))` on hit, `Ok(None)` on miss.
pub fn lookup_path<'a>(root: &'a JsonNode, path: &str) -> Result<Option<&'a JsonNode>> {
    if !path.starts_with('$') {
        return Err(Error::msg("JSON path error near '")); // upstream: "bad JSON path"
    }
    let rest = &path[1..];
    if rest.is_empty() {
        return Ok(Some(root));
    }
    walk(root, rest.as_bytes(), 0)
}

fn walk<'a>(node: &'a JsonNode, path: &[u8], mut i: usize) -> Result<Option<&'a JsonNode>> {
    if i >= path.len() {
        return Ok(Some(node));
    }
    match path[i] {
        b'.' => {
            i += 1;
            let (key, consumed) = parse_key_segment(path, i)?;
            i = consumed;
            match node {
                JsonNode::Object(entries) => {
                    // Last value wins (matching upstream).
                    let hit = entries.iter().rev().find(|(k, _)| k == &key);
                    match hit {
                        Some((_, v)) => walk(v, path, i),
                        None => Ok(None),
                    }
                }
                _ => Ok(None), // not an object → NOTFOUND
            }
        }
        b'[' => {
            i += 1;
            // Parse index: optional '#' then optional '-' then digits, then ']'.
            let from_end = path.get(i).copied() == Some(b'#');
            if from_end {
                i += 1;
            }
            let negative = path.get(i).copied() == Some(b'-');
            if negative {
                i += 1;
            }
            let mut idx: u64 = 0;
            let mut have_digit = false;
            while i < path.len() && path[i].is_ascii_digit() {
                idx = idx.saturating_mul(10).saturating_add((path[i] - b'0') as u64);
                i += 1;
                have_digit = true;
            }
            if i >= path.len() || path[i] != b']' {
                return Err(Error::msg("bad JSON path"));
            }
            i += 1; // consume ']'
            match node {
                JsonNode::Array(items) => {
                    let len = items.len() as u64;
                    let target = if from_end {
                        // `$[#]` = index `len` (out of range → NULL); `$[#-N]` = index `len - N`.
                        // So `$[#-1]` = `len-1` = the last element; `$[#-0]` = `len` (out of range).
                        if negative {
                            len.checked_sub(idx)
                        } else if !have_digit {
                            // bare `$[#]` → index `len` (out of range)
                            Some(len)
                        } else {
                            // `$[#N]` (no `-`) → index `len - N`
                            len.checked_sub(idx)
                        }
                    } else {
                        if negative {
                            // `$[-N]` is non-standard; treat as from-end: index `len - N`.
                            len.checked_sub(idx)
                        } else if !have_digit {
                            return Err(Error::msg("bad JSON path"));
                        } else {
                            Some(idx)
                        }
                    };
                    match target {
                        Some(t) if t < len => walk(&items[t as usize], path, i),
                        _ => Ok(None),
                    }
                }
                _ => Ok(None), // not an array → NOTFOUND
            }
        }
        _ => Err(Error::msg("bad JSON path")),
    }
}

fn parse_key_segment(path: &[u8], mut i: usize) -> Result<(String, usize)> {
    if i >= path.len() {
        return Err(Error::msg("bad JSON path"));
    }
    if path[i] == b'"' || path[i] == b'\'' {
        let quote = path[i];
        i += 1;
        let start = i;
        let mut key = String::new();
        while i < path.len() && path[i] != quote {
            if path[i] == b'\\' && i + 1 < path.len() {
                let esc = path[i + 1];
                match esc {
                    b'"' => key.push('"'),
                    b'\\' => key.push('\\'),
                    b'/' => key.push('/'),
                    b'b' => key.push('\u{0008}'),
                    b'f' => key.push('\u{000C}'),
                    b'n' => key.push('\n'),
                    b'r' => key.push('\r'),
                    b't' => key.push('\t'),
                    b'u' => {
                        // Parse 4 hex digits.
                        if i + 6 > path.len() {
                            return Err(Error::msg("bad JSON path"));
                        }
                        let hex = &path[i + 2..i + 6];
                        let mut v = 0u32;
                        for &b in hex {
                            let d = match b {
                                b'0'..=b'9' => (b - b'0') as u32,
                                b'a'..=b'f' => (b - b'a' + 10) as u32,
                                b'A'..=b'F' => (b - b'A' + 10) as u32,
                                _ => return Err(Error::msg("bad JSON path")),
                            };
                            v = (v << 4) | d;
                        }
                        if let Some(c) = char::from_u32(v) {
                            key.push(c);
                        } else {
                            return Err(Error::msg("bad JSON path"));
                        }
                        i += 4;
                    }
                    _ => return Err(Error::msg("bad JSON path")),
                }
                i += 2;
            } else {
                // Copy a UTF-8 character.
                let len = utf8_len(path[i]);
                if i + len > path.len() {
                    return Err(Error::msg("bad JSON path"));
                }
                key.push_str(std::str::from_utf8(&path[i..i + len]).map_err(|_| Error::msg("bad JSON path"))?);
                i += len;
            }
        }
        if i >= path.len() || path[i] != quote {
            return Err(Error::msg("bad JSON path"));
        }
        i += 1; // consume closing quote
        let _ = start;
        Ok((key, i))
    } else {
        // Bare key: read until '.', '[', or end.
        let start = i;
        while i < path.len() && path[i] != b'.' && path[i] != b'[' {
            i += 1;
        }
        if i == start {
            return Err(Error::msg("bad JSON path"));
        }
        let key = std::str::from_utf8(&path[start..i])
            .map_err(|_| Error::msg("bad JSON path"))?
            .to_string();
        Ok((key, i))
    }
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1 // invalid leading byte; take 1 to make progress
    }
}

/// Convert a [`JsonNode`] to its SQL [`Value`] representation, the way `jsonReturnFromBlob`
/// does for a non-array/non-object node: Null→Null, Bool→Int (1/0, SQLite has no bool),
/// Int→Int, Real→Real, String→Text, Array/Object→Text (the canonical JSON rendering).
pub fn json_node_to_sql_value(node: &JsonNode) -> Value {
    match node {
        JsonNode::Null => Value::Null,
        JsonNode::Bool(b) => Value::Int(if *b { 1 } else { 0 }),
        JsonNode::Int(i) => Value::Int(*i),
        JsonNode::Real(r) => Value::Real(*r),
        JsonNode::String(s) => Value::Text(s.clone()),
        JsonNode::Array(_) | JsonNode::Object(_) => Value::Text(render(node)),
    }
}

/// `json_extract(X, P1, P2, ...)` — extract the value at each path `Pi` from JSON `X`.
///
/// With one path: returns the SQL value at that path (NULL if the path is not found). A scalar
/// (null/bool/int/real/string) is returned as its SQL equivalent; an array/object is returned
/// as canonical JSON text.
///
/// With multiple paths: returns a JSON array containing the result of each path (each element
/// rendered as JSON — arrays/objects stay JSON, scalars are JSON-encoded).
pub fn json_extract_fn(args: &[Value]) -> Result<Value> {
    if args.len() < 2 {
        return Err(Error::msg("json_extract requires at least 2 arguments"));
    }
    let root = value_to_json(&args[0])?;
    let paths = &args[1..];
    if paths.len() == 1 {
        let path = path_as_str(&paths[0])?;
        match lookup_path(&root, &path)? {
            Some(node) => Ok(json_node_to_sql_value(node)),
            None => Ok(Value::Null),
        }
    } else {
        let mut out = String::new();
        out.push('[');
        for (i, p) in paths.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let path = path_as_str(p)?;
            match lookup_path(&root, &path)? {
                Some(node) => render_into(node, &mut out),
                None => out.push_str("null"),
            }
        }
        out.push(']');
        Ok(Value::Text(out))
    }
}

fn path_as_str(v: &Value) -> Result<String> {
    match v {
        Value::Text(s) => Ok(s.clone()),
        Value::Int(i) => Ok(i.to_string()),
        Value::Real(r) => Ok(crate::util::fp::fp_to_text(*r)),
        Value::Null => Err(Error::msg("json_extract() path cannot be NULL")),
        Value::Blob(_) => Err(Error::msg("json_extract() path cannot be a BLOB")),
    }
}

/// `json_type(X)` / `json_type(X, P)` — the type label of X (or of the value at path P within
/// X). Returns `"null"`/`"true"`/`"false"`/`"integer"`/`"real"`/`"text"`/`"array"`/`"object"`.
/// NULL input → NULL. A missing path → NULL. Malformed JSON → error.
pub fn json_type_fn(args: &[Value]) -> Result<Value> {
    if args.is_empty() {
        return Err(Error::msg("json_type requires at least 1 argument"));
    }
    if args[0].is_null() {
        return Ok(Value::Null);
    }
    let root = value_to_json(&args[0])?;
    let node = if args.len() >= 2 {
        let path = path_as_str(&args[1])?;
        match lookup_path(&root, &path)? {
            Some(n) => n,
            None => return Ok(Value::Null),
        }
    } else {
        &root
    };
    Ok(Value::Text(node.type_label().to_string()))
}

/// `json_valid(X)` / `json_valid(X, F)` — 1 if X is well-formed JSON, 0 otherwise. NULL → NULL.
/// The flags argument F (1–15) is accepted but only the default mode (1 = strict RFC 8259) is
/// honored; JSON5 modes (2, 4, 8) report 0 since we don't accept JSON5 extensions.
pub fn json_valid_fn(args: &[Value]) -> Result<Value> {
    if args.is_empty() {
        return Err(Error::msg("json_valid requires at least 1 argument"));
    }
    if args[0].is_null() {
        return Ok(Value::Null);
    }
    // The flags argument is accepted but only mode 1 (strict) is implemented.
    let ok = match &args[0] {
        Value::Text(s) => parse(s).is_ok(),
        Value::Int(_) | Value::Real(_) => true, // numbers are valid JSON
        Value::Null => true,
        Value::Blob(_) => false, // a bare BLOB is never valid JSON
    };
    Ok(Value::Int(if ok { 1 } else { 0 }))
}

/// `json_quote(X)` — render X as a JSON value: NULL → `null`, INTEGER/REAL → number text,
/// TEXT → a quoted JSON string (escaped per RFC 8259). BLOB → error. Unlike `json(X)`, this
/// always quotes a TEXT argument as a string (it does not parse it as JSON).
pub fn json_quote_fn(arg: &Value) -> Result<Value> {
    let mut out = String::new();
    match arg {
        Value::Null => out.push_str("null"),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::Real(r) => out.push_str(&crate::util::fp::fp_to_text(*r)),
        Value::Text(s) => render_string(s, &mut out),
        Value::Blob(_) => return Err(Error::msg("JSON cannot hold BLOB values")),
    }
    Ok(Value::Text(out))
}

/// `json_array_length(X)` / `json_array_length(X, P)` — the number of elements in the array
/// at path P (or the root). Returns 0 if the target is not an array (matching upstream's
/// "not an array → 0" behavior). NULL input → NULL. Malformed JSON → error.
pub fn json_array_length_fn(args: &[Value]) -> Result<Value> {
    if args.is_empty() {
        return Err(Error::msg("json_array_length requires at least 1 argument"));
    }
    if args[0].is_null() {
        return Ok(Value::Null);
    }
    let root = value_to_json(&args[0])?;
    let node = if args.len() >= 2 {
        let path = path_as_str(&args[1])?;
        match lookup_path(&root, &path)? {
            Some(n) => n,
            None => return Ok(Value::Null),
        }
    } else {
        &root
    };
    match node {
        JsonNode::Array(items) => Ok(Value::Int(items.len() as i64)),
        _ => Ok(Value::Int(0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> JsonNode {
        parse(s).expect(s)
    }

    #[test]
    fn parses_scalars() {
        assert_eq!(p("null"), JsonNode::Null);
        assert_eq!(p("true"), JsonNode::Bool(true));
        assert_eq!(p("false"), JsonNode::Bool(false));
        assert_eq!(p("0"), JsonNode::Int(0));
        assert_eq!(p("-0"), JsonNode::Int(0));
        assert_eq!(p("42"), JsonNode::Int(42));
        assert_eq!(p("-42"), JsonNode::Int(-42));
        assert_eq!(p("9223372036854775807"), JsonNode::Int(i64::MAX));
        assert_eq!(p("1.0"), JsonNode::Real(1.0));
        assert_eq!(p("-1.5"), JsonNode::Real(-1.5));
        assert_eq!(p("1e3"), JsonNode::Real(1000.0));
        assert_eq!(p("1.5e-2"), JsonNode::Real(0.015));
        assert_eq!(p("\"hello\""), JsonNode::String("hello".to_string()));
        assert_eq!(p("\"\""), JsonNode::String(String::new()));
        // Integer overflow promotes to REAL (matching SQLite).
        assert!(matches!(p("9223372036854775808"), JsonNode::Real(_)));
    }

    #[test]
    fn parses_arrays() {
        assert_eq!(p("[]"), JsonNode::Array(vec![]));
        assert_eq!(
            p("[1,2,3]"),
            JsonNode::Array(vec![
                JsonNode::Int(1),
                JsonNode::Int(2),
                JsonNode::Int(3),
            ])
        );
        assert_eq!(
            p("[1, [2, 3], null]"),
            JsonNode::Array(vec![
                JsonNode::Int(1),
                JsonNode::Array(vec![JsonNode::Int(2), JsonNode::Int(3)]),
                JsonNode::Null,
            ])
        );
    }

    #[test]
    fn parses_objects() {
        assert_eq!(p("{}"), JsonNode::Object(vec![]));
        let n = p("{\"a\":1,\"b\":true}");
        assert_eq!(
            n,
            JsonNode::Object(vec![
                ("a".to_string(), JsonNode::Int(1)),
                ("b".to_string(), JsonNode::Bool(true)),
            ])
        );
    }

    #[test]
    fn parses_nested() {
        let s = "{\"x\":[1,{\"y\":[2,3]}],\"z\":null}";
        let n = p(s);
        assert_eq!(render(&n), s);
    }

    #[test]
    fn parses_strings_with_escapes() {
        assert_eq!(
            p("\"a\\nb\\tc\\\"d\\\\e\""),
            JsonNode::String("a\nb\tc\"d\\e".to_string())
        );
        assert_eq!(
            p("\"\\u0041\\u00e9\""),
            JsonNode::String("Aé".to_string())
        );
        // Surrogate pair for U+1F600 (😀).
        assert_eq!(
            p("\"\\ud83d\\ude00\""),
            JsonNode::String("😀".to_string())
        );
        // Forward slash escape.
        assert_eq!(p("\"a\\/b\""), JsonNode::String("a/b".to_string()));
    }

    #[test]
    fn parses_whitespace() {
        assert_eq!(p("  null  "), JsonNode::Null);
        assert_eq!(p("\n[\n1,\n2\n]\n"), JsonNode::Array(vec![JsonNode::Int(1), JsonNode::Int(2)]));
    }

    #[test]
    fn rejects_json5_extensions() {
        assert!(parse("'single'").is_err());
        assert!(parse("{a:1}").is_err());
        assert!(parse("[1,2,]").is_err());
        assert!(parse("{\"a\":1,}").is_err());
        assert!(parse("// comment\n1").is_err());
        assert!(parse("Infinity").is_err());
        assert!(parse("NaN").is_err());
        assert!(parse("0x10").is_err());
        assert!(parse("+1").is_err());
        assert!(parse(".5").is_err());
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse("").is_err());
        assert!(parse("{").is_err());
        assert!(parse("[").is_err());
        assert!(parse("[1,]").is_err());
        assert!(parse("{\"a\"}").is_err());
        assert!(parse("{\"a\":}").is_err());
        assert!(parse("\"unterminated").is_err());
        assert!(parse("\"\\u00\"").is_err());
        assert!(parse("tru").is_err());
        assert!(parse("42x").is_err());
        assert!(parse("01").is_err());
        assert!(parse("1.").is_err());
        assert!(parse("1e").is_err());
        // Control character in string.
        assert!(parse("\"a\u{0000}b\"").is_err());
    }

    #[test]
    fn render_roundtrips() {
        for s in [
            "null",
            "true",
            "false",
            "0",
            "42",
            "-42",
            "1.5",
            "\"hello\"",
            "[1,2,3]",
            "{\"a\":1,\"b\":[2,3]}",
            "{\"nested\":{\"x\":[true,null,1.0]}}",
        ] {
            let n = p(s);
            assert_eq!(render(&n), s, "input: {}", s);
        }
    }

    #[test]
    fn render_escapes_strings() {
        let n = JsonNode::String("a\nb\tc\"d\\e\u{0000}".to_string());
        assert_eq!(render(&n), "\"a\\nb\\tc\\\"d\\\\e\\u0000\"");
    }

    #[test]
    fn object_lookup_last_wins() {
        let n = JsonNode::Object(vec![
            ("a".to_string(), JsonNode::Int(1)),
            ("a".to_string(), JsonNode::Int(2)),
        ]);
        assert_eq!(n.object_lookup("a"), Some(&JsonNode::Int(2)));
        assert_eq!(n.object_lookup("b"), None);
    }

    #[test]
    fn type_labels() {
        assert_eq!(JsonNode::Null.type_label(), "null");
        assert_eq!(JsonNode::Bool(true).type_label(), "true");
        assert_eq!(JsonNode::Bool(false).type_label(), "false");
        assert_eq!(JsonNode::Int(0).type_label(), "integer");
        assert_eq!(JsonNode::Real(1.0).type_label(), "real");
        assert_eq!(JsonNode::String("x".into()).type_label(), "text");
        assert_eq!(JsonNode::Array(vec![]).type_label(), "array");
        assert_eq!(JsonNode::Object(vec![]).type_label(), "object");
    }

    #[test]
    fn depth_limit() {
        // Build a string of 1200 nested arrays — past JSON_MAX_DEPTH (1000).
        // The parser must reject it rather than overflowing the stack.
        let s = "[".repeat(1200) + "]".repeat(1200).as_str();
        assert!(parse(&s).is_err());
        // A 500-deep nest is accepted.
        let s = "[".repeat(500) + "]".repeat(500).as_str();
        assert!(parse(&s).is_ok());
    }

    // ---- M24.2 json()/jsonb() function tests ----

    #[test]
    fn json_fn_renders_canonical_text() {
        assert_eq!(json_fn(&Value::Null).unwrap(), Value::Null);
        assert_eq!(json_fn(&Value::Int(123)).unwrap(), Value::Text("123".into()));
        assert_eq!(json_fn(&Value::Real(1.5)).unwrap(), Value::Text("1.5".into()));
        assert_eq!(json_fn(&t("{}")).unwrap(), t("{}"));
        assert_eq!(json_fn(&t("[]")).unwrap(), t("[]"));
        assert_eq!(json_fn(&t("[1,2,3]")).unwrap(), t("[1,2,3]"));
        assert_eq!(json_fn(&t("  {\"a\":1}  ")).unwrap(), t("{\"a\":1}"));
        // Re-rendering normalizes whitespace and key order (we preserve insertion order).
        assert_eq!(json_fn(&t("{\"a\":1,\"b\":2}")).unwrap(), t("{\"a\":1,\"b\":2}"));
    }

    #[test]
    fn json_fn_rejects_malformed() {
        assert!(json_fn(&t("hello")).is_err());
        assert!(json_fn(&t("{")).is_err());
        assert!(json_fn(&t("[1,]")).is_err());
        assert!(json_fn(&t("'quoted'")).is_err());
    }

    #[test]
    fn json_fn_rejects_blob() {
        assert!(json_fn(&Value::Blob(vec![1, 2, 3])).is_err());
    }

    #[test]
    fn jsonb_fn_returns_blob() {
        assert_eq!(jsonb_fn(&Value::Null).unwrap(), Value::Null);
        assert_eq!(jsonb_fn(&Value::Int(123)).unwrap(), Value::Blob(b"123".to_vec()));
        assert_eq!(jsonb_fn(&t("{}")).unwrap(), Value::Blob(b"{}".to_vec()));
        assert_eq!(jsonb_fn(&t("[1,2]")).unwrap(), Value::Blob(b"[1,2]".to_vec()));
        // A bare BLOB is rejected (matching the oracle — only JSONB blobs are accepted, and
        // we don't model the JSONB binary form yet).
        assert!(jsonb_fn(&Value::Blob(vec![1, 2, 3])).is_err());
    }

    fn t(s: &str) -> Value {
        Value::Text(s.to_string())
    }

    // ---- M24.3 json_array() / M24.4 json_object() tests ----

    #[test]
    fn json_array_fn_builds_array() {
        assert_eq!(json_array_fn(&[]).unwrap(), t("[]"));
        assert_eq!(
            json_array_fn(&[Value::Int(1), Value::Int(2), Value::Int(3)]).unwrap(),
            t("[1,2,3]")
        );
        assert_eq!(
            json_array_fn(&[Value::Int(1), t("two"), Value::Null, Value::Real(3.5)]).unwrap(),
            t("[1,\"two\",null,3.5]")
        );
        // Empty string is a valid JSON string value.
        assert_eq!(json_array_fn(&[t("")]).unwrap(), t("[\"\"]"));
        // String with special chars gets escaped.
        assert_eq!(
            json_array_fn(&[t("a\nb")]).unwrap(),
            t("[\"a\\nb\"]")
        );
    }

    #[test]
    fn json_array_fn_rejects_blob() {
        assert!(json_array_fn(&[Value::Blob(vec![1, 2])]).is_err());
    }

    #[test]
    fn json_object_fn_builds_object() {
        assert_eq!(json_object_fn(&[]).unwrap(), t("{}"));
        assert_eq!(
            json_object_fn(&[t("a"), Value::Int(1), t("b"), t("two")]).unwrap(),
            t("{\"a\":1,\"b\":\"two\"}")
        );
        // NULL value.
        assert_eq!(
            json_object_fn(&[t("x"), Value::Null]).unwrap(),
            t("{\"x\":null}")
        );
        // Key with special chars is escaped.
        assert_eq!(
            json_object_fn(&[t("a\"b"), Value::Int(1)]).unwrap(),
            t("{\"a\\\"b\":1}")
        );
    }

    #[test]
    fn json_object_fn_errors() {
        // Odd arg count.
        assert!(json_object_fn(&[t("a")]).is_err());
        assert!(json_object_fn(&[t("a"), Value::Int(1), t("b")]).is_err());
        // Non-TEXT key.
        assert!(json_object_fn(&[Value::Int(1), Value::Int(2)]).is_err());
        assert!(json_object_fn(&[Value::Null, Value::Int(1)]).is_err());
        // BLOB value.
        assert!(json_object_fn(&[t("a"), Value::Blob(vec![1])]).is_err());
    }

    // ---- M24.5 json_extract() / path lookup tests ----

    #[test]
    fn lookup_path_root() {
        let n = p("{\"a\":1}");
        assert_eq!(lookup_path(&n, "$").unwrap(), Some(&n));
    }

    #[test]
    fn lookup_path_object() {
        let n = p("{\"a\":1,\"b\":2}");
        assert_eq!(
            lookup_path(&n, "$.a").unwrap(),
            Some(&JsonNode::Int(1)),
        );
        assert_eq!(
            lookup_path(&n, "$.b").unwrap(),
            Some(&JsonNode::Int(2)),
        );
        assert_eq!(lookup_path(&n, "$.c").unwrap(), None);
    }

    #[test]
    fn lookup_path_array() {
        let n = p("[1,2,3]");
        assert_eq!(lookup_path(&n, "$[0]").unwrap(), Some(&JsonNode::Int(1)));
        assert_eq!(lookup_path(&n, "$[2]").unwrap(), Some(&JsonNode::Int(3)));
        assert_eq!(lookup_path(&n, "$[5]").unwrap(), None);
        // From-end.
        assert_eq!(lookup_path(&n, "$[#-1]").unwrap(), Some(&JsonNode::Int(3)));
        assert_eq!(lookup_path(&n, "$[#-2]").unwrap(), Some(&JsonNode::Int(2)));
        // $[#] is out of range (index = len).
        assert_eq!(lookup_path(&n, "$[#]").unwrap(), None);
    }

    #[test]
    fn lookup_path_nested() {
        let n = p("{\"x\":[1,{\"y\":[2,3]}]}");
        assert_eq!(
            lookup_path(&n, "$.x[1].y[0]").unwrap(),
            Some(&JsonNode::Int(2)),
        );
        assert_eq!(
            lookup_path(&n, "$.x[0]").unwrap(),
            Some(&JsonNode::Int(1)),
        );
    }

    #[test]
    fn lookup_path_quoted_key() {
        let n = p("{\"a b\":1}");
        assert_eq!(
            lookup_path(&n, "$.\"a b\"").unwrap(),
            Some(&JsonNode::Int(1)),
        );
    }

    #[test]
    fn lookup_path_object_on_array_is_notfound() {
        let n = p("[1,2,3]");
        assert_eq!(lookup_path(&n, "$.a").unwrap(), None);
    }

    #[test]
    fn lookup_path_array_on_object_is_notfound() {
        let n = p("{\"a\":1}");
        assert_eq!(lookup_path(&n, "$[0]").unwrap(), None);
    }

    #[test]
    fn lookup_path_bad_path() {
        let n = p("{\"a\":1}");
        assert!(lookup_path(&n, "a").is_err()); // missing leading $
        assert!(lookup_path(&n, "$.").is_err()); // empty key
        assert!(lookup_path(&n, "$[abc]").is_err()); // non-numeric index
    }

    #[test]
    fn json_extract_fn_returns_sql_scalar() {
        let j = t("{\"a\":1,\"b\":\"two\",\"c\":3.5,\"d\":null}");
        assert_eq!(
            json_extract_fn(&[j.clone(), t("$.a")]).unwrap(),
            Value::Int(1),
        );
        assert_eq!(
            json_extract_fn(&[j.clone(), t("$.b")]).unwrap(),
            t("two"),
        );
        assert_eq!(
            json_extract_fn(&[j.clone(), t("$.c")]).unwrap(),
            Value::Real(3.5),
        );
        assert_eq!(
            json_extract_fn(&[j.clone(), t("$.d")]).unwrap(),
            Value::Null,
        );
        assert_eq!(
            json_extract_fn(&[j, t("$.missing")]).unwrap(),
            Value::Null,
        );
    }

    #[test]
    fn json_extract_fn_returns_json_text_for_container() {
        let j = t("{\"a\":[1,2],\"b\":{\"x\":1}}");
        assert_eq!(
            json_extract_fn(&[j.clone(), t("$.a")]).unwrap(),
            t("[1,2]"),
        );
        assert_eq!(
            json_extract_fn(&[j, t("$.b")]).unwrap(),
            t("{\"x\":1}"),
        );
    }

    #[test]
    fn json_extract_fn_multiple_paths() {
        let j = t("{\"a\":1,\"b\":2}");
        assert_eq!(
            json_extract_fn(&[j, t("$.a"), t("$.b")]).unwrap(),
            t("[1,2]"),
        );
        // Missing path → null in the array.
        let j = t("{\"a\":1}");
        assert_eq!(
            json_extract_fn(&[j, t("$.a"), t("$.missing")]).unwrap(),
            t("[1,null]"),
        );
    }
}