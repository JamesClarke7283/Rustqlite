//! The Rustqlite shell binary (`rustqlite`) — mirrors the `sqlite3` command-line tool.
//!
//! Flags are parsed with clap derive ([`cli`]); dot-commands are dispatched in the REPL
//! ([`repl`]/[`dot_cmd`]), not as clap subcommands, exactly as in `shell.c`.

mod cli;
mod dot_cmd;
mod output;
mod repl;
mod state;

use clap::Parser;

use cli::{normalize_args, Cli};

fn main() {
    let args = normalize_args(std::env::args());
    let cli = Cli::parse_from(args);

    // `-version` mirrors `sqlite3 -version`: the targeted SQLite version + source id.
    if cli.version {
        println!(
            "{} {}",
            rustqlite::sqlite3_libversion(),
            rustqlite::sqlite3_sourceid()
        );
        return;
    }

    std::process::exit(repl::run(cli));
}
