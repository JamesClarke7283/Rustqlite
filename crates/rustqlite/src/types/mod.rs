//! Type system: storage classes ([`Value`]), column affinity ([`Affinity`]), and collating
//! sequences ([`Collation`]). Mirrors SQLite's dynamic typing rules from `vdbemem.c` /
//! `analyze.c` and the datatype documentation.

pub mod affinity;
pub mod collation;
pub mod value;

pub use affinity::{affinity_of, Affinity};
pub use collation::Collation;
pub use value::Value;
