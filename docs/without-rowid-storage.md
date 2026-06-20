# WITHOUT ROWID table storage layout

Notes from reading `~/Downloads/sqlite-src-3530200/src/build.c`
(`convertToWithoutRowidTable`) and `src/insert.c` / `src/btree.c` while
implementing M5.3.6.

## What WITHOUT ROWID changes

A normal (rowid) table is a **table b-tree** (`BTREE_INTKEY`): the rowid is
the b-tree key and the record body is the payload. A `WITHOUT ROWID` table
is a **blob-keyed index b-tree** (`BTREE_BLOBKEY`) — the same page format
used by `CREATE INDEX`. There is no rowid at all; the b-tree key is the
primary-key record.

`HasRowid(X)` in `sqliteInt.h` is `((X)->tabFlags & TF_WithoutRowid)==0`, so
"WITHOUT ROWID" is exactly "no rowid".

## The key record shape (covering index)

`convertToWithoutRowidTable` (build.c around line 2354) does six things; the
two that matter for storage layout are:

1. **Set all PRIMARY KEY columns to NOT NULL** (step 1). Even if the user
   wrote `b INTEGER PRIMARY KEY` without `NOT NULL`, the WITHOUT ROWID
   declaration makes `b` implicitly NOT NULL. Upstream uses
   `OP_HaltIfNull` at INSERT time to enforce this; we do the same.

2. **Make the PK index a covering index** (steps 4–5): the PK `Index` object
   gets `nColumn = nKeyCol + nExtra`, where the extra columns are every
   non-PK table column (in table column order, excluding virtual columns).
   The on-disk key record is therefore:

   ```
   [pk_col_0, pk_col_1, ..., pk_col_{nPk-1},
    non_pk_col_0, non_pk_col_1, ..., non_pk_col_{nExtra-1}]
   ```

   PK columns come first **in their declared PRIMARY KEY order** (with the
   DESC flag honored); the remaining table columns follow in table column
   order. There is no trailing rowid — the whole row IS the key.

   `pPk->isCovering = 1` and `pPk->uniqNotNull = 1` reflect this.

## The b-tree is `BTREE_BLOBKEY`

`OP_CreateBtree` is emitted with `p3 = 1` (`BTREE_INTKEY`) for a rowid
table and `p3 = 0` (`BTREE_BLOBKEY`) for a WITHOUT ROWID table. The
`sqlite3BtreeInsert` path branches on `pCur->pKeyInfo == 0`: when non-null
(the index/WITHOUT ROWID case), the key is the arbitrary byte sequence in
`pX->pKey,nKey` and `pX->pData` must be zero — there is no separate
payload, the key IS the row. Our `btree::create_index_btree` allocates an
empty `LeafIndex` page, which is exactly what WITHOUT ROWID tables need.

## Secondary indexes on WITHOUT ROWID tables

`convertToWithoutRowidTable` step 6 rewrites the trailing rowid on every
**other** automatically-generated UNIQUE index to be the PK columns
instead. For a user-declared `CREATE INDEX idx ON wrt(col)`, the key
record becomes `[col, pk_col_0, pk_col_1, ...]` — the PK columns replace
the rowid tail that a rowid-table index would carry. This is what
`emit_index_inserts_without_rowid` in
`crates/rustsqlite-core/src/codegen/insert.rs` does.

## `rowid` is not a valid column

`SELECT rowid FROM <without-rowid-table>` errors with
`no such column: rowid` — the magic names `rowid`/`_rowid_`/`oid` only
resolve on rowid tables. Our `Table::resolve_column` checks
`!self.without_rowid` before honoring them.

## What this implementation does NOT yet do

- `DELETE` / `UPDATE` on WITHOUT ROWID tables (the `IdxDelete` +
  `IdxInsert` path on the table b-tree). The codegen errors with a clear
  "not supported yet" message; a follow-up reuses the storage-order
  key-record helpers from `compile_insert_without_rowid`.
- `INSERT ... SELECT` into a WITHOUT ROWID table (the SELECT path needs
  the coroutine plumbing that M8 brings).
- Per-column `COLLATE` on PK columns (the KeyInfo uses BINARY today).
- `OR REPLACE` conflict resolution on the PK.
- Auto-vacuum / ptrmap awareness (5.3.7).

## Reference files

- `src/build.c` `convertToWithoutRowidTable` (line ~2354)
- `src/insert.c` `sqlite3Insert` (line ~920, `withoutRowid = !HasRowid(pTab)`)
- `src/btree.c` `sqlite3BtreeInsert` (line ~9402, the
  `pCur->pKeyInfo != 0` branch is the WITHOUT ROWID / index case)
- `src/sqliteInt.h` `TF_WithoutRowid` / `HasRowid` (line ~2489)