#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Lamella's Python runtime.

extern crate alloc;

pub mod bigint;
pub mod builtins;
pub mod gpio;
pub mod uart;
pub mod spi;
pub mod i2c;
pub mod adc;
pub mod fileio;
pub(crate) mod board_binding;
pub mod pystdlib;
pub(crate) mod shims;
pub(crate) mod tables;
pub mod interp;
pub mod object;
pub mod reactor;
pub mod stdlib;
pub mod trap;
pub mod value;

pub use builtins::Builtin;
pub use gpio::Board;
pub use interp::{run, run_bundle, run_module, Frame};
pub use lamella_py_bytecode::{
    is_bundle_header, BinOp, Bundle, CmpOp, CodeObject, Const, Functions, Module, Op, Source,
};
pub use object::{Arena, FinalizerSkips, Footprint, InlineCache, ObjectModel, PyType};
pub use trap::Trap;
pub use value::Value;

pub use lamella_py_bytecode::{Capability, Profile};

/// The capability [`Profile`] describing THIS interpreter build -- the cargo features above, read
/// out as a value a compiler can be given.
///
/// # Why it exists
///
/// The knobs are cargo features on this crate, resolved when the image is built. The front end is a
/// separate crate that compiles for every profile from one build, so it cannot read them: a `cfg!`
/// there would describe the machine the compiler was built for, not the board being targeted. The
/// bridge has to be a VALUE, and this is the one place a feature becomes one.
///
/// **It is deliberately the only such place.** The alternative shape -- building the front end
/// into a device image with the same feature set and trusting the two to agree -- is the failure
/// this project has learned to distrust, because nothing compares two build configurations and a
/// mismatch is therefore silent. One function, in the crate that owns the features, cannot disagree
/// with itself.
///
/// A host tool that is targeting some OTHER image (a bundler cross-compiling for a board) builds
/// that board's profile by name instead; this answers for the interpreter you are linked against,
/// which is what an on-device `eval` and a browser IDE both need.
#[must_use]
pub fn profile_of_this_build() -> Profile {
    let mut profile = Profile::BARE;
    if cfg!(feature = "float") {
        profile = profile.with(Capability::Float);
    }
    if cfg!(feature = "complex") {
        profile = profile.with(Capability::Complex);
    }
    if cfg!(feature = "introspection") {
        profile = profile.with(Capability::Introspection);
    }
    profile
}
