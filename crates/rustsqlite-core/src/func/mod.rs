//! Built-in functions (mirrors `func.c`, `date.c`, `printf.c`).
//!
//! M3a ships a starter set of **scalar** functions ([`scalar`]) behind a small [`registry`].
//! Aggregates (`count`/`sum`/`min`/`max`), the `date`/`time` family, and the remaining ~20
//! scalars arrive in later milestones. Function names and edge-case behavior mirror upstream
//! exactly (verified against the `sqlite3` binary).

pub mod aggregate;
pub mod date;
pub mod json;
pub mod like;
pub mod math;
pub mod registry;
pub mod scalar;
pub mod string;

pub use aggregate::{is_aggregate_name, Accumulator, AggregateKind};
pub use registry::{call_scalar, check};
