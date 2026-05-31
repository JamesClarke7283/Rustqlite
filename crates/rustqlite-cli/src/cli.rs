//! Command-line argument parsing (mirrors `shell.c`'s option handling).
//!
//! Flags mirror the `sqlite3` shell. The shell historically uses single-dash long options
//! (`-version`, `-csv`, `-init`); clap uses `--`, so [`normalize_args`] rewrites single-dash
//! *words* to `--word` before parsing, giving sqlite3-style invocation on top of clap derive.

use clap::{Parser, ValueEnum};

/// The Rustqlite shell.
#[derive(Parser, Debug)]
#[command(
    name = "rustqlite",
    about = "Rustqlite shell — a faithful, sqlite3-compatible command-line interface.",
    disable_version_flag = true
)]
pub struct Cli {
    /// Database file to open. Defaults to a transient in-memory database.
    pub filename: Option<String>,

    /// SQL statements and/or dot-commands to run, then exit (one-shot mode).
    pub args: Vec<String>,

    /// Read/run commands from FILE on startup.
    #[arg(long)]
    pub init: Option<String>,

    /// Stop after hitting an error.
    #[arg(long)]
    pub bail: bool,

    /// Print commands before execution.
    #[arg(long)]
    pub echo: bool,

    /// Open the database read-only.
    #[arg(long)]
    pub readonly: bool,

    /// Turn headers on.
    #[arg(long)]
    pub header: bool,

    /// Turn headers off.
    #[arg(long = "noheader")]
    pub noheader: bool,

    /// Set the column separator for `list` mode.
    #[arg(long)]
    pub separator: Option<String>,

    /// Set the text shown for NULL values.
    #[arg(long)]
    pub nullvalue: Option<String>,

    /// Set the row separator for `list` mode.
    #[arg(long)]
    pub newline: Option<String>,

    /// Run a command before reading stdin (may be given more than once).
    #[arg(long = "cmd")]
    pub cmd: Vec<String>,

    /// Force batch (non-interactive) I/O.
    #[arg(long)]
    pub batch: bool,

    /// Force interactive I/O.
    #[arg(long)]
    pub interactive: bool,

    /// Show the targeted SQLite version and exit.
    #[arg(long)]
    pub version: bool,

    // ---- output-mode flags (each selects a `--mode`) ----
    #[arg(long)]
    pub csv: bool,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub html: bool,
    #[arg(long)]
    pub line: bool,
    #[arg(long)]
    pub column: bool,
    #[arg(long = "box")]
    pub box_: bool,
    #[arg(long)]
    pub markdown: bool,
    #[arg(long)]
    pub list: bool,
    #[arg(long)]
    pub ascii: bool,
    #[arg(long)]
    pub quote: bool,
    #[arg(long)]
    pub table: bool,
    #[arg(long)]
    pub tabs: bool,

    /// Set the output mode explicitly.
    #[arg(long)]
    pub mode: Option<OutputMode>,
}

/// The shell's output modes (mirrors `shell.c`'s `MODE_*`).
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputMode {
    Ascii,
    #[value(name = "box")]
    Boxed,
    Csv,
    Column,
    Html,
    Insert,
    Json,
    Line,
    List,
    Markdown,
    Quote,
    Table,
    Tabs,
}

impl Cli {
    /// Resolve the output mode from the boolean flags / `--mode` (explicit `--mode` wins).
    pub fn resolved_mode(&self) -> Option<OutputMode> {
        if let Some(m) = self.mode {
            return Some(m);
        }
        let pairs = [
            (self.csv, OutputMode::Csv),
            (self.json, OutputMode::Json),
            (self.html, OutputMode::Html),
            (self.line, OutputMode::Line),
            (self.column, OutputMode::Column),
            (self.box_, OutputMode::Boxed),
            (self.markdown, OutputMode::Markdown),
            (self.list, OutputMode::List),
            (self.ascii, OutputMode::Ascii),
            (self.quote, OutputMode::Quote),
            (self.table, OutputMode::Table),
            (self.tabs, OutputMode::Tabs),
        ];
        pairs.into_iter().find_map(|(set, m)| set.then_some(m))
    }
}

/// Rewrite single-dash long options (`-version`) to clap's double-dash form (`--version`),
/// leaving real positionals and single-character flags untouched.
pub fn normalize_args(raw: impl IntoIterator<Item = String>) -> Vec<String> {
    raw.into_iter()
        .map(|arg| {
            let is_single_dash_word = arg.starts_with('-')
                && !arg.starts_with("--")
                && arg.len() > 2
                && arg.as_bytes()[1].is_ascii_alphabetic();
            if is_single_dash_word {
                format!("-{arg}")
            } else {
                arg
            }
        })
        .collect()
}
