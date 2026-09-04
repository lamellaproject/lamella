//! Which machine code a target can run, as one wire value in the `arch` field of a
//! [`TargetIdentity`](super::TargetIdentity). One value means ONE artifact-compatible target ABI:
//! two parts share a value only when an image built for either runs on both, so the instruction-set
//! subset, the floating-point convention and the byte order all distinguish values rather than
//! being read off a family name. Codes are wire values -- append-only, never renumbered.

/// Unknown. A target that does not say, and a host that must therefore ask a person or refuse.
pub const UNKNOWN: u16 = 0;

/// A 32-bit little-endian Thumb machine with the base instruction set only and no
/// floating-point instructions -- the `thumbv6m-none-eabi` ABI.
pub const THUMBV6M: u16 = 1;

/// A 32-bit little-endian Thumb machine with the main instruction set and no floating-point
/// instructions -- the `thumbv7m-none-eabi` ABI.
pub const THUMBV7M: u16 = 2;

/// A 32-bit little-endian Thumb machine with the main and DSP instruction sets, passing
/// floating-point arguments in integer registers -- the `thumbv7em-none-eabi` ABI.
///
/// Distinct from [`THUMBV7EM_HARD_FLOAT`] because the calling convention differs: an image built for
/// one does not link against the other, whatever the silicon underneath can do.
pub const THUMBV7EM: u16 = 3;

/// The same machine as [`THUMBV7EM`], passing floating-point arguments in floating-point registers
/// -- the `thumbv7em-none-eabihf` ABI.
pub const THUMBV7EM_HARD_FLOAT: u16 = 4;

/// A 32-bit little-endian Thumb machine with the v8 main instruction set, passing floating-point
/// arguments in integer registers -- the `thumbv8m.main-none-eabi` ABI.
pub const THUMBV8M_MAIN: u16 = 5;

/// The same machine as [`THUMBV8M_MAIN`], passing floating-point arguments in floating-point
/// registers -- the `thumbv8m.main-none-eabihf` ABI.
pub const THUMBV8M_MAIN_HARD_FLOAT: u16 = 6;

/// A 32-bit little-endian RISC-V machine with the base integer, multiply and compressed extensions
/// -- the `riscv32imc-unknown-none-elf` ABI.
pub const RISCV32IMC: u16 = 7;

/// A 32-bit little-endian RISC-V machine with the base integer and multiply extensions and no
/// compressed instructions -- the `riscv32im-unknown-none-elf` ABI.
pub const RISCV32IM: u16 = 8;

/// A 32-bit little-endian RISC-V machine with the base integer, multiply, atomic and compressed
/// extensions -- the `riscv32imac-unknown-none-elf` ABI.
pub const RISCV32IMAC: u16 = 9;

/// A 32-bit little-endian RISC-V machine on the EMBEDDED register set -- sixteen integer registers
/// rather than thirty-two, and no multiply or divide instructions, so both are library calls.
///
/// The narrowest architecture here and the one that shares least: the halved register file is a
/// different calling convention, not a subset of one.
pub const RISCV32EC: u16 = 10;

/// A 32-bit little-endian WebAssembly machine -- the `wasm32-unknown-unknown` ABI.
///
/// It is a target ABI like any other here even though nothing is soldered underneath: a browser
/// running this protocol's runner is a far end that runs artifacts, and a host that cannot name what
/// it is talking to cannot tell it what will run.
pub const WASM32: u16 = 11;

/// A 32-bit little-endian Thumb machine with the v8 BASELINE instruction set and no floating-point
/// instructions -- the `thumbv8m.base-none-eabi` ABI.
///
/// # A part that RUNS v6-M artifacts reports [`THUMBV6M`], not this
///
/// This field names an artifact ABI rather than a part, per this module's rule that two parts share
/// a value exactly when an image built for either runs on both. A Baseline v8-M part executes the
/// v6-M subset, so a target serving v6-M images must report [`THUMBV6M`] however modern its core is
/// -- reporting this instead would have a host decline to send the very artifact that runs.
///
/// **Report this only for a target whose artifacts are built as `thumbv8m.base-none-eabi`**, which
/// is a different instruction selection and a different image. The code existing here is not a
/// claim that anything builds for it; it is a reserved value so that whoever does build for it
/// first does not have to mint a number in a hurry.
pub const THUMBV8M_BASE: u16 = 12;

/// Every named architecture, as its wire value and its name, so a host can print an `arch` field
/// instead of a number.
///
/// Being INCOMPLETE is a survivable state and is handled rather than asserted away: [`name`]
/// answers `None` for a value this table does not carry, and a caller reports the number. A name
/// that is missing should cost a reader one puzzled moment, never a wrong answer.
pub const NAMED: &[(u16, &str)] = &[
    (THUMBV6M, "thumbv6m"),
    (THUMBV7M, "thumbv7m"),
    (THUMBV7EM, "thumbv7em"),
    (THUMBV7EM_HARD_FLOAT, "thumbv7em-hard-float"),
    (THUMBV8M_MAIN, "thumbv8m-main"),
    (THUMBV8M_MAIN_HARD_FLOAT, "thumbv8m-main-hard-float"),
    (RISCV32IMC, "riscv32imc"),
    (RISCV32IM, "riscv32im"),
    (RISCV32IMAC, "riscv32imac"),
    (RISCV32EC, "riscv32ec"),
    (WASM32, "wasm32"),
    (THUMBV8M_BASE, "thumbv8m-base"),
];

/// The name of an `arch` wire value, or `None` for [`UNKNOWN`] and for anything this build does not
/// name.
#[must_use]
pub fn name(arch: u16) -> Option<&'static str> {
    let mut index = 0;
    while index < NAMED.len() {
        if NAMED[index].0 == arch {
            return Some(NAMED[index].1);
        }
        index += 1;
    }
    None
}

/// Every architecture as its wire value and the rustc TARGET TRIPLE that produces it.
///
/// A separate table from [`NAMED`] because the two answer different questions and only one of them
/// is stable: [`NAMED`]'s strings are for a person reading a field, and shortening or spelling one
/// differently costs a reader nothing, while these are the exact triples a build reports and a typo
/// in one is a target that silently reports [`UNKNOWN`]. Folding them together would put a display
/// string and an identifier under one edit.
///
/// **Not every triple rustc knows -- the ones this tree builds for**, which is the same set
/// [`NAMED`] carries. A build for something else reports [`UNKNOWN`], which is a value with a
/// meaning rather than a wrong answer.
pub const TARGET_TRIPLES: &[(u16, &str)] = &[
    (THUMBV6M, "thumbv6m-none-eabi"),
    (THUMBV7M, "thumbv7m-none-eabi"),
    (THUMBV7EM, "thumbv7em-none-eabi"),
    (THUMBV7EM_HARD_FLOAT, "thumbv7em-none-eabihf"),
    (THUMBV8M_MAIN, "thumbv8m.main-none-eabi"),
    (THUMBV8M_MAIN_HARD_FLOAT, "thumbv8m.main-none-eabihf"),
    (RISCV32IMC, "riscv32imc-unknown-none-elf"),
    (RISCV32IM, "riscv32im-unknown-none-elf"),
    (RISCV32IMAC, "riscv32imac-unknown-none-elf"),
    (RISCV32EC, "riscv32ec-unknown-none-elf"),
    (WASM32, "wasm32-unknown-unknown"),
    (THUMBV8M_BASE, "thumbv8m.base-none-eabi"),
];

/// The `arch` wire value a rustc target triple produces, or [`UNKNOWN`] for a triple this table does
/// not carry -- a host build among them.
///
/// # Why a triple and not a `cfg`
///
/// `target_arch` cannot answer this. `thumbv6m` and `thumbv7em` are both `target_arch = "arm"`, and
/// the hard-float and soft-float ABIs of one machine are the same architecture and different
/// calling conventions -- so a firmware reasoning from `cfg` would report one value for targets
/// whose images do not interchange. The triple is the only thing in a build that names the ABI
/// exactly, and a build script is where it can be read.
#[must_use]
pub fn from_target_triple(triple: &str) -> u16 {
    let mut index = 0;
    while index < TARGET_TRIPLES.len() {
        let (arch, known) = TARGET_TRIPLES[index];
        if known.as_bytes() == triple.as_bytes() {
            return arch;
        }
        index += 1;
    }
    UNKNOWN
}
