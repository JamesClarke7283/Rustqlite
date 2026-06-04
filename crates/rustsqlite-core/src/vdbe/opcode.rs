//! VDBE opcodes (mirrors the opcode set generated from `vdbe.c` into `opcodes.h`).
//!
//! Opcode names and semantics mirror upstream exactly so that `EXPLAIN` output and behavior
//! match. This is an INCREMENTAL subset — the full ~190-opcode set is filled in as the engine
//! grows. The execution dispatch in [`super::exec`] `match`es exhaustively over this enum so
//! that an unhandled opcode is a compile-time error.
//!
//! Operand conventions follow upstream: registers are addressed by `p1..p3`; the typed `p4`
//! carries text/blob/collation/keyinfo; `p5` is a flag byte. Where an opcode is documented
//! below as `r[pN]` it means "the register numbered by operand pN". Binary operators follow
//! upstream's operand order `r[p3] = r[p2] OP r[p1]`, and the comparison opcodes test
//! `r[p3] OP r[p1]` and jump to `p2` (see [`super::program`] for the `p5` flag layout).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opcode {
    // --- control flow ---
    /// `Init p2`: program entry; jump to `p2` (the setup block at the end). Address 0.
    Init,
    /// `Goto p2`: unconditional jump to `p2`.
    Goto,
    /// `Halt`: stop execution and report the statement is done.
    Halt,

    // --- transactions / schema ---
    /// `Transaction p1 p2`: begin a transaction on database `p1`. `p2 != 0` opens a WRITE
    /// transaction (the rollback journal); `p2 == 0` is a read transaction (implicit in our
    /// engine). Mirrors `OP_Transaction` in `vdbe.c`.
    Transaction,
    /// `SetCookie p1 p2 p3`: write the value `p3` into header cookie `p2` of database `p1`. Used
    /// after DDL to bump the schema cookie (header bytes 40-43). Mirrors `OP_SetCookie`.
    SetCookie,
    /// `ParseSchema p1`: reload the in-memory schema (re-read `sqlite_schema`) so later statements
    /// see DDL committed by this one. Mirrors `OP_ParseSchema`.
    ParseSchema,
    /// `CreateBtree p1 p2 p3`: create a new b-tree (table when `p3 == 1`) in database `p1` and
    /// store its root page number in `r[p2]`. Mirrors `OP_CreateBtree`.
    CreateBtree,
    /// `Destroy p1 p2 p3`: erase the b-tree rooted at page `p1`. Currently `p2`/`p3` are
    /// unused (no `iMoved` / db-index plumbing in the first slice). Mirrors `OP_Destroy`.
    Destroy,

    // --- cursors ---
    /// `OpenRead p1 p2 p3 p4`: open read cursor `p1` on the b-tree rooted at page `p2`; `p4`
    /// carries the column count.
    OpenRead,
    /// `OpenWrite`: open a read/write cursor (write path; unimplemented in M3a).
    OpenWrite,
    /// `Close p1`: close cursor `p1`.
    Close,

    // --- table/index scans ---
    /// `Rewind p1 p2`: position cursor `p1` at its first row; if the b-tree is empty, jump to
    /// `p2`.
    Rewind,
    /// `Next p1 p2`: advance cursor `p1`; if a row remains jump to `p2`, else fall through.
    Next,
    /// `SeekGE`: seek to the first entry >= a key (index path; unimplemented in M3a).
    SeekGE,

    // --- row access ---
    /// `Rowid p1 p2`: `r[p2]` = the integer rowid of cursor `p1`'s current row.
    Rowid,
    /// `Column p1 p2 p3`: `r[p3]` = the value of column `p2` of cursor `p1`'s current row.
    Column,
    /// `ResultRow p1 p2`: emit `r[p1 .. p1+p2]` as a result row and yield to the caller.
    ResultRow,

    // --- record building / writes ---
    /// `MakeRecord p1 p2 p3`: encode registers `r[p1 .. p1+p2]` into a record and store the
    /// bytes (as a BLOB value) in `r[p3]`.
    MakeRecord,
    /// `NewRowid p1 p2`: `r[p2]` = an unused integer rowid for the table on cursor `p1` (the
    /// current maximum rowid + 1). Mirrors `OP_NewRowid`.
    NewRowid,
    /// `Insert p1 p2 p3`: insert the record blob in `r[p2]` keyed by the rowid in `r[p3]` into the
    /// table on cursor `p1`. Mirrors `OP_Insert`.
    Insert,
    /// `Delete p1`: remove the row at cursor `p1`'s current position. Mirrors `OP_Delete`.
    Delete,
    /// `IdxInsert`: insert into an index/sorter b-tree (write path; unimplemented in M3a).
    IdxInsert,

    // --- value loads ---
    /// `Integer p1 p2`: `r[p2]` = the integer `p1`.
    Integer,
    /// `Int64 p2 p4=Int(v)`: `r[p2]` = the 64-bit integer `v` (for values outside i32 range).
    Int64,
    /// `Real p2 p4=Real(v)`: `r[p2]` = the floating-point value `v`.
    Real,
    /// `String8 p2 p4=Text(s)`: `r[p2]` = the text string `s`.
    String8,
    /// `Null p2 p3`: set `r[p2 ..= p3]` (or just `r[p2]` when `p3 <= p2`) to NULL.
    Null,
    /// `Blob p2 p4=Blob(b)`: `r[p2]` = the BLOB `b`.
    Blob,

    // --- arithmetic (r[p3] = r[p2] OP r[p1]) ---
    /// `Add p1 p2 p3`: `r[p3] = r[p2] + r[p1]`.
    Add,
    /// `Subtract p1 p2 p3`: `r[p3] = r[p2] - r[p1]`.
    Subtract,
    /// `Multiply p1 p2 p3`: `r[p3] = r[p2] * r[p1]`.
    Multiply,
    /// `Divide p1 p2 p3`: `r[p3] = r[p2] / r[p1]`.
    Divide,
    /// `Remainder p1 p2 p3`: `r[p3] = r[p2] % r[p1]`.
    Remainder,
    /// `Concat p1 p2 p3`: `r[p3] = r[p2] || r[p1]` (text concatenation).
    Concat,

    // --- comparisons as jumps (test r[p3] OP r[p1], jump to p2; see program.rs p5 flags) ---
    /// `Eq p1 p2 p3`: if `r[p3] == r[p1]` jump to `p2`.
    Eq,
    /// `Ne p1 p2 p3`: if `r[p3] != r[p1]` jump to `p2`.
    Ne,
    /// `Lt p1 p2 p3`: if `r[p3] < r[p1]` jump to `p2`.
    Lt,
    /// `Le p1 p2 p3`: if `r[p3] <= r[p1]` jump to `p2`.
    Le,
    /// `Gt p1 p2 p3`: if `r[p3] > r[p1]` jump to `p2`.
    Gt,
    /// `Ge p1 p2 p3`: if `r[p3] >= r[p1]` jump to `p2`.
    Ge,

    // --- logic ---
    /// `And p1 p2 p3`: `r[p3] = r[p1] AND r[p2]` (three-valued).
    And,
    /// `Or p1 p2 p3`: `r[p3] = r[p1] OR r[p2]` (three-valued).
    Or,
    /// `Not p1 p2`: `r[p2] = NOT r[p1]` (three-valued; NOT NULL is NULL).
    Not,
    /// `IsNull p1 p2`: if `r[p1]` is NULL jump to `p2`.
    IsNull,
    /// `NotNull p1 p2`: if `r[p1]` is not NULL jump to `p2`.
    NotNull,
    /// `If p1 p2 p3`: jump to `p2` if `r[p1]` is true; if `r[p1]` is NULL, jump only when
    /// `p3 != 0`.
    If,
    /// `IfNot p1 p2 p3`: jump to `p2` if `r[p1]` is false; if `r[p1]` is NULL, jump only when
    /// `p3 != 0`.
    IfNot,

    // --- register moves ---
    /// `Copy p1 p2 p3`: deep-copy `r[p1 .. p1+p3]` to `r[p2 .. p2+p3]` (`p3+1` registers).
    Copy,
    /// `SCopy p1 p2`: shallow-copy `r[p1]` to `r[p2]`.
    SCopy,
    /// `Move p1 p2 p3`: move `p3` registers from `r[p1]` to `r[p2]`, leaving the source NULL.
    Move,

    // --- coercion / functions ---
    /// `Affinity p1 p2 p4=Symbol(affs)`: apply the affinity chars in `affs` to `r[p1 .. p1+p2]`.
    Affinity,
    /// `RealAffinity p1`: if `r[p1]` holds an integer, convert it to a real. Emitted after
    /// reading a REAL-affinity column, whose integer-valued rows are stored as integers on disk
    /// for space but should read back as REAL.
    RealAffinity,
    /// `Function p2 p3 p4=Symbol(name) p5=nArg`: `r[p3]` = `name(r[p2 .. p2+nArg])`.
    Function,

    // --- sorter (ORDER BY) ---
    /// `SorterOpen p1 p2 p4=KeyInfo`: open sorter cursor `p1` whose records have `p2` leading
    /// sort-key fields described by `p4`.
    SorterOpen,
    /// `SorterInsert p1 p2`: insert the record in `r[p2]` into sorter cursor `p1`.
    SorterInsert,
    /// `SorterSort p1 p2`: sort the records, position at the first, or jump to `p2` if empty.
    SorterSort,
    /// `SorterData p1`: load the current sorter record into sorter cursor `p1` for `Column`.
    SorterData,
    /// `SorterNext p1 p2`: advance sorter cursor `p1`; if a record remains jump to `p2`.
    SorterNext,

    // --- LIMIT / OFFSET ---
    /// `DecrJumpZero p1 p2`: decrement `r[p1]`; if it becomes 0, jump to `p2` (LIMIT).
    DecrJumpZero,
    /// `IfPos p1 p2 p3`: if `r[p1] > 0`, decrement it by `p3` and jump to `p2` (OFFSET).
    IfPos,

    // --- aggregates (M6) ---
    /// `AggStep`: accumulate an aggregate (unimplemented in M3a).
    AggStep,
    /// `AggFinal`: finalize an aggregate (unimplemented in M3a).
    AggFinal,
}

impl Opcode {
    /// The upstream mnemonic for this opcode, as it appears in `EXPLAIN` output and `opcodes.h`.
    pub fn name(&self) -> &'static str {
        match self {
            Opcode::Init => "Init",
            Opcode::Goto => "Goto",
            Opcode::Halt => "Halt",
            Opcode::Transaction => "Transaction",
            Opcode::SetCookie => "SetCookie",
            Opcode::ParseSchema => "ParseSchema",
            Opcode::CreateBtree => "CreateBtree",
            Opcode::Destroy => "Destroy",
            Opcode::OpenRead => "OpenRead",
            Opcode::OpenWrite => "OpenWrite",
            Opcode::Close => "Close",
            Opcode::Rewind => "Rewind",
            Opcode::Next => "Next",
            Opcode::SeekGE => "SeekGE",
            Opcode::Rowid => "Rowid",
            Opcode::Column => "Column",
            Opcode::ResultRow => "ResultRow",
            Opcode::MakeRecord => "MakeRecord",
            Opcode::NewRowid => "NewRowid",
            Opcode::Insert => "Insert",
            Opcode::Delete => "Delete",
            Opcode::IdxInsert => "IdxInsert",
            Opcode::Integer => "Integer",
            Opcode::Int64 => "Int64",
            Opcode::Real => "Real",
            Opcode::String8 => "String8",
            Opcode::Null => "Null",
            Opcode::Blob => "Blob",
            Opcode::Add => "Add",
            Opcode::Subtract => "Subtract",
            Opcode::Multiply => "Multiply",
            Opcode::Divide => "Divide",
            Opcode::Remainder => "Remainder",
            Opcode::Concat => "Concat",
            Opcode::Eq => "Eq",
            Opcode::Ne => "Ne",
            Opcode::Lt => "Lt",
            Opcode::Le => "Le",
            Opcode::Gt => "Gt",
            Opcode::Ge => "Ge",
            Opcode::And => "And",
            Opcode::Or => "Or",
            Opcode::Not => "Not",
            Opcode::IsNull => "IsNull",
            Opcode::NotNull => "NotNull",
            Opcode::If => "If",
            Opcode::IfNot => "IfNot",
            Opcode::Copy => "Copy",
            Opcode::SCopy => "SCopy",
            Opcode::Move => "Move",
            Opcode::Affinity => "Affinity",
            Opcode::RealAffinity => "RealAffinity",
            Opcode::Function => "Function",
            Opcode::SorterOpen => "SorterOpen",
            Opcode::SorterInsert => "SorterInsert",
            Opcode::SorterSort => "SorterSort",
            Opcode::SorterData => "SorterData",
            Opcode::SorterNext => "SorterNext",
            Opcode::DecrJumpZero => "DecrJumpZero",
            Opcode::IfPos => "IfPos",
            Opcode::AggStep => "AggStep",
            Opcode::AggFinal => "AggFinal",
        }
    }
}
