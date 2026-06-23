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
    /// `Gosub p1 p2`: store the next instruction's address in `r[p1]` and jump to `p2`.
    /// Mirrors `OP_Gosub` in `vdbe.c` — the subroutine-call opcode paired with `Return`.
    Gosub,
    /// `Return p1 p2 p3`: jump to the address stored in `r[p1]`. `p3 == 1` makes the jump
    /// conditional on `r[p1]` being an integer (fall through if not); `p3 == 0` is the
    /// strict form (used after `Gosub`). `p2` is an EXPLAIN indentation hint, unused at runtime.
    /// Mirrors `OP_Return` in `vdbe.c`.
    Return,
    /// `InitCoroutine p1 p2 p3`: set `r[p1] = p3 - 1` so that the first `Yield` to `r[p1]`
    /// jumps to address `p3`. If `p2 != 0`, jump over the coroutine body to address `p2`
    /// (the coroutine implementation immediately follows this opcode). Mirrors
    /// `OP_InitCoroutine` in `vdbe.c` — used to set up a coroutine for `FROM (subquery)`,
    /// `EXISTS`, and `IN (SELECT ...)` materialization.
    InitCoroutine,
    /// `EndCoroutine p1`: jump to the `p2` parameter of the `Yield` whose address is in
    /// `r[p1]`, and leave `r[p1]` set so subsequent `Yield`s return to this instruction.
    /// Mirrors `OP_EndCoroutine` in `vdbe.c`.
    EndCoroutine,
    /// `Yield p1 p2`: swap the program counter with the value in `r[p1]`. If the coroutine
    /// ended via `EndCoroutine`, jump to `p2`; otherwise continue to the next instruction.
    /// Mirrors `OP_Yield` in `vdbe.c`.
    Yield,
    /// `Once p1 p2`: jump to `p2` on the second and subsequent encounters (within one run of
    /// the program). The first time `Once` is reached it falls through and records `p1` (the
    /// "cookie" slot) in the program's `aOp[0].p1` so future hits with the same `p1` jump.
    /// Used to wrap non-correlated scalar subquery code so it runs only once per statement.
    /// Mirrors `OP_Once` in `vdbe.c`.
    Once,
    /// `Program p1 p2 p3 p4=SubProgram p5=token`: invoke a sub-VDBE program (trigger program,
    /// view body, or other sub-program). Save the current program state into a frame stored in
    /// `r[p3]`, install the sub-program from `p4` with a fresh register file and cursor table,
    /// and begin executing it at its first instruction. When the sub-program halts (its `Halt`
    /// with `p1 == SQLITE_OK`), the frame is popped and execution resumes in the parent at the
    /// instruction following this `Program`. `p1` is the register in the *parent* where the
    /// sub-program's inputs begin (the base for `OP_Param`); `p2` is the jump target when the
    /// sub-program halts with `OE_Ignore`. `p5` non-zero enables recursive-trigger guard
    /// (a sub-program with the same `p5` token already on the frame stack is a no-op).
    /// Mirrors `OP_Program` in `vdbe.c`.
    Program,
    /// `Param p1 p2`: copy a value from the calling (parent) frame's register file into the
    /// current frame's `r[p2]`. The parent register index is `p1 + (parent_program.p1 at the
    /// calling Program instruction)`. Used inside sub-programs (trigger bodies, correlated
    /// subqueries) to access the outer row's `NEW.*` / `OLD.*` values or outer-query columns.
    /// Mirrors `OP_Param` in `vdbe.c`.
    Param,
    /// `Compare p1 p2 p3 p4=KeyInfo`: compare `n=p3` registers starting at `r[p1]` against
    /// `r[p2]` under the per-key collation in `p4`, leaving the result (`-1/0/+1`) in a hidden
    /// `last_compare` cell that the immediately following `Jump` reads. Mirrors `OP_Compare`.
    Compare,
    /// `Jump p1 p2 p3`: route to `p1`/`p2`/`p3` depending on whether the most recent `Compare`
    /// found the P1 vector less than, equal to, or greater than the P2 vector. Mirrors `OP_Jump`.
    Jump,

    // --- transactions / schema ---
    /// `Transaction p1 p2`: begin a transaction on database `p1`. `p2 != 0` opens a WRITE
    /// transaction (the rollback journal); `p2 == 0` is a read transaction (implicit in our
    /// engine). Mirrors `OP_Transaction` in `vdbe.c`.
    Transaction,
    /// `AutoCommit p1 p2`: toggle the connection's autocommit flag. `p1 = 1` turns autocommit
    /// ON (commit when transitioning from off→on if `p2 == 0`; rollback if `p2 == 1`).
    /// `p1 = 0` turns autocommit OFF (BEGIN). When the desired state equals the current state,
    /// raises "cannot start a transaction within a transaction" / "cannot commit - no transaction
    /// is active" / "cannot rollback - no transaction is active". Mirrors `OP_AutoCommit` in
    /// `vdbe.c`.
    AutoCommit,
    /// `Savepoint p1 * * P4=Text(name)`: open (`p1 == 0`), release (`p1 == 1`), or
    /// rollback (`p1 == 2`) the savepoint named by `P4`. Opening a savepoint when the connection
    /// is in autocommit mode starts an implicit transaction (the savepoint becomes the
    /// "transaction savepoint"); releasing that outermost transaction savepoint commits the
    /// transaction. Releasing any other savepoint discards it and any nested savepoints
    /// (their changes become part of the enclosing transaction). Rolling back to a savepoint
    /// discards the changes made since the savepoint was created, keeping the savepoint on the
    /// stack. Mirrors `OP_Savepoint` in `vdbe.c`.
    Savepoint,
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
    /// `Clear p1`: delete all rows from the table b-tree rooted at page `p1`, leaving an empty
    /// b-tree. Mirrors `OP_Clear`.
    Clear,

    // --- cursors ---
    /// `OpenRead p1 p2 p3 p4`: open read cursor `p1` on the b-tree rooted at page `p2`; `p4`
    /// carries the column count.
    OpenRead,
    /// `OpenWrite`: open a read/write cursor (write path; unimplemented in M3a).
    OpenWrite,
    /// `OpenWriteReg p1 p2=root_reg p3`: open a read/write cursor `p1` on the b-tree whose
    /// root page is the value of `r[p2]`. The cursor type (table vs index) is decided by
    /// `p3` (1 = table, 0 = index), matching the same convention as `CreateBtree`/`OpenWrite`.
    /// M5.1 only: lets `CREATE INDEX` open the populate cursor on a freshly-created index
    /// b-tree whose rootpage was just written into a register by `CreateBtree`.
    OpenWriteReg,
    /// `Close p1`: close cursor `p1`.
    Close,

    // --- table/index scans ---
    /// `Rewind p1 p2`: position cursor `p1` at its first row; if the b-tree is empty, jump to
    /// `p2`.
    Rewind,
    /// `Next p1 p2`: advance cursor `p1`; if a row remains jump to `p2`, else fall through.
    Next,
    /// `NotExists p1 p2 p3`: position table cursor `p1` at the row whose rowid is `r[p3]`; if
    /// no such row exists, jump to `p2`. Mirrors `OP_NotExists` from `vdbe.c` and is the rowid-
    /// seek used by `UPDATE`'s two-pass rewrite.
    NotExists,
    /// `NullRow p1`: set cursor `p1` to a synthetic all-NULL row. Used by LEFT JOIN to emit
    /// a NULL-filled right-table row when no inner match is found. Subsequent `Column` reads
    /// from this cursor return NULL until the cursor is repositioned by `Next`/`Rewind`/etc.
    /// Mirrors `OP_NullRow` in `vdbe.c`.
    NullRow,

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
    /// `IdxInsert p1 p2 p3 p4=nMem p5=flags`: insert a new index entry. `r[p2]` holds the
    /// pre-built key record (the indexed columns + the trailing rowid). The cursor `p1` is
    /// an index b-tree cursor opened with `OpenRead`/`OpenWrite` + `P4::KeyInfo`. `p3` is
    /// the first of `nMem` (`p4`) extra registers that may hold additional values used by
    /// the index — for a single-column index the M5.1 path uses `nMem = 0`. `p5` may carry
    /// `OPFLAG_NCHANGE` (bump `changes()`) and `OPFLAG_PREFORMAT` (the record is already
    /// encoded). Mirrors `OP_IdxInsert` in `vdbe.c`.
    IdxInsert,
    /// `IdxDelete p1 p2 p3`: delete the index entry whose key is in the `p3` registers
    /// starting at `r[p2]` from the index b-tree on cursor `p1`. The trailing rowid is
    /// included in the `p3` registers. A no-op when no matching entry exists (matches
    /// upstream's silent miss). Mirrors `OP_IdxDelete`.
    IdxDelete,
    /// `IdxRowid p1 p2`: `r[p2]` = the trailing rowid of the current index entry on cursor
    /// `p1`. Mirrors `OP_IdxRowid`.
    IdxRowid,
    /// `SeekGE p1 p2 p3 p4=nField`: position index cursor `p1` at the first entry `>=` the
    /// key in `r[p3..p3+nField]`; jump to `p2` when no such entry exists. `p4` is the number
    /// of index columns the comparison uses (the key is the record header's first `nField`
    /// values; the trailing rowid is the tiebreaker, not part of the comparison). Mirrors
    /// `OP_SeekGE` in `vdbe.c`.
    SeekGE,
    /// `SeekGT`: same shape as `SeekGE` but the position is at the first entry `>` the key.
    /// Cursor jumps to `p2` when no such entry exists.
    SeekGT,
    /// `SeekLE`: position at the last entry `<=` the key, jump to `p2` on miss.
    SeekLE,
    /// `SeekLT`: position at the last entry `<` the key, jump to `p2` on miss.
    SeekLT,
    /// `IdxGE p1 p2 p3 p4=nField`: after a `SeekLE` (or any seek), jump to `p2` when the
    /// current entry's prefix (the first `nField` values of the key record) is `<` the
    /// search key in `r[p3..p3+nField]`. Together with `SeekLE` this implements `<= key`.
    /// Mirrors `OP_IdxGE` in `vdbe.c`.
    IdxGE,
    /// `IdxGT`: same as `IdxGE` but the jump happens when the entry is `<=` the search key.
    /// Together with `SeekGE` implements `> key`.
    IdxGT,
    /// `IdxLE`: jump to `p2` when the entry's prefix is `>` the search key. Together with
    /// `SeekLE` implements `< key`.
    IdxLE,
    /// `IdxLT`: jump to `p2` when the entry's prefix is `>=` the search key. Together with
    /// `SeekGE` implements `< key`.
    IdxLT,
    /// `Found p1 p2 p3 p4=Int(n)`: search cursor `p1` for the record formed by
    /// `r[p3..p3+n]`; jump to `p2` if found, fall through if not. Currently operates on
    /// ephemeral index cursors (used by `SELECT DISTINCT` dedup), matching `OP_Found` on an
    /// `OP_OpenEphemeral`-opened index. Mirrors `OP_Found` in `vdbe.c`.
    Found,
    /// `NotFound p1 p2 p3 p4=Int(n)`: the inverse of `Found` — jump to `p2` when the record
    /// is *not* present on cursor `p1`. Mirrors `OP_NotFound`.
    NotFound,
    /// `NoConflict p1 p2 p3 p4=Int(n)`: seek index cursor `p1` at the first entry `>=` the
    /// key in `r[p3..p3+n]`; jump to `p2` when no conflicting entry exists. A "conflict" is an
    /// entry whose indexed-column prefix equals the search key under the cursor's per-column
    /// collation. A NULL in any search-key column means "no conflict" (NULL is never equal to
    /// NULL in SQL), so the jump is taken regardless of the cursor's content. Used by the
    /// INSERT/UPDATE conflict-resolution codegen to detect UNIQUE-index collisions before the
    /// `IdxInsert` would raise them. Mirrors `OP_NoConflict` in `vdbe.c`.
    NoConflict,

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
    /// `BitAnd p1 p2 p3`: `r[p3] = r[p2] & r[p1]`.
    BitAnd,
    /// `BitOr p1 p2 p3`: `r[p3] = r[p2] | r[p1]`.
    BitOr,
    /// `ShiftLeft p1 p2 p3`: `r[p3] = r[p2] << r[p1]`.
    ShiftLeft,
    /// `ShiftRight p1 p2 p3`: `r[p3] = r[p2] >> r[p1]` (arithmetic right shift).
    ShiftRight,
    /// `BitNot p1 p2`: `r[p2] = ~r[p1]`.
    BitNot,

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

    // --- ephemeral table (RETURNING) ---
    /// `OpenEphemeral p1 p2`: open an in-memory, rowid-keyed ephemeral table cursor `p1` that
    /// holds records with `p2` fields. Used by `RETURNING` to buffer one result row per modified
    /// row, then rewind and emit them after the write transaction completes. `NewRowid` allocates
    /// a unique key and `Insert` stores the buffered record.
    OpenEphemeral,
    /// `OpenPseudo p1 p2 p3`: open a pseudo-cursor `p1` that reads a single record stored in
    /// register `r[p2]`. `p3` is the column count. `Column` on a pseudo-cursor decodes a field
    /// from the register's record blob. Used by recursive CTEs to expose the single "Current"
    /// row to the recursive query's scan. Mirrors `OP_OpenPseudo` in `vdbe.c`.
    OpenPseudo,
    /// `OpenDup p1 p2`: open a new cursor `p1` that shares the underlying storage of the
    /// existing ephemeral cursor `p2`. Used by the window-function sliding-frame algorithm to
    /// keep multiple cursors (start/current/end) on the same partition cache. Mirrors
    /// `OP_OpenDup` in `vdbe.c`.
    OpenDup,
    /// `RowData p1 p2`: copy the full record blob of cursor `p1`'s current row into `r[p2]`.
    /// Used by recursive CTEs to transfer a row from the Queue ephemeral into the Current
    /// pseudo-cursor's register. Mirrors `OP_RowData` in `vdbe.c`.
    RowData,

    // --- LIMIT / OFFSET ---
    /// `DecrJumpZero p1 p2`: decrement `r[p1]`; if it becomes 0, jump to `p2` (LIMIT).
    DecrJumpZero,
    /// `IfPos p1 p2 p3`: if `r[p1] > 0`, decrement it by `p3` and jump to `p2` (OFFSET).
    IfPos,

    // --- aggregates (M6) / window functions (M11.3) ---
    /// `AggStep p1=0 P2 P3 P4=FuncDef P5=nArg`: accumulate one row's arguments from
    /// `r[P2..P2+nArg]` into the accumulator at `r[P3]`. Mirrors `OP_AggStep` in `vdbe.c`.
    AggStep,
    /// `AggInverse p1=1 P2 P3 P4=FuncDef P5=nArg`: remove one row's arguments from the
    /// accumulator at `r[P3]` (the window-frame "inverse step" that slides the frame start
    /// forward). Mirrors `OP_AggInverse` in `vdbe.c`. Only valid for aggregates that implement
    /// `xInverse` (`count`/`sum`/`total`/`avg`/`group_concat`); `min`/`max` use a different
    /// (VDBE-instruction) path for non-default frames and never emit `AggInverse`.
    AggInverse,
    /// `AggFinal P1 P2 P3=0 P4=FuncDef`: finalize the accumulator at `r[P1]` and store the
    /// result there (consumes the accumulator). Mirrors `OP_AggFinal` in `vdbe.c`.
    AggFinal,
    /// `AggValue P3 P4=FuncDef`: invoke the aggregate's `xValue` and store the result in
    /// `r[P3]` *without* consuming the accumulator (so a window function can keep stepping
    /// after reading the current frame's value). `P1`/`P2` are unused (upstream carries the
    /// arg count in `P2` for disambiguation only). Mirrors `OP_AggValue` in `vdbe.c`.
    AggValue,
    /// `HaltIfNull p3 p4=Text(msg)`: if `r[p3]` is NULL, halt the program with a constraint
    /// error whose message is `p4`. Used by WITHOUT ROWID inserts to enforce the implicit
    /// NOT NULL on PRIMARY KEY columns. Mirrors `OP_HaltIfNull` in `vdbe.c`.
    HaltIfNull,
    /// `AddImm p1 p2`: `r[p1] += p2`. Mirrors `OP_AddImm` in `vdbe.c` — a short-form integer
    /// add used by the window-function sliding-frame counters.
    AddImm,
    /// `MemMax p1 p2`: `r[p1] = max(r[p1], r[p2])`. Mirrors `OP_MemMax` in `vdbe.c` — used by
    /// the AUTOINCREMENT counter to track the maximum rowid across all inserted rows.
    MemMax,
    /// `SeekRowid p1 p2 p3`: position table cursor `p1` at the row whose rowid equals `r[p3]`;
    /// jump to `p2` if no such row exists. Mirrors `OP_SeekRowid` in `vdbe.c`. For ephemeral
    /// cursors, our rowids are sequential 1..=n, so this maps rowid → index.
    SeekRowid,
    /// `ResetSorter p1`: clear all records from sorter/ephemeral cursor `p1` but keep the
    /// cursor open. Used by the window-function codegen to reset the partition cache between
    /// partitions. Mirrors `OP_ResetSorter` in `vdbe.c`.
    ResetSorter,
    /// `Last p1 p2`: position cursor `p1` at its last row; jump to `p2` if the b-tree is empty.
    /// Mirrors `OP_Last` in `vdbe.c` — used by reverse scans (e.g. `min`/`max` optimization and
    /// the window-function sliding-frame end cursor).
    Last,
    /// `Prev p1 p2`: move cursor `p1` to the previous row; jump to `p2` if a row remains,
    /// fall through if at the beginning. Mirrors `OP_Prev` in `vdbe.c`.
    Prev,
    /// `Checkpoint p1 p2 p3`: run a WAL checkpoint on database `p1` (always 0 — main only) in
    /// mode `p2` (0=PASSIVE, 1=FULL, 2=RESTART, 3=TRUNCATE) and write three result registers at
    /// `r[p3..p3+3]`: `r[p3]=0` on success or `1` if busy, `r[p3+1]=` number of frames in the WAL,
    /// `r[p3+2]=` number of frames checkpointed. Mirrors `OP_Checkpoint` in `vdbe.c`.
    Checkpoint,
    /// `FkCheck p1 p2 p3 P4=FkCheck`: verify a single foreign-key constraint for the row whose
    /// child-key columns live in registers `r[p1..p1+n]` (where `n` is the FK's column count,
    /// carried in `P4::FkCheck`). If any child-key column is NULL, the check is skipped (NULL
    /// foreign keys never violate — mirrors upstream's `OP_IsNull → addrOk` early-out). When
    /// the parent row matching the child key is found, execution falls through; when the
    /// parent row is missing, execution jumps to `p2` (the constraint-violation handler, which
    /// typically emits a `Halt` with `p5 = 4` for the "FOREIGN KEY constraint failed" prefix).
    /// `p3` carries the FK constraint's 0-based index (for the error message). This is the
    /// runtime side of M17.6 FK enforcement; the lookup strategy (rowid seek, index seek, or
    /// full parent scan) is resolved at codegen time and carried in `P4::FkCheck`.
    FkCheck,
}

impl Opcode {
    /// The upstream mnemonic for this opcode, as it appears in `EXPLAIN` output and `opcodes.h`.
    pub fn name(&self) -> &'static str {
        match self {
            Opcode::Init => "Init",
            Opcode::Goto => "Goto",
            Opcode::Halt => "Halt",
            Opcode::Gosub => "Gosub",
            Opcode::Return => "Return",
            Opcode::InitCoroutine => "InitCoroutine",
            Opcode::EndCoroutine => "EndCoroutine",
            Opcode::Yield => "Yield",
            Opcode::Once => "Once",
            Opcode::Program => "Program",
            Opcode::Param => "Param",
            Opcode::Compare => "Compare",
            Opcode::Jump => "Jump",
            Opcode::Transaction => "Transaction",
            Opcode::AutoCommit => "AutoCommit",
            Opcode::Savepoint => "Savepoint",
            Opcode::SetCookie => "SetCookie",
            Opcode::ParseSchema => "ParseSchema",
            Opcode::CreateBtree => "CreateBtree",
            Opcode::Destroy => "Destroy",
            Opcode::Clear => "Clear",
            Opcode::OpenRead => "OpenRead",
            Opcode::OpenWrite => "OpenWrite",
            Opcode::OpenWriteReg => "OpenWriteReg",
            Opcode::Close => "Close",
            Opcode::Rewind => "Rewind",
            Opcode::Next => "Next",
            Opcode::SeekGE => "SeekGE",
            Opcode::SeekGT => "SeekGT",
            Opcode::SeekLE => "SeekLE",
            Opcode::SeekLT => "SeekLT",
            Opcode::IdxGE => "IdxGE",
            Opcode::IdxGT => "IdxGT",
            Opcode::IdxLE => "IdxLE",
            Opcode::IdxLT => "IdxLT",
            Opcode::Found => "Found",
            Opcode::NotFound => "NotFound",
            Opcode::NoConflict => "NoConflict",
            Opcode::NotExists => "NotExists",
            Opcode::NullRow => "NullRow",
            Opcode::Rowid => "Rowid",
            Opcode::Column => "Column",
            Opcode::ResultRow => "ResultRow",
            Opcode::MakeRecord => "MakeRecord",
            Opcode::NewRowid => "NewRowid",
            Opcode::Insert => "Insert",
            Opcode::Delete => "Delete",
            Opcode::IdxInsert => "IdxInsert",
            Opcode::IdxDelete => "IdxDelete",
            Opcode::IdxRowid => "IdxRowid",
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
            Opcode::BitAnd => "BitAnd",
            Opcode::BitOr => "BitOr",
            Opcode::ShiftLeft => "ShiftLeft",
            Opcode::ShiftRight => "ShiftRight",
            Opcode::BitNot => "BitNot",
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
            Opcode::OpenEphemeral => "OpenEphemeral",
            Opcode::OpenPseudo => "OpenPseudo",
            Opcode::OpenDup => "OpenDup",
            Opcode::RowData => "RowData",
            Opcode::DecrJumpZero => "DecrJumpZero",
            Opcode::IfPos => "IfPos",
            Opcode::AggStep => "AggStep",
            Opcode::AggInverse => "AggInverse",
            Opcode::AggFinal => "AggFinal",
            Opcode::AggValue => "AggValue",
            Opcode::HaltIfNull => "HaltIfNull",
            Opcode::AddImm => "AddImm",
            Opcode::MemMax => "MemMax",
            Opcode::SeekRowid => "SeekRowid",
            Opcode::ResetSorter => "ResetSorter",
            Opcode::Last => "Last",
            Opcode::Prev => "Prev",
            Opcode::Checkpoint => "Checkpoint",
            Opcode::FkCheck => "FkCheck",
        }
    }
}
