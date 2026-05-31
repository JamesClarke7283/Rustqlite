//! Page cache (mirrors `pcache.c` / `pcache1.c`).
//!
//! For M1 the cache is a simple `HashMap<PgNo, PageRef>` held inline in [`super::Pager`]. This
//! module is the home for the eventual faithful page cache: LRU eviction, dirty-page tracking,
//! reference-count pinning, and a cache-size budget (`PRAGMA cache_size`). Tracked for the
//! write-path milestone.
