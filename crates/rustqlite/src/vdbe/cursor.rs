//! VDBE cursor table (mirrors `VdbeCursor` in `vdbe.c`).
//!
//! Placeholder for M3: the per-program table of open cursors (`OpenRead`/`OpenWrite`), each
//! wrapping a b-tree read/write cursor and the bookkeeping the VM needs between opcodes.
