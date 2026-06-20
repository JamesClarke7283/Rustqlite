# Auto-vacuum & Pointer-Map Pages

Implementation notes for Rustqlite's auto-vacuum support (M5.3.7), mirroring `btree.c`'s
`autoVacuumCommit` / `incrVacuumStep` / `relocatePage` / `modifyPagePointer` and the `PTRMAP_*`
machinery in `btreeInt.h`.

## Header field layout (meta[])

The 100-byte database header stores 4-byte "meta" values starting at byte offset 36. The
auto-vacuum-relevant meta slots are:

| meta idx | byte offset | field name              | meaning                                  |
|----------|-------------|-------------------------|------------------------------------------|
| 4        | 52-55       | `largest_root_page`     | autoVacuum flag (0/1) at init; tracks the largest root-page number after the first `CREATE TABLE` in auto-vacuum mode. ANY non-zero value means auto-vacuum is on. |
| 7        | 64-67       | `incremental_vacuum`    | incr-vacuum flag (0 = FULL, 1 = INCREMENTAL). |

C SQLite reads the autoVacuum flag at `page1_init` via `pBt->autoVacuum = (get4byte(&page1[36 + 4*4])?1:0)`.
The flag is written at `newDatabase` time via `put4byte(&data[36 + 4*4], pBt->autoVacuum)`.

`sqlite3BtreeCreateTable` in auto-vacuum mode reads meta[4] as the largest root page so far,
increments it to get the new root-page slot, allocates a page there (relocating any existing
content), and writes the new root-page number back to meta[4]. So **meta[4] is dual-purpose**:
it's the autoVacuum flag (0 or 1) at init, then becomes the largest root-page number (>= 3)
after the first table is created.

## Pointer-map page math

`ptrmapPageno(usableSize, pgno)` returns the page number of the pointer-map page holding the
entry for `pgno`. Returns 0 for `pgno < 2` (page 1 has no ptrmap entry). The formula (mirrors
`btreeInt.h`):

```
nPagesPerMapPage = (usableSize / 5) + 1   // +1 accounts for the ptrmap page itself
iPtrMap = (pgno - 2) / nPagesPerMapPage
ret = iPtrMap * nPagesPerMapPage + 2
if ret == PENDING_BYTE_PAGE: ret += 1
```

For a 4096-byte page: `nPagesPerMapPage = 819 + 1 = 820`. So page 2 is a ptrmap page covering
pages 3..821, page 822 is the next ptrmap page, etc.

`PENDING_BYTE = 0x40000000` (1 GiB). The page containing it (`PENDING_BYTE / pageSize + 1` =
262145 for 4096-byte pages) is reserved for file locking and never holds b-tree or ptrmap data.

A page is "reserved" (cannot hold b-tree data) if it's a ptrmap page or the pending-byte page.
`Pager::allocate_page` skips reserved pages when auto-vacuum is on (mirroring
`allocateBtreePage`'s `if (autoVacuum && PTRMAP_ISPAGE(pBt, pBt->nPage)) { pBt->nPage++ }`).

## Ptrmap entry types

Each non-reserved page has a 5-byte ptrmap entry: 1-byte type + 4-byte parent page number.

| code | name        | parent field                          |
|------|-------------|---------------------------------------|
| 1    | ROOTPAGE    | unused (0)                            |
| 2    | FREEPAGE    | unused (0)                            |
| 3    | OVERFLOW1   | the cell's host page                   |
| 4    | OVERFLOW2   | the previous overflow page in the chain |
| 5    | BTREE       | the parent page in the b-tree          |

## Auto-vacuum commit (FULL mode)

When `PRAGMA auto_vacuum = FULL` is set and a write transaction commits with pages on the
freelist, `autoVacuumCommit` runs BEFORE the journal is synced:

1. Compute `nFin = finalDbSize(nOrig, nFree)` — the final page count after vacuum.
2. Walk from `iLast = nOrig` down to `nFin + 1`, skipping reserved pages:
   - Read the ptrmap entry for `iLast`.
   - If it's a FREEPAGE, drop it from the freelist.
   - Otherwise, find a free page `iFreePg <= nFin` (via `BTALLOC_LE` search of the freelist),
     relocate `iLast`'s content to `iFreePg`, and update the parent's pointer via
     `modifyPagePointer` (which scans the parent's cells for the pointer to `iLast` and
     rewrites it to `iFreePg`).
3. Reset the freelist head/count to 0 and the in-header size to `nFin`.
4. Truncate the in-memory page image to `nFin`.

## Incremental vacuum (INCREMENTAL mode)

`PRAGMA auto_vacuum = INCREMENTAL` defers the vacuum to `PRAGMA incremental_vacuum(N)`, which
runs up to N steps of `incrVacuumStep` (one page move per step) in a write transaction, yielding
the new page count as a result row per step. With no argument, it runs until the freelist is
exhausted.

## Rustqlite implementation notes

- `btree/ptrmap.rs` — ptrmap page math, `ptrmap_get`/`ptrmap_put` (async), `is_ptrmap_page`,
  `is_pending_byte_page`, `PtrMapType` enum.
- `btree/autovac.rs` — `auto_vacuum_commit`, `incr_vacuum_step`, `find_free_page_at_or_below`,
  `relocate_page`, `modify_page_pointer`, `set_child_ptrmaps`, `final_db_size`.
- `pager/mod.rs` — `auto_vacuum()`/`incr_vacuum()`/`set_auto_vacuum()` accessors;
  `allocate_page` skips reserved pages; `truncate_image()` for cache cleanup after vacuum;
  `free_page` records the FREEPAGE ptrmap entry; the commit path calls `auto_vacuum_commit`
  when auto-vacuum is on and the freelist is non-empty.
- `btree/mod.rs` — `create_table_btree` / `create_index_btree` dispatch to autovac-aware
  paths that place root pages at `meta[4] + 1` (skipping reserved pages) and update meta[4].
- `btree/balance.rs` — split paths write BTREE ptrmap entries for new children;
  `promote_root_and_split` / `split_leaf` / `split_index_leaf` / `promote_index_root` handle
  the single-cell-too-big case by leaving the right sibling empty (the pending insert fills it).
- `btree/cell.rs` — `build_table_leaf_cell_with_host` / `build_index_leaf_cell_with_host`
  accept an optional `host_pgno` for OVERFLOW1/OVERFLOW2 ptrmap entries. The sync cell builders
  cannot drive async I/O, so `ptrmap_put_sync` is a no-op placeholder; the async caller is
  responsible for recording overflow ptrmap entries after the cell is written (currently
  deferred — overflow-page relocation during vacuum is not yet fully wired).
- `capi/stmt.rs` — `compile_pragma` dispatches `auto_vacuum` and `incremental_vacuum` pragmas.

## Known gaps

- Overflow-page ptrmap entries (OVERFLOW1/OVERFLOW2) are not recorded by the sync cell builders
  (the async caller doesn't yet apply them). This means vacuuming a database where overflow
  pages need to be relocated (not just freed) will not update the overflow chain's parent
  pointer. The canonical autovacuum-1 test (DELETE all rows) works because overflow pages are
  freed, not relocated.
- `create_table_btree_autovac` does not relocate existing content when the new root-page slot
  is already occupied (the full `sqlite3BtreeCreateTable` path uses `relocatePage` for this).
  This only matters when the file has grown past `meta[4] + 1` before a new table is created.
- Multi-leaf index delete (`index_leaf_delete`) only handles single-leaf indexes; the
  autovac test uses small enough rows to keep the index on one leaf.