//! B-tree balancing (mirrors the `balance*` routines in `btree.c`).
//!
//! Placeholder for the write-path milestone (M4): cell insertion/deletion, page splits and
//! merges (`balance_deeper`, `balance_quick`, `balance_nonroot`), and freelist management. The
//! read cursor in [`super::cursor`] does not modify the tree, so none of this is needed yet.
