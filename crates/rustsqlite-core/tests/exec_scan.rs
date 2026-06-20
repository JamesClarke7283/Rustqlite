//! Executor scan skeleton: hand-build a `SELECT * FROM t` program and confirm the [`Vdbe`]
//! produces the table's rows, matching what a direct catalog/scan read returns. Validates the
//! core opcode loop (Init/OpenRead/Rewind/Column/ResultRow/Next/Halt) before codegen exists.
//!
//! Driven against the engine internals directly (so it runs in a tokio runtime); the fixture is
//! built with the system `sqlite3` and the test skips if it is absent.

use std::process::Command;
use std::sync::Arc;

use rustsqlite_core::pager::Pager;
use rustsqlite_core::schema::read_catalog;
use rustsqlite_core::types::Value;
use rustsqlite_core::vdbe::program::{Instruction, Program, P4};
use rustsqlite_core::vdbe::{Opcode, StepResult, Vdbe};
use rustsqlite_core::vfs::{OpenFlags, OsTokioVfs, Vfs};

fn sqlite3_available() -> bool {
    Command::new("sqlite3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn inst(opcode: Opcode, p1: i32, p2: i32, p3: i32) -> Instruction {
    Instruction::new(opcode, p1, p2, p3)
}

#[tokio::test]
async fn hand_built_select_star_scans_table() {
    if !sqlite3_available() {
        eprintln!("skipping: system `sqlite3` binary not found");
        return;
    }

    let mut path = std::env::temp_dir();
    path.push(format!("rustsqlite_exec_{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let path_str = path.to_str().unwrap().to_string();

    let out = Command::new("sqlite3")
        .arg(&path_str)
        .arg("CREATE TABLE t(a,b,c); INSERT INTO t VALUES (1,'x',3.5),(2,'y',-2),(9999999,'z',0);")
        .output()
        .expect("run sqlite3");
    assert!(out.status.success());

    let vfs: Arc<dyn Vfs> = Arc::new(OsTokioVfs::new());
    let file = vfs
        .open(&path_str, OpenFlags::READONLY)
        .await
        .expect("open");
    let pager = Arc::new(
        Pager::open(vfs.clone(), path_str.clone(), file)
            .await
            .expect("pager"),
    );
    let root = read_catalog(&pager)
        .await
        .expect("catalog")
        .find_table("t")
        .expect("t")
        .rootpage as i32;

    // Canonical SELECT a,b,c FROM t layout. Result registers are r[1..=3].
    //   0 Init        -> 9 (setup)
    //   1 OpenRead     cur0, root, ncols=3
    //   2 Rewind       cur0 -> 8 (halt) if empty
    //   3 Column       cur0, col0 -> r1
    //   4 Column       cur0, col1 -> r2
    //   5 Column       cur0, col2 -> r3
    //   6 ResultRow    r1, count 3
    //   7 Next         cur0 -> 3
    //   8 Halt
    //   9 Transaction
    //   10 Goto        -> 1
    let mut openread = inst(Opcode::OpenRead, 0, root, 0);
    openread.p4 = P4::Int(3);
    let program = Program {
        instructions: vec![
            inst(Opcode::Init, 0, 9, 0),
            openread,
            inst(Opcode::Rewind, 0, 8, 0),
            inst(Opcode::Column, 0, 0, 1),
            inst(Opcode::Column, 0, 1, 2),
            inst(Opcode::Column, 0, 2, 3),
            inst(Opcode::ResultRow, 1, 3, 0),
            inst(Opcode::Next, 0, 3, 0),
            inst(Opcode::Halt, 0, 0, 0),
            inst(Opcode::Transaction, 0, 0, 0),
            inst(Opcode::Goto, 0, 1, 0),
        ],
        num_registers: 4, num_cursors: 0,
    };

    let mut vdbe = Vdbe::new(Arc::new(program), Some(pager));
    let mut rows: Vec<Vec<Value>> = Vec::new();
    while let StepResult::Row = vdbe.step().await.expect("step") {
        let n = vdbe.result_count();
        rows.push((0..n).map(|i| vdbe.result_value(i)).collect());
    }

    let _ = std::fs::remove_file(&path);

    assert_eq!(
        rows,
        vec![
            vec![Value::Int(1), Value::Text("x".into()), Value::Real(3.5)],
            vec![Value::Int(2), Value::Text("y".into()), Value::Int(-2)],
            vec![
                Value::Int(9_999_999),
                Value::Text("z".into()),
                Value::Int(0)
            ],
        ]
    );
}
