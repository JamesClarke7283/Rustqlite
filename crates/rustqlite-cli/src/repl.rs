//! The read-eval-print loop and one-shot/batch drivers (mirrors `process_input` in `shell.c`).

use std::io::{BufRead, IsTerminal};

use rustqlite::vfs::OpenFlags;
use rustqlite::{sqlite3_open_v2, sqlite3_prepare_v2};

use crate::cli::Cli;
use crate::dot_cmd::{self, Flow};
use crate::state::ShellState;

/// Run the shell to completion, returning the process exit code.
pub fn run(cli: Cli) -> i32 {
    let mut state = ShellState::from_cli(&cli);

    let filename = cli
        .filename
        .clone()
        .unwrap_or_else(|| ":memory:".to_string());
    let flags = if cli.readonly {
        OpenFlags::READONLY
    } else {
        OpenFlags::READWRITE_CREATE
    };
    match sqlite3_open_v2(&filename, flags) {
        Ok(conn) => {
            state.conn = Some(conn);
            state.db_filename = if filename == ":memory:" {
                String::new()
            } else {
                filename.clone()
            };
        }
        Err(e) => {
            eprintln!("Error: unable to open database \"{filename}\": {e}");
            return 1;
        }
    }

    // `-cmd` commands run before any input.
    for cmd in &cli.cmd {
        if let Flow::Quit = execute(&mut state, cmd) {
            return 0;
        }
    }

    // One-shot mode: run the trailing arguments, then exit.
    if !cli.args.is_empty() {
        for arg in &cli.args {
            if let Flow::Quit = execute(&mut state, arg) {
                break;
            }
        }
        return 0;
    }

    let interactive = cli.interactive || (!cli.batch && std::io::stdin().is_terminal());
    if interactive {
        interactive_loop(&mut state)
    } else {
        batch_loop(&mut state)
    }
}

/// Run one line of input: a dot-command, or (accumulated) SQL.
fn execute(state: &mut ShellState, text: &str) -> Flow {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Flow::Continue;
    }
    if state.echo {
        println!("{trimmed}");
    }
    if trimmed.starts_with('.') {
        dot_cmd::dispatch(state, trimmed)
    } else {
        let had_error = handle_sql(state, text);
        // `-bail` stops at the first error, as in the sqlite3 shell.
        if had_error && state.bail {
            Flow::Quit
        } else {
            Flow::Continue
        }
    }
}

/// Compile and (eventually) run SQL. At M1 there is no VDBE, so execution is reported as
/// pending; syntax errors are surfaced via the real parser. Returns `true` on error.
fn handle_sql(state: &mut ShellState, sql: &str) -> bool {
    let Some(conn) = state.conn.as_mut() else {
        eprintln!("Error: no database is open");
        return true;
    };
    match sqlite3_prepare_v2(conn, sql) {
        Ok((_stmt, _tail)) => {
            eprintln!(
                "Error: SQL execution is not implemented yet (statement parsed OK; \
                 pending the VDBE, M3). Use .tables / .schema to read databases today."
            );
            true
        }
        Err(e) => {
            eprintln!("Parse error: {}", e.message);
            true
        }
    }
}

fn interactive_loop(state: &mut ShellState) -> i32 {
    use rustyline::error::ReadlineError;
    use rustyline::DefaultEditor;

    let mut rl = match DefaultEditor::new() {
        Ok(rl) => rl,
        Err(e) => {
            eprintln!("Error: cannot start line editor: {e}");
            return 1;
        }
    };

    let mut buffer = String::new();
    loop {
        let prompt = if buffer.is_empty() {
            "rustqlite> "
        } else {
            "    ...> "
        };
        match rl.readline(prompt) {
            Ok(line) => {
                let _ = rl.add_history_entry(line.as_str());
                if buffer.is_empty() && line.trim_start().starts_with('.') {
                    if let Flow::Quit = execute(state, &line) {
                        break;
                    }
                    continue;
                }
                buffer.push_str(&line);
                buffer.push('\n');
                if statement_complete(&buffer) {
                    let stmt = std::mem::take(&mut buffer);
                    handle_sql(state, &stmt);
                }
            }
            Err(ReadlineError::Interrupted) => buffer.clear(), // Ctrl-C abandons the line
            Err(ReadlineError::Eof) => break,                  // Ctrl-D exits
            Err(e) => {
                eprintln!("Error: {e}");
                break;
            }
        }
    }
    0
}

fn batch_loop(state: &mut ShellState) -> i32 {
    let stdin = std::io::stdin();
    let mut buffer = String::new();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if buffer.is_empty() && line.trim_start().starts_with('.') {
            if let Flow::Quit = execute(state, &line) {
                return 0;
            }
            continue;
        }
        buffer.push_str(&line);
        buffer.push('\n');
        if statement_complete(&buffer) {
            let stmt = std::mem::take(&mut buffer);
            let had_error = handle_sql(state, &stmt);
            if had_error && state.bail {
                return 1;
            }
        }
    }
    0
}

/// A naive statement-completion check: the (trimmed) buffer ends with a semicolon. This does
/// not yet understand semicolons inside string/blob literals; the tokenizer-driven check from
/// `shell.c` arrives with the full parser integration.
fn statement_complete(buffer: &str) -> bool {
    buffer.trim_end().ends_with(';')
}
