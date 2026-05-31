//! The read-eval-print loop and one-shot/batch drivers (mirrors `process_input` in `shell.c`).

use std::io::{BufRead, IsTerminal};

use rustsqlite_core::capi::ResultCode;
use rustsqlite_core::vfs::OpenFlags;
use rustsqlite_core::{sqlite3_open_v2, sqlite3_prepare_v2, Value};

use crate::cli::Cli;
use crate::dot_cmd::{self, Flow};
use crate::output::{format_rows, render_eqp_tree, render_explain_bytecode};
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

/// Compile and run one SQL statement, printing its result rows in the current output mode.
/// Returns `true` on error.
fn handle_sql(state: &mut ShellState, sql: &str) -> bool {
    // Borrow the mode/output settings up front so we can re-borrow `conn` mutably below.
    let mode = state.mode;
    let conn = match state.conn.as_mut() {
        Some(c) => c,
        None => {
            eprintln!("Error: no database is open");
            return true;
        }
    };

    let (mut stmt, _tail) = match sqlite3_prepare_v2(conn, sql) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: {}", e.message);
            return true;
        }
    };

    let ncol = stmt.column_count();
    let columns: Vec<String> = (0..ncol)
        .map(|i| stmt.column_name(i).unwrap_or("").to_string())
        .collect();
    // The shell renders EXPLAIN regardless of the active `.mode`: plain EXPLAIN as a fixed
    // columnar table, EXPLAIN QUERY PLAN as the `QUERY PLAN` tree. 0 = normal, 1 = EXPLAIN,
    // 2 = EXPLAIN QUERY PLAN (mirrors sqlite3_stmt_isexplain).
    let explain_kind = stmt.explain_kind();

    let mut rows: Vec<Vec<Value>> = Vec::new();
    loop {
        match stmt.step() {
            ResultCode::Row => {
                rows.push((0..ncol).map(|i| stmt.column_value(i)).collect());
            }
            ResultCode::Done => break,
            _ => {
                eprintln!("Error: {}", stmt.errmsg());
                return true;
            }
        }
    }

    if ncol > 0 {
        match explain_kind {
            // EXPLAIN QUERY PLAN: the tree, not a column table, regardless of `.mode`.
            2 => print!("{}", render_eqp_tree(&rows)),
            // Plain EXPLAIN: a fixed columnar table (addr|opcode|p1|p2|p3|p4|p5|comment),
            // headers on, regardless of the user's `.mode`.
            1 => print!("{}", render_explain_bytecode(&columns, &rows)),
            _ => print!("{}", format_rows(mode, state, &columns, &rows)),
        }
    }
    false
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
            "rustsqlite> "
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
