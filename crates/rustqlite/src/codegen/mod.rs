//! Code generator + query planner (mirrors `build.c`, `select.c`, `insert.c`, `update.c`,
//! `delete.c`, `expr.c`, `where*.c`, `attach.c`, `trigger.c`).
//!
//! Placeholder: this is where an AST from [`rustqlite_parser`] is lowered to a VDBE
//! [`crate::vdbe::program::Program`]. The intended sub-modules mirror upstream:
//! `select`, `insert`, `update`, `delete`, `expr`, `where_`, and `schema_cmds`. The read
//! query path (`SELECT`) is the first to land in M3.
