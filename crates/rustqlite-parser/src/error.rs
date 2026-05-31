//! Parser error type.

use std::fmt;

/// An error produced while parsing SQL text.
///
/// Wraps the underlying pest error message (which carries a caret-annotated location), so
/// callers get a human-readable, location-aware message similar in spirit to SQLite's
/// `"near \"X\": syntax error"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    message: String,
}

impl ParseError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        ParseError {
            message: message.into(),
        }
    }

    /// The full, rendered error message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ParseError {}
