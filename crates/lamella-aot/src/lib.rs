#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Lamella's ahead-of-time backend: lowering the middle IR to target machine code.

extern crate alloc;

pub mod cil;
pub mod debugmap;
pub mod dwarf;
/// The closed generic instantiation set and the canonical spelling that names each instantiation.
///
/// **EXTRACTED TO ITS OWN LEAF CRATE, BECAUSE BOTH TIERS MONOMORPHIZE.** The interpreter's baker
/// needs the same closed set and the same canonical name this tier does, and it cannot depend on
/// `lamella-aot` without pulling the whole backend into the runtime's tree. The alternatives were a
/// second COLLECTOR (a second walker over one format) or a second SPELLING of a wire value the two
/// tiers must agree on byte for byte, and a second
/// spelling does not fail loudly: two implementations agreeing on
/// `` Pair`2[System.Int32,System.String] `` and diverging on the first NESTED or ARRAY argument is a
/// type that exists twice -- a cast that fails and a static field with two copies.
///
/// Re-exported here so every existing `generics::` path in this crate is unchanged.
pub use lamella_generics as generics;
pub mod resolver;
pub mod target;

mod regalloc;

#[cfg(any(feature = "arm32", feature = "riscv32", feature = "wasm"))]
mod stringgen;

mod stackmaps;

#[cfg(feature = "arm32")]
pub mod arm32;

#[cfg(feature = "riscv32")]
pub mod riscv32;

#[cfg(feature = "wasm")]
pub mod wasm;

#[cfg(any(feature = "arm32", feature = "wasm", feature = "riscv32"))]
pub mod build;
