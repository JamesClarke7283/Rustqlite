//! `EXPLAIN` rendering (mirrors the EXPLAIN output path in `vdbeaux.c`).
//!
//! Placeholder for M3: render a [`super::program::Program`] as the `addr|opcode|p1..p5|comment`
//! table that `EXPLAIN` produces, byte-compatibly with the shell. `EXPLAIN QUERY PLAN` is
//! rendered from the planner instead.
