//! Shell session state (mirrors the `ShellState` struct in `shell.c`).

use rustsqlite_core::Sqlite3;

use crate::cli::{Cli, OutputMode};

/// Mutable state carried across a shell session: the open connection, output mode, separators,
/// and the various on/off toggles.
pub struct ShellState {
    pub conn: Option<Sqlite3>,
    pub db_filename: String,
    pub mode: OutputMode,
    pub headers: bool,
    pub colsep: String,
    pub rowsep: String,
    pub nullvalue: String,
    /// Target table name for `insert` mode (set by `.mode insert <TABLE>`; default `"tab"`).
    pub insert_table: String,
    pub echo: bool,
    pub bail: bool,
}

impl ShellState {
    /// Build initial state from parsed CLI flags (before the database is opened).
    pub fn from_cli(cli: &Cli) -> ShellState {
        ShellState {
            conn: None,
            db_filename: String::new(),
            mode: cli.resolved_mode().unwrap_or(OutputMode::List),
            headers: cli.header && !cli.noheader,
            colsep: cli.separator.clone().unwrap_or_else(|| "|".to_string()),
            rowsep: cli.newline.clone().unwrap_or_else(|| "\n".to_string()),
            nullvalue: cli.nullvalue.clone().unwrap_or_default(),
            insert_table: "tab".to_string(),
            echo: cli.echo,
            bail: cli.bail,
        }
    }
}
