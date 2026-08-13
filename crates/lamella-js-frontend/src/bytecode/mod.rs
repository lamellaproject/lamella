//! The two bridges between the syntax tree and the precompiled artifact format.

pub(crate) mod census;
pub(crate) mod decode;
pub(crate) mod encode;

pub use census::Stats;
pub use decode::decode;
pub use encode::{encode, encode_with_stats, Options};
