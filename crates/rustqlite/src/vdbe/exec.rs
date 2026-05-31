//! VDBE execution dispatch (mirrors the giant `switch` in `vdbe.c`).
//!
//! Placeholder for M3: the register VM main loop that steps a [`super::program::Program`],
//! dispatching each [`super::opcode::Opcode`] via an exhaustive `match`, driving b-tree
//! cursors through the pager and yielding result rows on `ResultRow`.
