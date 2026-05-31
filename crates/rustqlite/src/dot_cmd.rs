//! Dot-command dispatch (mirrors `do_meta_command` in `shell.c`).
//!
//! Dot-commands are NOT clap subcommands — they are parsed and dispatched here in the REPL.
//! Unambiguous prefixes are accepted (`.tab` → `.tables`), matching the shell.

use rustsqlite_core::format::TextEncoding;

use crate::cli::OutputMode;
use crate::output;
use crate::state::ShellState;

/// What the REPL should do after a command.
pub enum Flow {
    Continue,
    Quit,
}

const COMMANDS: &[&str] = &[
    "databases",
    "dbinfo",
    "exit",
    "headers",
    "help",
    "mode",
    "nullvalue",
    "open",
    "quit",
    "schema",
    "separator",
    "show",
    "tables",
    "version",
];

/// Dispatch a single dot-command line (the leading `.` included).
pub fn dispatch(state: &mut ShellState, line: &str) -> Flow {
    let tokens = tokenize(line.trim());
    let Some(cmd_tok) = tokens.first() else {
        return Flow::Continue;
    };
    let name = cmd_tok.strip_prefix('.').unwrap_or(cmd_tok);
    let args = &tokens[1..];

    match resolve(name) {
        Some("quit") | Some("exit") => return Flow::Quit,
        Some("help") => print_help(),
        Some("open") => cmd_open(state, args),
        Some("databases") => cmd_databases(state),
        Some("tables") => cmd_tables(state),
        Some("schema") => cmd_schema(state, args),
        Some("mode") => cmd_mode(state, args),
        Some("headers") => cmd_headers(state, args),
        Some("nullvalue") => state.nullvalue = args.first().cloned().unwrap_or_default(),
        Some("separator") => {
            if let Some(s) = args.first() {
                state.colsep = s.clone();
            }
        }
        Some("show") => cmd_show(state),
        Some("version") => print_version(),
        Some("dbinfo") => cmd_dbinfo(state),
        _ => eprintln!(
            "Error: unknown command or invalid arguments: \"{name}\". Enter \".help\" for help"
        ),
    }
    Flow::Continue
}

/// Resolve a (possibly abbreviated) command name to its canonical form, requiring an
/// unambiguous prefix as the shell does.
fn resolve(name: &str) -> Option<&'static str> {
    if let Some(exact) = COMMANDS.iter().find(|c| **c == name) {
        return Some(exact);
    }
    let mut matches = COMMANDS.iter().filter(|c| c.starts_with(name));
    let first = matches.next()?;
    if matches.next().is_none() {
        Some(first)
    } else {
        None // ambiguous prefix
    }
}

fn cmd_open(state: &mut ShellState, args: &[String]) {
    let Some(path) = args.first() else {
        eprintln!("Error: .open requires a filename");
        return;
    };
    match rustsqlite_core::sqlite3_open(path) {
        Ok(conn) => {
            state.conn = Some(conn);
            state.db_filename = path.clone();
        }
        Err(e) => eprintln!("Error: unable to open database \"{path}\": {e}"),
    }
}

fn cmd_databases(state: &ShellState) {
    // The shell prints: seq "name" "file". The main database is sequence 0, name "main".
    let file = if state.db_filename.is_empty() {
        ""
    } else {
        &state.db_filename
    };
    println!("main: {file}");
}

fn cmd_tables(state: &mut ShellState) {
    let Some(conn) = state.conn.as_mut() else {
        eprintln!("Error: no database is open");
        return;
    };
    match conn.read_schema() {
        Ok(catalog) => {
            let mut names: Vec<String> = catalog
                .objects
                .iter()
                .filter(|o| {
                    (o.obj_type == "table" || o.obj_type == "view")
                        && !o.name.starts_with("sqlite_")
                })
                .map(|o| o.name.clone())
                .collect();
            names.sort();
            print!("{}", output::tables_grid(&names));
        }
        Err(e) => eprintln!("Error: {e}"),
    }
}

fn cmd_schema(state: &mut ShellState, args: &[String]) {
    let Some(conn) = state.conn.as_mut() else {
        eprintln!("Error: no database is open");
        return;
    };
    let filter = args.first();
    match conn.read_schema() {
        Ok(catalog) => {
            for obj in &catalog.objects {
                let Some(sql) = &obj.sql else { continue };
                if let Some(f) = filter {
                    if !obj.name.eq_ignore_ascii_case(f) && !obj.tbl_name.eq_ignore_ascii_case(f) {
                        continue;
                    }
                }
                println!("{sql};");
            }
        }
        Err(e) => eprintln!("Error: {e}"),
    }
}

fn cmd_mode(state: &mut ShellState, args: &[String]) {
    match args.first().map(|s| s.as_str()) {
        Some(name) => match parse_mode(name) {
            Some(mode) => {
                state.mode = mode;
                // The tabular modes turn headers on (the shell does this when the mode is set);
                // an explicit `.headers off` afterwards still wins.
                if matches!(
                    mode,
                    OutputMode::Column | OutputMode::Boxed | OutputMode::Table | OutputMode::Markdown
                ) {
                    state.headers = true;
                }
            }
            None => eprintln!("Error: mode should be one of: ascii box csv column html insert json line list markdown quote table tabs"),
        },
        None => println!("current output mode: {:?}", state.mode),
    }
}

fn cmd_headers(state: &mut ShellState, args: &[String]) {
    match args.first().map(|s| s.as_str()) {
        Some("on") => state.headers = true,
        Some("off") => state.headers = false,
        _ => eprintln!("Error: .headers requires \"on\" or \"off\""),
    }
}

fn cmd_show(state: &ShellState) {
    println!("        echo: {}", on_off(state.echo));
    println!("        mode: {:?}", state.mode);
    println!("   nullvalue: \"{}\"", state.nullvalue);
    println!("   separator: \"{}\"", state.colsep);
    println!("rowseparator: \"{}\"", state.rowsep.escape_default());
    println!("     headers: {}", on_off(state.headers));
    println!("    filename: {}", state.db_filename);
}

fn cmd_dbinfo(state: &ShellState) {
    let Some(conn) = state.conn.as_ref() else {
        eprintln!("Error: no database is open");
        return;
    };
    match conn.db_header() {
        Some(h) => {
            println!("{:<20} {}", "page size:", h.page_size);
            println!("{:<20} {}", "number of pages:", conn.page_count());
            println!("{:<20} {}", "write version:", h.write_version);
            println!("{:<20} {}", "read version:", h.read_version);
            println!("{:<20} {}", "reserved bytes:", h.reserved_space);
            println!("{:<20} {}", "schema cookie:", h.schema_cookie);
            println!("{:<20} {}", "schema format:", h.schema_format);
            println!(
                "{:<20} {}",
                "text encoding:",
                match h.text_encoding {
                    TextEncoding::Utf8 => "1 (utf8)",
                    TextEncoding::Utf16Le => "2 (utf16le)",
                    TextEncoding::Utf16Be => "3 (utf16be)",
                }
            );
            println!("{:<20} {}", "user version:", h.user_version);
        }
        None => eprintln!("Error: database is empty (no header yet)"),
    }
}

fn print_version() {
    println!(
        "SQLite {} {}",
        rustsqlite_core::sqlite3_libversion(),
        rustsqlite_core::sqlite3_sourceid()
    );
    println!("(rustsqlite — a faithful Rust reimplementation)");
}

fn print_help() {
    println!(
        "\
.databases             List names and files of attached databases
.dbinfo                Show status information about the database
.exit                  Exit this program
.headers on|off        Turn display of headers on or off
.help                  Show this message
.mode MODE             Set output mode
.nullvalue STRING      Use STRING in place of NULL values
.open FILE             Open FILE as the database
.quit                  Exit this program
.schema [NAME]         Show CREATE statements
.separator STRING      Set the column separator
.show                  Show the current values for various settings
.tables                List names of tables
.version               Show the SQLite version targeted by rustsqlite"
    );
}

fn on_off(b: bool) -> &'static str {
    if b {
        "on"
    } else {
        "off"
    }
}

fn parse_mode(name: &str) -> Option<OutputMode> {
    Some(match name {
        "ascii" => OutputMode::Ascii,
        "box" => OutputMode::Boxed,
        "csv" => OutputMode::Csv,
        "column" => OutputMode::Column,
        "html" => OutputMode::Html,
        "insert" => OutputMode::Insert,
        "json" => OutputMode::Json,
        "line" => OutputMode::Line,
        "list" => OutputMode::List,
        "markdown" => OutputMode::Markdown,
        "quote" => OutputMode::Quote,
        "table" => OutputMode::Table,
        "tabs" => OutputMode::Tabs,
        _ => return None,
    })
}

/// Split a dot-command line into tokens, honoring simple single/double quoting so that
/// `.open "my db.db"` and `.separator "|"` work.
fn tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut quote: Option<char> = None;
    for ch in line.chars() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                } else {
                    current.push(ch);
                }
            }
            None => {
                if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                    in_token = true;
                } else if ch.is_whitespace() {
                    if in_token {
                        tokens.push(std::mem::take(&mut current));
                        in_token = false;
                    }
                } else {
                    current.push(ch);
                    in_token = true;
                }
            }
        }
    }
    if in_token {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_unambiguous_prefix() {
        assert_eq!(resolve("tables"), Some("tables"));
        assert_eq!(resolve("tab"), Some("tables"));
        assert_eq!(resolve("q"), Some("quit"));
        // "s" is ambiguous: schema, separator, show.
        assert_eq!(resolve("s"), None);
        assert_eq!(resolve("sch"), Some("schema"));
        assert_eq!(resolve("nope"), None);
    }

    #[test]
    fn tokenize_handles_quotes() {
        assert_eq!(tokenize(".open db.sqlite"), vec![".open", "db.sqlite"]);
        assert_eq!(
            tokenize(".open \"my db.sqlite\""),
            vec![".open", "my db.sqlite"]
        );
        assert_eq!(tokenize(".separator '|'"), vec![".separator", "|"]);
        assert_eq!(tokenize("   "), Vec::<String>::new());
    }
}
