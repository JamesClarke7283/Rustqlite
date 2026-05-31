//! VDBE register cell (mirrors `Mem` in `vdbemem.c`).
//!
//! A `Mem` is the value held in a VDBE register. For M1 it is an alias for the plain
//! [`Value`]; in M3 it gains affinity flags and cheaper sharing (`Rc<str>`/`Rc<[u8]>`) so that
//! copying registers around is cheap, exactly as upstream's `Mem` does.

use crate::types::Value;

/// A VDBE register value. (Placeholder alias; see the module docs.)
pub type Mem = Value;
