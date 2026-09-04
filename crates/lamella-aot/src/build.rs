//! The AOT build entry point: compile a CIL assembly to a target's native bytes in one call. This is
//! the library face of the pipeline the `wasm-program`/`deploy-microbit` examples drive -- the
//! website's client-side `lamella_aot_build(cil, target)` exporter (a wasm binding around this) turns
//! a C# assembly into a `.wasm` widget OR a flashable chip image in the browser. No filesystem or
//! `std`: it takes the CIL bytes and returns the output bytes, so it runs inside the compile-only wasm.

#[cfg(feature = "wasm")]
use alloc::format;
#[cfg(feature = "wasm")]
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use lamella_ir::{
    BasicBlock, BinOp, BlockId, CmpOp, ConvKind, Function, Inst, MirType, StaticOwner, Terminator,
    TypeHandle, ValueId,
};
use lamella_metadata::tables::table;
use lamella_metadata::{Assembly, SigType, TargetLayout};
use lamella_token::Token;

#[cfg(feature = "arm32")]
use crate::arm32;
use crate::cil;
use crate::resolver::{ELEMENT_KIND_REFERENCE, MetadataResolver};
#[cfg(feature = "riscv32")]
use crate::riscv32;
#[cfg(feature = "wasm")]
use crate::wasm;

/// Why an AOT build failed.
#[derive(Debug)]
pub enum BuildError {
    /// The CIL assembly's metadata could not be read.
    Parse,
    /// The assembly demands that its consumer understand something this backend does not implement
    /// -- a required custom modifier (ECMA-335 II.7.1.1) or a `CompilerFeatureRequiredAttribute`.
    ///
    /// **The decision is `lamella_metadata::demands`, not a copy of it here.** That crate is where
    /// signatures and custom attributes are read, so this backend and the interpreter's loader --
    /// which have no code in common -- answer the identical question from the identical lists. A
    /// refusal present in one tier and absent in the other makes the lenient tier the one nobody
    /// tests.
    UnmetDemand(alloc::string::String),
    /// The target string is not one this build supports.
    UnsupportedTarget,
    /// A function could not be lowered to the WASM target.
    #[cfg(feature = "wasm")]
    LowerWasm(wasm::LowerError),
    /// A function could not be lowered to the ARM32 target.
    #[cfg(feature = "arm32")]
    LowerArm(arm32::LowerError),
    /// A function could not be lowered to the RISC-V (RV32IM) target.
    #[cfg(feature = "riscv32")]
    LowerRiscv(riscv32::LowerError),
    /// The assembly declares no static `Main`, so there is no entry point to build a runnable object
    /// around. (`build_object_riscv` requires one; a library object has no entry -- that path differs.)
    #[cfg(feature = "riscv32")]
    NoEntryPoint,
    /// A method's CIL body could not be lowered to MIR (e.g. an unsupported construct). Reported rather
    /// than silently leaving the method an empty stub, which would miscompile the program -- a stubbed
    /// `Main` returns nothing.
    LowerCil {
        /// The MethodDef row of the method whose body failed to lower.
        rid: u32,
        /// The CIL-lowering error.
        error: cil::CilError,
    },
    /// Emitted code CALLS a `[RuntimeProvided]` seam this build does not synthesize, whose default is
    /// not declared `[IntendedDefault]`, and which is not trapped -- so the call would link (a static
    /// placeholder is exported like any other method) and then answer a constant the caller cannot
    /// tell from a real result. Refused rather than shipped: an unmarked seam is UNDECLARED, not
    /// assumed safe, and a build is entitled to say so. Marking the seam, synthesizing it, or gating
    /// its callers out of the profile all clear it -- silence does not.
    SilentSeamCallEdge {
        /// The calling method's readable name (`Namespace.Type::Method`).
        caller: alloc::string::String,
        /// The seam's readable name.
        seam: alloc::string::String,
        /// How many such edges the build found; the named pair is the first.
        total: usize,
    },
    /// TWO BODIES WERE WRITTEN FOR ONE MethodDef ROW. A program is a `Vec<Function>` indexed by rid
    /// and every emitted symbol is `f<rid>`, so a second body does not collide -- it REPLACES the
    /// first, and the image is built around whichever won with no diagnostic anywhere. Refused
    /// rather than shipped, for the same reason [`Self::SilentSeamCallEdge`] is: the failure mode is
    /// a confident wrong answer, not a crash.
    ///
    /// Unreachable from well-formed C# 1.0 metadata today -- one row, one method, one body. A
    /// monomorphizer meets it immediately, because N instantiations of one generic method all share
    /// a single MethodDef rid.
    DuplicateMethodBody {
        /// The MethodDef row a second body was written to; the first is the one that was lost.
        rid: u32,
        /// How many rows this build found; `rid` is the first.
        total: usize,
    },
    /// A MONOMORPHIZED BODY the plan named could not be produced: its definition method, its
    /// `TypeSpec`'s arguments, or one of its substituted slot types did not resolve.
    ///
    /// It is an error rather than a skipped body for the reason the whole emission path is shaped
    /// around: a planned index whose slot is never written stays a `stub()` on the RISC-V object
    /// path, and a stub RETURNS. A call to it would answer zero and keep going, which is the silent
    /// wrong answer this tier refuses to ship. A build that cannot emit a body it promised must
    /// stop.
    MonomorphizedBody {
        /// The function index the plan assigned the body.
        index: u32,
        /// The instantiation's canonical spelling.
        instantiation: alloc::string::String,
        /// The method's name.
        method: alloc::string::String,
        /// What could not be resolved.
        reason: MonoGap,
    },
    /// THE INSTANTIATION SET COULD NOT BE PLANNED: a method signature did not decode, so a body
    /// could not be keyed for its call sites.
    ///
    /// Refused rather than planned partially. A missing body is not an absent feature -- the call
    /// to it lands on a `stub()` that returns, which is a wrong answer the program cannot see. The
    /// collector's own refusals (growth on a cycle, a handle collision) arrive here too.
    Instantiations(crate::generics::Refusal),
    /// A LOCAL OR ARGUMENT SLOT IS AN INSTANTIATION OF A VALUE TYPE (`Holder<int>`), whose MIR type
    /// this tier cannot supply: it carries a SIZE and a trace map, and both come from the
    /// substituted layout that monomorphizing value types would produce.
    ///
    /// The twin of [`cil::CilError::GenericValueTypeSlot`], raised from the typing the IMAGE is
    /// emitted from rather than from the one a diagnostic reads. It exists as a separate error
    /// because the two paths are separate, and the defect it closes is what happens when only one
    /// of them refuses: the fallback was `int32`, so an eight-byte struct got a four-byte cell (the
    /// second field's store ran off the end of it) under `TypeHandle(0)`, the anonymous no-type
    /// handle, which made two instantiations of one definition into one type that is no type.
    ///
    /// **It BUILT, LINKED and RAN.** A refusal is not a regression against that; it is the first
    /// honest answer the tier has given about value-type generics.
    ValueTypeInstantiationSlot {
        /// The instantiation's canonical spelling (`generics::spell_sig`), so the build names what
        /// it refused instead of reporting that something somewhere did not type.
        instantiation: alloc::string::String,
    },
    /// AN INSTANTIATION WHOSE DEFINITION DECLARES VIRTUALS OR IMPLEMENTS INTERFACES, whose vtable
    /// and itable would have to be built from SUBSTITUTED signatures and are not yet.
    ///
    /// **THIS REFUSES THE BUILD BECAUSE THE OLD REFUSAL ONLY DROPPED THE DESCRIPTOR.**
    /// `MetadataResolver::instantiated_reference_layout` declines such an instantiation, and
    /// `instantiation_descriptors` then filters it out -- which keeps a WRONG descriptor from being
    /// emitted and does nothing about the image. MEASURED: a program whose generic definition gains
    /// one `virtual` method BUILDS CLEANLY and then HARD FAULTS on an emulated Cortex-M0, with the
    /// identical program minus the `virtual` answering 42. A filter is not a gate.
    UndispatchableInstantiation {
        /// The instantiation's canonical spelling, so the refusal names what cannot be dispatched.
        instantiation: alloc::string::String,
    },
    /// A value type's GC trace map does not fit [`lamella_ir::RefWords`]' 32-word bound -- a struct
    /// larger than 128 bytes with a reference past its 31st word, in a frame.
    ///
    /// **IT REFUSES RATHER THAN TRUNCATING, AND THE DIFFERENCE IS A COLLECTED-WHILE-LIVE OBJECT.** A
    /// narrowed map is a reference the collector never visits on a mark-compact heap; a refusal is a
    /// build that stops. If this ever fires on real code the answer is a wider representation, not a
    /// wider bound.
    ValueTypeTraceMap {
        /// The value type, spelled.
        type_name: alloc::string::String,
        /// Its size in bytes, so the refusal says how far past the bound it is.
        size: u32,
    },
}

/// Which part of a monomorphized body did not resolve, for a [`BuildError::MonomorphizedBody`] that
/// names a defect rather than reporting a count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonoGap {
    /// The plan's `MethodDef` rid names no method, or one with no CIL body.
    NoDefinitionBody,
    /// The plan's `TypeSpec` did not decode to a generic instantiation, so there are no arguments
    /// to substitute.
    NoArguments,
    /// A parameter, local or return type still mentioned a type parameter after substitution, or
    /// did not resolve to a MIR type. It REFUSES rather than falling back to `int32`: a `!0` that
    /// silently became an `I32` would type `Box<string>`'s field as an integer, and the difference
    /// between an integer and a reference is the GC TRACE MAP -- invisible to every size.
    UnsubstitutedSlot,
    /// The definition's CIL did not lower under this instantiation.
    LowerCil(cil::CilError),
    /// The body is stamped [`BodyOwner::Reference`](crate::generics::BodyOwner::Reference) at an
    /// ordinal that names no attached reference, or at one with no file bytes to derive its symbol
    /// family from. A plan built against one reference list and consumed against another is a WRONG
    /// BIND rather than a lookup miss, which is why it refuses instead of falling back to the
    /// module's own tables.
    CrossAssemblyOwner,
    /// **A CROSS-ASSEMBLY INSTANTIATION WHOSE TYPE ARGUMENT NAMES A TYPE, WHICH IS THE HALF OF
    /// THIS CAPABILITY THAT IS NOT BUILT.** A type argument is only token-bearing when it is NAMED:
    /// `Box<int>` and `Box<string>` carry no token at all, so the owner's CIL can be lowered under
    /// them with nothing of the caller's to read. `Box<MyProgramClass>` carries a token of the
    /// CALLER's, and the owner's resolver would read that number out of the OWNER's tables -- a real,
    /// unrelated type, named plausibly. The two are different problems and this refusal is what
    /// keeps them from being one.
    CrossAssemblyNamedArgument {
        /// The argument's spelling as the CALLER writes it, so the refusal names the type.
        argument: alloc::string::String,
    },
    /// **A CROSS-ASSEMBLY INSTANTIATION WHOSE VALUE-TYPE ARGUMENT NAMES A ROW THAT MEANS NOTHING IN
    /// THE WORLD THAT WROTE IT.** A `Class` argument needs no assembly to interpret -- it is a
    /// reference, four bytes and one traced word wherever it came from -- which is why
    /// `Box<MyProgramClass>` builds. A `ValueType` argument is LAID OUT: its width, its field offsets
    /// and its reference map come from a `TypeDef` row, and the row number is the CALLER's while the
    /// assembly the body is read against is the OWNER's.
    ///
    /// **THE ORDINARY VALUE-TYPE ARGUMENT IS CARRIED RATHER THAN REFUSED, AND THAT IS WHY THIS IS
    /// NARROW.** An enum is re-expressed as its underlying primitive and a struct's token is MARKED
    /// (`resolver::ARGUMENT_WORLD_BIT`), so each layout reader resolves it where it was written.
    /// What is left for this refusal is an argument that resolves to no value type AT ALL in the
    /// caller's own world -- refused here rather than downstream because here the argument can still
    /// be named in the message.
    ///
    /// **IT IS SEPARATE FROM [`Self::CrossAssemblyNamedArgument`] BECAUSE IT FIRES FOR A DIFFERENT
    /// REASON AND AT A DIFFERENT TIME.** That one asks what the lowered BODY did -- it refuses a body
    /// that minted an identity out of the owner's tables. This one asks about the ARGUMENT, before
    /// any body is lowered. An ENUM argument is why the two cannot be one rule: it erases to its
    /// underlying integer, so it mints no identity for the other refusal to key on.
    CrossAssemblyValueTypeArgument {
        /// The argument's spelling as the CALLER writes it, so the refusal names the type.
        argument: alloc::string::String,
    },
    /// A lowered cross-assembly body still carries an identity that has no caller-side spelling --
    /// see [`crate::resolver::rebased_handle`]. The refusal is the BACKSTOP for the whole rebase:
    /// what it catches would otherwise be a descriptor named for whichever of the caller's types
    /// shares that row.
    CrossAssemblyIdentity {
        /// The offending handle, as the owner minted it.
        handle: u32,
    },
    /// A lowered cross-assembly body still addresses its OWN static region. Word 0 is the reserved
    /// EH-tag marker, which is one global per image and stays own; anything else would send the
    /// owner's `ldsfld` into the caller's region, where the same slot belongs to whatever field the
    /// caller happens to declare there.
    CrossAssemblyStatic {
        /// The byte offset into the region the body addressed.
        offset: u32,
    },
}

/// Reads an assembly this backend is about to compile, refusing one its author marked unusable by a
/// consumer that does not understand it.
///
/// Every parse this backend performs goes through here, so a demand is read the same way whether it
/// came from the program, the corlib or a library. The check itself is `lamella_metadata::demands`,
/// shared with the interpreter's loader.
fn read_assembly(bytes: &[u8]) -> Result<Assembly<'_>, BuildError> {
    let assembly = Assembly::read(bytes).map_err(|_| BuildError::Parse)?;
    if let Some(message) = lamella_metadata::demands::unmet_demand(&assembly) {
        return Err(BuildError::UnmetDemand(message));
    }
    Ok(assembly)
}

/// Compiles a CIL assembly to native bytes for `target`. `target = "wasm"` emits a WebAssembly module
/// with the embedding ABI (per-method exports + `alloc`/`dealloc` + memory) -- the C# -> `.wasm`
/// widget. A chip `target` ("microbit" for the nRF51 Cortex-M0, "rp2040" for the Pico / Pico H
/// Cortex-M0+, "rp2350" for the Pico 2 / Pico 2 W Cortex-M33) emits a flashable bare-metal image --
/// the flat, linker-free fast path. `"qemu-riscv32"` emits the RV32IM twin of that image for QEMU's
/// `virt` machine, and `"ch32v003"` the RV32EC twin for the CH32V003's 16 KB flash at `0x0000_0000`.
pub fn build(cil: &[u8], target: &str) -> Result<Vec<u8>, BuildError> {
    match target {
        #[cfg(feature = "wasm")]
        "wasm" => build_wasm(cil),
        #[cfg(feature = "arm32")]
        t if CORTEX_M_TARGETS.contains(&t) => build_cortex_m(cil, t),
        #[cfg(feature = "riscv32")]
        "qemu-riscv32" => build_riscv32(cil, target),
        #[cfg(feature = "riscv32")]
        "ch32v003" => build_ch32v003(cil),
        _ => Err(BuildError::UnsupportedTarget),
    }
}

/// QEMU `virt` RAM base. `-bios`/`-kernel` loads the image here and the hart begins at the FIRST
/// INSTRUCTION -- there is no vector table to place, which is why this image has no [SP][reset]
/// prologue like the Cortex-M ones. RISC-V has no hardware SP init either, so the stub sets `sp`
/// before it calls anything that opens a frame.
#[cfg(feature = "riscv32")]
pub const RISCV_VIRT_LOAD_ADDR: u32 = 0x8000_0000;
/// The stack descends from 2 MiB into RAM -- clear of the image at the base of RAM. The same value
/// the `qemu-riscv` harness has used since the backend's first RISC-V program.
#[cfg(feature = "riscv32")]
const RISCV_VIRT_SP_TOP: u32 = 0x8020_0000;
/// The `virt` board's SiFive test finisher: a word written here terminates QEMU. `0x5555` exits 0;
/// `(code << 16) | 0x3333` exits with `code`. This is how the image reports the entry's result --
/// the RISC-V analogue of the RP2350 mailbox, and cheaper, because the machine can simply exit.
#[cfg(feature = "riscv32")]
const RISCV_VIRT_FINISHER: u32 = 0x0010_0000;

/// Compiles a CIL assembly to a runnable bare-metal RV32IM image for QEMU's `virt` machine: the same
/// front end the Cortex-M path uses, laid out by [`riscv32::lower_module`], behind a boot stub that
/// sets `sp`, calls the entry, and EXITS QEMU WITH THE ENTRY'S RETURN VALUE through the SiFive test
/// finisher. So `qemu-system-riscv32 -machine virt -bios <out.bin> -nographic` exits 42 for a `Main`
/// returning 42, with no probe, mailbox read or semihosting.
///
/// This is the flat, linker-free fast path, exactly as [`build_cortex_m`] is: no external calls, so
/// no soft-float helpers, no GC seam, no P/Invoke. The linked object pipeline for this backend is
/// [`riscv32::lower_object`] plus `lamella-link`, which is what the differential harness drives.
///
/// **It is QEMU, not silicon.** The RV32IM code it emits is the same code the 519-row differential
/// runs, but a real RISC-V part boots its own way (an ESP32-C6 wants an ESP image header and its
/// bootloader; a CH32V003 wants the RV32EC profile and flash at 0). Wiring those is a per-part boot
/// image beside this one, not a change to the lowering.
#[cfg(feature = "riscv32")]
pub fn build_riscv32(cil: &[u8], target: &str) -> Result<Vec<u8>, BuildError> {
    if target != "qemu-riscv32" {
        return Err(BuildError::UnsupportedTarget);
    }
    let assembly = read_assembly(cil)?;
    let entry = find_main(&assembly);
    let (funcs, _plan) = lower_assembly(&assembly, entry, &[])?;
    let code = riscv32::lower_module(&funcs).map_err(BuildError::LowerRiscv)?;
    Ok(riscv_virt_boot_image(&code))
}

/// Wraps RV32IM code whose function 0 is the entry in a QEMU `virt` boot image. Single-sourced here
/// so [`build_riscv32`] and any harness that wants the same shape agree by construction, the way
/// [`rp2350_boot_image`] serves both the browser export and the object-path flasher.
#[cfg(feature = "riscv32")]
pub fn riscv_virt_boot_image(code: &[u8]) -> Vec<u8> {
    use lamella_asm_riscv32::{BranchCond, Encoder, Reg};
    let mut enc = Encoder::new();
    let entry = enc.new_label();
    let pass = enc.new_label();
    let write = enc.new_label();
    let halt = enc.new_label();
    enc.li(Reg::SP, RISCV_VIRT_SP_TOP as i32);
    enc.jal(Reg::RA, entry);
    enc.branch(BranchCond::Eq, Reg::A0, Reg::ZERO, pass);
    enc.slli(Reg::T2, Reg::A0, 16);
    enc.li(Reg::T0, 0x3333);
    enc.or(Reg::T2, Reg::T2, Reg::T0);
    enc.j(write);
    enc.bind_label(pass);
    enc.li(Reg::T2, 0x5555);
    enc.bind_label(write);
    enc.lui(Reg::T1, RISCV_VIRT_FINISHER >> 12);
    enc.sw(Reg::T2, Reg::T1, 0);
    enc.bind_label(halt);
    enc.j(halt);
    enc.bind_label(entry);
    let stub = enc.finish().expect("the riscv virt boot stub assembles").bytes;
    let mut image = Vec::with_capacity(stub.len() + code.len());
    image.extend_from_slice(&stub);
    image.extend_from_slice(code);
    image
}

/// CH32V003 reset vector. The 16 KB flash executes from `0x0000_0000` on reset -- the boot alias of
/// the `0x0800_0000`-native flash (CH32V003RM) -- so the image is linked to run from zero and the
/// reset stub must lay out first.
#[cfg(feature = "riscv32")]
pub const CH32V003_FLASH_BASE: u32 = 0x0000_0000;
/// Top of the CH32V003's 2 KB SRAM at `0x2000_0000`; the stack grows down from it. RISC-V has no
/// hardware SP init, so the boot stub sets `sp` before it calls anything that opens a frame.
///
/// Public alongside [`CH32V003_FLASH_BASE`] so the LINKED pipeline (`examples/ch32v003-blink`) and
/// this flat one cannot disagree about the part's memory map. The two emit different images by
/// design; the chip they describe is the same one, and it should be written down once.
#[cfg(feature = "riscv32")]
pub const CH32V003_SRAM_TOP: u32 = 0x2000_0800;

/// Compiles a CIL assembly to a flashable bare-metal image for the CH32V003 (QingKe RV32EC): the
/// same front end every other target uses, laid out by [`riscv32::lower_module_profile`] under
/// [`riscv32::RiscvProfile::Rv32ec`] (x0-x15 only, no M-extension), behind a reset stub that sets
/// `sp` and calls the entry. Flash the result to `0x0000_0000` with a WCH-LinkE and reset.
///
/// **This is the flat, linker-free fast path**, exactly as [`build_riscv32`] and [`build_cortex_m`]
/// are: a whole program with no external calls. On RV32EC that bound is TIGHTER than on the other
/// targets, and it is worth stating rather than discovering: the profile lowers scalar `mul`/`div`/
/// `rem` to `__mulsi3`/`__divsi3`-style soft-routine CALLS, and a flat image has nothing to resolve
/// them against, so a program that multiplies or divides is REFUSED with
/// [`riscv32::LowerError::ControlFlowUnsupported`] rather than mislinked. Array access is
/// unaffected, by two different mechanisms: a single-dimension index scales by a power-of-two
/// element size, which is a shift, and the multi-dimensional index/alloc arithmetic goes through an
/// INLINE shift-and-add multiply instead of the routine. The linked pipeline that DOES resolve those
/// routines is [`build_object_riscv_profile`] plus `lamella-link`, which is the path
/// `examples/ch32v003-blink` drives and the one that has been on silicon.
#[cfg(feature = "riscv32")]
pub fn build_ch32v003(cil: &[u8]) -> Result<Vec<u8>, BuildError> {
    let assembly = read_assembly(cil)?;
    let entry = find_main(&assembly);
    let (funcs, _plan) = lower_assembly(&assembly, entry, &[])?;
    let code = riscv32::lower_module_profile(&funcs, riscv32::RiscvProfile::Rv32ec)
        .map_err(BuildError::LowerRiscv)?;
    Ok(ch32v003_boot_image(&code))
}

/// Wraps RV32EC code whose function 0 is the entry in a CH32V003 boot image. Single-sourced here so
/// [`build_ch32v003`] and any harness wanting the same shape agree by construction, the way
/// [`rp2350_boot_image`] and [`riscv_virt_boot_image`] already do.
///
/// The stub sets `sp` to the top of SRAM and calls the entry trampoline; a `Main` that returns lands
/// in the spin below it. A real chip has no SiFive finisher to write a pass/fail word to, so unlike
/// [`riscv_virt_boot_image`] there is nowhere to report a result -- the image parks instead.
#[cfg(feature = "riscv32")]
pub fn ch32v003_boot_image(code: &[u8]) -> Vec<u8> {
    use lamella_asm_riscv32::{Encoder, Reg};
    let mut enc = Encoder::new();
    let entry = enc.new_label();
    let spin = enc.new_label();
    enc.li(Reg::SP, CH32V003_SRAM_TOP as i32);
    enc.jal(Reg::RA, entry);
    enc.bind_label(spin);
    enc.j(spin);
    enc.bind_label(entry);
    let stub = enc.finish().expect("the ch32v003 boot stub assembles").bytes;
    let mut image = Vec::with_capacity(stub.len() + code.len());
    image.extend_from_slice(&stub);
    image.extend_from_slice(code);
    image
}

/// Compiles a CIL assembly to a WebAssembly module: every method lowered through the same
/// `resolver` + `cil` front end the ARM/RISC-V backends use, then `wasm::lower_module_with_exports`.
/// Exports every public static method by name (the widget surface) plus `main` for the entry, if any.
#[cfg(feature = "wasm")]
pub fn build_wasm(cil: &[u8]) -> Result<Vec<u8>, BuildError> {
    let assembly = read_assembly(cil)?;
    let entry = find_main(&assembly);
    let (funcs, plan) = lower_assembly(&assembly, entry, &[])?;
    let exports = method_exports(&assembly, entry.is_some());
    let export_refs: Vec<(&str, u32)> = exports.iter().map(|(n, i)| (n.as_str(), *i)).collect();
    let resolver = MetadataResolver::new(&assembly).with_monomorphized(plan);
    let mut descriptors = resolver.image_descriptors();
    append_reference_descriptors(&funcs, &resolver, &mut descriptors);
    let string_handle = resolver.string_type_handle().map(|handle| handle.0);
    wasm::lower_module_with_exports(&funcs, &export_refs, &descriptors, string_handle)
        .map_err(BuildError::LowerWasm)
}

/// Compiles a CIL assembly to a flashable bare-metal image for a Cortex-M chip `target` (e.g.
/// "microbit"): every method lowered through the same front end and laid out by `arm32::lower_module`,
/// the entry's trampoline at code offset 0, behind a minimal vector table (initial SP, then a reset
/// vector pointing at that trampoline, Thumb bit set). The entry method IS the program -- it should
/// loop forever, since an embedded reset handler never returns.
///
/// This is the flat, linker-free fast path: it cannot resolve external or cross-object calls, so float
/// helpers, the GC seam, P/Invoke, and `CallNative` are unavailable. For the full object pipeline, build
/// the device image through `lamella-firmware`'s `build_cortex_m_image` (object lowering + link).
#[cfg(feature = "arm32")]
pub fn build_cortex_m(cil: &[u8], target: &str) -> Result<Vec<u8>, BuildError> {
    if !CORTEX_M_TARGETS.contains(&target) {
        return Err(BuildError::UnsupportedTarget);
    }
    let assembly = read_assembly(cil)?;
    let entry = find_main(&assembly);
    let (funcs, _plan) = lower_assembly(&assembly, entry, &[])?;
    let code = arm32::lower_module(&funcs).map_err(BuildError::LowerArm)?;
    cortex_m_boot_image(target, &code)
}

/// The Cortex-M chips this crate can shape a boot image for. Both the up-front validation in
/// [`build_cortex_m`]/[`build_py`] and [`cortex_m_boot_image`]'s arms are driven from this one list,
/// so a chip cannot be accepted by the guard and then fall through unhandled -- the test below walks
/// it and requires an image from every entry.
#[cfg(feature = "arm32")]
pub const CORTEX_M_TARGETS: [&str; 4] = ["microbit", "nrf52833", "rp2040", "rp2350"];

/// Wraps Cortex-M `code` whose function 0 is the entry in `target`'s boot image. Single-sourced here
/// so every front end reaching this backend -- CIL through [`build_cortex_m`], Python through
/// [`build_py`] -- gets the SAME bytes in front of the same code, the way [`rp2350_boot_image`],
/// [`riscv_virt_boot_image`] and [`ch32v003_boot_image`] already serve their own callers.
#[cfg(feature = "arm32")]
fn cortex_m_boot_image(target: &str, code: &[u8]) -> Result<Vec<u8>, BuildError> {
    Ok(match target {
        "rp2350" => rp2350_boot_image(0, code),
        "rp2040" => rp2040_boot_image(0, code),
        "microbit" | "nrf52833" => {
            let initial_sp: u32 = match target {
                "nrf52833" => 0x2002_0000,
                _ => 0x2000_4000,
            };
            let mut image = Vec::with_capacity(8 + code.len());
            image.extend_from_slice(&initial_sp.to_le_bytes());
            image.extend_from_slice(&0x0000_0009u32.to_le_bytes());
            image.extend_from_slice(code);
            image
        }
        _ => return Err(BuildError::UnsupportedTarget),
    })
}

/// Compiles an already-lowered PYTHON module to a flashable bare-metal image for a Cortex-M chip,
/// the twin of [`build_cortex_m`] for the other front end: `funcs` is what
/// `lamella_py_frontend::lower::lower_module` produces, **`funcs[0]` is the entry** (the same
/// function-0-is-the-entry contract [`riscv_virt_boot_image`] and [`ch32v003_boot_image`] state), and
/// the entry should loop forever because a reset handler never returns.
///
/// It takes MIR rather than bytes because that is where the two front ends meet: `build()` and
/// [`build_cortex_m`] start from a CIL assembly, and Python has no CIL to hand them. This crate
/// cannot depend on the Python front end (that crate depends on THIS one), so the seam is
/// [`lamella_ir::Function`], which both already speak.
///
/// **THIS IS THE FLAT, LINKER-FREE PATH and its limits are the flat path's, not Python's.** It
/// resolves no external calls, so a module that ALLOCATES (no `lamella_gc_alloc` address is threaded
/// here), needs soft-float helpers, or reaches the runtime-support archive for a console will not
/// build -- and a `PyIntrinsic` (`getattr`/`len`/`call`) errors as `CallUnsupported`, because
/// [`arm32::PySupport`] carries ADDRESSES and a flat image has no linker to resolve them against.
/// What it does cover is the shape that has been on silicon: a self-contained typed function doing
/// MMIO in a loop. The object path (`arm32::lower_object` + `lamella_link::link_with_archives`,
/// which `lamella-py-frontend`'s `microbit-run` example drives end to end) is the one that lifts
/// those limits, and it is not reachable from here either.
#[cfg(feature = "arm32")]
pub fn build_py(funcs: &[Function], target: &str) -> Result<Vec<u8>, BuildError> {
    if !CORTEX_M_TARGETS.contains(&target) {
        return Err(BuildError::UnsupportedTarget);
    }
    let (code, _maps) = arm32::lower_module_py(funcs, None, arm32::PySupport::default())
        .map_err(BuildError::LowerArm)?;
    cortex_m_boot_image(target, &code)
}

/// The per-method debug info [`build_debug`] returns: `(MethodDef rid, the function's image offset, its
/// LineTable)`. A native PC maps to a method by the offset bracket, then via the LineTable to a CIL byte
/// offset, then via the method's source/PDB to source.
#[cfg(feature = "arm32")]
pub type MethodDebug = alloc::vec::Vec<(u32, u32, arm32::LineTable)>;

/// As [`build_cortex_m`], but also returns per-method debug line tables -- so a device debugger steps the
/// flashed image. It is build()'s EXACT chip path (the trampoline at code offset 0, rid-indexed methods,
/// stub gaps), so the SAME bytes are produced and the line tables match the layout BY CONSTRUCTION.
/// Offsets are IMAGE-relative (the code sits at image offset 8, after the vector table); cross-method
/// calls resolve (the rid-indexed layout). `device-dap-server` uses this instead of single-method debug.
#[cfg(feature = "arm32")]
pub fn build_debug(cil: &[u8], target: &str) -> Result<(Vec<u8>, MethodDebug), BuildError> {
    let initial_sp: u32 = match target {
        "microbit" => 0x2000_4000,
        _ => return Err(BuildError::UnsupportedTarget),
    };
    let assembly = read_assembly(cil)?;
    let entry = find_main(&assembly);
    let (funcs, maps, fails, duplicates, _plan) = lower_assembly_debug(&assembly, entry, &[])?;
    refuse_duplicate_bodies(&duplicates)?;
    if let Some((rid, error)) = fails.into_iter().next() {
        return Err(BuildError::LowerCil { rid, error });
    }
    let (code, method_lines) =
        arm32::lower_module_debug(&funcs, None, &maps).map_err(BuildError::LowerArm)?;
    let mut image = Vec::with_capacity(8 + code.len());
    image.extend_from_slice(&initial_sp.to_le_bytes());
    image.extend_from_slice(&0x0000_0009u32.to_le_bytes());
    image.extend_from_slice(&code);
    const PREFIX: u32 = 8;
    let debug = method_lines
        .into_iter()
        .enumerate()
        .map(|(rid, (func_offset, line_table))| {
            let shifted = arm32::LineTable(
                line_table
                    .0
                    .into_iter()
                    .map(|(pos, cil_off)| (pos + PREFIX, cil_off))
                    .collect(),
            );
            (rid as u32, func_offset + PREFIX, shifted)
        })
        .collect();
    Ok((image, debug))
}

/// The ARMv6-M/v7-M vector-table offset register, shared by the RP2040 and RP2350 boot paths: each
/// points it at its own vector table so a fault vectors through the image's table rather than
/// whatever the mask ROM left behind.
#[cfg(feature = "arm32")]
const SCB_VTOR: u32 = 0xE000_ED08;

/// The RP2040 mask ROM reads exactly this many bytes from flash offset 0 (datasheet 2.8.1): 252
/// bytes of stage 2 followed by its 4-byte checksum.
#[cfg(feature = "arm32")]
pub const RP2040_BOOT2_BYTES: usize = 256;
/// The checksummed part of those 256 bytes -- the stage 2 itself.
#[cfg(feature = "arm32")]
pub const RP2040_BOOT2_PAYLOAD: usize = 252;
/// Where the RP2040 image's own vector table sits: directly after the stage 2, at XIP flash base +
/// 0x100. The serve firmware for this part splits its memory the same way.
#[cfg(feature = "arm32")]
pub const RP2040_VECTOR_BASE: u32 = 0x1000_0100;

/// RP2350 (Pico 2 / Pico 2 W) result mailbox: the `rp2350` boot stub stamps `[magic][return value]`
/// near the top of SRAM, read over SWD WITHOUT halting the core (the flasher's `rp2350-peek`, or the
/// browser's MEM-AP reads) to confirm the image ran and recover the entry's return value.
#[cfg(feature = "arm32")]
pub const RP2350_RESULT_ADDR: u32 = 0x2007_F000;
/// Stamped at [`RP2350_RESULT_ADDR`] before the entry runs ("booted, in managed code").
#[cfg(feature = "arm32")]
pub const RP2350_BOOT_MAGIC: u32 = 0xB007_1A6D;
/// Stamped over [`RP2350_BOOT_MAGIC`] once the entry returns; `RESULT_ADDR + 4` then holds the result.
#[cfg(feature = "arm32")]
pub const RP2350_DONE_MAGIC: u32 = 0x4C41_4D44;

/// Wraps AOT-lowered Cortex-M code -- the flat [`build_cortex_m`] blob OR a linked object-path image
/// -- in a flashable RP2350 boot image: the 16-entry vector table, the PICOBIN IMAGE_DEF block the
/// bootrom validates, and a reset stub that points VTOR at the flash base, seeds the heap pointer,
/// ZEROES the statics/heap band (power-on RAM is garbage on real silicon), stamps
/// [`RP2350_BOOT_MAGIC`], calls the entry (`entry_offset` past the code base at 0x1000_0100), stores
/// `[RP2350_DONE_MAGIC, return value]` in the mailbox, and parks. The entry RETURNS its result
/// (unlike the microbit flat model, where it loops forever), so a host reads the verdict at
/// [`RP2350_RESULT_ADDR`]. Single-sourced here so the browser AOT export ([`build`]'s `rp2350` arm)
/// and the object-path flasher agree on the boot layout + verdict mailbox.
#[cfg(feature = "arm32")]
pub fn rp2350_boot_image(entry_offset: u32, code: &[u8]) -> Vec<u8> {
    use lamella_asm_arm32::{Cond, Encoder, Reg};
    /// XIP flash base: the bootrom boots the vector table here after validating IMAGE_DEF.
    const CODE_REGION: u32 = 0x1000_0000;
    /// Link base for the program text, after the vector table + IMAGE_DEF + reset stub region.
    const CODE_BASE: u32 = CODE_REGION + 0x100;
    /// Top of the 512 KB main SRAM; the stack descends from here.
    const SP_TOP: u32 = 0x2008_0000;
    /// The bump allocator's high-water pointer word -- the fixed address runtime-support uses.
    const HEAP_PTR: u32 = 0x2000_0100;
    /// The bump heap grows up from here -- above the statics window + the archive's static band.
    const HEAP_BASE: u32 = 0x2001_0000;
    /// The stub ZEROES [ZERO_START, ZERO_END) before managed code runs: power-on RAM is garbage on
    /// real silicon (the statics window's word 0 is the EH tag; garbage there HardFaults startup).
    const ZERO_START: u32 = 0x2000_0100;
    const ZERO_END: u32 = HEAP_BASE + 0x1_0000;
    /// One past the last byte the bump allocator may hand out -- the archive returns NULL rather than
    /// bumping past it, and a zero here means "no heap", so seeding it is not optional.
    ///
    /// The ceiling is [`ZERO_END`], i.e. the heap is exactly the band this stub PREPARED. That is the
    /// honest bound rather than the larger one the 512 KB SRAM would allow: an object's reference
    /// fields have to read as null before a constructor writes them, and only the zeroed band
    /// guarantees that. Handing out memory this stub never cleared would seed live objects from
    /// power-on garbage -- which on a mark-compact heap is a wrong ROOT, not merely a wrong value.
    const HEAP_LIMIT: u32 = ZERO_END;
    /// The PICOBIN IMAGE_DEF block the bootrom validates: a self-looping Arm RP2350 EXE, no signing.
    const IMAGE_DEF: [u32; 5] = [0xffff_ded3, 0x1021_0142, 0x0000_01ff, 0x0000_0000, 0xab12_3579];

    let entry_addr = (CODE_BASE + entry_offset) | 1;
    const FAULT_OFF: u32 = 16 * 4 + 5 * 4;
    const STUB_OFF: u32 = FAULT_OFF + 2;
    let mut enc = Encoder::new();
    enc.emit_word(SP_TOP);
    enc.emit_word((CODE_REGION + STUB_OFF) | 1);
    for _ in 2..16 {
        enc.emit_word((CODE_REGION + FAULT_OFF) | 1);
    }
    debug_assert_eq!(enc.position(), 64);
    for word in IMAGE_DEF {
        enc.emit_word(word);
    }
    debug_assert_eq!(enc.position(), FAULT_OFF);
    let fault = enc.new_label();
    enc.bind_label(fault);
    enc.b(fault);
    debug_assert_eq!(enc.position(), STUB_OFF);

    let vtor_word = enc.new_label();
    let region_word = enc.new_label();
    let zero_start_word = enc.new_label();
    let zero_end_word = enc.new_label();
    let heap_ptr_word = enc.new_label();
    let heap_base_word = enc.new_label();
    let heap_limit_word = enc.new_label();
    let boot_magic_word = enc.new_label();
    let done_magic_word = enc.new_label();
    let result_word = enc.new_label();
    let result_hi_word = enc.new_label();
    let entry_word = enc.new_label();
    enc.ldr_literal(Reg::R0, region_word).unwrap();
    enc.ldr_literal(Reg::R1, vtor_word).unwrap();
    enc.str_imm(Reg::R0, Reg::R1, 0).unwrap();
    enc.movs_imm(Reg::R0, 0).unwrap();
    enc.ldr_literal(Reg::R1, zero_start_word).unwrap();
    enc.ldr_literal(Reg::R2, zero_end_word).unwrap();
    let zero_loop = enc.new_label();
    enc.bind_label(zero_loop);
    enc.str_imm(Reg::R0, Reg::R1, 0).unwrap();
    enc.adds_imm8(Reg::R1, 4).unwrap();
    enc.cmp_reg(Reg::R1, Reg::R2).unwrap();
    enc.b_cond(Cond::CarryClear, zero_loop);
    enc.ldr_literal(Reg::R0, heap_base_word).unwrap();
    enc.ldr_literal(Reg::R1, heap_ptr_word).unwrap();
    enc.str_imm(Reg::R0, Reg::R1, 0).unwrap();
    enc.ldr_literal(Reg::R0, heap_limit_word).unwrap();
    enc.str_imm(Reg::R0, Reg::R1, 4).unwrap();
    enc.ldr_literal(Reg::R0, boot_magic_word).unwrap();
    enc.ldr_literal(Reg::R1, result_word).unwrap();
    enc.str_imm(Reg::R0, Reg::R1, 0).unwrap();
    enc.ldr_literal(Reg::R0, entry_word).unwrap();
    enc.blx(Reg::R0);
    enc.ldr_literal(Reg::R1, result_hi_word).unwrap();
    enc.str_imm(Reg::R0, Reg::R1, 0).unwrap();
    enc.ldr_literal(Reg::R0, done_magic_word).unwrap();
    enc.ldr_literal(Reg::R1, result_word).unwrap();
    enc.str_imm(Reg::R0, Reg::R1, 0).unwrap();
    let park = enc.new_label();
    enc.bind_label(park);
    enc.b(park);
    enc.align_to_word();
    enc.bind_label(vtor_word);
    enc.emit_word(SCB_VTOR);
    enc.bind_label(region_word);
    enc.emit_word(CODE_REGION);
    enc.bind_label(zero_start_word);
    enc.emit_word(ZERO_START);
    enc.bind_label(zero_end_word);
    enc.emit_word(ZERO_END);
    enc.bind_label(heap_ptr_word);
    enc.emit_word(HEAP_PTR);
    enc.bind_label(heap_base_word);
    enc.emit_word(HEAP_BASE);
    enc.bind_label(heap_limit_word);
    enc.emit_word(HEAP_LIMIT);
    enc.bind_label(boot_magic_word);
    enc.emit_word(RP2350_BOOT_MAGIC);
    enc.bind_label(done_magic_word);
    enc.emit_word(RP2350_DONE_MAGIC);
    enc.bind_label(result_word);
    enc.emit_word(RP2350_RESULT_ADDR);
    enc.bind_label(result_hi_word);
    enc.emit_word(RP2350_RESULT_ADDR + 4);
    enc.bind_label(entry_word);
    enc.emit_word(entry_addr);
    let pad = ((CODE_BASE - CODE_REGION) - enc.position()) as usize;
    enc.emit_bytes(&vec![0u8; pad]);
    enc.emit_bytes(code);
    enc.finish().unwrap().bytes
}

/// RP2040 (Pico / Pico H) result mailbox -- the RP2350 mailbox's twin, one bank lower because this
/// part has 264 KB of SRAM rather than 520. Read over SWD without halting the core to confirm the
/// image ran and recover the entry's return value.
#[cfg(feature = "arm32")]
pub const RP2040_RESULT_ADDR: u32 = 0x2003_F000;
/// Stamped at [`RP2040_RESULT_ADDR`] before the entry runs ("booted, in managed code").
#[cfg(feature = "arm32")]
pub const RP2040_BOOT_MAGIC: u32 = 0xB007_1A6D;
/// Stamped over [`RP2040_BOOT_MAGIC`] once the entry returns; `RESULT_ADDR + 4` then holds the result.
#[cfg(feature = "arm32")]
pub const RP2040_DONE_MAGIC: u32 = 0x4C41_4D44;

/// The 256 bytes the RP2040 mask ROM reads from flash offset 0: a stage 2 generated here rather
/// than vendored.
///
/// RP2040 datasheet 2.8.1 gives the boot sequence, and the fact that makes this correct is that the
/// mask ROM has ALREADY set up the QSPI pad/IO muxing and the SSI, and already issued the XIP exit
/// sequence, before it copies these 256 bytes into SRAM and jumps to them. **So a stage 2 does not
/// have to know the fitted flash part to be CORRECT -- only to be FAST.** The vendored stage 2 this
/// replaces (`boot2_w25q080.padded.bin`) is named for a Winbond W25Q080 and configures a quad-read
/// XIP mode specific to it; a board with a different QSPI part needs a different blob.
///
/// So this one asks the ROM for the universal path instead. Datasheet 2.8.3 publishes the ROM's own
/// functions through a lookup helper -- the table pointer at `0x14`, the helper at `0x18`, a code
/// being two ASCII bytes packed `(c2 << 8) | c1` -- and among them (2.8.3.1.3):
///
/// > `'C','X'  void _flash_enter_cmd_xip(void)` -- Configure the SSI to generate a standard 03h
/// > serial read command, with 24 address bits, upon each XIP access. This is a very slow XIP
/// > configuration, but is very widely supported.
///
/// **NAMED TRADEOFF, because it should not be discovered with a benchmark: this boots and executes
/// SLOWER than a part-specific stage 2**, since 03h single-lane reads are slower than a quad-read
/// XIP mode, and on an XIP part that is every instruction fetch, not just the boot. The future
/// optimization is a per-flash fast path selected from board facts -- the shape this tree already
/// uses everywhere else. Correct and part-agnostic first.
///
/// `_flash_flush_cache` ('F','C') is called first because that is the order the datasheet's own
/// documented sequence uses (2.8.3.1.3): it enables the XIP cache and clears any IO forcing left on
/// QSPI CSn. It is a no-op on a cache that is already clean, so it costs 4 instructions to not
/// depend on what the mask ROM happened to leave behind.
///
/// The result is position-independent (PC-relative literals only, no self-referencing words), which
/// it must be: the ROM copies it to SRAM and runs it there, not at the address it was linked for.
#[cfg(feature = "arm32")]
pub fn rp2040_boot2() -> [u8; RP2040_BOOT2_BYTES] {
    use lamella_asm_arm32::{Encoder, Reg};
    /// Datasheet 2.8.3, table 178: the ROM's well-known low-memory layout. Both are 16-bit
    /// pointers -- the whole bootrom lives in the low 16 KB.
    const ROM_FUNC_TABLE_PTR: u8 = 0x14;
    const ROM_TABLE_LOOKUP_PTR: u8 = 0x18;
    /// `rom_table_code(c1, c2) = (c2 << 8) | c1` (datasheet 2.8.3.1).
    const fn rom_code(c1: u8, c2: u8) -> u32 {
        ((c2 as u32) << 8) | c1 as u32
    }

    let mut enc = Encoder::new();
    let flush_code_word = enc.new_label();
    let xip_code_word = enc.new_label();
    let vectors_word = enc.new_label();
    let vtor_word = enc.new_label();

    enc.movs_imm(Reg::R0, ROM_FUNC_TABLE_PTR).unwrap();
    enc.ldrh_imm(Reg::R4, Reg::R0, 0).unwrap();
    enc.movs_imm(Reg::R0, ROM_TABLE_LOOKUP_PTR).unwrap();
    enc.ldrh_imm(Reg::R5, Reg::R0, 0).unwrap();

    for code_word in [flush_code_word, xip_code_word] {
        enc.mov_reg(Reg::R0, Reg::R4);
        enc.ldr_literal(Reg::R1, code_word).unwrap();
        enc.blx(Reg::R5);
        enc.movs_imm(Reg::R1, 1).unwrap();
        enc.orrs(Reg::R0, Reg::R1).unwrap();
        enc.blx(Reg::R0);
    }

    enc.ldr_literal(Reg::R0, vectors_word).unwrap();
    enc.ldr_literal(Reg::R1, vtor_word).unwrap();
    enc.str_imm(Reg::R0, Reg::R1, 0).unwrap();
    enc.ldr_imm(Reg::R1, Reg::R0, 4).unwrap();
    enc.ldr_imm(Reg::R0, Reg::R0, 0).unwrap();
    enc.mov_reg(Reg::SP, Reg::R0);
    enc.bx(Reg::R1);

    enc.align_to_word();
    enc.bind_label(flush_code_word);
    enc.emit_word(rom_code(b'F', b'C'));
    enc.bind_label(xip_code_word);
    enc.emit_word(rom_code(b'C', b'X'));
    enc.bind_label(vectors_word);
    enc.emit_word(RP2040_VECTOR_BASE);
    enc.bind_label(vtor_word);
    enc.emit_word(SCB_VTOR);
    let stub = enc.finish().expect("the rp2040 boot2 stub assembles").bytes;

    assert!(
        stub.len() <= RP2040_BOOT2_PAYLOAD,
        "the rp2040 stage 2 must fit the mask ROM's 252-byte payload"
    );
    let mut boot2 = [0u8; RP2040_BOOT2_BYTES];
    boot2[..stub.len()].copy_from_slice(&stub);
    let checksum = boot2_checksum(&boot2[..RP2040_BOOT2_PAYLOAD]);
    boot2[RP2040_BOOT2_PAYLOAD..].copy_from_slice(&checksum.to_le_bytes());
    boot2
}

/// The checksum the RP2040 mask ROM validates before it will run a stage 2, over the first 252
/// bytes, stored little-endian in the last 4. Datasheet 2.8.1.3.1 states it as five parameters and
/// this is written from those, not from the name they add up to (which is CRC-32/MPEG-2 -- a
/// mnemonic, where the parameters are the specification):
///
/// > Polynomial: `0x04c11db7` / Input reflection: no / Output reflection: no /
/// > Initial value: `0xffffffff` / Final XOR: `0x00000000`
#[cfg(feature = "arm32")]
fn boot2_checksum(payload: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in payload {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Wraps AOT-lowered Cortex-M code in a flashable RP2040 image: [`rp2040_boot2`] at flash offset 0,
/// then the vector table at [`RP2040_VECTOR_BASE`] and a reset stub that seeds and zeroes RAM,
/// stamps [`RP2040_BOOT_MAGIC`], calls the entry, stores `[RP2040_DONE_MAGIC, return value]` in the
/// mailbox at [`RP2040_RESULT_ADDR`], and parks.
///
/// The shape past the boot2 is [`rp2350_boot_image`]'s, deliberately, and the differences are the
/// two parts' own: the RP2040 has no PICOBIN `IMAGE_DEF` (its mask ROM checksums a stage 2 instead
/// of scanning the image), and its RAM is 264 KB rather than 520.
#[cfg(feature = "arm32")]
pub fn rp2040_boot_image(entry_offset: u32, code: &[u8]) -> Vec<u8> {
    use lamella_asm_arm32::{Cond, Encoder, Reg};
    /// Link base for the program text, past the vector table + reset stub region.
    const CODE_BASE: u32 = RP2040_VECTOR_BASE + 0x100;
    /// Top of the 264 KB SRAM window (SRAM0-3 256 KB + SRAM4/5 2x4 KB, contiguous); the stack
    /// descends from here. The serve firmware for this part uses the
    /// serve firmware -- one part, one stack top.
    const SP_TOP: u32 = 0x2004_2000;
    /// The bump allocator's high-water pointer word -- the fixed address runtime-support uses.
    const HEAP_PTR: u32 = 0x2000_0100;
    /// The bump heap grows up from here -- above the statics window + the archive's static band.
    const HEAP_BASE: u32 = 0x2001_0000;
    /// The stub ZEROES [ZERO_START, ZERO_END) before managed code runs: power-on RAM is garbage on
    /// real silicon (the statics window's word 0 is the EH tag; garbage there HardFaults startup).
    const ZERO_START: u32 = 0x2000_0100;
    const ZERO_END: u32 = HEAP_BASE + 0x1_0000;
    /// One past the last byte the bump allocator may hand out; a zero here means "no heap", so
    /// seeding it is not optional. The ceiling is the band this stub actually PREPARED -- handing
    /// out memory it never cleared would seed live objects from power-on garbage, which on a
    /// mark-compact heap is a wrong ROOT and not merely a wrong value.
    const HEAP_LIMIT: u32 = ZERO_END;

    let entry_addr = (CODE_BASE + entry_offset) | 1;
    const FAULT_OFF: u32 = 16 * 4;
    const STUB_OFF: u32 = FAULT_OFF + 2;
    let mut enc = Encoder::new();
    enc.emit_word(SP_TOP);
    enc.emit_word((RP2040_VECTOR_BASE + STUB_OFF) | 1);
    for _ in 2..16 {
        enc.emit_word((RP2040_VECTOR_BASE + FAULT_OFF) | 1);
    }
    debug_assert_eq!(enc.position(), FAULT_OFF);
    let fault = enc.new_label();
    enc.bind_label(fault);
    enc.b(fault);
    debug_assert_eq!(enc.position(), STUB_OFF);

    let vtor_word = enc.new_label();
    let vectors_word = enc.new_label();
    let zero_start_word = enc.new_label();
    let zero_end_word = enc.new_label();
    let heap_ptr_word = enc.new_label();
    let heap_base_word = enc.new_label();
    let heap_limit_word = enc.new_label();
    let boot_magic_word = enc.new_label();
    let done_magic_word = enc.new_label();
    let result_word = enc.new_label();
    let result_hi_word = enc.new_label();
    let entry_word = enc.new_label();
    enc.ldr_literal(Reg::R0, vectors_word).unwrap();
    enc.ldr_literal(Reg::R1, vtor_word).unwrap();
    enc.str_imm(Reg::R0, Reg::R1, 0).unwrap();
    enc.movs_imm(Reg::R0, 0).unwrap();
    enc.ldr_literal(Reg::R1, zero_start_word).unwrap();
    enc.ldr_literal(Reg::R2, zero_end_word).unwrap();
    let zero_loop = enc.new_label();
    enc.bind_label(zero_loop);
    enc.str_imm(Reg::R0, Reg::R1, 0).unwrap();
    enc.adds_imm8(Reg::R1, 4).unwrap();
    enc.cmp_reg(Reg::R1, Reg::R2).unwrap();
    enc.b_cond(Cond::CarryClear, zero_loop);
    enc.ldr_literal(Reg::R0, heap_base_word).unwrap();
    enc.ldr_literal(Reg::R1, heap_ptr_word).unwrap();
    enc.str_imm(Reg::R0, Reg::R1, 0).unwrap();
    enc.ldr_literal(Reg::R0, heap_limit_word).unwrap();
    enc.str_imm(Reg::R0, Reg::R1, 4).unwrap();
    enc.ldr_literal(Reg::R0, boot_magic_word).unwrap();
    enc.ldr_literal(Reg::R1, result_word).unwrap();
    enc.str_imm(Reg::R0, Reg::R1, 0).unwrap();
    enc.ldr_literal(Reg::R0, entry_word).unwrap();
    enc.blx(Reg::R0);
    enc.ldr_literal(Reg::R1, result_hi_word).unwrap();
    enc.str_imm(Reg::R0, Reg::R1, 0).unwrap();
    enc.ldr_literal(Reg::R0, done_magic_word).unwrap();
    enc.ldr_literal(Reg::R1, result_word).unwrap();
    enc.str_imm(Reg::R0, Reg::R1, 0).unwrap();
    let park = enc.new_label();
    enc.bind_label(park);
    enc.b(park);
    enc.align_to_word();
    enc.bind_label(vtor_word);
    enc.emit_word(SCB_VTOR);
    enc.bind_label(vectors_word);
    enc.emit_word(RP2040_VECTOR_BASE);
    enc.bind_label(zero_start_word);
    enc.emit_word(ZERO_START);
    enc.bind_label(zero_end_word);
    enc.emit_word(ZERO_END);
    enc.bind_label(heap_ptr_word);
    enc.emit_word(HEAP_PTR);
    enc.bind_label(heap_base_word);
    enc.emit_word(HEAP_BASE);
    enc.bind_label(heap_limit_word);
    enc.emit_word(HEAP_LIMIT);
    enc.bind_label(boot_magic_word);
    enc.emit_word(RP2040_BOOT_MAGIC);
    enc.bind_label(done_magic_word);
    enc.emit_word(RP2040_DONE_MAGIC);
    enc.bind_label(result_word);
    enc.emit_word(RP2040_RESULT_ADDR);
    enc.bind_label(result_hi_word);
    enc.emit_word(RP2040_RESULT_ADDR + 4);
    enc.bind_label(entry_word);
    enc.emit_word(entry_addr);
    let pad = ((CODE_BASE - RP2040_VECTOR_BASE) - enc.position()) as usize;
    enc.emit_bytes(&vec![0u8; pad]);
    enc.emit_bytes(code);
    let body = enc.finish().unwrap().bytes;

    let mut image = Vec::with_capacity(RP2040_BOOT2_BYTES + body.len());
    image.extend_from_slice(&rp2040_boot2());
    image.extend_from_slice(&body);
    image
}

/// Compiles a CIL assembly to ONE ARM/Thumb relocatable ELF object through the RELOCATING path
/// ([`arm32::lower_object`]): every method becomes a `STT_FUNC` symbol named `f<rid>` (so `f0` is the
/// startup -> `.cctor`s -> `Main`), cross-method calls become `R_ARM_THM_CALL` relocations, and any
/// soft-float helper a float op needs is an undefined `__aeabi_*` extern. A linker turns this into a
/// runnable image -- the bare-metal [`build_cortex_m`] resolves everything itself into a flat blob,
/// whereas this object path carries the call graph + the relocation-dependent features (float,
/// function pointers, native calls) the linker resolves. Emitting the object stays linker-free (the
/// driver/examples own the link step); the `hosted-csharp-arm` example links + runs the result.
#[cfg(feature = "arm32")]
pub fn build_object(cil: &[u8]) -> Result<Vec<u8>, BuildError> {
    build_object_inner(cil, None, &[])
}

/// As [`build_object`], but with the REFERENCED assembly (corlib) attached for cross-assembly
/// vtable-slot agreement: a program type extending a referenced base numbers its slots INCLUDING the
/// base's inherited virtuals (as the referenced assembly numbers them itself), an inherited slot is an
/// extern vtable entry the linker resolves against [`build_library_object`]'s export of it, and a
/// `callvirt` naming a referenced method (a `MemberRef`, e.g. `object.GetHashCode()` on a base-typed
/// receiver) dispatches through that shared slot instead of static-devirtualizing.
#[cfg(feature = "arm32")]
pub fn build_object_with_corlib(cil: &[u8], corlib: &[u8]) -> Result<Vec<u8>, BuildError> {
    build_object_inner(cil, Some(corlib), &[])
}

/// As [`build_object_with_corlib`], but the object also CARRIES DWARF for its own code: a
/// `.debug_line` program plus a `.debug_info`/`.debug_abbrev` compilation unit, relocated against
/// the object's function symbols so the linker supplies their virtual addresses.
///
/// This is the path a DEBUGGABLE DEVICE IMAGE comes from -- link the result and the debugger can map
/// a device PC to a C# file, line and function. `pdb` is the program's Portable PDB (what `lcsc`
/// writes beside the assembly); its sequence points are resolved here, up front, so the code
/// generator never needs the metadata layer.
///
/// **IT IS THE SAME BUILD, WITH DWARF ADDED.** This runs the same object build
/// [`build_object_with_corlib`] runs and asks it for debug info; it is not a second lowering of the
/// same program. Debug info describing a DIFFERENT build than the one flashed would be worse than
/// none, because every address in it would resolve and be wrong.
#[cfg(feature = "arm32")]
pub fn build_object_with_corlib_debug(
    cil: &[u8],
    corlib: &[u8],
    pdb: &lamella_metadata::PortablePdb,
) -> Result<Vec<u8>, BuildError> {
    build_object_core(cil, Some(corlib), &[], false, false, Some(pdb)).map(|(bytes, _)| bytes)
}

/// The PDB-derived text a compilation unit names, OWNED -- [`crate::debugmap::MethodSource`] borrows
/// its file, display name and sequence points, so they need a home that outlives the borrow.
#[cfg(feature = "arm32")]
struct MethodSources {
    files: Vec<alloc::string::String>,
    display: Vec<alloc::string::String>,
    points: Vec<Vec<(u32, u32, u32)>>,
}

#[cfg(feature = "arm32")]
impl MethodSources {
    /// Resolves every method's sequence points ONCE, up front, where the PDB is. `funcs` is
    /// rid-indexed (index 0 is the entry trampoline), so each method's points land at its own index;
    /// a method the PDB says nothing about keeps an empty entry and is simply not described.
    fn resolve(pdb: &lamella_metadata::PortablePdb, assembly: &Assembly, count: usize) -> Self {
        let mut display: Vec<alloc::string::String> =
            alloc::vec![alloc::string::String::new(); count];
        for type_def in assembly.type_defs() {
            let type_name = type_def.name().map_or("", |n| n.name);
            for method in type_def.methods() {
                if let Some(slot) = display.get_mut(method.rid() as usize) {
                    *slot = alloc::format!("{type_name}.{}", method.name().unwrap_or("?"));
                }
            }
        }
        Self {
            files: (0..count)
                .map(|rid| pdb.method_document(rid as u32).unwrap_or_default())
                .collect(),
            display,
            points: (0..count)
                .map(|rid| {
                    pdb.sequence_points(rid as u32)
                        .into_iter()
                        .filter(|p| !p.is_hidden)
                        .map(|p| (p.il_offset, p.start_line, p.start_column))
                        .collect()
                })
                .collect(),
        }
    }

    fn rows(&self) -> Vec<crate::debugmap::MethodSource<'_>> {
        (0..self.files.len())
            .map(|i| crate::debugmap::MethodSource {
                name: self.display[i].as_str(),
                file: self.files[i].as_str(),
                points: self.points[i].as_slice(),
            })
            .collect()
    }

    /// The compilation unit's name: the primary source file, i.e. the first method that has one.
    fn unit_name(&self) -> &str {
        self.files
            .iter()
            .find(|f| !f.is_empty())
            .map_or("", alloc::string::String::as_str)
    }
}

/// The `DW_AT_producer` string stamped into every compilation unit this backend emits.
#[cfg(feature = "arm32")]
const PRODUCER: &str = "Lamella AOT";

/// As [`build_object_with_corlib`], but with FURTHER referenced library assemblies -- the
/// multi-assembly deploy shape (program + corlib + e.g. a BSP or `System.Net.NetworkInformation`).
/// The whole ordered list `[corlib, libraries...]` flows: the startup chains every reference's
/// `.cctor`s (corlib's first, then the libraries in the given order), cross-assembly NAMES resolve
/// against the list first-declarer-wins, descriptor identity is assembly-qualified per reference,
/// and a cross-assembly `ldsfld`/`stsfld` lands on its OWNER's region symbol at the owner's slot.
/// Remaining N-reference gap: a program type EXTENDING a reference's type sees the base's
/// visible-portion-only field layout (the cross-assembly base-chain slice), so keep such
/// hierarchies single-assembly for now.
#[cfg(feature = "arm32")]
pub fn build_object_with_libraries(
    cil: &[u8],
    corlib: &[u8],
    libraries: &[&[u8]],
) -> Result<Vec<u8>, BuildError> {
    build_object_inner(cil, Some(corlib), libraries)
}

/// As [`build_object_with_libraries`], but DEFERRING instead of failing: a method whose body
/// fails CIL->MIR becomes an `Unreachable` trap body, one that fails object-scale encoding
/// becomes a `udf` trap stub, and the report lists both sets `(rid, name, why)` for the build
/// to PRINT -- deferral is never silent. The single-assembly device-demo bake (app + BSP +
/// System.Device sources compiled as ONE program assembly) is the customer: library-grade
/// surface rides along that the program never calls, gc-sections drops it unreached, and a
/// reached deferred method faults loud at its exact call site. `wide` targets a Mainline (M33)
/// part -- a far branch/`adr` relaxes to `B.W`/`ADR.W`; on a v6-M target (`false`) it splices a
/// literal-pool veneer instead. Either way a far reference ENCODES rather than deferring; the two
/// paths are byte-identical for a method with no out-of-reach reference.
#[cfg(feature = "arm32")]
pub fn build_object_with_libraries_report(
    cil: &[u8],
    corlib: &[u8],
    libraries: &[&[u8]],
    wide: bool,
) -> Result<(Vec<u8>, LibraryBuildReport), BuildError> {
    build_object_core(cil, Some(corlib), libraries, true, wide, None)
}

#[cfg(feature = "arm32")]
fn build_object_inner(
    cil: &[u8],
    corlib: Option<&[u8]>,
    libraries: &[&[u8]],
) -> Result<Vec<u8>, BuildError> {
    build_object_core(cil, corlib, libraries, false, false, None).map(|(bytes, _)| bytes)
}

#[cfg(feature = "arm32")]
fn build_object_core(
    cil: &[u8],
    corlib: Option<&[u8]>,
    libraries: &[&[u8]],
    defer: bool,
    wide: bool,
    pdb: Option<&lamella_metadata::PortablePdb>,
) -> Result<(Vec<u8>, LibraryBuildReport), BuildError> {
    let assembly = read_assembly(cil)?;
    let reference = match corlib {
        Some(bytes) => Some(read_assembly(bytes)?),
        None => None,
    };
    let entry = find_main(&assembly);
    let library_assemblies: Vec<Assembly> = libraries
        .iter()
        .map(|lib| read_assembly(lib))
        .collect::<Result<_, _>>()?;
    let references: Vec<&Assembly> = reference.iter().chain(library_assemblies.iter()).collect();
    let qualifiers = arm32::DescQualifiers {
        string: MetadataResolver::new(&assembly)
            .with_references(&references)
            .string_type_meta()
            .map(|m| m.handle.0),
        own: None,
        references: corlib
            .iter()
            .copied()
            .chain(libraries.iter().copied())
            .map(|bytes| alloc::format!("{:08x}", lamella_metadata::fnv1a32(0x811c_9dc5, bytes)))
            .collect(),
    };
    let (mut funcs, maps, cil_fails, seams, duplicates, _thunks, plan) =
        lower_assembly_seams(&assembly, entry, &references)?;
    refuse_duplicate_bodies(&duplicates)?;
    let cil_fail_rows: Vec<(u32, cil::CilError)> = if defer {
        for (rid, _) in &cil_fails {
            funcs[*rid as usize] = deferred_trap_body();
        }
        cil_fails
    } else {
        if let Some((rid, error)) = cil_fails.into_iter().next() {
            return Err(BuildError::LowerCil { rid, error });
        }
        Vec::new()
    };
    if let (Some(entry_rid), Some(reference), Some(bytes)) = (entry, reference.as_ref(), corlib) {
        let mut reference_cctors: Vec<alloc::string::String> = Vec::new();
        let mut chain = |bytes: &[u8], assembly: &Assembly| {
            let prefix = alloc::format!("L{:08x}.", lamella_metadata::fnv1a32(0x811c_9dc5, bytes));
            reference_cctors.extend(
                reference_startup_cctors(assembly)
                    .into_iter()
                    .map(|rid| alloc::format!("{prefix}f{rid}")),
            );
        };
        chain(bytes, reference);
        for (lib_bytes, lib_assembly) in libraries.iter().zip(&library_assemblies) {
            chain(lib_bytes, lib_assembly);
        }
        funcs[0] = startup_with_references(
            find_native_export(&assembly, "lamella_time_init"),
            &reference_cctors,
            &startup_cctors(&assembly, &references),
            entry_rid,
        );
    }
    let resolver = MetadataResolver::new(&assembly)
        .with_references(&references)
        .with_monomorphized(plan);
    let mut descriptors = resolver.image_descriptors();
    let mut names = object_symbol_names(&assembly, funcs.len());
    names.extend(append_enum_to_string(
        &assembly,
        &resolver,
        &mut funcs,
        &mut descriptors,
        "",
    ));
    replace_exception_message(&assembly, &mut funcs);
    let funcs = funcs;
    let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let display_names = method_display_names(&assembly, funcs.len());
    let mut silent_edges = silent_seam_call_edges(&assembly, &funcs, &seams, &display_names);
    let imports = pinvoke_imports(&funcs);
    let import_names: Vec<&str> = imports.iter().map(|(name, _)| name.as_str()).collect();
    for (ordinal, reference) in references.iter().enumerate() {
        for (rid, seam, symbol) in
            imported_silent_seams(reference, &references[..ordinal], &import_names)
        {
            let caller_rid = imports
                .iter()
                .find(|(name, _)| *name == symbol)
                .map_or(0, |(_, caller)| *caller);
            silent_edges.push(SeamCallEdge {
                caller_rid,
                caller: display_names
                    .get(caller_rid as usize)
                    .cloned()
                    .flatten()
                    .unwrap_or_else(|| alloc::format!("f{caller_rid}")),
                seam_rid: rid,
                seam,
            });
        }
    }
    if !defer {
        if let Some(edge) = silent_edges.first() {
            return Err(BuildError::SilentSeamCallEdge {
                caller: edge.caller.clone(),
                seam: edge.seam.clone(),
                total: silent_edges.len(),
            });
        }
    }
    append_reference_descriptors(&funcs, &resolver, &mut descriptors);
    let statics = assembly_statics(cil, &assembly, true, resolver.monomorphized(), resolver.references());
    if !defer {
        let sources = pdb.map(|pdb| MethodSources::resolve(pdb, &assembly, funcs.len()));
        let rows = sources.as_ref().map(MethodSources::rows);
        let bytes = match (&sources, &rows) {
            (Some(sources), Some(rows)) => arm32::lower_object_vtables_statics_debug(
                &funcs,
                &name_refs,
                &[],
                &descriptors,
                &statics,
                &qualifiers,
                &crate::debugmap::ObjectDebug {
                    source_maps: &maps,
                    methods: rows,
                    unit_name: sources.unit_name(),
                    producer: PRODUCER,
                },
            )
            .map(|(bytes, _)| bytes),
            _ => arm32::lower_object_vtables_statics(
                &funcs,
                &name_refs,
                &[],
                &descriptors,
                &statics,
                &qualifiers,
            ),
        }
        .map_err(BuildError::LowerArm)?;
        return Ok((bytes, LibraryBuildReport::default()));
    }
    let (bytes, emit_stubs) = arm32::lower_object_vtables_statics_report(
        &funcs,
        &name_refs,
        &[],
        &descriptors,
        &statics,
        &qualifiers,
        wide,
    )
    .map_err(BuildError::LowerArm)?;
    let name_of = |rid: usize| {
        display_names
            .get(rid)
            .cloned()
            .flatten()
            .unwrap_or_else(|| alloc::format!("f{rid}"))
    };
    let report = LibraryBuildReport {
        cil_fails: cil_fail_rows
            .into_iter()
            .map(|(rid, error)| (rid, name_of(rid as usize), alloc::format!("{error:?}")))
            .collect(),
        emit_stubs: emit_stubs
            .into_iter()
            .map(|(index, error)| (index as u32, name_of(index), alloc::format!("{error:?}")))
            .collect(),
        unsynthesized_seams: unsynthesized_seam_rows(
            &assembly,
            &seams,
            &display_names,
            &names,
            &vtable_slot_rids(&resolver),
        ),
        silent_seam_edges: silent_edges,
    };
    Ok((bytes, report))
}

/// Appends the REFERENCE-OWNED descriptors a lowered module mentions but this assembly does not
/// declare, each with the OWNER's rich meta (payload + cross-assembly vtable + base chain, via
/// [`MetadataResolver::reference_type_meta`]). An `Alloc` of a referenced class needs it so the
/// object's `obj-4` dispatches the owner's overrides -- and a cast/box/unbox identity
/// (`TypeDescAddr`, feeding the address compares and `CastClassScan`) needs it just as much: with
/// cast and box handles now OWNER-QUALIFIED, an object that only casts against a referenced type
/// would otherwise lay a MINIMAL descriptor under the type's canonical qualified name and -- in a
/// program, where descriptors are STRONG -- clobber the owner's rich WEAK copy at the link's
/// dedupe, breaking every virtual dispatch through it. Array and unresolved handles pass through
/// untouched (`reference_type_meta` only answers for reference-owned handles).
#[cfg(any(feature = "arm32", feature = "riscv32", feature = "wasm"))]
fn append_reference_descriptors(
    funcs: &[Function],
    resolver: &MetadataResolver,
    descriptors: &mut Vec<crate::resolver::TypeMeta>,
) {
    let mut added: Vec<u32> = Vec::new();
    let array_base = resolver.system_array_meta();
    if funcs.iter().any(|f| {
        f.blocks.iter().flat_map(|b| &b.insts).any(|(_, i)| {
            matches!(
                i,
                Inst::StringLiteral { .. } | Inst::StringConcat { .. } | Inst::IntToString { .. }
            )
        })
    }) {
        if let Some(meta) = resolver.string_type_meta() {
            if !descriptors.iter().any(|d| d.handle == meta.handle) {
                added.push(meta.handle.0);
                descriptors.push(meta);
            }
        }
    }
    for func in funcs {
        for block in &func.blocks {
            for (_, inst) in &block.insts {
                if let (Inst::AllocArray { handle, .. }, Some(base)) = (inst, array_base.as_ref()) {
                    let known = descriptors.iter().any(|d| d.handle == *handle)
                        || added.contains(&handle.0);
                    if !known {
                        added.push(handle.0);
                        descriptors.push(crate::resolver::TypeMeta {
                            handle: *handle,
                            type_tag: 0,
                            vtable: base.vtable.clone(),
                            itable: base.itable.clone(),
                            base: Some(base.handle),
                            words: None,
                            exported: false,
                            full_name: None,
                        });
                    }
                }
                let handle = match inst {
                    Inst::Alloc { handle, .. } | Inst::TypeDescAddr { handle } => *handle,
                    Inst::AllocArray {
                        element: Some(handle),
                        ..
                    } => *handle,
                    _ => continue,
                };
                let known =
                    descriptors.iter().any(|d| d.handle == handle) || added.contains(&handle.0);
                if !known {
                    if let Some(meta) = resolver.reference_type_meta(handle) {
                        added.push(handle.0);
                        descriptors.push(meta);
                    }
                }
            }
        }
    }
    let mut i = 0;
    while i < descriptors.len() {
        if let Some(base) = descriptors[i].base {
            let known = descriptors.iter().any(|d| d.handle == base);
            if !known {
                if let Some(meta) = resolver.reference_type_meta(base) {
                    descriptors.push(meta);
                }
            }
        }
        i += 1;
    }
}

/// Synthesizes a `ToString()` for every enum this assembly declares and points that enum's ToString
/// vtable slot at it, returning one symbol name per appended function (the caller extends its symbol
/// table by them, in the same order).
///
/// WHY THERE IS ANYTHING TO SYNTHESIZE. `Console.WriteLine(e)` on an enum is `box` then
/// `WriteLine(object)` then a virtual `ToString()`, so by the time the call happens the enum's static
/// type is gone and only the receiver's descriptor can answer. Corlib's `System.Enum` deliberately
/// declares no parameterless `ToString` -- its comment says the call "arrives as a `constrained.
/// callvirt object::ToString()`, which the VES renders in place" -- which is true of the interpreter
/// and has no counterpart here, because until this pass runs NO ENUM MEMBER NAME EXISTS ANYWHERE IN
/// AN AOT IMAGE. The slot therefore held the synthesized `Object::ToString`, which answers the
/// descriptor's NAME word, and every enum printed as its own TYPE name (`Color` for `Color.Green`).
/// That is correct .NET behaviour for a type that overrides nothing -- the defect is the missing
/// override, and this is it. A call-site rewrite cannot serve: the token is present only at the
/// statically-typed `c.ToString()`, and every print goes through the object-typed path instead.
///
/// The member names and values are compile-time constants (`Field` rows with a `Constant`), so the
/// body is a compare chain over the boxed payload returning a literal per member -- no runtime table,
/// no metadata on device, and nothing for the collector to trace.
///
/// THE SLOT IS FOUND BY NAME, never by index ([`MetadataResolver::nullary_vtable_slot`]). An enum's
/// vtable is `System.Object`'s three slots whenever `ValueType` and `Enum` declare no virtuals of
/// their own -- but a body placed at a guessed index would be a wrong method under a right name, and a
/// test that only reads the printed string would pass on it.
///
/// SCOPE, stated because each limit shows up as a printed answer rather than an error:
/// - ENUMS THIS ASSEMBLY DECLARES. A referenced assembly's enum (`DayOfWeek`) reaches its consumer as
///   a descriptor whose slots are EXTERNS into the owner -- naming a per-enum export there is its own
///   change, and this one does not pretend to it.
/// - THE NUMERIC FALLBACK is emitted only where an unmatched value renders EXACTLY as a signed 32-bit
///   decimal, which is every underlying type but `uint`, `long` and `ulong` ([`enum_numeric_fallback`]).
///   Those three keep the type name for an unmatched value -- what a non-overriding type answers with
///   -- rather than a plausible wrong number; the backend has no unsigned-32 or 64-bit integer
///   formatter to call, and adding one is a separate change with its own proof.
#[cfg(any(feature = "arm32", feature = "riscv32"))]
fn append_enum_to_string(
    assembly: &Assembly,
    resolver: &MetadataResolver,
    funcs: &mut Vec<Function>,
    descriptors: &mut [crate::resolver::TypeMeta],
    prefix: &str,
) -> Vec<alloc::string::String> {
    let target = TargetLayout::ilp32();
    let mut names = Vec::new();
    for type_def in assembly.type_defs() {
        let token = type_def.token();
        let Some(underlying) = crate::resolver::enum_underlying(assembly, token, &[], &target) else {
            continue;
        };
        let members = enum_members(&type_def);
        if members.is_empty() {
            continue;
        }
        let Some(meta) = descriptors
            .iter_mut()
            .find(|d| d.handle == TypeHandle(token.0))
        else {
            continue;
        };
        let Some(slot) = resolver.nullary_vtable_slot(type_def, "ToString") else {
            continue;
        };
        let Some(entry) = meta.vtable.get_mut(slot) else {
            continue;
        };
        let fallback = enum_numeric_fallback(&type_def, underlying);
        let flags = underlying == MirType::I32
            && assembly.has_attribute(token, "System", "FlagsAttribute");
        let index = funcs.len() as u32;
        funcs.push(if flags {
            enum_flags_to_string_body(&members, fallback)
        } else {
            enum_to_string_body(&members, underlying, fallback)
        });
        *entry = crate::resolver::VtableEntry::Func(index);
        names.push(alloc::format!(
            "{prefix}__lamella_enum_tostring_{:08x}",
            token.0
        ));
    }
    names
}

/// Replaces `System.Exception::get_Message`'s BODY with the one `exception_strings = type-name`
/// specifies: the receiver's own type name.
///
/// WHY THERE IS ANYTHING TO REPLACE. The in-flight exception is a TAG, so a caught exception is
/// materialized with a ZEROED payload -- no constructor ran, and under this knob none can, because
/// `throw` keeps costing one word. Corlib's own `Message` therefore reads a null `_message` and hands
/// back null, and `"caught " + e.Message` then faults inside [`Inst::StringConcat`] on the null
/// operand. That is the fault the object model would otherwise MOVE rather than fix: the binding stops
/// hard-faulting on dispatch and starts hard-faulting one instruction later.
///
/// WHY IT REPLACES A BODY INSTEAD OF APPENDING ONE AND REPOINTING SLOTS. A consumer of a REFERENCED
/// exception type builds its own copy of that type's descriptor, and the copy's vtable slots are
/// EXTERNS read from the OWNER's metadata -- not from the owner's patched [`crate::resolver::TypeMeta`].
/// So repointing corlib's slot leaves every program's `catch (ArgumentException e)` dispatching to
/// corlib's ORIGINAL getter, which is exactly the named gap [`append_enum_to_string`] carries for a
/// referenced enum. Replacing the body makes the extern symbol itself resolve to the new behavior, so
/// one edit serves corlib and every assembly that links it, with no new symbol to collide.
///
/// ONE BODY SERVES EVERY EXCEPTION TYPE. It is `LoadTypeDesc(this)` then [`Inst::TypeName`] -- the same
/// pair the synthesized `Object::ToString` uses -- so it answers the RECEIVER's descriptor name rather
/// than a baked literal. Corlib declares 43 exception types and NONE of them overrides `Message`, so
/// all 43 inherit this one slot and each still reports its own name.
///
/// SCOPE, stated because each limit is a printed answer rather than an error:
/// - **The message text of `throw new E("boom")` is not retained on this tier.** This returns `E`'s name, which
///   is what `exception_strings = type-name` means and is closer to .NET than the null it replaces
///   (.NET's own message-less default is "Exception of type 'E' was thrown.").
/// - **`_message` is not consulted, because on this tier it can never be set.** No constructor runs at
///   a throw, and a `newobj E` that is NOT thrown is still lowered to a tag rather than an object. When
///   that second case becomes a real allocation, this body wants a `_message != null` arm ahead of the
///   type name -- corlib's field is at payload offset 0, since `System.Exception` is the base-most block
///   of every exception layout.
/// - A program that OVERRIDES `Message` keeps its own body: this only rewrites the declaring type's.
#[cfg(any(feature = "arm32", feature = "riscv32"))]
fn replace_exception_message(assembly: &Assembly, funcs: &mut [Function]) {
    let Some(type_def) = assembly.find_type("System", "Exception") else {
        return;
    };
    for method in type_def.methods() {
        if method.name() != Some("get_Message") {
            continue;
        }
        let rid = method.token().row() as usize;
        if let Some(slot) = funcs.get_mut(rid) {
            *slot = exception_message_body();
        }
    }
}

/// The MIR for the shared exception `Message` getter: the receiver's own type name.
///
/// Three instructions, and none of them touches a field -- which is what lets one function serve every
/// exception type. [`Inst::TypeName`] answers null for a null descriptor rather than dereferencing, so
/// the body is total even on a receiver whose header was never written.
#[cfg(any(feature = "arm32", feature = "riscv32"))]
fn exception_message_body() -> Function {
    let objt = MirType::ObjectRef;
    let (mut mb, params) = MirBuilder::new(&[objt]);
    let object = params[0];
    mb.at(0);
    let descriptor = mb.emit(MirType::I32, Inst::LoadTypeDesc { object });
    let text = mb.emit(objt, Inst::TypeName { descriptor });
    mb.ret(text);
    mb.finish(Some(objt))
}

/// An enum's members in DECLARATION order -- its static fields carrying a `Constant`, which is every
/// field but the instance `value__` that holds the underlying storage.
///
/// Declaration order is what makes a DUPLICATE value answer the way .NET does: `Enum.GetName` reads a
/// by-value sort of the same rows, and a stable sort leaves equal values in metadata order, so the
/// FIRST-DECLARED name wins there and in the compare chain below.
#[cfg(any(feature = "arm32", feature = "riscv32"))]
fn enum_members(type_def: &lamella_metadata::TypeDef) -> Vec<(alloc::string::String, i64)> {
    let mut members = Vec::new();
    for field in type_def.fields() {
        if !field.is_static() {
            continue;
        }
        let (Some(name), Some(constant)) = (field.name(), field.constant()) else {
            continue;
        };
        let value = match constant {
            lamella_metadata::ConstantValue::Bool(v) => i64::from(v),
            lamella_metadata::ConstantValue::Char(v) => i64::from(v),
            lamella_metadata::ConstantValue::I1(v) => i64::from(v),
            lamella_metadata::ConstantValue::U1(v) => i64::from(v),
            lamella_metadata::ConstantValue::I2(v) => i64::from(v),
            lamella_metadata::ConstantValue::U2(v) => i64::from(v),
            lamella_metadata::ConstantValue::I4(v) => i64::from(v),
            lamella_metadata::ConstantValue::U4(v) => i64::from(v),
            lamella_metadata::ConstantValue::I8(v) => v,
            lamella_metadata::ConstantValue::U8(v) => v as i64,
            _ => continue,
        };
        members.push((alloc::string::String::from(name), value));
    }
    members
}

/// Whether an UNMATCHED value of this enum renders exactly as a signed 32-bit decimal -- the only
/// integer formatting the backend can emit inline ([`Inst::IntToString`]).
///
/// True for every underlying type whose whole range fits in `i32`: `bool`, `char`, `sbyte`, `byte`,
/// `short`, `ushort`, `int`. False for `uint` (values at or above 2^31 would print negative) and for
/// `long`/`ulong` (which do not fit at all). A false answer does not stop the member names being
/// synthesized -- only the no-match arm changes, and it keeps the type name a non-overriding type
/// already answers with, rather than a number that is right for most values and silently wrong for
/// the rest.
#[cfg(any(feature = "arm32", feature = "riscv32"))]
fn enum_numeric_fallback(type_def: &lamella_metadata::TypeDef, underlying: MirType) -> bool {
    if underlying != MirType::I32 {
        return false;
    }
    !matches!(
        type_def
            .fields()
            .find(|field| !field.is_static())
            .and_then(|field| field.signature()),
        Some(SigType::U4)
    )
}

/// The MIR for one enum's `ToString()`: read the boxed payload, compare it against each member's
/// constant in declaration order, and return that member's name as a string literal.
///
/// The payload is at offset 0 of the object -- `box` lowers to an `Alloc` plus
/// `FieldStore { offset: 0 }`, so this is the same read `unbox.any` makes, at the same width (the
/// enum's underlying [`MirType`], so a `long`-backed enum compares both words).
///
/// With no member matching, `numeric` decides the arm: [`Inst::IntToString`] of the payload, which IS
/// .NET's rule for a non-`[Flags]` enum; or, where that would not render exactly, the receiver's type
/// name -- the same `LoadTypeDesc` + `TypeName` pair the synthesized `Object::ToString` uses, so the
/// unrenderable case keeps precisely the answer it has today instead of gaining a new wrong one.
#[cfg(any(feature = "arm32", feature = "riscv32"))]
fn enum_to_string_body(
    members: &[(alloc::string::String, i64)],
    underlying: MirType,
    numeric: bool,
) -> Function {
    let objt = MirType::ObjectRef;
    let (mut mb, params) = MirBuilder::new(&[objt]);
    let object = params[0];
    let hits: Vec<usize> = members.iter().map(|_| mb.block()).collect();
    let tests: Vec<usize> = members.iter().skip(1).map(|_| mb.block()).collect();
    let none = mb.block();

    mb.at(0);
    let value = mb.emit(
        underlying,
        Inst::FieldLoad {
            base: object,
            offset: 0,
        },
    );
    for (index, (_, constant)) in members.iter().enumerate() {
        if index > 0 {
            mb.at(tests[index - 1]);
        }
        let expected = mb.emit(
            underlying,
            Inst::ConstInt {
                ty: underlying,
                value: narrow(*constant, underlying),
            },
        );
        let equal = mb.emit(
            MirType::I32,
            Inst::Compare {
                op: CmpOp::Eq,
                lhs: value,
                rhs: expected,
            },
        );
        let otherwise = tests.get(index).copied().unwrap_or(none);
        mb.branch(equal, hits[index], otherwise);
    }
    for (index, (name, _)) in members.iter().enumerate() {
        mb.at(hits[index]);
        let literal = mb.emit(objt, string_literal(name));
        mb.ret(literal);
    }

    mb.at(none);
    if numeric {
        let text = mb.emit(objt, Inst::IntToString { value });
        mb.ret(text);
    } else {
        let descriptor = mb.emit(MirType::I32, Inst::LoadTypeDesc { object });
        let text = mb.emit(objt, Inst::TypeName { descriptor });
        mb.ret(text);
    }
    mb.finish(Some(objt))
}

/// The MIR for a `[Flags]` enum's `ToString()`: .NET's `InternalFlagsFormat`, unrolled over the
/// members this build already knows.
///
/// The algorithm is theirs, walked step for step because the composite cases only agree if it is:
/// sort the members by value ASCENDING (stably, so equal values keep declaration order), walk from
/// the TOP down, and take a member whose bits are all still present -- subtracting them, so a
/// composite member (`ab = 3` beside `a = 1`, `b = 2`) consumes its parts and they do not appear
/// again. The smallest member is skipped when it is zero, which is .NET's `break` and the reason a
/// `None = 0` never joins a non-zero rendering. Names are PREPENDED, so walking down yields
/// ascending output: `A | C` is `"A, C"`.
///
/// Three exits, and each one is .NET's:
/// - bits left over that no member claims -> the number (or, where that cannot render, the type
///   name -- see [`append_enum_to_string`]);
/// - the value was zero -> the zero member's name if the enum declares one, else the literal `"0"`;
/// - otherwise the joined names.
///
/// The separator rides INSIDE each member's second literal (`"Name, "`), so a join is one
/// concatenation rather than two, and the `acc == null` test is what distinguishes the first name
/// taken from the rest.
///
/// I32 UNDERLYING ONLY; the caller keeps a 64-bit `[Flags]` enum on the plain exact-match chain. The
/// decomposition needs `And`/`Sub` on the payload, and this build has no proof those lower on both
/// backends at 64 bits -- an untested arm here would trade a wrong string for a refused BUILD.
#[cfg(any(feature = "arm32", feature = "riscv32"))]
fn enum_flags_to_string_body(members: &[(alloc::string::String, i64)], numeric: bool) -> Function {
    let i32t = MirType::I32;
    let objt = MirType::ObjectRef;
    let mut ordered: Vec<&(alloc::string::String, i64)> = members.iter().collect();
    ordered.sort_by_key(|(_, value)| *value as u64);
    let zero_name = ordered
        .first()
        .filter(|(_, value)| *value == 0)
        .map(|(name, _)| name.as_str());
    let walk: Vec<&(alloc::string::String, i64)> = ordered
        .iter()
        .skip(usize::from(zero_name.is_some()))
        .rev()
        .copied()
        .collect();

    let (mut mb, params) = MirBuilder::new(&[objt]);
    let object = params[0];
    let zero_case = mb.block();
    let start = mb.block();
    let steps: Vec<usize> = walk.iter().map(|_| mb.block()).collect();
    let takes: Vec<usize> = walk.iter().map(|_| mb.block()).collect();
    let firsts: Vec<usize> = walk.iter().map(|_| mb.block()).collect();
    let mores: Vec<usize> = walk.iter().map(|_| mb.block()).collect();
    let skips: Vec<usize> = walk.iter().map(|_| mb.block()).collect();
    let done = mb.block();
    let joined = mb.block();
    let leftover = mb.block();
    let first_step = steps.first().copied().unwrap_or(done);

    mb.at(0);
    let value = mb.emit(
        i32t,
        Inst::FieldLoad {
            base: object,
            offset: 0,
        },
    );
    let zero = mb.emit(i32t, Inst::ConstInt { ty: i32t, value: 0 });
    let empty = mb.emit(
        objt,
        Inst::Convert {
            value: zero,
            kind: ConvKind::IntToRef,
        },
    );
    let is_zero = mb.emit(
        i32t,
        Inst::Compare {
            op: CmpOp::Eq,
            lhs: value,
            rhs: zero,
        },
    );
    mb.branch(is_zero, zero_case, start);

    mb.at(zero_case);
    let zero_text = mb.emit(objt, string_literal(zero_name.unwrap_or("0")));
    mb.ret(zero_text);

    mb.at(start);
    mb.jump(first_step, alloc::vec![value, empty]);

    for (index, (name, constant)) in walk.iter().enumerate() {
        let next = steps.get(index + 1).copied().unwrap_or(done);
        let state = mb.enter(steps[index], &[i32t, objt]);
        let (rest, text) = (state[0], state[1]);
        let bits = mb.emit(
            i32t,
            Inst::ConstInt {
                ty: i32t,
                value: narrow(*constant, i32t),
            },
        );
        let masked = mb.emit(
            i32t,
            Inst::Binary {
                op: BinOp::And,
                lhs: rest,
                rhs: bits,
            },
        );
        let present = mb.emit(
            i32t,
            Inst::Compare {
                op: CmpOp::Eq,
                lhs: masked,
                rhs: bits,
            },
        );
        mb.branch(present, takes[index], skips[index]);

        mb.at(takes[index]);
        let remaining = mb.emit(
            i32t,
            Inst::Binary {
                op: BinOp::Sub,
                lhs: rest,
                rhs: bits,
            },
        );
        let as_int = mb.emit(
            i32t,
            Inst::Convert {
                value: text,
                kind: ConvKind::RefToInt,
            },
        );
        let nothing_yet = mb.emit(
            i32t,
            Inst::Compare {
                op: CmpOp::Eq,
                lhs: as_int,
                rhs: zero,
            },
        );
        mb.branch(nothing_yet, firsts[index], mores[index]);

        mb.at(firsts[index]);
        let only = mb.emit(objt, string_literal(name));
        mb.jump(next, alloc::vec![remaining, only]);

        mb.at(mores[index]);
        let prefix = mb.emit(objt, string_literal(&alloc::format!("{name}, ")));
        let grown = mb.emit(
            objt,
            Inst::StringConcat {
                lhs: prefix,
                rhs: text,
            },
        );
        mb.jump(next, alloc::vec![remaining, grown]);

        mb.at(skips[index]);
        mb.jump(next, alloc::vec![rest, text]);
    }

    let state = mb.enter(done, &[i32t, objt]);
    let (rest, text) = (state[0], state[1]);
    let all_claimed = mb.emit(
        i32t,
        Inst::Compare {
            op: CmpOp::Eq,
            lhs: rest,
            rhs: zero,
        },
    );
    mb.branch(all_claimed, joined, leftover);

    mb.at(joined);
    mb.ret(text);

    mb.at(leftover);
    if numeric {
        let text = mb.emit(objt, Inst::IntToString { value });
        mb.ret(text);
    } else {
        let descriptor = mb.emit(i32t, Inst::LoadTypeDesc { object });
        let text = mb.emit(objt, Inst::TypeName { descriptor });
        mb.ret(text);
    }
    mb.finish(Some(objt))
}

/// A UTF-16 string literal instruction for `text`.
#[cfg(any(feature = "arm32", feature = "riscv32"))]
fn string_literal(text: &str) -> Inst {
    Inst::StringLiteral {
        utf16: text.encode_utf16().collect::<Vec<u16>>().into_boxed_slice(),
    }
}

/// A member constant re-narrowed to the enum's underlying width, so the emitted `ConstInt` is the
/// bit pattern the boxed payload holds: a `byte` enum's 255 must compare equal to the 255 the box
/// stored, and a `short` enum's -1 to the sign-extended word.
#[cfg(any(feature = "arm32", feature = "riscv32"))]
fn narrow(value: i64, underlying: MirType) -> i64 {
    if underlying == MirType::I64 {
        value
    } else {
        i64::from(value as i32)
    }
}

/// One assembly's [`crate::stackmaps::AssemblyStatics`]: its region-symbol identity (the
/// fnv1a32 of the CIL bytes -- the SAME hash that prefixes a library object's internal symbols, so
/// the two views of one assembly agree), its dense region size, and its GLOBAL-roots record rows
/// -- every ref-typed static field's dense slot (the resolver's [`static_field_slots`] layout; the
/// record and the `ldsfld`/`stsfld` lowering share that one source, or the collector would walk
/// the wrong words). `include_eh_row` adds word 0 for the PROGRAM assembly only: the linker
/// aliases the shared `__lamella_eh_tag` word to the ENTRY object's reserved word 0, and that word
/// holds a type TAG today (an integer; the no-GC exception model), so it is emitted `ManagedPtr`
/// -- the collector range-checks a maybe-heap word and skips a non-heap value, and when an
/// object-carrying exception model lands the same entry covers the in-flight exception reference.
/// A library's word 0 is dead (never aliased, never written), so its record claims no root there.
#[cfg(any(feature = "arm32", feature = "riscv32"))]
fn assembly_statics<'x>(
    cil: &[u8],
    assembly: &'x Assembly<'x>,
    include_eh_row: bool,
    plan: &crate::generics::MonoPlan,
    references: &[&'x Assembly<'x>],
) -> crate::stackmaps::AssemblyStatics {
    let slots = crate::resolver::static_field_slots(assembly, references);
    let mut roots = Vec::new();
    if include_eh_row {
        roots.push(crate::stackmaps::STACKMAP_KIND_MANAGED_PTR << 14);
    }
    let mut ref_rows: alloc::collections::BTreeSet<u32> = alloc::collections::BTreeSet::new();
    for type_def in assembly.type_defs() {
        for field in type_def.fields() {
            let is_ref = matches!(
                field.signature(),
                Some(
                    SigType::Class(_)
                        | SigType::Object
                        | SigType::String
                        | SigType::SzArray(_)
                        | SigType::Array { .. }
                )
            );
            if is_ref {
                ref_rows.insert(field.token().row());
            }
        }
    }
    let mut struct_refs: alloc::collections::BTreeMap<u32, lamella_ir::RefWords> =
        alloc::collections::BTreeMap::new();
    for type_def in assembly.type_defs() {
        for field in type_def.fields() {
            if !field.is_static() || field.is_literal() {
                continue;
            }
            if let Some(Ok(MirType::ValueType { refs, .. })) = field
                .signature()
                .map(|sig| mir_type(&sig, assembly, None, references))
            {
                if !refs.is_empty() {
                    struct_refs.insert(field.token().row(), refs);
                }
            }
        }
    }
    for (row, slot, _) in &slots {
        if *slot < 0x4000 && ref_rows.contains(row) {
            roots.push((*slot as u16) | (crate::stackmaps::STACKMAP_KIND_OBJECT_REF << 14));
        }
        if let Some(refs) = struct_refs.get(row) {
            for offset in refs.offsets() {
                let word = slot + offset / 4;
                if word < 0x4000 {
                    roots.push((word as u16) | (crate::stackmaps::STACKMAP_KIND_OBJECT_REF << 14));
                }
            }
        }
    }
    for (_, _, slot, _, is_reference) in
        crate::resolver::generic_static_slots(assembly, plan, references)
    {
        if slot < 0x4000 && is_reference {
            roots.push((slot as u16) | (crate::stackmaps::STACKMAP_KIND_OBJECT_REF << 14));
        }
    }
    crate::stackmaps::AssemblyStatics {
        suffix: alloc::format!("{:08x}", lamella_metadata::fnv1a32(0x811c_9dc5, cil)),
        region_bytes: crate::resolver::static_region_words(assembly, plan, references) * 4,
        roots,
    }
}

/// Compiles a self-contained CIL assembly to ONE RV32IM relocatable ELF object through the RELOCATING
/// path ([`riscv32::lower_object`]): every reachable method becomes an `f<rid>` `STT_FUNC` symbol
/// (`f0` is the entry trampoline -> `Main`), and each cross-method call becomes an `R_RISCV_CALL_PLT`
/// relocation `lamella_link` resolves. This is the RISC-V twin of the ARM [`build_object`] -- it proves the
/// object path handles real compiler output, and it is the substrate the linked-path bricks (native
/// calls, cross-assembly calls, the descriptor object lane) build on.
///
/// It is REACHABILITY-LIMITED: only methods reachable from `Main` (direct `Call` edges, the `.cctor`s
/// the startup chains, an initialization thunk's trigger sites, and every this-assembly vtable/itable
/// dispatch target) are lowered; every other rid -- notably the implicit `.ctor`, which calls
/// `object::.ctor()` in corlib -- stays a stub. That
/// lets a SELF-CONTAINED program (no `/reference`) build with no external call to resolve, exactly as
/// the flat `lower_module_gc` driver does. Once the cross-assembly `Call` + gc-sections path lands this
/// converges to the lower-all shape of [`build_object`] (the implicit `.ctor` becomes an extern the
/// linker drops when unreached). A reachable method that fails to lower is reported, never silently
/// stubbed. Emitting the object stays linker-free (the driver/examples own the link + boot).
#[cfg(feature = "riscv32")]
pub fn build_object_riscv(cil: &[u8]) -> Result<Vec<u8>, BuildError> {
    build_object_riscv_inner(cil, &[], riscv32::RiscvProfile::Rv32im)
}

/// As [`build_object_riscv`], but for a chosen RISC-V [`RiscvProfile`](riscv32::RiscvProfile). `Rv32ec`
/// restricts the object to the CH32V003's x0-x15 (all-spilled path, `s1` scratch, a0-a5 arguments) and
/// lowers a scalar i32 mul/div/rem to a soft-routine call (`__mulsi3`/`__divsi3`/...), so a purely-i32
/// object carries no M-extension opcodes. int64 mul/div/rem and the array-address multiplies keep the
/// hardware M path, so an object using those runs on an M-capable core (QEMU `virt`).
#[cfg(feature = "riscv32")]
pub fn build_object_riscv_profile(
    cil: &[u8],
    profile: riscv32::RiscvProfile,
) -> Result<Vec<u8>, BuildError> {
    build_object_riscv_inner(cil, &[], profile)
}

/// As [`build_object_riscv`], but with a REFERENCED assembly (a corlib or helper library) attached, so
/// the CIL lowering resolves a cross-assembly `new`/call: the resolver reads the referenced type's field
/// layout (an `Alloc`'s payload size) and numbers cross-assembly vtable slots, and a call to a referenced
/// method becomes an extern the linker binds against [`build_library_object_riscv`]'s export. The RISC-V
/// twin of [`build_object_with_corlib`].
#[cfg(feature = "riscv32")]
pub fn build_object_riscv_with_reference(
    cil: &[u8],
    reference: &[u8],
) -> Result<Vec<u8>, BuildError> {
    build_object_riscv_inner(cil, &[reference], riscv32::RiscvProfile::Rv32im)
}

/// As [`build_object_riscv_with_reference`], but against an ORDERED reference list (the
/// multi-assembly deploy shape: corlib + a library + ...). A `ldsfld` into a reference's static
/// resolves to `StaticOwner::Reference(ordinal)` -- that owner's dense slot in that owner's
/// `__lamella_statics_<ownerhash>` region -- so the ordinals here must match the list the
/// program was COMPILED against. The RISC-V twin of [`build_object_with_libraries`].
#[cfg(feature = "riscv32")]
pub fn build_object_riscv_with_references(
    cil: &[u8],
    references: &[&[u8]],
) -> Result<Vec<u8>, BuildError> {
    build_object_riscv_inner(cil, references, riscv32::RiscvProfile::Rv32im)
}

#[cfg(feature = "riscv32")]
fn build_object_riscv_inner(
    cil: &[u8],
    reference_cils: &[&[u8]],
    profile: riscv32::RiscvProfile,
) -> Result<Vec<u8>, BuildError> {
    let assembly = read_assembly(cil)?;
    let reference_assemblies: Vec<Assembly> = reference_cils
        .iter()
        .map(|bytes| read_assembly(bytes))
        .collect::<Result<_, _>>()?;
    let references: Vec<&Assembly> = reference_assemblies.iter().collect();
    let entry = find_main(&assembly).ok_or(BuildError::NoEntryPoint)?;
    let resolver = MetadataResolver::new(&assembly).with_references(&references);
    let mut descriptors = resolver.type_descriptors();
    let mut reference_cctors: Vec<alloc::string::String> = Vec::new();
    for (bytes, reference) in reference_cils.iter().zip(&reference_assemblies) {
        let prefix = alloc::format!("L{:08x}.", lamella_metadata::fnv1a32(0x811c_9dc5, bytes));
        reference_cctors.extend(
            reference_startup_cctors(reference)
                .into_iter()
                .map(|rid| alloc::format!("{prefix}f{rid}")),
        );
    }
    let (mut funcs, plan) = lower_reachable(&assembly, entry, &references, &reference_cctors)?;
    let resolver = resolver.with_monomorphized(plan);
    descriptors = resolver.type_descriptors();
    descriptors.extend(resolver.instantiation_descriptors());
    let mut names: Vec<alloc::string::String> =
        (0..funcs.len()).map(|i| alloc::format!("f{i}")).collect();
    names.extend(append_enum_to_string(
        &assembly,
        &resolver,
        &mut funcs,
        &mut descriptors,
        "",
    ));
    replace_exception_message(&assembly, &mut funcs);
    append_reference_descriptors(&funcs, &resolver, &mut descriptors);
    let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let imports = pinvoke_imports(&funcs);
    let import_names: Vec<&str> = imports.iter().map(|(name, _)| name.as_str()).collect();
    let mut silent_edges: Vec<(alloc::string::String, alloc::string::String)> = Vec::new();
    for (ordinal, reference) in reference_assemblies.iter().enumerate() {
        for (_, seam, symbol) in
            imported_silent_seams(reference, &references[..ordinal], &import_names)
        {
            let caller_rid = imports
                .iter()
                .find(|(name, _)| *name == symbol)
                .map_or(0, |(_, caller)| *caller);
            silent_edges.push((alloc::format!("f{caller_rid}"), seam));
        }
    }
    if let Some((caller, seam)) = silent_edges.first() {
        return Err(BuildError::SilentSeamCallEdge {
            caller: caller.clone(),
            seam: seam.clone(),
            total: silent_edges.len(),
        });
    }
    let statics = assembly_statics(cil, &assembly, true, resolver.monomorphized(), resolver.references());
    let reference_regions: Vec<alloc::string::String> = reference_cils
        .iter()
        .zip(&reference_assemblies)
        .map(|(bytes, reference)| {
            assembly_statics(bytes, reference, false, &crate::generics::MonoPlan::default(), &[])
                .region_symbol()
        })
        .collect();
    let reference_region_refs: Vec<&str> = reference_regions.iter().map(|s| s.as_str()).collect();
    let qualifiers = crate::resolver::DescQualifiers {
        string: resolver.string_type_meta().map(|m| m.handle.0),
        own: None,
        references: reference_cils
            .iter()
            .map(|bytes| alloc::format!("{:08x}", lamella_metadata::fnv1a32(0x811c_9dc5, bytes)))
            .collect(),
    };
    riscv32::lower_object_profile_statics_references(
        &funcs,
        &name_refs,
        &[],
        &descriptors,
        Some(&statics),
        &reference_region_refs,
        &qualifiers,
        profile,
    )
    .map_err(BuildError::LowerRiscv)
}

/// Lowers ONE monomorphized body: the generic definition's own CIL, lowered under the type
/// arguments of the instantiation the plan assigned it.
///
/// **THE DEFINITION'S CIL IS THE SAME BYTES FOR EVERY INSTANTIATION, AND THE CONTEXT IS THE ONLY
/// THING THAT DIFFERS.** Inside `` Box`1 ``'s own body a field access names its field by a plain
/// `Field` token whose signature is `!0` -- there is no `TypeSpec` anywhere to decode, because the
/// definition is talking about itself. So the instantiation is supplied as
/// [`with_type_arguments`](MetadataResolver::with_type_arguments) context rather than read out of
/// the token, and the SAME token answers `I32` under `[I4]` and `ObjectRef` under `[String]`.
///
/// **Every slot type is substituted BEFORE it is typed** -- see [`substituted_mir_type`] for why
/// that is a refusal rather than a fallback.
pub fn lower_monomorphized_body<'a>(
    assembly: &'a Assembly<'a>,
    resolver: &MetadataResolver<'a>,
    body: &crate::generics::MonoBody,
) -> Result<Function, BuildError> {
    let gap = |reason: MonoGap| BuildError::MonomorphizedBody {
        index: body.index,
        instantiation: alloc::string::String::from(&*body.instantiation),
        method: alloc::string::String::from(&*body.name),
        reason,
    };
    let (definitions, rebased) = match body.owner {
        crate::generics::BodyOwner::Own => (resolver.clone(), None),
        crate::generics::BodyOwner::Reference(ordinal) => {
            let owner = *resolver
                .references()
                .get(usize::from(ordinal))
                .ok_or_else(|| gap(MonoGap::CrossAssemblyOwner))?;
            let symbols = library_function_symbols(owner, &resolver.references()[..usize::from(ordinal)])
                .ok_or_else(|| gap(MonoGap::CrossAssemblyOwner))?;
            let rebased = resolver
                .rebased_on_reference(ordinal, symbols)
                .ok_or_else(|| gap(MonoGap::CrossAssemblyOwner))?;
            (rebased, Some(ordinal))
        }
    };
    let owner_assembly = definitions.assembly();
    let method = owner_assembly
        .method(body.rid)
        .ok_or_else(|| gap(MonoGap::NoDefinitionBody))?;
    let cil_body = method.body().ok_or_else(|| gap(MonoGap::NoDefinitionBody))?;
    let arguments = match &body.arguments {
        Some(carried) => carried.clone(),
        None => {
            crate::generics::instantiation_of(assembly, body.spec)
                .ok_or_else(|| gap(MonoGap::NoArguments))?
                .1
        }
    };
    let layout_arguments = rebased
        .is_some()
        .then(|| crate::resolver::caller_resolved_arguments(&arguments, assembly, resolver.references()))
        .unwrap_or_else(|| Some(arguments.clone()));
    let argument_world = rebased.is_some().then_some(assembly);
    let Some(layout_arguments) = layout_arguments else {
        let argument = arguments
            .iter()
            .find(|argument| {
                names_a_value_type(argument)
                    && crate::resolver::caller_resolved_argument(
                        argument,
                        assembly,
                        resolver.references(),
                    )
                    .is_none()
            })
            .or_else(|| arguments.first());
        return Err(gap(MonoGap::CrossAssemblyValueTypeArgument {
            argument: argument
                .and_then(|argument| crate::generics::spell_sig(assembly, argument))
                .unwrap_or_else(|| alloc::string::String::from("an unnameable type argument")),
        }));
    };
    let named_argument = rebased
        .is_some()
        .then(|| arguments.iter().find(|argument| names_a_type(argument)))
        .flatten()
        .map(|argument| {
            crate::generics::spell_sig(assembly, argument)
                .unwrap_or_else(|| alloc::format!("{argument:?}"))
        });
    let mut arg_types = Vec::new();
    if let Some(signature) = method.signature() {
        if signature.has_this {
            arg_types.push(MirType::ObjectRef);
        }
        for parameter in &signature.parameters {
            arg_types.push(
                substituted_mir_type(
                    parameter,
                    &layout_arguments,
                    owner_assembly,
                    argument_world,
                    definitions.references(),
                )
                .ok_or_else(|| gap(MonoGap::UnsubstitutedSlot))?,
            );
        }
    }
    let mut local_types = Vec::new();
    for local in &method.local_variables() {
        local_types.push(
            substituted_mir_type(
                local,
                &layout_arguments,
                owner_assembly,
                argument_world,
                definitions.references(),
            )
            .ok_or_else(|| gap(MonoGap::UnsubstitutedSlot))?,
        );
    }
    let instantiated = definitions
        .with_type_arguments(arguments)
        .with_layout_arguments(layout_arguments);
    let mut func = cil::lower_method_typed(&cil_body, &instantiated, &arg_types, &local_types)
        .map(|(func, _map)| func)
        .map_err(|error| gap(MonoGap::LowerCil(error)))?;
    if let Some(ordinal) = rebased {
        let own_band_base =
            crate::resolver::non_generic_region_words(assembly, resolver.references()) * 4;
        let mints_an_identity =
            rebase_identities(&mut func, ordinal, own_band_base).map_err(gap)?;
        if let Some(argument) = named_argument {
            if mints_an_identity {
                return Err(gap(MonoGap::CrossAssemblyNamedArgument { argument }));
            }
        }
    }
    Ok(func)
}

/// Respells every TYPE IDENTITY a cross-assembly body minted while reading its OWNER's metadata as
/// the CALLER spells it, and REFUSES the body if any one of them has no such spelling.
///
/// # Why this is a pass over the MIR and not a rule at each mint site
///
/// A handle is minted at a dozen places in the resolver -- an `Alloc`, a cast target, an array's
/// element, a delegate's layout, a boxed value's slot type -- and a flag threaded through all of
/// them is this lane's recurring bug class: the thirteenth site gains no case, keeps the own-assembly
/// answer, and the result is a descriptor named for whichever of the CALLER's types shares that row.
/// Here the correction is applied once, to the finished body, by a rule that is TOTAL over the handle
/// encoding -- so a shape with no arm REFUSES rather than passing through. The three corrections
/// that CANNOT be made this way (a call out, a static, an initialization thunk) are made in the
/// resolver instead, because each needs to know what the token meant, which the MIR no longer says.
///
/// **A STATIC IS CHECKED HERE AND REBASED IN THE RESOLVER, WHICH IS NOT A DUPLICATE RULE.** The
/// resolver knows which region a field belongs to; this only knows that an own-region access should
/// no longer exist. Word 0 is the reserved EH-tag marker -- one global symbol per image, not a
/// field, and matched by exact equality at emission -- so it stays `Own` and is the single case
/// this admits.
/// Returns whether the body minted ANY type identity, which is what the named-argument backstop
/// keys on -- a body that mints none cannot mint a wrong one.
fn rebase_identities(
    func: &mut Function,
    ordinal: u8,
    own_band_base: u32,
) -> Result<bool, MonoGap> {
    let mut minted = false;
    let rebase = |handle: lamella_ir::TypeHandle| {
        crate::resolver::rebased_handle(handle, ordinal)
            .ok_or(MonoGap::CrossAssemblyIdentity { handle: handle.0 })
    };
    for slot in func
        .params
        .iter_mut()
        .chain(func.ret.iter_mut())
        .chain(func.value_types.iter_mut())
    {
        if let MirType::ValueType { handle, .. } = slot {
            if crate::stackmaps::is_frame_cell_handle(*handle) {
                continue;
            }
            match crate::resolver::argument_world_handle(*handle) {
                Some(caller) => *handle = caller,
                None => {
                    *handle = rebase(*handle)?;
                    minted = true;
                }
            }
        }
    }
    for block in &mut func.blocks {
        for (_, inst) in &mut block.insts {
            match inst {
                Inst::Alloc { handle, .. }
                | Inst::TypeDescAddr { handle }
                | Inst::AllocArray2D { handle, .. }
                | Inst::AllocArrayMD { handle, .. } => {
                    let assembly_independent =
                        handle.0 >> 24 == crate::generics::INSTANTIATION_HANDLE_TABLE;
                    *handle = rebase(*handle)?;
                    if !assembly_independent {
                        minted = true;
                    }
                }
                Inst::AllocArray {
                    handle, element, ..
                } => {
                    match crate::resolver::argument_world_handle(*handle) {
                        Some(caller) => *handle = caller,
                        None => {
                            *handle = rebase(*handle)?;
                            minted = true;
                        }
                    }
                    if let Some(element) = element {
                        match crate::resolver::argument_world_handle(*element) {
                            Some(caller) => *element = caller,
                            None => {
                                *element = rebase(*element)?;
                                minted = true;
                            }
                        }
                    }
                }
                Inst::TypeDescLiteral { handle, .. } => {
                    return Err(MonoGap::CrossAssemblyIdentity { handle: *handle });
                }
                Inst::StaticLoad { owner, offset, .. } | Inst::StaticStore { owner, offset, .. } => {
                    let in_own_band = own_band_base != 0 && *offset >= own_band_base;
                    if matches!(owner, StaticOwner::Own)
                        && *offset != cil::G_EXCEPTION_TAG_OFFSET
                        && !in_own_band
                    {
                        return Err(MonoGap::CrossAssemblyStatic { offset: *offset });
                    }
                }
                _ => {}
            }
        }
    }
    Ok(minted)
}

/// Refuses the build if any instantiation the plan carries got no descriptor, naming it.
///
/// **A FILTER IS NOT A GATE, AND THIS IS THE GATE.** The descriptor path declines an instantiation
/// it cannot describe exactly, and then simply leaves it out -- which keeps a WRONG descriptor from
/// being emitted and does nothing whatever about the image. The bodies still lower, the allocation
/// still proceeds, and the dispatch goes through a descriptor that is not there. MEASURED, one
/// variable changed: a program whose generic definition gained one `virtual` BUILT CLEANLY and then
/// HARD FAULTED on an emulated Cortex-M0, where the same program without it answered 42. Nothing
/// reported anything.
///
/// **IT ASKS THE EMITTER'S OWN QUESTION RATHER THAN A SECOND PREDICATE, AND THAT IS WHY IT
/// NARROWS BY ITSELF.** The first version of this gate re-derived "does the definition declare a
/// virtual or implement an interface" -- the layout path's rule, copied. The moment the descriptor
/// path learned to BUILD those tables, the copy would have gone on refusing exactly what the
/// emitter could now describe, and the two would have to be kept in step by hand. Comparing the
/// descriptors PRODUCED against the instantiations PLANNED cannot drift from the emission, because
/// it is the emission: whatever the descriptor path declines tomorrow is refused here tomorrow,
/// with no edit. [`MetadataResolver::undescribed_instantiations`] is that diff, and it carries the
/// one applicability rule -- a VALUE-type instantiation has no descriptor to be missing.
fn refuse_undispatchable_instantiations(resolver: &MetadataResolver<'_>) -> Result<(), BuildError> {
    match resolver.undescribed_instantiations().first() {
        Some(name) => Err(BuildError::UndispatchableInstantiation {
            instantiation: alloc::string::String::from(&**name),
        }),
        None => Ok(()),
    }
}

/// Whether a closed type argument NAMES a type -- the criterion that separates the cross-assembly
/// slice this tier lowers from the one it refuses (see [`MonoGap::CrossAssemblyNamedArgument`]).
///
/// It asks the question STRUCTURALLY rather than by listing the safe cases: a `Class`/`ValueType`
/// carries a token, and every composite spelling is token-bearing exactly when one of its parts is.
/// A list of primitives would go quiet the day a new primitive `SigType` is added; this does not.
fn names_a_type(sig: &SigType) -> bool {
    match sig {
        SigType::Class(_) | SigType::ValueType(_) => true,
        SigType::SzArray(element) | SigType::ByRef(element) | SigType::Pointer(element) => {
            names_a_type(element)
        }
        SigType::Array { element, .. } => names_a_type(element),
        SigType::GenericInst {
            definition,
            arguments,
        } => names_a_type(definition) || arguments.iter().any(names_a_type),
        _ => false,
    }
}

/// Whether a closed type argument is LAID OUT rather than merely referenced -- the criterion for
/// [`MonoGap::CrossAssemblyValueTypeArgument`], and the reason it is a narrower question than
/// [`names_a_type`].
///
/// **A REFERENCE NEEDS NO ASSEMBLY TO INTERPRET AND A VALUE TYPE DOES.** `Box<MyProgramClass>` is
/// lowered here because every reference is four bytes and one traced word, whatever it names -- the
/// token is carried, never read. `Box<MyProgramEnum>` reads a `TypeDef` row for the underlying
/// width, and `Box<MyProgramStruct>` reads one for a size, field offsets and a reference map. The
/// row number belongs to the caller and the tables belong to the owner, so the answer describes an
/// unrelated type that happens to sit at that row.
///
/// **AN ADDRESS IS NOT A LAYOUT, WHICH IS WHY `SzArray`/`ByRef`/`Pointer` DO NOT RECURSE HERE.** An
/// array of a caller's struct is still an `ObjectRef` in the slot, and the element layout is the
/// array descriptor's question rather than this slot's. [`names_a_type`] recurses through them
/// because it asks whether a token is PRESENT; this asks whether one is READ.
///
/// **A VALUE-TYPE INSTANTIATION RECURSES INTO ITS ARGUMENTS AND A CLASS ONE DOES NOT**, which is
/// the same line [`mir_type`] draws: a `GenericInst` over a class is a reference whatever its
/// arguments are, and one over a value type is laid inline, so every token inside it is read.
fn names_a_value_type(sig: &SigType) -> bool {
    match sig {
        SigType::ValueType(_) => true,
        SigType::GenericInst {
            definition,
            arguments,
        } => {
            names_a_value_type(definition)
                || (matches!(**definition, SigType::ValueType(_))
                    && arguments.iter().any(names_a_value_type))
        }
        _ => false,
    }
}

/// The per-function symbol names the assembly at `owner` actually DEFINES in its own library
/// object, `MethodDef`-rid indexed -- the names a rebased body's calls out have to use, and `None`
/// at every rid whose body that object does not carry.
///
/// The names are [`library_symbol_names`]' own output, asked with the same inputs the owner's own
/// build asks with: its own reference prefix, and a table sized past its last `MethodDef` row. That
/// sizing is why the count is derived here rather than passed in -- the tail entries are the unique
/// `L<hash>.f<i>` strings the duplicate-name demotion cannot collide with, so a table sized to the
/// rows alone names every row exactly as the owner's own, longer table does.
///
/// **AN OPEN GENERIC DEFINITION'S METHODS ARE WITHHELD, AND THAT IS A REFUSAL THIS TIER NEEDS
/// RATHER THAN A CONSERVATISM.** [`lower_assembly_seams`] skips every method of an open generic type
/// and every generic method, so the owner's object has a `stub()` at those rids -- a symbol that is
/// DEFINED and RETURNS. A rebased body calling its own definition's sibling would therefore link,
/// boot and answer zero, which is the exact silent shape the whole cross-assembly landing is
/// arranged to avoid. Withheld here, the call site finds no target and fails LOUD instead.
///
/// `None` (the whole list) for an owner with no file bytes (an [`Assembly::from_image`] parse):
/// there is no content hash, so there is no `L<hash>.` family to call into and nothing to name.
fn library_function_symbols<'a>(
    owner: &'a Assembly<'a>,
    owner_references: &[&'a Assembly<'a>],
) -> Option<Vec<Option<alloc::string::String>>> {
    let bytes = owner.file()?;
    let prefix = alloc::format!("L{:08x}.", lamella_metadata::fnv1a32(0x811c_9dc5, bytes));
    let count = owner
        .tables()
        .row_count(lamella_metadata::tables::table::METHOD_DEF) as usize
        + 1;
    let named = library_symbol_names(owner, owner_references, count, &prefix);
    let generic_types = owner.type_parameter_names();
    let generic_methods = owner.method_type_parameter_names();
    let mut lowered = alloc::vec![true; named.len()];
    for type_def in owner.type_defs() {
        let open = generic_types.contains_key(&type_def.token().row());
        for method in type_def.methods() {
            let rid = method.rid() as usize;
            if let Some(slot) = lowered.get_mut(rid) {
                *slot = !(open || generic_methods.contains_key(&method.rid()));
            }
        }
    }
    Some(
        named
            .into_iter()
            .zip(lowered)
            .map(|(name, lowered)| lowered.then_some(name))
            .collect(),
    )
}

/// Lowers one generic METHOD's body under the type arguments a call site supplied -- the method-axis
/// twin of [`lower_monomorphized_body`].
///
/// The two differ only in which of `substitute_sig_with`'s argument lists is filled: this one fills
/// the METHOD half and leaves the type half empty, because a generic method on a non-generic type
/// has no enclosing instantiation. That is also the bound on what is planned today -- a generic
/// method on a generic TYPE would need both halves composed, and `MonoPlan::method_axis` declines
/// that pair rather than lowering it with one of them missing.
///
/// **Every slot is substituted before it is typed, and an unsubstituted one REFUSES** -- the reason
/// is [`substituted_mir_type`]'s: `!!0` falling back to `int32` types `Pick<string>`'s parameter as
/// an integer, which is a wrong GC trace map rather than a wrong size, and no test that only checks
/// `Pick<int>` can see it.
///
/// **UNGATED, LIKE ITS TWIN, AND DELIBERATELY SO.** Its ONLY caller (`lower_assembly_seams`) is
/// ungated, so a `#[cfg(any(feature = "arm32", feature = "riscv32"))]` here stops
/// `--no-default-features --features wasm` compiling the crate at all -- not a missing feature, a
/// missing FUNCTION. `default = ["arm32"]` and `cargo test --workspace` never build that
/// configuration, so the only thing that holds it is a build of each code model IN ISOLATION. A
/// `#[cfg]` on a function reached from ungated code is not a smaller build, it is a build that does
/// not exist.
pub fn lower_monomorphized_method_body<'a>(
    assembly: &'a Assembly<'a>,
    resolver: &MetadataResolver<'a>,
    body: &crate::generics::MonoMethodBody,
) -> Result<Function, BuildError> {
    let gap = |reason: MonoGap| BuildError::MonomorphizedBody {
        index: body.index,
        instantiation: alloc::string::String::from(&*body.instantiation),
        method: alloc::string::String::from(&*body.name),
        reason,
    };
    let (definitions, rebased) = match body.owner {
        crate::generics::BodyOwner::Own => (resolver.clone(), None),
        crate::generics::BodyOwner::Reference(ordinal) => {
            let owner = *resolver
                .references()
                .get(usize::from(ordinal))
                .ok_or_else(|| gap(MonoGap::CrossAssemblyOwner))?;
            let symbols =
                library_function_symbols(owner, &resolver.references()[..usize::from(ordinal)])
                    .ok_or_else(|| gap(MonoGap::CrossAssemblyOwner))?;
            let rebased = resolver
                .rebased_on_reference(ordinal, symbols)
                .ok_or_else(|| gap(MonoGap::CrossAssemblyOwner))?;
            (rebased, Some(ordinal))
        }
    };
    let owner_assembly = definitions.assembly();
    let method = owner_assembly
        .method(body.rid)
        .ok_or_else(|| gap(MonoGap::NoDefinitionBody))?;
    let cil_body = method.body().ok_or_else(|| gap(MonoGap::NoDefinitionBody))?;
    if rebased.is_some() {
        if let Some(argument) = body.arguments.iter().find(|argument| names_a_type(argument)) {
            return Err(gap(MonoGap::CrossAssemblyNamedArgument {
                argument: crate::generics::spell_sig(assembly, argument)
                    .unwrap_or_else(|| alloc::format!("{argument:?}")),
            }));
        }
    }
    let typed = |sig: &SigType| -> Result<MirType, BuildError> {
        let closed = crate::generics::substitute_sig_with(sig, &[], &body.arguments)
            .ok_or_else(|| gap(MonoGap::UnsubstitutedSlot))?;
        match &closed {
            SigType::GenericInst { definition, .. } => match definition.as_ref() {
                SigType::Class(_) => Ok(MirType::ObjectRef),
                _ => Err(gap(MonoGap::UnsubstitutedSlot)),
            },
            other => mir_type(other, owner_assembly, None, definitions.references()),
        }
    };
    let mut arg_types = Vec::new();
    if let Some(signature) = method.signature() {
        if signature.has_this {
            arg_types.push(MirType::ObjectRef);
        }
        for parameter in &signature.parameters {
            arg_types.push(typed(parameter)?);
        }
    }
    let mut local_types = Vec::new();
    for local in &method.local_variables() {
        local_types.push(typed(local)?);
    }
    let instantiated = definitions.with_method_arguments(body.arguments.clone());
    let mut func = cil::lower_method_typed(&cil_body, &instantiated, &arg_types, &local_types)
        .map(|(func, _map)| func)
        .map_err(|error| gap(MonoGap::LowerCil(error)))?;
    if let Some(ordinal) = rebased {
        let own_band_base =
            crate::resolver::non_generic_region_words(assembly, resolver.references()) * 4;
        rebase_identities(&mut func, ordinal, own_band_base).map_err(gap)?;
    }
    Ok(func)
}

/// A slot's MIR type with the instantiation applied FIRST, or `None` when it does not close.
///
/// **IT REFUSES WHERE [`mir_type`] FALLS BACK TO `I32`, AND THAT IS THE WHOLE REASON IT EXISTS.**
/// `mir_type` answers `MirType::I32` for every `SigType` it does not name, and `SigType::Var(0)` is
/// one of those. So handing it an unsubstituted `!0` types `Box<string>`'s parameter as an INTEGER
/// -- not a wrong size, a wrong GC TRACE MAP, which no size and no test that only checks `Box<int>`
/// can see. A body that cannot be typed is refused; it is never typed approximately.
///
/// **A NESTED instantiation is answered only where the answer is provable.** `` List`1<Box`1<int>> ``
/// as a slot type is a CLASS instantiation and therefore an `ObjectRef` whatever its arguments are.
/// A nested VALUE-type instantiation's MIR type needs the instantiated layout's SIZE, which is a
/// different seam, so it refuses rather than taking `mir_type`'s `I32`.
fn substituted_mir_type<'x>(
    sig: &SigType,
    arguments: &[SigType],
    assembly: &'x Assembly<'x>,
    argument_world: Option<&'x Assembly<'x>>,
    references: &[&'x Assembly<'x>],
) -> Option<MirType> {
    let closed = crate::generics::substitute_sig(sig, arguments)?;
    match &closed {
        SigType::Var(_) | SigType::MVar(_) => None,
        SigType::GenericInst { definition, .. } => match definition.as_ref() {
            SigType::Class(_) => Some(MirType::ObjectRef),
            _ => None,
        },
        other => mir_type(other, assembly, argument_world, references).ok(),
    }
}

/// Lowers the methods of a self-contained assembly REACHABLE from `entry`, rid-indexed, into a dense
/// module for [`riscv32::lower_object`]. Index 0 is the entry [`startup`] (board-init hook, then the
/// `.cctor`s the chain still owns, then `Main`); each reachable method sits at its `MethodDef` rid;
/// each type demanding precise initialization takes an index above the monomorphized bodies for its
/// [`type_init_thunk_body`]; every unreached rid is a [`stub`]. Reachability is a BFS over direct
/// `Call` edges seeded with `Main`, the CHAINED `.cctor`s, the board-init hook, and every
/// this-assembly vtable/itable dispatch target (an indirect call has no `Call` edge). Skipping the
/// unreached rids keeps the implicit `.ctor`'s `object::.ctor()` corlib call out of a self-contained
/// build -- the flat driver relies on the same property.
///
/// **A PRECISE TYPE'S INITIALIZER IS REACHED THROUGH ITS THUNK AND NOWHERE ELSE, WHICH IS THIS
/// PATH'S ONE STRUCTURAL DIFFERENCE FROM [`lower_assembly_seams`].** That tier lowers every planned
/// body unconditionally, so a thunk it emits is emitted whether or not anything calls it; here a
/// thunk is lowered only when a trigger site reaches it, and the `.cctor` only through the thunk. So
/// the seed must NOT contain the precise `.cctor`s: seeding them keeps the bodies alive for a reason
/// unrelated to the triggers, and a trigger site that was never emitted would then look exactly like
/// one that was.
///
/// **IT TAKES NO DESCRIPTOR LIST, AND DERIVING ITS OWN IS THE POINT RATHER THAN A CONVENIENCE.** A
/// caller's copy is computed before the plan exists, and a virtual GENERIC method's vtable slots
/// come FROM the plan -- so a list passed in is a twin that agrees with the emitted tables only
/// while no plan can change an ORDINARY type's numbering. This asks the resolver that holds the
/// plan; the caller re-derives its emitted copy from the same function.
#[cfg(feature = "riscv32")]
fn lower_reachable<'a>(
    assembly: &'a Assembly<'a>,
    entry: u32,
    references: &[&'a Assembly<'a>],
    reference_cctors: &[alloc::string::String],
) -> Result<(Vec<Function>, crate::generics::MonoPlan), BuildError> {
    let mut max_rid = entry;
    for type_def in assembly.type_defs() {
        for method in type_def.methods() {
            max_rid = max_rid.max(method.rid());
        }
    }
    let plan = crate::generics::MonoPlan::for_assembly_with_references(
        assembly,
        references,
        max_rid + 1,
    )
    .map_err(BuildError::Instantiations)?;
    let precise = crate::resolver::precise_init_types(assembly, references);
    let thunk_base = max_rid as usize + 1 + plan.len();
    let thunk_indices: Vec<(u32, u32)> = precise
        .iter()
        .enumerate()
        .map(|(i, (type_row, _, _))| (*type_row, (thunk_base + i) as u32))
        .collect();
    let mut funcs: Vec<Function> = (0..thunk_base + precise.len()).map(|_| stub()).collect();
    let mut lowered = vec![false; funcs.len()];
    let cctors = startup_cctors(assembly, references);
    let init = find_native_export(assembly, "lamella_time_init");
    let resolver = MetadataResolver::new(assembly)
        .with_references(references)
        .with_monomorphized(plan.clone())
        .with_type_init_thunks(thunk_indices.clone());
    let instantiated = resolver.instantiation_descriptors();
    refuse_undispatchable_instantiations(&resolver)?;
    let mut worklist: Vec<u32> = core::iter::once(entry)
        .chain(cctors.iter().copied())
        .chain(init)
        .collect();
    let own = resolver.type_descriptors();
    for meta in own.iter().chain(&instantiated) {
        for slot in &meta.vtable {
            if let crate::resolver::VtableEntry::Func(index) = slot {
                worklist.push(*index);
            }
        }
        for (_, impl_) in &meta.itable {
            if let crate::resolver::VtableEntry::Func(index) = impl_ {
                worklist.push(*index);
            }
        }
    }
    while let Some(rid) = worklist.pop() {
        let Some(seen) = lowered.get_mut(rid as usize) else {
            continue;
        };
        if *seen {
            continue;
        }
        *seen = true;
        let func = match plan.bodies().iter().find(|body| body.index == rid) {
            Some(body) if body.declaration_only => deferred_trap_body(),
            Some(body) => lower_monomorphized_body(assembly, &resolver, body)?,
            None => match plan.method_bodies().iter().find(|body| body.index == rid) {
                Some(body) => lower_monomorphized_method_body(assembly, &resolver, body)?,
                None => match thunk_indices.iter().position(|(_, index)| *index == rid) {
                    Some(i) => {
                        let (_, cctor, flag_slot) = precise[i];
                        type_init_thunk_body(flag_slot * 4, cctor)
                    }
                    None => match lower_one_reachable(assembly, &resolver, rid)? {
                        Some(func) => func,
                        None => continue,
                    },
                },
            },
        };
        for block in &func.blocks {
            for (_, inst) in &block.insts {
                if let Inst::Call { callee, .. } = inst {
                    if lowered.get(*callee as usize) == Some(&false) {
                        worklist.push(*callee);
                    }
                }
                if let Inst::FuncAddr { func } = inst {
                    if lowered.get(*func as usize) == Some(&false) {
                        worklist.push(*func);
                    }
                }
            }
        }
        funcs[rid as usize] = func;
    }
    funcs[0] = startup_with_references(init, reference_cctors, &cctors, entry);
    Ok((funcs, plan))
}

/// Lowers the method at `MethodDef` rid `rid` to MIR (its plain managed body -- the same path
/// [`lower_assembly_debug`] takes for an ordinary method), or `Ok(None)` if there is no such method or
/// it has no LOWERABLE body. A body that fails to lower is `Err(BuildError::LowerCil)` -- FAIL LOUD,
/// never a silent stub (a stubbed reachable method would miscompile the program).
///
/// **A method with no CIL is not automatically a method with no body.** A delegate's `Invoke` is
/// Runtime-implemented, and it gets a synthesized dispatch from the same
/// [`delegate_invoke_synthesis`] the whole-assembly path uses. Abstract and extern methods answer
/// `None`.
#[cfg(feature = "riscv32")]
fn lower_one_reachable(
    assembly: &Assembly,
    resolver: &MetadataResolver,
    rid: u32,
) -> Result<Option<Function>, BuildError> {
    for type_def in assembly.type_defs() {
        for method in type_def.methods() {
            if method.rid() != rid {
                continue;
            }
            let signature = method.signature();
            let Some(body) = method.body() else {
                return delegate_invoke_synthesis(
                    assembly,
                    resolver.references(),
                    crate::resolver::is_delegate_type_of(assembly, &type_def),
                    method.name(),
                    &signature,
                );
            };
            let mut arg_types = Vec::new();
            if let Some(sig) = &signature {
                if sig.has_this {
                    arg_types.push(MirType::ObjectRef);
                }
                for parameter in &sig.parameters {
                    arg_types.push(mir_type(parameter, assembly, None, resolver.references())?);
                }
            }
            let local_types: Vec<MirType> = method
                .local_variables()
                .iter()
                .map(|sig| mir_type(sig, assembly, None, resolver.references()))
                .collect::<Result<_, BuildError>>()?;
            return match cil::lower_method_typed(&body, resolver, &arg_types, &local_types) {
                Ok((func, _map)) => Ok(Some(func)),
                Err(error) => Err(BuildError::LowerCil { rid, error }),
            };
        }
    }
    Ok(None)
}

/// AOT-lowers a whole assembly as a LINKABLE LIBRARY object for RISC-V (a corlib, a helper library) --
/// the RISC-V twin of [`build_library_object`]: every method is lowered with NO entry/startup, a public
/// method takes its stable cross-assembly symbol ([`extern_method_symbol`](crate::resolver::extern_method_symbol))
/// so a program's extern call binds against it, and every other method takes an assembly-unique internal
/// name (`L<hash>.f<rid>`, so two libraries' internals never clash). A method whose CIL body does not
/// lower stays a stub (the rest of the library still builds); a method whose MIR the RISC-V backend
/// cannot lower is a LOUD error (there is no RISC-V-level dry-run tolerance), fine for a small
/// library where every method lowers.
#[cfg(feature = "riscv32")]
pub fn build_library_object_riscv(cil: &[u8]) -> Result<Vec<u8>, BuildError> {
    build_library_object_riscv_inner(cil, &[]).map(|(bytes, _)| bytes)
}

/// As [`build_library_object_riscv`], but with the library's OWN references (its corlib, a
/// helper library) attached, so its bodies resolve cross-assembly types the way its compile did:
/// a `new object()` inside a library ctor needs corlib's layout, or the method stubs and the
/// object it builds is inert. A cross-assembly `ldsfld` in a library body resolves to its
/// owner's region, exactly as a program's does.
#[cfg(feature = "riscv32")]
pub fn build_library_object_riscv_with_references(
    cil: &[u8],
    references: &[&[u8]],
) -> Result<Vec<u8>, BuildError> {
    build_library_object_riscv_inner(cil, references).map(|(bytes, _)| bytes)
}

#[cfg(feature = "riscv32")]
fn build_library_object_riscv_inner(
    cil: &[u8],
    reference_cils: &[&[u8]],
) -> Result<(Vec<u8>, LibraryBuildReport), BuildError> {
    let assembly = read_assembly(cil)?;
    let reference_assemblies: Vec<Assembly> = reference_cils
        .iter()
        .map(|bytes| read_assembly(bytes))
        .collect::<Result<_, _>>()?;
    let references: Vec<&Assembly> = reference_assemblies.iter().collect();
    let (mut funcs, _maps, fails, seams, duplicates, thunks, plan) =
        lower_assembly_seams(&assembly, None, &references)?;
    refuse_duplicate_bodies(&duplicates)?;
    let prefix = alloc::format!("L{:08x}.", lamella_metadata::fnv1a32(0x811c_9dc5, cil));
    let resolver = MetadataResolver::new(&assembly)
        .with_references(&references)
        .with_monomorphized(plan);
    let mut descriptors = resolver.image_descriptors();
    let mut names = library_symbol_names(&assembly, &references, funcs.len(), &prefix);
    name_type_init_thunks(&assembly, &thunks, &mut names);
    names.extend(append_enum_to_string(
        &assembly,
        &resolver,
        &mut funcs,
        &mut descriptors,
        &prefix,
    ));
    replace_exception_message(&assembly, &mut funcs);
    let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    append_reference_descriptors(&funcs, &resolver, &mut descriptors);
    let statics = assembly_statics(cil, &assembly, false, resolver.monomorphized(), resolver.references());
    let reference_regions: Vec<alloc::string::String> = reference_cils
        .iter()
        .zip(&reference_assemblies)
        .map(|(bytes, reference)| {
            assembly_statics(bytes, reference, false, &crate::generics::MonoPlan::default(), &[])
                .region_symbol()
        })
        .collect();
    let reference_region_refs: Vec<&str> = reference_regions.iter().map(|s| s.as_str()).collect();
    let qualifiers = crate::resolver::DescQualifiers {
        string: resolver.string_type_meta().map(|m| m.handle.0),
        own: Some(alloc::format!(
            "{:08x}",
            lamella_metadata::fnv1a32(0x811c_9dc5, cil)
        )),
        references: reference_cils
            .iter()
            .map(|bytes| alloc::format!("{:08x}", lamella_metadata::fnv1a32(0x811c_9dc5, bytes)))
            .collect(),
    };
    let (bytes, stubs) = riscv32::lower_object_library_statics_report(
        &funcs,
        &name_refs,
        &[],
        &descriptors,
        Some(&statics),
        &reference_region_refs,
        &qualifiers,
    )
    .map_err(BuildError::LowerRiscv)?;
    let display_names = method_display_names(&assembly, funcs.len());
    let name_of = |rid: usize| {
        display_names
            .get(rid)
            .cloned()
            .flatten()
            .unwrap_or_else(|| alloc::format!("f{rid}"))
    };
    let report = LibraryBuildReport {
        cil_fails: fails
            .into_iter()
            .map(|(rid, error)| (rid, name_of(rid as usize), alloc::format!("{error:?}")))
            .collect(),
        emit_stubs: stubs
            .into_iter()
            .map(|(index, error)| (index as u32, name_of(index), alloc::format!("{error:?}")))
            .collect(),
        unsynthesized_seams: unsynthesized_seam_rows(
            &assembly,
            &seams,
            &display_names,
            &names,
            &vtable_slot_rids(&resolver),
        ),
        silent_seam_edges: silent_seam_call_edges(&assembly, &funcs, &seams, &display_names),
    };
    Ok((bytes, report))
}

/// As [`build_library_object_riscv`], but ALSO returning the [`LibraryBuildReport`] -- the RISC-V
/// twin of [`build_library_object_report`]. The object bytes are identical; the report is
/// observation only.
#[cfg(feature = "riscv32")]
pub fn build_library_object_riscv_report(
    cil: &[u8],
) -> Result<(Vec<u8>, LibraryBuildReport), BuildError> {
    build_library_object_riscv_inner(cil, &[])
}

/// As [`build_library_object_riscv_report`], with the library's OWN references attached.
#[cfg(feature = "riscv32")]
pub fn build_library_object_riscv_report_with_references(
    cil: &[u8],
    references: &[&[u8]],
) -> Result<(Vec<u8>, LibraryBuildReport), BuildError> {
    build_library_object_riscv_inner(cil, references)
}

/// AOT-lowers a whole assembly as a LINKABLE LIBRARY object (a corlib, a helper library): every public
/// static method becomes a global symbol (named by `extern_method_symbol`) a program's extern call
/// resolves against, and a method that does not lower yet becomes a STUB so the rest of the library
/// still builds -- gaps are fixed iteratively. No entry/startup ([`arm32::lower_object_library`]).
#[cfg(feature = "arm32")]
pub fn build_library_object(cil: &[u8]) -> Result<Vec<u8>, BuildError> {
    build_library_object_inner(cil, &[], false).map(|(bytes, _)| bytes)
}

/// As [`build_library_object`], but with the library's OWN referenced assembly (corlib) attached
/// -- a NON-corlib library (`System.Net.NetworkInformation`) allocates and
/// extends corlib types, so its bodies need corlib's layouts/slot numbering exactly as a program's
/// do. Without this, every method touching a corlib type (`new object()` in a `.cctor`, a thrown
/// exception type's ctor) silently falls to the CIL-fail stub list.
#[cfg(feature = "arm32")]
pub fn build_library_object_with_reference(
    cil: &[u8],
    reference: &[u8],
) -> Result<Vec<u8>, BuildError> {
    build_library_object_inner(cil, &[reference], false).map(|(bytes, _)| bytes)
}

/// [`build_library_object_with_reference`] for an ORDERED reference list -- the N-reference
/// deploy shape's middle layer: a BSP assembly references corlib AND System.Device, so its
/// bodies resolve names across both (first declarer wins), its reference-owned descriptors and
/// cross-assembly statics qualify by each owner's hash, and its derived types span cross-
/// assembly base chains. Pass the references in the SAME order every consumer of this library
/// set uses (corlib first by convention) -- ordinals are identity.
///
/// `wide` targets a Mainline (M33) part: a far reference relaxes to its wide Thumb-2 form
/// (`B.W`/`ADR.W`). On a v6-M target (`false`) it splices a literal-pool veneer instead -- a far
/// branch becomes a `ldr; add pc; ...; pop {pc}` long branch and a far `adr` a `ldr; add pc`
/// address computation -- so a big-branch method (a Pico2 BSP's `SpiDriver::Configure`) encodes on
/// EITHER target rather than deferring. The object is byte-identical to the wide build for a method
/// with no out-of-reach reference (the veneer/widen fires only for one).
#[cfg(feature = "arm32")]
pub fn build_library_object_with_references(
    cil: &[u8],
    references: &[&[u8]],
    wide: bool,
) -> Result<Vec<u8>, BuildError> {
    build_library_object_inner(cil, references, wide).map(|(bytes, _)| bytes)
}

/// One entry of a [`LibraryBuildReport`]: the METHOD_DEF rid, the method's readable name
/// (`Namespace.Type::Method`), and the error text that demoted it.
pub type LibraryReportEntry = (u32, alloc::string::String, alloc::string::String);

/// What a `[RuntimeProvided]` seam that this build did NOT synthesize got emitted as instead. The
/// two differ in exactly one property that matters: whether reaching it is observable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeamDisposition {
    /// The corlib's own PLACEHOLDER body lowered as written -- `return null`, `return 0`, or nothing
    /// at all. The method answers a constant and no caller can tell it apart from a real answer, so
    /// this is the silent-wrong-answer disposition (`TlsNative::SessionFlags` reporting "no
    /// validation errors" because 0 is also its "clean" encoding).
    Placeholder,
    /// A TRAP body: reaching it faults. Not silent, so it needs no build-time refusal -- the fault
    /// IS the enforcement. `System.Array`'s untyped element primitives take this until they are
    /// synthesized.
    Trap,
}

/// A CALL EDGE from emitted code into a `[RuntimeProvided]` seam this build left as a SILENT
/// placeholder (no synthesis, no `[IntendedDefault]`, no trap) -- the caller will be told a constant
/// and cannot tell. This is the edge a strict build refuses over
/// ([`BuildError::SilentSeamCallEdge`]); an edge into a marked or trapped seam is not one.
#[derive(Debug, Clone)]
pub struct SeamCallEdge {
    /// The METHOD_DEF rid of the calling method.
    pub caller_rid: u32,
    /// The caller's readable name (`Namespace.Type::Method`).
    pub caller: alloc::string::String,
    /// The METHOD_DEF rid of the seam called.
    pub seam_rid: u32,
    /// The seam's readable name.
    pub seam: alloc::string::String,
}

/// One `[RuntimeProvided]` seam that reached codegen without a synthesized body -- the third
/// silent-demotion layer of a [`LibraryBuildReport`], and the set an image's placeholder audit
/// intersects with the linker's post-`--gc-sections` symbols to answer "which of these does this
/// program actually reach".
#[derive(Debug, Clone)]
pub struct UnsynthesizedSeam {
    /// The METHOD_DEF rid of the seam.
    pub rid: u32,
    /// Its readable name (`Namespace.Type::Method`).
    pub name: alloc::string::String,
    /// The SYMBOL this build emits it under, so the row joins directly against a link map or an
    /// `llvm-nm` listing: the mangled cross-assembly export for an accessible method, else the
    /// internal `L<hash>.f<rid>` (or a program's `f<rid>`).
    pub symbol: alloc::string::String,
    /// Whether the seam declares `[IntendedDefault]` -- its compiled-out default IS the intended
    /// answer, so a build that does not synthesize it is still correct.
    pub intended_default: bool,
    /// What was emitted in place of the missing body.
    pub disposition: SeamDisposition,
    /// Whether the lowering FOLDS every `call` to this method into a backend intrinsic
    /// ([`crate::resolver::folded_intrinsic`]). A folded seam's placeholder body cannot be reached by
    /// a call, so it is NOT a live silent-wrong-answer even though nothing synthesized it, and
    /// counting it as one overstates the risk. Scope: this answers for CALLS only -- a seam reached
    /// through a VIRTUAL slot or through a reference's internals is the linker's reachability
    /// question, which this flag does not speak to.
    pub folded_to_intrinsic: bool,
    /// Whether this seam OCCUPIES A VTABLE SLOT of some type in this assembly -- so a `callvirt` on
    /// that type reaches it with no `call` naming its rid, and the call-edge audit
    /// ([`LibraryBuildReport::silent_seam_edges`]) structurally cannot see it.
    ///
    /// The failure this guards against: `System.Object::ToString` reached through every type's slot
    /// as an unsynthesized seam whose body returns `null` is refused by no build, so every AOT image
    /// answers `null` for a type name. A SILENT seam in a slot is therefore the
    /// worst row in this census rather than a milder one -- the loud guard is the one thing that
    /// does not apply to it.
    pub in_vtable_slot: bool,
}

/// What [`build_library_object_report`] observed while building: the THREE distinct silent-demotion
/// layers a library method can fall through, none of which a plain build surfaces.
#[derive(Debug, Default)]
pub struct LibraryBuildReport {
    /// Methods whose CIL BODY failed to lower to MIR -- they kept the placeholder body, so calling
    /// one returns a constant. `(rid, name, CilError)`.
    pub cil_fails: Vec<LibraryReportEntry>,
    /// Methods whose MIR failed the OBJECT-EMIT stage -- emitted as a bare `bx lr`, which silently
    /// returns its first argument (the WaitOne-style truthy no-op). `(rid, name, LowerError)`;
    /// `CodeTooLarge` marks a fixpoint stub (the body lowers alone, the whole object could not
    /// encode with it in).
    pub emit_stubs: Vec<LibraryReportEntry>,
    /// `[RuntimeProvided]` seams the build did not synthesize. Unlike the two above this is not a
    /// FAILURE -- the body was never in the assembly to lower -- which is exactly why it stayed
    /// invisible: the method compiles, links, and answers a constant.
    pub unsynthesized_seams: Vec<UnsynthesizedSeam>,
    /// Where this assembly's OWN emitted code calls one of those seams in its silent disposition.
    /// A seam nobody calls is a gap; a seam somebody calls is a wrong answer in waiting, and this is
    /// the list that separates them.
    pub silent_seam_edges: Vec<SeamCallEdge>,
}

/// As [`build_library_object`], but also returning the [`LibraryBuildReport`] -- the CIL->MIR fail
/// list `lower_assembly_debug` computes and the object-emit stub set the emit fixpoint tracks
/// internally. The object bytes are IDENTICAL to
/// [`build_library_object`]'s; the report is observation only. A caller diagnosing a silently
/// wrong library call reads this to separate "the method is a stub" from "dispatch reached the
/// wrong slot" in one run.
#[cfg(feature = "arm32")]
pub fn build_library_object_report(cil: &[u8]) -> Result<(Vec<u8>, LibraryBuildReport), BuildError> {
    build_library_object_inner(cil, &[], false)
}

/// [`build_library_object_with_reference`]'s report twin (see [`build_library_object_report`]).
#[cfg(feature = "arm32")]
pub fn build_library_object_report_with_reference(
    cil: &[u8],
    reference: &[u8],
) -> Result<(Vec<u8>, LibraryBuildReport), BuildError> {
    build_library_object_inner(cil, &[reference], false)
}

/// [`build_library_object_with_references`]'s report twin (see [`build_library_object_report`]);
/// `wide` is the same Mainline (M33) far-branch relaxation flag.
#[cfg(feature = "arm32")]
pub fn build_library_object_report_with_references(
    cil: &[u8],
    references: &[&[u8]],
    wide: bool,
) -> Result<(Vec<u8>, LibraryBuildReport), BuildError> {
    build_library_object_inner(cil, references, wide)
}

#[cfg(feature = "arm32")]
fn build_library_object_inner(
    cil: &[u8],
    references: &[&[u8]],
    wide: bool,
) -> Result<(Vec<u8>, LibraryBuildReport), BuildError> {
    let assembly = read_assembly(cil)?;
    let reference_assemblies: Vec<Assembly> = references
        .iter()
        .map(|bytes| read_assembly(bytes))
        .collect::<Result<_, _>>()?;
    let reference_list: Vec<&Assembly> = reference_assemblies.iter().collect();
    let (mut funcs, _maps, fails, seams, duplicates, thunks, plan) =
        lower_assembly_seams(&assembly, None, &reference_list)?;
    refuse_duplicate_bodies(&duplicates)?;
    let prefix = alloc::format!("L{:08x}.", lamella_metadata::fnv1a32(0x811c_9dc5, cil));
    let mut names = library_symbol_names(&assembly, &reference_list, funcs.len(), &prefix);
    name_type_init_thunks(&assembly, &thunks, &mut names);
    let qualifiers = arm32::DescQualifiers {
        string: MetadataResolver::new(&assembly)
            .with_references(&reference_list)
            .string_type_meta()
            .map(|m| m.handle.0),
        own: Some(alloc::format!("{:08x}", lamella_metadata::fnv1a32(0x811c_9dc5, cil))),
        references: references
            .iter()
            .map(|bytes| alloc::format!("{:08x}", lamella_metadata::fnv1a32(0x811c_9dc5, bytes)))
            .collect(),
    };
    let resolver = MetadataResolver::new(&assembly)
        .with_references(&reference_list)
        .with_monomorphized(plan);
    let mut descriptors = resolver.image_descriptors();
    names.extend(append_enum_to_string(
        &assembly,
        &resolver,
        &mut funcs,
        &mut descriptors,
        &prefix,
    ));
    replace_exception_message(&assembly, &mut funcs);
    let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    append_reference_descriptors(&funcs, &resolver, &mut descriptors);
    let statics = assembly_statics(cil, &assembly, false, resolver.monomorphized(), resolver.references());
    let (bytes, stubs) = arm32::lower_object_library_vtables_report(
        &funcs,
        &name_refs,
        &[],
        &descriptors,
        Some(&statics),
        &qualifiers,
        wide,
    )
    .map_err(BuildError::LowerArm)?;
    let display_names = method_display_names(&assembly, funcs.len());
    let name_of = |rid: usize| {
        display_names
            .get(rid)
            .cloned()
            .flatten()
            .unwrap_or_else(|| alloc::format!("f{rid}"))
    };
    let report = LibraryBuildReport {
        cil_fails: fails
            .into_iter()
            .map(|(rid, error)| (rid, name_of(rid as usize), alloc::format!("{error:?}")))
            .collect(),
        emit_stubs: stubs
            .into_iter()
            .map(|(index, error)| (index as u32, name_of(index), alloc::format!("{error:?}")))
            .collect(),
        unsynthesized_seams: unsynthesized_seam_rows(
            &assembly,
            &seams,
            &display_names,
            &names,
            &vtable_slot_rids(&resolver),
        ),
        silent_seam_edges: silent_seam_call_edges(&assembly, &funcs, &seams, &display_names),
    };
    Ok((bytes, report))
}

/// The [`LibraryBuildReport`] rows for the seams [`lower_assembly_seams`] left unsynthesized: each
/// named twice -- readably, and by the SYMBOL this build emits it under, so a row joins straight
/// against a link map (`symbols` is the same rid-indexed list the emission was handed, so the two
/// cannot disagree). `[IntendedDefault]` is read here rather than in the lowering because it is a
/// property of the seam's CONTRACT, not of what the backend managed to emit.
#[cfg(any(feature = "arm32", feature = "riscv32"))]
fn unsynthesized_seam_rows(
    assembly: &Assembly,
    seams: &[SeamRow],
    display_names: &[Option<alloc::string::String>],
    symbols: &[alloc::string::String],
    vtable_slot_rids: &[u32],
) -> Vec<UnsynthesizedSeam> {
    seams
        .iter()
        .map(|(rid, disposition, folded)| UnsynthesizedSeam {
            folded_to_intrinsic: *folded,
            in_vtable_slot: vtable_slot_rids.contains(rid),
            rid: *rid,
            name: display_names
                .get(*rid as usize)
                .cloned()
                .flatten()
                .unwrap_or_else(|| alloc::format!("f{rid}")),
            symbol: symbols
                .get(*rid as usize)
                .cloned()
                .unwrap_or_else(|| alloc::format!("f{rid}")),
            intended_default: assembly.is_intended_default(Token::new(table::METHOD_DEF, *rid)),
            disposition: *disposition,
        })
        .collect()
}

/// Every method rid this assembly places in a VTABLE SLOT -- the set a `callvirt` can reach without
/// any `call` naming the rid, which is precisely what [`silent_seam_call_edges`] cannot see.
///
/// A slot filled from a REFERENCED assembly is an `Extern` symbol rather than a rid and is not
/// collected: this answers "which of MY methods are virtually reachable", which is the question the
/// census rows are about. Duplicates are kept -- the caller only membership-tests -- because a method
/// legitimately occupies the same slot in every derived type that inherits it.
fn vtable_slot_rids(resolver: &MetadataResolver<'_>) -> Vec<u32> {
    resolver
        .vtables()
        .into_iter()
        .flat_map(|(_, entries)| entries)
        .filter_map(|entry| match entry {
            crate::resolver::VtableEntry::Func(rid) => Some(rid),
            crate::resolver::VtableEntry::Extern(_) => None,
        })
        .collect()
}

/// Every direct CALL EDGE from `funcs` into a seam of `seams` whose disposition is SILENT -- an
/// unmarked, untrapped placeholder. Both edge kinds a rid names directly are counted: a `call`
/// ([`Inst::Call`]) and taking the method's ADDRESS ([`Inst::FuncAddr`], a `ldftn` behind a delegate
/// -- deferring the call does not make the answer less wrong).
///
/// SCOPE, and it is a LOWER BOUND rather than the whole answer: this sees the edges a rid names, so
/// it covers every direct `call`. It does NOT resolve a `callvirt`/interface dispatch back to a
/// candidate seam -- a slot maps to a method only through a type's descriptor, and an override may
/// displace it -- and a VIRTUAL seam is a real case, not a hypothetical one: `System.Object::ToString`
/// is itself an unsynthesized seam, reached through every type's vtable slot rather than by any call
/// edge. Nor does it see what a caller reaches through a REFERENCE's own internals. Both of those are
/// decided by the linker's reachability, which is where the census rows' symbols are meant to be
/// intersected -- this refuses what a single assembly can prove on its own.
#[cfg(any(feature = "arm32", feature = "riscv32"))]
fn silent_seam_call_edges(
    assembly: &Assembly,
    funcs: &[Function],
    seams: &[SeamRow],
    display_names: &[Option<alloc::string::String>],
) -> Vec<SeamCallEdge> {
    let silent: Vec<u32> = seams
        .iter()
        .filter(|(rid, disposition, folded)| {
            *disposition == SeamDisposition::Placeholder
                && !*folded
                && !assembly.is_intended_default(Token::new(table::METHOD_DEF, *rid))
        })
        .map(|(rid, ..)| *rid)
        .collect();
    if silent.is_empty() {
        return Vec::new();
    }
    let name_of = |rid: u32| {
        display_names
            .get(rid as usize)
            .cloned()
            .flatten()
            .unwrap_or_else(|| alloc::format!("f{rid}"))
    };
    let mut edges: Vec<SeamCallEdge> = Vec::new();
    for (caller_rid, func) in funcs.iter().enumerate() {
        let caller_rid = caller_rid as u32;
        for block in &func.blocks {
            for (_, inst) in &block.insts {
                let callee = match inst {
                    Inst::Call { callee, .. } => *callee,
                    Inst::FuncAddr { func } => *func,
                    _ => continue,
                };
                if callee == caller_rid || !silent.contains(&callee) {
                    continue;
                }
                if edges
                    .iter()
                    .any(|e| e.caller_rid == caller_rid && e.seam_rid == callee)
                {
                    continue;
                }
                edges.push(SeamCallEdge {
                    caller_rid,
                    caller: name_of(caller_rid),
                    seam_rid: callee,
                    seam: name_of(callee),
                });
            }
        }
    }
    edges
}

/// The CROSS-ASSEMBLY half of the caller audit: every SILENT seam of `reference` that this module's
/// emitted code calls by symbol. A managed call into another assembly lowers to [`Inst::PInvoke`]
/// carrying the callee's mangled export symbol ([`crate::resolver::extern_method_symbol`]) -- the
/// same name the defining library object exports it under, which is exactly why the link SUCCEEDS
/// and the caller is then told a constant.
///
/// Ordered so the expensive question is asked last: the flag tests and the mangle are cheap, and only
/// a symbol this module actually imports pays for the attribute walk and the synthesis-table lookup.
/// That matters -- an unconditional `[RuntimeProvided]` walk of a corlib costs ~230 ms in a debug
/// build, which no program build can afford to spend on every reference.
///
/// The export condition it mirrors is [`library_symbol_names`]'s, reduced by what is known here: a
/// marked method is never `is_plain_instance`, and one this backend synthesizes is not silent, so
/// what remains is `(static || virtual) && accessible`. A symbol two methods share is DEMOTED to
/// internal by the library build, so it cannot be a silent path -- the program fails to link instead.
#[cfg(any(feature = "arm32", feature = "riscv32"))]
fn imported_silent_seams<'a>(
    reference: &'a Assembly<'a>,
    reference_refs: &[&'a Assembly<'a>],
    imports: &[&str],
) -> Vec<(u32, alloc::string::String, alloc::string::String)> {
    let mut found: Vec<(u32, alloc::string::String, alloc::string::String)> = Vec::new();
    if imports.is_empty() {
        return found;
    }
    for type_def in reference.type_defs() {
        let Some(type_name) = type_def.name() else {
            continue;
        };
        for method in type_def.methods() {
            if !(method.is_static() || method.is_virtual())
                || !matches!(method.flags() & 0x7, 0x4..=0x6)
            {
                continue;
            }
            let Some(method_name) = method.name() else {
                continue;
            };
            let Some(sig) = crate::resolver::decodable_signature(&method) else {
                continue;
            };
            let symbol = crate::resolver::extern_method_symbol(
                type_name.namespace,
                type_name.name,
                method_name,
                &sig.parameters,
                &sig.return_type,
                &|token| {
                    reference
                        .type_token_name(token)
                        .map(|n| crate::resolver::joined_full_name(&n))
                },
            );
            if !imports.contains(&symbol.as_str()) {
                continue;
            }
            let token = Token::new(table::METHOD_DEF, method.rid());
            if !reference.is_runtime_provided(token) || reference.is_intended_default(token) {
                continue;
            }
            let signature = method.signature();
            if !matches!(
                synthesized_seam_body(reference, reference_refs, &type_name, &method, &signature),
                SeamEmission::Placeholder
            ) {
                continue;
            }
            found.push((
                method.rid(),
                alloc::format!(
                    "{}{}{}::{method_name}",
                    type_name.namespace,
                    if type_name.namespace.is_empty() { "" } else { "." },
                    type_name.name
                ),
                symbol,
            ));
        }
    }
    found
}

/// Every extern managed symbol `funcs` calls -- the [`Inst::PInvoke`] imports, deduplicated. The
/// input side of [`imported_silent_seams`].
#[cfg(any(feature = "arm32", feature = "riscv32"))]
fn pinvoke_imports(funcs: &[Function]) -> Vec<(alloc::string::String, u32)> {
    let mut imports: Vec<(alloc::string::String, u32)> = Vec::new();
    for (caller_rid, func) in funcs.iter().enumerate() {
        for block in &func.blocks {
            for (_, inst) in &block.insts {
                if let Inst::PInvoke { import, .. } = inst {
                    if !imports.iter().any(|(name, _)| name.as_str() == &**import) {
                        imports.push((alloc::string::String::from(&**import), caller_rid as u32));
                    }
                }
            }
        }
    }
    imports
}

/// `rid -> "Namespace.Type::Method"` for every METHOD_DEF row (None for a rid no method occupies
/// -- the rid-indexed layout keeps gaps as placeholder functions).
#[cfg(any(feature = "arm32", feature = "riscv32"))]
fn method_display_names(
    assembly: &Assembly,
    count: usize,
) -> Vec<Option<alloc::string::String>> {
    let mut names: Vec<Option<alloc::string::String>> = vec![None; count];
    for type_def in assembly.type_defs() {
        let Some(type_name) = type_def.name() else {
            continue;
        };
        for method in type_def.methods() {
            let rid = method.rid() as usize;
            let method_name = method.name().unwrap_or("?");
            if let Some(slot) = names.get_mut(rid) {
                *slot = Some(if type_name.namespace.is_empty() {
                    alloc::format!("{}::{}", type_name.name, method_name)
                } else {
                    alloc::format!(
                        "{}.{}::{}",
                        type_name.namespace, type_name.name, method_name
                    )
                });
            }
        }
    }
    names
}

/// The per-function symbol names for [`build_library_object`]: a cross-assembly-ACCESSIBLE static
/// method takes its stable cross-assembly symbol (`extern_method_symbol`), so a program links its
/// extern call against it; an accessible VIRTUAL instance method likewise, so a program type
/// inheriting it fills the vtable slot with an extern entry the linker resolves here
/// (cross-assembly dispatch of a never-overridden base virtual, e.g. `System.Object.ToString.` or
/// a BSP driver's inherited `protected virtual Dispose(bool)`); every other method keeps `f<rid>`
/// (internal). Accessible = Public, Family, or FamORAssem -- a `protected`/`protected internal`
/// member is exactly what a DERIVED type in another assembly calls (`base.Dispose(disposing)`)
/// or inherits into its vtable, so exporting only Public left those slots undefined at link.
///
/// **UNGATED, BECAUSE A CROSS-ASSEMBLY MONOMORPHIZED BODY NEEDS IT UNDER EVERY CODE MODEL.** A
/// call out of such a body names the symbol the OWNER's object defines, and this is the one function
/// that decides what that is (`library_function_symbols` is its caller). It was gated with the two
/// object emitters that call it directly; a `#[cfg]` on a function reached from ungated code is not
/// a smaller build, it is a build that does not exist.
fn library_symbol_names<'a>(
    assembly: &'a Assembly<'a>,
    references: &[&'a Assembly<'a>],
    count: usize,
    prefix: &str,
) -> Vec<alloc::string::String> {
    let mut names: Vec<alloc::string::String> =
        (0..count).map(|i| alloc::format!("{prefix}f{i}")).collect();
    for type_def in assembly.type_defs() {
        let Some(type_name) = type_def.name() else {
            continue;
        };
        for method in type_def.methods() {
            let rid = method.rid() as usize;
            if rid >= names.len() {
                continue;
            }
            let token = Token::new(table::METHOD_DEF, method.rid());
            let runtime_provided = assembly.is_runtime_provided(token);
            let is_synth_seam = runtime_provided
                && matches!(
                    synthesized_seam_body(
                        assembly,
                        references,
                        &type_name,
                        &method,
                        &method.signature()
                    ),
                    SeamEmission::Synthesized(_)
                );
            let is_plain_instance = !method.is_static()
                && !method.is_virtual()
                && !runtime_provided
                && method.body().is_some();
            if (method.is_static() || method.is_virtual() || is_synth_seam || is_plain_instance)
                && matches!(method.flags() & 0x7, 0x4..=0x6)
            {
                if let (Some(method_name), Some(sig)) =
                    (method.name(), crate::resolver::decodable_signature(&method))
                {
                    names[rid] = crate::resolver::extern_method_symbol(
                        type_name.namespace,
                        type_name.name,
                        method_name,
                        &sig.parameters,
                        &sig.return_type,
                        &|token| {
                            assembly
                                .type_token_name(token)
                                .map(|n| crate::resolver::joined_full_name(&n))
                        },
                    );
                }
            }
        }
    }
    let mut counts: alloc::collections::BTreeMap<alloc::string::String, u32> =
        alloc::collections::BTreeMap::new();
    for name in &names {
        *counts.entry(name.clone()).or_insert(0) += 1;
    }
    for (rid, name) in names.iter_mut().enumerate() {
        if counts.get(name.as_str()).copied().unwrap_or(0) > 1 {
            *name = alloc::format!("{prefix}f{rid}");
        }
    }
    names
}

/// The per-function symbol names for [`build_object`]: `f{rid}` by default (`f0` = the startup), but a
/// method marked `[UnmanagedCallersOnly]` takes its OWN method name instead. That makes it a global symbol
/// the linker resolves a `CallNative` against -- so a managed method can back a native seam: a C#
/// `lamella_gc_alloc` the AOT's own `new` then calls (the 100%-C# allocator/GC, no native stub).
#[cfg(feature = "arm32")]
fn object_symbol_names(assembly: &Assembly, count: usize) -> Vec<alloc::string::String> {
    let mut names: Vec<alloc::string::String> =
        (0..count).map(|i| alloc::format!("f{i}")).collect();
    let exports = assembly.unmanaged_callers_only();
    if !exports.is_empty() {
        for type_def in assembly.type_defs() {
            for method in type_def.methods() {
                let rid = method.rid();
                let Some(entry_point) = exports.get(&rid) else {
                    continue;
                };
                if (rid as usize) < names.len() {
                    if let Some(name) = entry_point.as_deref().or_else(|| method.name()) {
                        names[rid as usize] = name.into();
                    }
                }
            }
        }
    }
    names
}

/// Renames a LIBRARY's initialization thunks from the internal `L<hash>.f<index>` the symbol tables
/// default to, to the exported `L<hash>.init<type_row>` a linking program's trigger sites call.
///
/// **THE NAME IS THE INTERFACE, AND IT IS DERIVED FROM METADATA ON BOTH SIDES.** A program never
/// sees this library's function table, so an index-derived name would be a fact only one side holds;
/// `type_init_thunk_symbol` computes the same string from the content hash and the `TypeDef` row,
/// which both sides have. `thunks` supplies only WHICH SLOT to write into, and it comes from the one
/// place those indices are assigned rather than from a second derivation of `thunk_base`.
///
/// A slot past the end of `names` is skipped rather than panicking: `names` is sized from the same
/// `funcs.len()` the thunks were allocated within, so that cannot happen -- and if the two ever
/// disagreed, the thunk would simply keep its internal name and a linking program would fail to
/// resolve the symbol, which is loud.
#[cfg(any(feature = "arm32", feature = "riscv32"))]
fn name_type_init_thunks(
    assembly: &Assembly,
    thunks: &[(u32, u32)],
    names: &mut [alloc::string::String],
) {
    for (type_row, index) in thunks {
        let Some(symbol) = crate::resolver::type_init_thunk_symbol(assembly, *type_row) else {
            continue;
        };
        if let Some(slot) = names.get_mut(*index as usize) {
            *slot = symbol;
        }
    }
}

/// The MethodDef row of a static `Main` (the run-once widget entry), if the assembly has one.
fn find_main(assembly: &Assembly) -> Option<u32> {
    for type_def in assembly.type_defs() {
        for method in type_def.methods() {
            if method.is_static() && method.name() == Some("Main") {
                return Some(method.rid());
            }
        }
    }
    None
}

/// Every type initializer (`.cctor`) in the assembly, by `MethodDef` rid, in metadata order. The
/// startup runs these before `Main` so static field initializers (`static int X = 5;`) take effect.
fn find_cctors(assembly: &Assembly) -> Vec<u32> {
    let mut cctors = Vec::new();
    for type_def in assembly.type_defs() {
        for method in type_def.methods() {
            if method.is_static() && method.name() == Some(".cctor") {
                cctors.push(method.rid());
            }
        }
    }
    cctors
}

/// The type initializers the STARTUP still runs -- [`find_cctors`] minus the ones a trigger owns.
///
/// A type marked `beforefieldinit` licenses running its initializer at any time before first static
/// field access, so the startup chain remains a conformant place to run it and it stays here. A type
/// demanding precise timing does not: its initializer must run AT first access, which is what
/// [`type_init_thunk_body`] does, so running it here as well would defeat the trigger it was built
/// for -- the observable order would be eager again and every check would find the flag already set.
///
/// **THE SUBTRACTION IS THE WHOLE CHANGE IN BEHAVIOR, AND IT IS ONLY SAFE BECAUSE THE TRIGGER
/// EXISTS.** Removing a `.cctor` from this list without a site that calls it does not make the tier
/// lazy, it makes the initializer never run: `static-init-corlib` answers 2 instead of 42 that way,
/// a wrong answer rather than a smaller image.
fn startup_cctors<'x>(assembly: &'x Assembly<'x>, references: &[&'x Assembly<'x>]) -> Vec<u32> {
    let precise = crate::resolver::precise_init_types(assembly, references);
    find_cctors(assembly)
        .into_iter()
        .filter(|rid| !precise.iter().any(|(_, cctor, _)| cctor == rid))
        .collect()
}

/// [`startup_cctors`] across the assembly boundary: the type initializers of a REFERENCED assembly
/// that a linking program's startup still chains, by rid in that reference.
///
/// Same subtraction, decided by the same rule -- but it asks [`crate::resolver::cross_assembly_type_init`]
/// rather than [`crate::resolver::precise_init_types`], because across the boundary "demands precise
/// timing" is not sufficient on its own: the thunk must also be NAMEABLE. A reference with no file
/// bytes has no content hash and therefore no `L<hash>.init<row>` symbol for a site here to call, and
/// for such a type this keeps the `.cctor` in the chain and the trigger sites emit nothing. Eager,
/// which is a conformance deviation this tier already carried, rather than never -- which would be a
/// wrong answer.
///
/// **DROPPING ONE THAT HAS NO TRIGGER IS THE SILENT FAILURE, AND IT IS WHY THIS IS NOT
/// `find_cctors` MINUS `precise_init_types`.** The image still links and still boots; the type just
/// answers from zeroed storage. `static-init-corlib` scores exactly that shape as 2 instead of 42,
/// and `static-init-reference` scores it for a type that demands precise timing.
fn reference_startup_cctors(assembly: &Assembly) -> Vec<u32> {
    let triggered: Vec<u32> = assembly
        .type_defs()
        .filter_map(|type_def| {
            crate::resolver::cross_assembly_type_init(assembly, &type_def).map(|(cctor, _)| cctor)
        })
        .collect();
    find_cctors(assembly)
        .into_iter()
        .filter(|rid| !triggered.contains(rid))
        .collect()
}

/// The MIR type the AOT lowers a metadata signature type as.
///
/// **TOTAL EXCEPT FOR ONE SHAPE, AND THE ASYMMETRY IS THE POINT.** Every signature this function
/// does not name still falls to `int32` -- widening that would trade a known silent wrong answer for
/// an unknown loud one. The one case that refuses is the one measured to miscompile: an
/// instantiation of a VALUE type, whose MIR type needs a size and a trace map this tier does not
/// have. See [`BuildError::ValueTypeInstantiationSlot`].
///
/// **AND WHAT THE FALLBACK ACTUALLY COVERS IS KNOWN RATHER THAN SUSPECTED.** Four shapes could
/// reach it -- `void`, an open type's `!n`, a generic method's `!!n`, and a function pointer -- and
/// only the third ever did. A `void` return is tested before the call; an open TYPE's body is
/// skipped before lowering, and so is a generic METHOD's; and
/// a function pointer cannot arrive at all, because `ELEMENT_TYPE_FNPTR` has no decode arm and is a
/// loud `BadElementType` in the signature reader long before this.
///
/// **THIS IS THE HALF THE IMAGE COMES FROM.** `resolver::slot_types` types what a diagnostic
/// reads. The two are twins, they already carried comments saying so, and a refusal landed in that
/// one alone left every image byte-identical while the MIR dump reported it working. They now
/// key on ONE predicate (`generics::is_value_type_instantiation`) rather than on two arms written
/// out separately, which is the only version of "stay in step" a compiler can enforce.
fn mir_type<'x>(
    sig: &SigType,
    assembly: &'x Assembly<'x>,
    argument_world: Option<&'x Assembly<'x>>,
    references: &[&'x Assembly<'x>],
) -> Result<MirType, BuildError> {
    if crate::generics::is_value_type_instantiation(sig) {
        return crate::resolver::instantiated_value_type_slot(
            sig,
            assembly,
            references,
            &TargetLayout::ilp32(),
        )
        .ok_or_else(|| BuildError::ValueTypeInstantiationSlot {
            instantiation: crate::generics::spell_sig(assembly, sig).unwrap_or_else(|| {
                alloc::string::String::from("an unnameable value-type instantiation")
            }),
        });
    }
    Ok(match sig {
        SigType::I8 | SigType::U8 => MirType::I64,
        SigType::R4 => MirType::F32,
        SigType::R8 => MirType::F64,
        SigType::String
        | SigType::Object
        | SigType::Class(_)
        | SigType::SzArray(_)
        | SigType::Array { .. } => MirType::ObjectRef,
        SigType::GenericInst { definition, .. } if matches!(**definition, SigType::Class(_)) => {
            MirType::ObjectRef
        }
        SigType::Pointer(_) => MirType::NativeInt,
        SigType::ByRef(_) => MirType::ManagedPtr,
        SigType::IntPtr | SigType::UIntPtr => MirType::NativeInt,
        SigType::ValueType(token) => {
            if let Some(underlying) =
                crate::resolver::enum_underlying(assembly, *token, references, &TargetLayout::ilp32())
            {
                underlying
            } else {
                let layout = crate::resolver::value_type_layout_across(
                    assembly,
                    argument_world,
                    *token,
                    references,
                    &TargetLayout::ilp32(),
                );
                let size = layout.as_ref().map_or(0, |layout| layout.size);
                let refs = match layout.as_ref() {
                    Some(layout) => crate::resolver::ref_words_of(&layout.reference_offsets)
                        .ok_or_else(|| BuildError::ValueTypeTraceMap {
                            type_name: crate::generics::spell_sig(assembly, sig)
                                .unwrap_or_else(|| alloc::format!("{:#x}", token.0)),
                            size,
                        })?,
                    None => lamella_ir::RefWords::NONE,
                };
                MirType::ValueType {
                    handle: crate::resolver::qualified_handle_across(
                        assembly,
                        crate::resolver::marked_handle_token(*token, argument_world),
                        references,
                    ),
                    size,
                    refs,
                }
            }
        }
        _ => MirType::I32,
    })
}

/// A DEFERRED body for a program method that failed CIL->MIR under the deferring build: one
/// `Unreachable` block, which lowers to a hard trap. Reached means a LOUD fault at the exact
/// call site (never the library path's silent-truthy `bx lr`); unreached means `--gc-sections`
/// removes it and the image is byte-equivalent to a strict build of the reached set.
fn deferred_trap_body() -> Function {
    Function {
        params: Vec::new(),
        ret: None,
        value_types: Vec::new(),
        entry: BlockId(0),
        blocks: vec![BasicBlock {
            params: Vec::new(),
            insts: Vec::new(),
            terminator: Some(Terminator::Unreachable),
        }],
    }
}

/// A void no-op placeholder for a method that does not lower (never called by lowered code).
fn stub() -> Function {
    Function {
        params: Vec::new(),
        ret: None,
        value_types: Vec::new(),
        entry: BlockId(0),
        blocks: vec![BasicBlock {
            params: Vec::new(),
            insts: Vec::new(),
            terminator: Some(Terminator::Return(None)),
        }],
    }
}

/// The three states of a precise type's initialization flag. `0` is "not yet run", and it is the
/// zeroed static region rather than a value anything writes. `RAN` is "ran to completion";
/// `POISONED` is "ran and left an exception in flight", which is a state the flag has to carry
/// because the two cannot be told apart afterwards by looking at the type's statics -- a failed
/// initializer that assigned nothing and a successful one that assigned zero are the same bytes.
///
/// NOTE: the values are read ONLY by [`type_init_thunk_body`], which both writes them and compares
/// against them, so they are an internal encoding rather than an ABI. What IS cross-assembly is the
/// distinction being non-zero: a library's thunk and a program's trigger site agree only that the
/// word is zero before the first call, and the library owns the word.
const TYPE_INIT_RAN: i64 = 1;
const TYPE_INIT_POISONED: i64 = 2;

/// The initialization thunk for one type demanding precise timing: run its `.cctor` on the first
/// call, raise on every later one if that run threw, and do nothing on every later one if it did
/// not. Every trigger site for that type calls this, so the cost at a site is one call rather than
/// an inlined test -- which matters because the sites outnumber the types by an order of magnitude
/// (corlib: 13 sites, 1 type).
///
/// ```text
///   entry:  if flag != 0 -> already
///   run:    flag = RAN; cctor(); if g_exception_tag != 0 -> poison else -> done
///   poison: flag = POISONED; g_exception_tag = <TypeInitializationException> -> done
///   already: if flag == POISONED -> raise else -> done
///   raise:  g_exception_tag = <TypeInitializationException> -> done
///   done:   return
/// ```
///
/// **THE POISONED STATE IS REQUIRED BY THE ONCE-ONLY GUARANTEE, NOT BY A SEPARATE RULE, AND THE
/// CITATION THIS TIER CARRIED FOR IT WAS WRONG.** ECMA-335 4th ed **II.10.5.3.2 is "Relaxed
/// guarantees" -- the `beforefieldinit` clause** -- and says nothing about failure. The rule is
/// **II.10.5.3.1**, whose guarantee 2 is *"A type initializer shall be executed exactly once for any
/// given type, unless explicitly called by user code"* and whose guarantee 3 is *"No methods other
/// than those called directly or indirectly from the type initializer are able to access members of
/// a type before its initializer completes execution."* An initializer that threw has been executed
/// (so it shall not run again) and did not complete (so no access is permitted), and those two
/// together are exactly a poisoned type. The wrapping type comes from the CLI library
/// specification's `System.TypeInitializationException` -- *"When a static constructor fails to
/// initialize a type, a `TypeInitializationException` instance is created and passed a reference to
/// the exception thrown by the static constructor"* -- which is a library requirement and is not in
/// the ECMA-335 PDF at all. Partition I 12.4.2.4 adds that there is *"no guarantee when
/// `System.TypeInitializationException` might be thrown"*, which is why raising it at the trigger
/// site rather than at load time is conformant.
///
/// **AND THE INNER EXCEPTION IS LOST, WHICH IS A GAP IN THE EH MODEL AND NOT IN THIS THUNK.** An
/// in-flight exception here is a numeric TAG, not an object, so there is no instance to hold an
/// `InnerException`. The tag written is `System.TypeInitializationException`'s, and the tag the
/// `.cctor` left is OVERWRITTEN. That is the conformant half (a caller writing
/// `catch (TypeInitializationException)` matches, and so does `catch (Exception)`) and the
/// non-conformant half (the inner exception is unrecoverable) of one decision. It is stated here
/// rather than presented as compliance: the alternative -- letting the raw inner tag escape -- gets
/// the wrong catch clause, which is worse than losing detail.
///
/// **THE FLAG IS SET BEFORE THE CALL, NOT AFTER, AND THAT ORDER IS LOAD-BEARING FOR ORDINARY
/// CODE -- NOT AS A GUARD AGAINST AN EXOTIC SHAPE.** A type initializer reaches its own type almost
/// always, because assigning a static field is what an initializer is FOR: `Value = 40;` inside
/// `static Late()` is a `stsfld` on `Late`, which is a trigger site for `Late`. With the store
/// afterwards that re-entry finds the flag still clear and calls the initializer again, forever.
///
/// **RE-ENTRANCY IS THE NORMAL CASE.** With the two stores swapped, BOTH scored fixtures hard-fault
/// on emulated silicon -- the one written to have no deliberate re-entrancy fails identically to the
/// one written to have some. Setting the flag first makes the re-entrant call a no-op and lets the
/// outer one finish, which is what ECMA-335 I.8.9.5 requires: the initializer runs at most once, and
/// a re-entrant access sees whatever it has assigned so far rather than a second run.
///
/// Single-threaded: the flag is a plain word with no interlock. That matches this tier's threading
/// model today, and it is the piece that has to change first if a `.cctor` can ever run on two
/// threads at once -- the spec's own model is a lock, not a test-and-set.
#[cfg(any(feature = "arm32", feature = "riscv32", feature = "wasm"))]
fn type_init_thunk_body(flag_offset: u32, cctor: u32) -> Function {
    let tie_tag = i64::from(lamella_metadata::exception_tag_for_name(
        "System",
        "TypeInitializationException",
    ));
    const ENTRY: u32 = 0;
    const RUN: u32 = 1;
    const POISON: u32 = 2;
    const ALREADY: u32 = 3;
    const RAISE: u32 = 4;
    const DONE: u32 = 5;
    let flag = ValueId(0);
    let ran = ValueId(1);
    let poisoned = ValueId(2);
    let tie = ValueId(3);
    let stored_ran = ValueId(4);
    let called = ValueId(5);
    let in_flight = ValueId(6);
    let stored_poisoned = ValueId(7);
    let raised_on_failure = ValueId(8);
    let is_poisoned = ValueId(9);
    let raised_on_reaccess = ValueId(10);
    Function {
        params: Vec::new(),
        ret: None,
        value_types: vec![MirType::I32; 11],
        entry: BlockId(ENTRY),
        blocks: vec![
            BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        flag,
                        Inst::StaticLoad {
                            owner: StaticOwner::Own,
                            offset: flag_offset,
                        },
                    ),
                    (
                        ran,
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: TYPE_INIT_RAN,
                        },
                    ),
                    (
                        poisoned,
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: TYPE_INIT_POISONED,
                        },
                    ),
                    (
                        tie,
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: tie_tag,
                        },
                    ),
                ],
                terminator: Some(Terminator::Branch {
                    cond: flag,
                    if_true: BlockId(ALREADY),
                    true_args: Vec::new(),
                    if_false: BlockId(RUN),
                    false_args: Vec::new(),
                }),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        stored_ran,
                        Inst::StaticStore {
                            owner: StaticOwner::Own,
                            offset: flag_offset,
                            value: ran,
                        },
                    ),
                    (
                        called,
                        Inst::Call {
                            callee: cctor,
                            args: Vec::new(),
                        },
                    ),
                    (
                        in_flight,
                        Inst::StaticLoad {
                            owner: StaticOwner::Own,
                            offset: cil::G_EXCEPTION_TAG_OFFSET,
                        },
                    ),
                ],
                terminator: Some(Terminator::Branch {
                    cond: in_flight,
                    if_true: BlockId(POISON),
                    true_args: Vec::new(),
                    if_false: BlockId(DONE),
                    false_args: Vec::new(),
                }),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        stored_poisoned,
                        Inst::StaticStore {
                            owner: StaticOwner::Own,
                            offset: flag_offset,
                            value: poisoned,
                        },
                    ),
                    (
                        raised_on_failure,
                        Inst::StaticStore {
                            owner: StaticOwner::Own,
                            offset: cil::G_EXCEPTION_TAG_OFFSET,
                            value: tie,
                        },
                    ),
                ],
                terminator: Some(Terminator::Jump {
                    target: BlockId(DONE),
                    args: Vec::new(),
                }),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    is_poisoned,
                    Inst::Compare {
                        op: CmpOp::Eq,
                        lhs: flag,
                        rhs: poisoned,
                    },
                )],
                terminator: Some(Terminator::Branch {
                    cond: is_poisoned,
                    if_true: BlockId(RAISE),
                    true_args: Vec::new(),
                    if_false: BlockId(DONE),
                    false_args: Vec::new(),
                }),
            },
            BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    raised_on_reaccess,
                    Inst::StaticStore {
                        owner: StaticOwner::Own,
                        offset: cil::G_EXCEPTION_TAG_OFFSET,
                        value: tie,
                    },
                )],
                terminator: Some(Terminator::Jump {
                    target: BlockId(DONE),
                    args: Vec::new(),
                }),
            },
            BasicBlock {
                params: Vec::new(),
                insts: Vec::new(),
                terminator: Some(Terminator::Return(None)),
            },
        ],
    }
}

/// The program startup at index 0 (exported as `main`): runs each type initializer (`.cctor`) for
/// its side effects, then `return entry()`. With no `.cctor`s this is just `return entry()` -- the
/// plain trampoline.
///
/// **RUNNING EVERY INITIALIZER EARLY IS CONFORMANT FOR A `beforefieldinit` TYPE AND A DEVIATION FOR
/// THE REST.** ECMA-335 I.8.9.5 permits a marked type's initializer to run at any point at or before
/// the first access to one of its static fields, so the chain is exactly right for those. An
/// UNMARKED type is required to be triggered by first static-field access, first static-method call,
/// first value-type instance call or first construction, and running it before `Main` is early.
///
/// So the deviation is bounded by the UNMARKED, INITIALIZER-BEARING population and by nothing else.
/// `lamella-assemble` writes the flag under csc's rule -- every type except one declaring an
/// explicit `static C()` (`TYPE_BEFORE_FIELD_INIT` in `compile.rs`) -- which keeps that population
/// small. A cctor census prices it for a given assembly, and it reports
/// the two halves separately because a type carrying the flag WITHOUT an initializer costs nothing.
///
/// Precise, before-first-access initialization is what a trigger-site rewrite replaces this function
/// with; until then a caller gets eager order.
fn startup(init: Option<u32>, cctors: &[u32], entry_rid: u32) -> Function {
    startup_with_references(init, &[], cctors, entry_rid)
}

/// As [`startup`], but also chaining REFERENCE-assembly `.cctor`s (as extern `PInvoke` calls to
/// the library object's internal `L<hash>.f<rid>` symbols). ORDER:
/// board-init hook, then REFERENCE cctors, then the program's own, then the entry. Reference-FIRST
/// is the dependency direction: a program `.cctor` may call referenced-assembly surface that reads
/// referenced statics (`new AutoResetEvent(false)` reaches `WaitHandle.coordinator`), while a
/// reference `.cctor` cannot name program state at all -- corlib does not know the program. Each
/// call before the entry is void (its result is a dead placeholder).
fn startup_with_references(
    init: Option<u32>,
    reference_cctors: &[alloc::string::String],
    cctors: &[u32],
    entry_rid: u32,
) -> Function {
    let mut insts: Vec<(ValueId, Inst)> = Vec::new();
    for callee in init.into_iter() {
        insts.push((
            ValueId(insts.len() as u32),
            Inst::Call {
                callee,
                args: Vec::new(),
            },
        ));
    }
    for import in reference_cctors {
        insts.push((
            ValueId(insts.len() as u32),
            Inst::PInvoke {
                import: import.as_str().into(),
                args: Vec::new(),
            },
        ));
    }
    for &callee in cctors {
        insts.push((
            ValueId(insts.len() as u32),
            Inst::Call {
                callee,
                args: Vec::new(),
            },
        ));
    }
    insts.push((
        ValueId(insts.len() as u32),
        Inst::Call {
            callee: entry_rid,
            args: Vec::new(),
        },
    ));
    let result = ValueId((insts.len() - 1) as u32);
    Function {
        params: Vec::new(),
        ret: Some(MirType::I32),
        value_types: vec![MirType::I32; insts.len()],
        entry: BlockId(0),
        blocks: vec![BasicBlock {
            params: Vec::new(),
            insts,
            terminator: Some(Terminator::Return(Some(result))),
        }],
    }
}

/// The `MethodDef` rid of the method this assembly exports under native-seam name `export` (its
/// `[UnmanagedCallersOnly]` EntryPoint, or its own name), if any -- e.g. `lamella_time_init`, which the
/// startup chains in ahead of the `.cctor`s.
fn find_native_export(assembly: &Assembly, export: &str) -> Option<u32> {
    let marked = assembly.unmanaged_callers_only();
    if marked.is_empty() {
        return None;
    }
    for type_def in assembly.type_defs() {
        for method in type_def.methods() {
            let rid = method.rid();
            if let Some(entry_point) = marked.get(&rid) {
                if entry_point.as_deref().or_else(|| method.name()) == Some(export) {
                    return Some(rid);
                }
            }
        }
    }
    None
}

/// A synthesized MIR body for a `[RuntimeProvided]` `System.String` / `System.Array` reader, over the
/// AOT `[len: u32][data ...]` layout both share (an ObjectRef points at the `len` word). `get_Length`
/// loads the len word at `this + 0` -- for `String` the unit count, for `Array` the element count.
/// `Array.get_Length` MATTERS because lcsc emits `arr.Length` as a `callvirt` of this getter where csc
/// emits `ldlen`: without a synthesized body the placeholder's `return 0` lowers as-is and every
/// lcsc-compiled array length is 0 on device (Socket.Send(buffer) sent zero bytes). A rank-N (2D+)
/// rectangular array's TOTAL length (the dims' product) is a follow-up -- this reads its first header
/// word (dim0); the 1-D vector, every corlib use, is exact. `String.get_Chars(i)` loads the `u16` at
/// `this + 4 + i*2`, zero-extended to i32. These are non-virtual now (the getter-virtual fix), so a
/// program's `s.Length` / `arr.Length` / `s[i]` is a direct cross-assembly call that links to corlib's
/// copy of this. Returns `None` for a marked method this backend does not synthesize
/// (Substring/Concat/Console.*): it keeps its placeholder body, so a program calling one fails to LINK
/// loudly rather than binding a wrong value.
fn synthesize_runtime_reader(
    namespace: &str,
    type_name: &str,
    method_name: Option<&str>,
    param_count: usize,
) -> Option<Function> {
    if namespace != "System" || !matches!(type_name, "String" | "Array") {
        return None;
    }
    match (method_name, param_count) {
        (Some("get_Length"), 0) => Some(Function {
            params: vec![MirType::ObjectRef],
            ret: Some(MirType::I32),
            value_types: vec![MirType::ObjectRef, MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![
                    (
                        ValueId(1),
                        Inst::Convert {
                            value: ValueId(0),
                            kind: ConvKind::RefToInt,
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::Load {
                            address: ValueId(1),
                            width: 4,
                            signed: false,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        }),
        (Some("get_Chars"), 1) if type_name == "String" => Some(Function {
            params: vec![MirType::ObjectRef, MirType::I32],
            ret: Some(MirType::I32),
            value_types: vec![
                MirType::ObjectRef,
                MirType::I32,
                MirType::I32,
                MirType::I32,
                MirType::I32,
                MirType::I32,
                MirType::I32,
                MirType::I32,
                MirType::I32,
            ],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0), ValueId(1)],
                insts: vec![
                    (
                        ValueId(2),
                        Inst::Convert {
                            value: ValueId(0),
                            kind: ConvKind::RefToInt,
                        },
                    ),
                    (
                        ValueId(3),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 2,
                        },
                    ),
                    (
                        ValueId(4),
                        Inst::Binary {
                            op: BinOp::Mul,
                            lhs: ValueId(1),
                            rhs: ValueId(3),
                        },
                    ),
                    (
                        ValueId(5),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 4,
                        },
                    ),
                    (
                        ValueId(6),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(2),
                            rhs: ValueId(5),
                        },
                    ),
                    (
                        ValueId(7),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(6),
                            rhs: ValueId(4),
                        },
                    ),
                    (
                        ValueId(8),
                        Inst::Load {
                            address: ValueId(7),
                            width: 2,
                            signed: false,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(8)))),
            }],
        }),
        (Some("Substring"), 1) => Some(Function {
            params: vec![MirType::ObjectRef, MirType::I32],
            ret: Some(MirType::ObjectRef),
            value_types: vec![
                MirType::ObjectRef,
                MirType::I32,
                MirType::I32,
                MirType::I32,
                MirType::I32,
                MirType::ObjectRef,
            ],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0), ValueId(1)],
                insts: vec![
                    (
                        ValueId(2),
                        Inst::Convert {
                            value: ValueId(0),
                            kind: ConvKind::RefToInt,
                        },
                    ),
                    (
                        ValueId(3),
                        Inst::Load {
                            address: ValueId(2),
                            width: 4,
                            signed: false,
                        },
                    ),
                    (
                        ValueId(4),
                        Inst::Binary {
                            op: BinOp::Sub,
                            lhs: ValueId(3),
                            rhs: ValueId(1),
                        },
                    ),
                    (
                        ValueId(5),
                        Inst::PInvoke {
                            import: "lamella_string_substring".into(),
                            args: vec![ValueId(2), ValueId(1), ValueId(4)],
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(5)))),
            }],
        }),
        (Some("Substring"), 2) => Some(Function {
            params: vec![MirType::ObjectRef, MirType::I32, MirType::I32],
            ret: Some(MirType::ObjectRef),
            value_types: vec![
                MirType::ObjectRef,
                MirType::I32,
                MirType::I32,
                MirType::I32,
                MirType::ObjectRef,
            ],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0), ValueId(1), ValueId(2)],
                insts: vec![
                    (
                        ValueId(3),
                        Inst::Convert {
                            value: ValueId(0),
                            kind: ConvKind::RefToInt,
                        },
                    ),
                    (
                        ValueId(4),
                        Inst::PInvoke {
                            import: "lamella_string_substring".into(),
                            args: vec![ValueId(3), ValueId(1), ValueId(2)],
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(4)))),
            }],
        }),
        (Some("CreateFromChars"), 3) => Some(Function {
            params: vec![MirType::ObjectRef, MirType::I32, MirType::I32],
            ret: Some(MirType::ObjectRef),
            value_types: vec![
                MirType::ObjectRef,
                MirType::I32,
                MirType::I32,
                MirType::I32,
                MirType::ObjectRef,
            ],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0), ValueId(1), ValueId(2)],
                insts: vec![
                    (
                        ValueId(3),
                        Inst::Convert {
                            value: ValueId(0),
                            kind: ConvKind::RefToInt,
                        },
                    ),
                    (
                        ValueId(4),
                        Inst::PInvoke {
                            import: "lamella_string_substring".into(),
                            args: vec![ValueId(3), ValueId(1), ValueId(2)],
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(4)))),
            }],
        }),
        _ => None,
    }
}

/// A synthesized MIR body for the two `[RuntimeProvided]` seams that make `typeof(T)` real.
///
/// `System.Type` is HANDLE-BACKED (the model `corlib/System/Type.cs` documents): `typeof(T)` compiles
/// to `ldtoken T ; call GetTypeFromHandle`, and `ldtoken` already lowers to the type's CANONICAL
/// descriptor address (`Inst::TypeDescAddr`). So that address IS the `Type`, which makes
/// `GetTypeFromHandle(handle)` the IDENTITY -- it retypes the incoming word to a reference and
/// returns it (`IntToRef` is a pure retype, a no-op at the machine level) -- and makes
/// `HandleEquals(a, b)` a POINTER COMPARE of the two descriptor addresses.
/// A descriptor is canonical per type (one symbol, dedup'd strong/weak across assemblies), so pointer
/// equality IS type identity: `typeof(P) == typeof(P)` holds and two distinct types never collide.
///
/// Without these the placeholders lower AS WRITTEN -- `GetTypeFromHandle` returns `null` and
/// `HandleEquals` returns `false` -- so `typeof(P) == typeof(P)` is silently FALSE on device (the
/// `[RuntimeProvided]`-compiled-out-returns-default hazard). Both seams are static: no receiver.
fn synthesize_type_seam(
    namespace: &str,
    type_name: &str,
    method_name: Option<&str>,
    param_count: usize,
) -> Option<Function> {
    if (namespace, type_name) != ("System", "Type") {
        return None;
    }
    match (method_name, param_count) {
        (Some("GetTypeFromHandle"), 1) => Some(Function {
            params: vec![MirType::I32],
            ret: Some(MirType::ObjectRef),
            value_types: vec![MirType::I32, MirType::ObjectRef],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![(
                    ValueId(1),
                    Inst::Convert {
                        value: ValueId(0),
                        kind: ConvKind::IntToRef,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        }),
        (Some("HandleEquals"), 2) => Some(Function {
            params: vec![MirType::ObjectRef, MirType::ObjectRef],
            ret: Some(MirType::I32),
            value_types: vec![
                MirType::ObjectRef,
                MirType::ObjectRef,
                MirType::I32,
                MirType::I32,
                MirType::I32,
            ],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0), ValueId(1)],
                insts: vec![
                    (
                        ValueId(2),
                        Inst::Convert {
                            value: ValueId(0),
                            kind: ConvKind::RefToInt,
                        },
                    ),
                    (
                        ValueId(3),
                        Inst::Convert {
                            value: ValueId(1),
                            kind: ConvKind::RefToInt,
                        },
                    ),
                    (
                        ValueId(4),
                        Inst::Compare {
                            op: CmpOp::Eq,
                            lhs: ValueId(2),
                            rhs: ValueId(3),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(4)))),
            }],
        }),
        _ => None,
    }
}

/// A synthesized MIR body for a `System.Console` output overload. It threads an optional argument
/// (`param`) -- reinterpreted from an ObjectRef to a raw pointer first when `ref_to_int` (the string
/// form) -- into a runtime-support value-writer (`writer`), then optionally a trailing newline. So
/// `Write(x)` = the writer, `WriteLine(x)` = the writer + newline, `WriteLine()` = just the newline.
/// All are void `[RuntimeProvided]` statics; the object path rewrites each `PInvoke` to a `CallNative`
/// the linker resolves against `tools/runtime/runtime-support`. The writer matches the interpreter / .NET
/// formatting (signed/unsigned decimal, `True`/`False`, a `char`'s code unit).
fn console_body(
    param: Option<MirType>,
    ref_to_int: bool,
    writer: Option<&str>,
    newline: bool,
) -> Function {
    let mut value_types: Vec<MirType> = Vec::new();
    let mut block_params: Vec<ValueId> = Vec::new();
    let mut insts: Vec<(ValueId, Inst)> = Vec::new();
    let mut next = 0u32;
    let arg = param.map(|ty| {
        value_types.push(ty);
        block_params.push(ValueId(next));
        next += 1;
        ValueId(next - 1)
    });
    let write_arg = if ref_to_int {
        let src = arg.expect("ref_to_int implies a parameter");
        value_types.push(MirType::I32);
        next += 1;
        insts.push((
            ValueId(next - 1),
            Inst::Convert {
                value: src,
                kind: ConvKind::RefToInt,
            },
        ));
        Some(ValueId(next - 1))
    } else {
        arg
    };
    if let Some(symbol) = writer {
        value_types.push(MirType::I32);
        next += 1;
        insts.push((
            ValueId(next - 1),
            Inst::PInvoke {
                import: symbol.into(),
                args: write_arg.map(|a| vec![a]).unwrap_or_default(),
            },
        ));
    }
    if newline {
        value_types.push(MirType::I32);
        next += 1;
        insts.push((
            ValueId(next - 1),
            Inst::PInvoke {
                import: "lamella_console_newline".into(),
                args: Vec::new(),
            },
        ));
    }
    Function {
        params: param.map(|p| vec![p]).unwrap_or_default(),
        ret: None,
        value_types,
        entry: BlockId(0),
        blocks: vec![BasicBlock {
            params: block_params,
            insts,
            terminator: Some(Terminator::Return(None)),
        }],
    }
}

/// A synthesized MIR body for `System.Double.ToString()` (its `[RuntimeProvided]` placeholder). `this` is
/// a managed pointer to the `f64` value; load the 8-byte value from it and hand it to the runtime-support
/// `lamella_double_to_string`, which formats it (byte-identical to the interpreter's `format_double`) and
/// returns a GC-allocated `[len: u32][u16 units ...]` string. The object path rewrites the `PInvoke` to a
/// `CallNative` the linker resolves against `tools/runtime/runtime-support`. `this` is dead by the allocating call,
/// so nothing improper is a GC root there; the returned ObjectRef is rooted as the live result.
fn double_to_string_body() -> Function {
    Function {
        params: vec![MirType::ManagedPtr],
        ret: Some(MirType::ObjectRef),
        value_types: vec![MirType::ManagedPtr, MirType::F64, MirType::ObjectRef],
        entry: BlockId(0),
        blocks: vec![BasicBlock {
            params: vec![ValueId(0)],
            insts: vec![
                (
                    ValueId(1),
                    Inst::Load {
                        address: ValueId(0),
                        width: 8,
                        signed: false,
                    },
                ),
                (
                    ValueId(2),
                    Inst::PInvoke {
                        import: "lamella_double_to_string".into(),
                        args: vec![ValueId(1)],
                    },
                ),
            ],
            terminator: Some(Terminator::Return(Some(ValueId(2)))),
        }],
    }
}

/// A synthesized MIR body for `System.Char.ToString()` (its `[RuntimeProvided]` placeholder). `this` is a
/// managed pointer to the `char`; load the code unit and hand it to the runtime-support
/// `lamella_char_to_string`, which allocates a one-unit `[len][u16]` string. Like `Double.ToString`, the
/// object path turns the `PInvoke` into a linker-resolved `CallNative`.
fn char_to_string_body() -> Function {
    Function {
        params: vec![MirType::ManagedPtr],
        ret: Some(MirType::ObjectRef),
        value_types: vec![MirType::ManagedPtr, MirType::I32, MirType::ObjectRef],
        entry: BlockId(0),
        blocks: vec![BasicBlock {
            params: vec![ValueId(0)],
            insts: vec![
                (
                    ValueId(1),
                    Inst::Load {
                        address: ValueId(0),
                        width: 2,
                        signed: false,
                    },
                ),
                (
                    ValueId(2),
                    Inst::PInvoke {
                        import: "lamella_char_to_string".into(),
                        args: vec![ValueId(1)],
                    },
                ),
            ],
            terminator: Some(Terminator::Return(Some(ValueId(2)))),
        }],
    }
}

/// What the `[RuntimeProvided]` SYNTHESIS TABLE ([`synthesized_seam_body`]) produced for one marked
/// method. The three cases are what the seam census reports on
/// ([`LibraryBuildReport::unsynthesized_seams`]), and they differ in one property: whether a caller
/// can tell the seam is missing.
enum SeamEmission {
    /// A real body -- this build backs the seam, and it is not a gap at all.
    Synthesized(Function),
    /// A TRAP body: the gap is loud. Reaching it faults instead of answering.
    Trap(Function),
    /// Nothing: the assembly's own placeholder body lowers as written, so the seam answers a
    /// constant and the caller cannot tell.
    Placeholder,
}

/// The `System.Console` output overloads: a body that formats + writes the argument over the device
/// console seam, or `None` for an overload this table does not back (the managed `decimal` forms,
/// which have real bodies). Routed by the parameter's EXACT signature type -- `int` vs `uint`, `long`
/// vs `ulong` differ only in signedness, which the MIR type collapses, so the sink symbol is what
/// distinguishes them.
///
/// `Write` and `WriteLine` come in PAIRS, differing only in the trailing newline, and the pairing is
/// a test rather than a convention: four `Write` overloads were missing here while their `WriteLine`
/// twins were present, so `Console.Write(true)` and `Console.Write(1L)` compiled to the corlib
/// placeholder and printed NOTHING -- on the program's primary observable, which is how a user
/// diagnoses everything else.
#[cfg(any(feature = "arm32", feature = "riscv32", feature = "wasm"))]
fn console_seam_body(name: Option<&str>, params: &[SigType]) -> Option<Function> {
    let s = |sym| Some(sym);
    match (name, params) {
    (Some("WriteLine"), []) => Some(console_body(None, false, None, true)),
    (Some("Write"), [SigType::String]) => Some(console_body(
        Some(MirType::ObjectRef),
        true,
        s("lamella_console_write"),
        false,
    )),
    (Some("WriteLine"), [SigType::String]) => Some(console_body(
        Some(MirType::ObjectRef),
        true,
        s("lamella_console_write"),
        true,
    )),
    (Some("Write"), [SigType::I4]) => Some(console_body(
        Some(MirType::I32),
        false,
        s("lamella_console_write_i32"),
        false,
    )),
    (Some("WriteLine"), [SigType::I4]) => Some(console_body(
        Some(MirType::I32),
        false,
        s("lamella_console_write_i32"),
        true,
    )),
    (Some("Write"), [SigType::U4]) => Some(console_body(
        Some(MirType::I32),
        false,
        s("lamella_console_write_u32"),
        false,
    )),
    (Some("WriteLine"), [SigType::U4]) => Some(console_body(
        Some(MirType::I32),
        false,
        s("lamella_console_write_u32"),
        true,
    )),
    (Some("Write"), [SigType::Char]) => Some(console_body(
        Some(MirType::I32),
        false,
        s("lamella_console_write_char"),
        false,
    )),
    (Some("WriteLine"), [SigType::Char]) => Some(console_body(
        Some(MirType::I32),
        false,
        s("lamella_console_write_char"),
        true,
    )),
    (Some("Write"), [SigType::Boolean]) => Some(console_body(
        Some(MirType::I32),
        false,
        s("lamella_console_write_bool"),
        false,
    )),
    (Some("WriteLine"), [SigType::Boolean]) => Some(console_body(
        Some(MirType::I32),
        false,
        s("lamella_console_write_bool"),
        true,
    )),
    (Some("Write"), [SigType::I8]) => Some(console_body(
        Some(MirType::I64),
        false,
        s("lamella_console_write_i64"),
        false,
    )),
    (Some("WriteLine"), [SigType::I8]) => Some(console_body(
        Some(MirType::I64),
        false,
        s("lamella_console_write_i64"),
        true,
    )),
    (Some("Write"), [SigType::U8]) => Some(console_body(
        Some(MirType::I64),
        false,
        s("lamella_console_write_u64"),
        false,
    )),
    (Some("WriteLine"), [SigType::U8]) => Some(console_body(
        Some(MirType::I64),
        false,
        s("lamella_console_write_u64"),
        true,
    )),
        _ => None,
    }
}

/// The MIR types of a CLOSED-LIST seam's parameters, or `None` when one of them does not type.
///
/// Every caller sits behind a table that names its method by hand (`net_seam_import`,
/// `thread_seam_import`, `monitor_seam_import`, `Thread.StartThread`), so the one shape
/// `mir_type` refuses -- an instantiation of a value type -- cannot reach a signature here. It is
/// threaded anyway rather than unwrapped, because **a refusal a caller maps to a default is not a
/// refusal**, and that is the shape this tier has now paid for five times.
///
/// Declining to synthesize is the LOUD outcome, not a quiet one: the seam census records the method
/// as unsynthesized, and [`BuildError::SilentSeamCallEdge`] then refuses any build whose code calls
/// it. So an untypable seam becomes a named row and, if reachable, a failed build.
fn seam_param_types<'x>(
    parameters: &[SigType],
    assembly: &'x Assembly<'x>,
    references: &[&'x Assembly<'x>],
) -> Option<Vec<MirType>> {
    parameters
        .iter()
        .map(|p| mir_type(p, assembly, None, references).ok())
        .collect()
}

/// THE `[RuntimeProvided]` SYNTHESIS TABLE: the body this backend supplies for a marked method whose
/// real one lives in the runtime, or [`SeamEmission::Placeholder`] when it supplies none.
///
/// Factored out of the lowering loop so it can be asked WITHOUT lowering an assembly -- the caller
/// audit needs "would this seam be synthesized?" for a REFERENCE's method, and a second copy of the
/// answer is the one thing that must not exist (a seam the audit thinks is synthesized and the
/// emission leaves a placeholder is exactly the silent wrong answer the audit is for).
#[allow(clippy::too_many_lines)]
fn synthesized_seam_body<'a>(
    assembly: &'a Assembly<'a>,
    references: &[&'a Assembly<'a>],
    type_name: &lamella_metadata::TypeName<'a>,
    method: &lamella_metadata::Method<'a>,
    signature: &Option<lamella_metadata::MethodSig>,
) -> SeamEmission {
    let params = signature.as_ref().map_or(0, |sig| sig.parameters.len());
    if let Some(func) = synthesize_runtime_reader(
        type_name.namespace,
        type_name.name,
        method.name(),
        params,
    ) {
        return SeamEmission::Synthesized(func);
    }
    if let Some(func) =
        synthesize_type_seam(type_name.namespace, type_name.name, method.name(), params)
    {
        return SeamEmission::Synthesized(func);
    }
    if (type_name.namespace, type_name.name) == ("System", "Object")
        && method.name() == Some("ToString")
        && params == 0
    {
        return SeamEmission::Synthesized(Function {
            params: vec![MirType::ObjectRef],
            ret: Some(MirType::ObjectRef),
            value_types: vec![MirType::ObjectRef, MirType::I32, MirType::ObjectRef],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![
                    (
                        ValueId(1),
                        Inst::LoadTypeDesc {
                            object: ValueId(0),
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::TypeName {
                            descriptor: ValueId(1),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        });
    }
    if (type_name.namespace, type_name.name) == ("System", "Console") {
        let params = signature
            .as_ref()
            .map(|s| s.parameters.as_slice())
            .unwrap_or(&[]);
        if let Some(func) = console_seam_body(method.name(), params) {
            return SeamEmission::Synthesized(func);
        }
    }
    if (type_name.namespace, type_name.name) == ("System", "Double")
        && method.name() == Some("ToString")
        && params == 0
    {
        return SeamEmission::Synthesized(double_to_string_body());
    }
    if (type_name.namespace, type_name.name) == ("System", "Char")
        && method.name() == Some("ToString")
        && params == 0
    {
        return SeamEmission::Synthesized(char_to_string_body());
    }
    if (type_name.namespace, type_name.name) == ("System", "Delegate") && params == 2 {
        if method.name() == Some("Combine") {
            return SeamEmission::Synthesized(delegate_combine_body());
        }
        if method.name() == Some("Remove") {
            return SeamEmission::Synthesized(delegate_remove_body());
        }
    }
    if (type_name.namespace, type_name.name) == ("Lamella.Hardware", "Mmio") {
        let mmio_body = match (method.name(), params) {
            (Some("Read8"), 1) => Some(mmio_read_body(1)),
            (Some("Read16"), 1) => Some(mmio_read_body(2)),
            (Some("Read32"), 1) => Some(mmio_read_body(4)),
            (Some("Write8"), 2) => Some(mmio_write_body(1)),
            (Some("Write16"), 2) => Some(mmio_write_body(2)),
            (Some("Write32"), 2) => Some(mmio_write_body(4)),
            _ => None,
        };
        if let Some(body) = mmio_body {
            return SeamEmission::Synthesized(body);
        }
    }
    if (type_name.namespace, type_name.name) == ("System", "Array") {
        let core = match (method.name(), params) {
            (Some("CopyCore"), 5) => Some(array_copy_core_body()),
            (Some("ClearCore"), 3) => Some(array_clear_core_body()),
            (Some("get_Rank"), 0) => Some(array_rank_body()),
            (Some("GetValue"), 1) => Some(array_get_value_body()),
            (Some("SetValue"), 2) => Some(array_set_value_body()),
            (Some("Clone"), 0) => Some(array_clone_body()),
            _ => None,
        };
        if let Some(body) = core {
            return SeamEmission::Synthesized(body);
        }
    }
    if let Some(import) =
        net_seam_import(type_name.namespace, type_name.name, method.name())
    {
        if let Some(sig) = &signature {
            let Some(param_types) = seam_param_types(&sig.parameters, assembly, references)
            else {
                return SeamEmission::Trap(deferred_trap_body());
            };
            return SeamEmission::Synthesized(runtime_seam_body(
                &param_types,
                &sig.parameters,
                !matches!(sig.return_type, SigType::Void),
                net_seam_folds_buffer(method.name()),
                import,
            ));
        }
    }
    if net_seam_deferred(type_name.namespace, type_name.name, method.name()) {
        if let Some(sig) = &signature {
            let Some(param_types) = seam_param_types(&sig.parameters, assembly, references)
            else {
                return SeamEmission::Trap(deferred_trap_body());
            };
            return SeamEmission::Synthesized(net_deferred_body(&param_types, -2));
        }
    }
    if (type_name.namespace, type_name.name) == ("System.Threading", "Thread")
        && method.name() == Some("StartThread")
    {
        if let (Some(sig), Some(entry_rid)) = (
            &signature,
            find_method_rid(assembly, "System.Threading", "Thread", "ThreadEntry"),
        ) {
            let Some(param_types) = seam_param_types(&sig.parameters, assembly, references)
            else {
                return SeamEmission::Trap(deferred_trap_body());
            };
            return SeamEmission::Synthesized(thread_start_body(&param_types, entry_rid));
        }
    }
    if let Some(import) =
        thread_seam_import(type_name.namespace, type_name.name, method.name())
    {
        if let Some(sig) = &signature {
            let Some(param_types) = seam_param_types(&sig.parameters, assembly, references)
            else {
                return SeamEmission::Trap(deferred_trap_body());
            };
            return SeamEmission::Synthesized(runtime_seam_body(
                &param_types,
                &sig.parameters,
                !matches!(sig.return_type, SigType::Void),
                false,
                import,
            ));
        }
    }
    if let Some(import) =
        monitor_seam_import(type_name.namespace, type_name.name, method.name())
    {
        if let Some(sig) = &signature {
            let Some(param_types) = seam_param_types(&sig.parameters, assembly, references)
            else {
                return SeamEmission::Trap(deferred_trap_body());
            };
            return SeamEmission::Synthesized(runtime_seam_body(
                &param_types,
                &sig.parameters,
                !matches!(sig.return_type, SigType::Void),
                false,
                import,
            ));
        }
    }
    if (type_name.namespace, type_name.name) == ("System", "Array") {
        return SeamEmission::Trap(deferred_trap_body());
    }
    SeamEmission::Placeholder
}

/// Lowers an assembly's methods to a `Vec<Function>` keyed by MethodDef row. Index 0 is a trampoline
/// to `entry` (if any) -- the `main` export -- or a stub. A method that does not lower stays a stub.
/// `references` are the referenced assemblies (corlib first) for cross-assembly vtable-slot
/// agreement, in resolution order; empty for this-assembly-relative numbering.
fn lower_assembly<'a>(
    assembly: &'a Assembly<'a>,
    entry: Option<u32>,
    references: &[&'a Assembly<'a>],
) -> Result<(Vec<Function>, crate::generics::MonoPlan), BuildError> {
    let (funcs, _maps, fails, duplicates, plan) =
        lower_assembly_debug(assembly, entry, references)?;
    refuse_duplicate_bodies(&duplicates)?;
    if let Some((rid, error)) = fails.into_iter().next() {
        return Err(BuildError::LowerCil { rid, error });
    }
    Ok((funcs, plan))
}

/// As [`lower_assembly`], but also returns each function's [`cil::CilSourceMap`] (rid-indexed, empty for
/// the trampoline and the stub gaps) -- so the SAME image build()'s chip path produces also carries debug
/// info, and a debugger's line tables match the flashed layout by construction.
fn lower_assembly_debug<'a>(
    assembly: &'a Assembly<'a>,
    entry: Option<u32>,
    references: &[&'a Assembly<'a>],
) -> Result<LoweredAssemblyDebug, BuildError> {
    let (funcs, maps, fails, _seams, duplicates, _thunks, plan) =
        lower_assembly_seams(assembly, entry, references)?;
    Ok((funcs, maps, fails, duplicates, plan))
}

/// What [`lower_assembly_debug`] returns: [`LoweredAssembly`] without the seam census, which its
/// callers do not read.
type LoweredAssemblyDebug = (
    Vec<Function>,
    Vec<cil::CilSourceMap>,
    Vec<(u32, cil::CilError)>,
    Vec<u32>,
    crate::generics::MonoPlan,
);

/// The rid-indexed function table, with the 1:1 invariant MADE OBSERVABLE.
///
/// A program is a `Vec<Function>` indexed by MethodDef rid and every emitted symbol is `f<rid>`, so
/// writing a body is an index assignment: a SECOND body for one row does not collide, it REPLACES
/// the first, and the image is built around whichever won with no diagnostic anywhere. This type
/// exists so that write cannot happen without being recorded. It does not refuse -- the caller
/// does, through [`refuse_duplicate_bodies`] -- because the lowering has a lenient path and the
/// decision about what is tolerable belongs at the build entry point, not here.
///
/// Unreachable from well-formed C# 1.0 metadata, where one row means one method means one body. A
/// monomorphizer meets it immediately, because N instantiations of one generic method share a rid.
struct BodySlots {
    /// The bodies, indexed by MethodDef rid.
    funcs: Vec<Function>,
    /// Which rids [`Self::write`] has already filled. Not derivable from `funcs` -- every slot
    /// starts as a `stub()` and a legitimately-stubbed body is indistinguishable from an empty one.
    written: Vec<bool>,
    /// The rids a second body was written to, in the order the collisions happened.
    duplicates: Vec<u32>,
}

impl BodySlots {
    fn new(len: usize) -> Self {
        Self {
            funcs: (0..len).map(|_| stub()).collect(),
            written: alloc::vec![false; len],
            duplicates: Vec::new(),
        }
    }

    /// Writes `func` as the body of MethodDef row `rid`, recording the rid if it already had one.
    /// The FIRST body is the one that is lost, matching the existing assignment's behavior exactly
    /// -- this changes what is observable, not what is emitted.
    fn write(&mut self, rid: u32, func: Function) {
        let slot = rid as usize;
        if self.written[slot] {
            self.duplicates.push(rid);
        }
        self.written[slot] = true;
        self.funcs[slot] = func;
    }
}

/// Turns the rows [`lower_assembly_seams`] saw a second body written to into a refusal.
///
/// One call per build entry point rather than one check inside the lowering, because the lowering
/// has a LENIENT path that tolerates CIL failures (`build_object_with_libraries_report` collects
/// them into a report instead of refusing), and a duplicate body must not be tolerable there: a
/// method that failed to lower is a known hole a caller can read, while a method that lowered TWICE
/// is a wrong answer nothing downstream can see.
fn refuse_duplicate_bodies(duplicates: &[u32]) -> Result<(), BuildError> {
    match duplicates.first() {
        Some(&rid) => Err(BuildError::DuplicateMethodBody {
            rid,
            total: duplicates.len(),
        }),
        None => Ok(()),
    }
}

/// What [`lower_assembly_seams`] observed: the rid-indexed functions and source maps, the methods
/// whose CIL did not lower, the `[RuntimeProvided]` seams nothing synthesized, and any MethodDef row
/// a SECOND body was written to (see [`BuildError::DuplicateMethodBody`]) -- and the initialization
/// thunks it emitted as `(TypeDef row, function index)`, which a library build needs to give each
/// one the exported name a linking program's trigger sites call it by.
///
/// **AND THE MONOMORPHIZATION PLAN, WHICH IS HANDED OUT RATHER THAN RE-DERIVED.** The caller needs
/// it to build the descriptor table -- an instantiation's descriptor is keyed by the plan's
/// spelling -- and deriving it a second time means two answers that agree until one meets a case the
/// other does not. It is decided here, once, beside the `max_rid` it is indexed from.
type LoweredAssembly = (
    Vec<Function>,
    Vec<cil::CilSourceMap>,
    Vec<(u32, cil::CilError)>,
    Vec<SeamRow>,
    Vec<u32>,
    Vec<(u32, u32)>,
    crate::generics::MonoPlan,
);

/// One census row as the lowering decided it: the seam's rid, what was emitted in its place, and
/// whether every `call` to it is FOLDED to an intrinsic. The fold flag is captured HERE, beside the
/// synthesis decision, so the report cannot answer differently from the emission -- the same reason
/// `seams` records the disposition rather than re-deriving it.
type SeamRow = (u32, SeamDisposition, bool);

/// As [`lower_assembly_debug`], but ALSO returns the `[RuntimeProvided]` seams this build did not
/// synthesize -- the third silent-demotion layer, alongside the CIL->MIR fails and the object-emit
/// stubs (see [`LibraryBuildReport`]). A caller that must not ship a silent wrong answer reads this;
/// [`lower_assembly_debug`] is the thin wrapper for the callers that do not, so their bytes are
/// unchanged by construction.
fn lower_assembly_seams<'a>(
    assembly: &'a Assembly<'a>,
    entry: Option<u32>,
    references: &[&'a Assembly<'a>],
) -> Result<LoweredAssembly, BuildError> {
    let mut methods = Vec::new();
    let mut max_rid = entry.unwrap_or(0);
    let generic_types = assembly.type_parameter_names();
    let generic_methods = assembly.method_type_parameter_names();
    for type_def in assembly.type_defs() {
        let type_name = type_def.name();
        let delegate = crate::resolver::is_delegate_type_of(assembly, &type_def);
        let open_generic = generic_types.contains_key(&type_def.token().row());
        for method in type_def.methods() {
            let rid = method.rid();
            max_rid = max_rid.max(rid);
            if open_generic || generic_methods.contains_key(&rid) {
                continue;
            }
            methods.push((rid, method, type_name, delegate));
        }
    }
    let plan = crate::generics::MonoPlan::for_assembly_with_references(
        assembly,
        references,
        max_rid + 1,
    )
    .map_err(BuildError::Instantiations)?;
    let precise = crate::resolver::precise_init_types(assembly, references);
    let thunk_base = max_rid as usize + 1 + plan.len();
    let total = thunk_base + precise.len();
    let mut bodies = BodySlots::new(total);
    let mut maps: Vec<cil::CilSourceMap> =
        (0..total).map(|_| cil::CilSourceMap(Vec::new())).collect();
    if let Some(entry_rid) = entry {
        bodies.funcs[0] = startup(
            find_native_export(assembly, "lamella_time_init"),
            &startup_cctors(assembly, references),
            entry_rid,
        );
    }
    let thunk_indices: Vec<(u32, u32)> = precise
        .iter()
        .enumerate()
        .map(|(i, (type_row, _, _))| (*type_row, (thunk_base + i) as u32))
        .collect();
    let resolver = MetadataResolver::new(assembly)
        .with_references(references)
        .with_monomorphized(plan.clone())
        .with_type_init_thunks(thunk_indices.clone());
    refuse_undispatchable_instantiations(&resolver)?;
    let mut fails: Vec<(u32, cil::CilError)> = Vec::new();
    let mut seams: Vec<SeamRow> = Vec::new();
    for (rid, method, type_name, is_delegate) in &methods {
        let signature = method.signature();
        let Some(body) = method.body() else {
            if let Some(func) = delegate_invoke_synthesis(
                assembly,
                resolver.references(),
                *is_delegate,
                method.name(),
                &signature,
            )? {
                bodies.write(*rid, func);
            }
            continue;
        };
        let folded = type_name.as_ref().is_some_and(|name| {
            crate::resolver::folded_intrinsic(
                name.namespace,
                name.name,
                method.name(),
                signature
                    .as_ref()
                    .map_or(&[][..], |sig| sig.parameters.as_slice()),
            )
            .is_some()
        });
        if assembly.is_runtime_provided(Token::new(table::METHOD_DEF, method.rid())) {
            let emission = type_name.as_ref().map_or(SeamEmission::Placeholder, |name| {
                synthesized_seam_body(assembly, references, name, method, &signature)
            });
            match emission {
                SeamEmission::Synthesized(func) => {
                    bodies.write(*rid, func);
                    continue;
                }
                SeamEmission::Trap(func) => {
                    bodies.write(*rid, func);
                    seams.push((*rid, SeamDisposition::Trap, folded));
                    continue;
                }
                SeamEmission::Placeholder => {
                    seams.push((*rid, SeamDisposition::Placeholder, folded));
                }
            }
        }
        let mut arg_types = Vec::new();
        if let Some(sig) = &signature {
            if sig.has_this {
                arg_types.push(MirType::ObjectRef);
            }
            for parameter in &sig.parameters {
                arg_types.push(mir_type(parameter, assembly, None, resolver.references())?);
            }
        }
        let local_types: Vec<MirType> = method
            .local_variables()
            .iter()
            .map(|sig| mir_type(sig, assembly, None, resolver.references()))
            .collect::<Result<_, BuildError>>()?;
        match cil::lower_method_typed(&body, &resolver, &arg_types, &local_types) {
            Ok((func, map)) => {
                bodies.write(*rid, func);
                maps[*rid as usize] = map;
            }
            Err(error) => fails.push((*rid, error)),
        }
    }
    for body in plan.bodies() {
        let func = if body.declaration_only {
            deferred_trap_body()
        } else {
            lower_monomorphized_body(assembly, &resolver, body)?
        };
        bodies.write(body.index, func);
    }
    for body in plan.method_bodies() {
        bodies.write(
            body.index,
            lower_monomorphized_method_body(assembly, &resolver, body)?,
        );
    }
    for (i, (_, cctor, flag_slot)) in precise.iter().enumerate() {
        bodies.write(
            (thunk_base + i) as u32,
            type_init_thunk_body(flag_slot * 4, *cctor),
        );
    }
    Ok((
        bodies.funcs,
        maps,
        fails,
        seams,
        bodies.duplicates,
        thunk_indices,
        plan,
    ))
}

/// The embedding ABI's export list: `main` (the entry trampoline at index 0, if there is an entry)
/// plus every public static method by a `<Type>_<Method>` name (overloads disambiguated by arity then
/// rid), each at its MethodDef row = WASM function index, so a page's JS calls them by name.
#[cfg(feature = "wasm")]
fn method_exports(assembly: &Assembly, has_main: bool) -> Vec<(String, u32)> {
    let mut exports = Vec::new();
    let mut taken: Vec<String> = Vec::new();
    if has_main {
        exports.push(("main".to_string(), 0u32));
        taken.push("main".to_string());
    }
    for type_def in assembly.type_defs() {
        let type_name = type_def.name().map_or("", |n| n.name);
        for method in type_def.methods() {
            if !method.is_static() || method.flags() & 0x7 != 0x6 || method.body().is_none() {
                continue;
            }
            let Some(method_name) = method.name() else {
                continue;
            };
            let mut name = format!("{type_name}_{method_name}");
            if taken.contains(&name) {
                name = match crate::resolver::decodable_params(&method) {
                    Some(params) => format!("{type_name}_{method_name}_{}", params.len()),
                    None => format!("{type_name}_{method_name}_{}", method.rid()),
                };
                if taken.contains(&name) {
                    name = format!("{type_name}_{method_name}_{}", method.rid());
                }
            }
            taken.push(name.clone());
            exports.push((name, method.rid()));
        }
    }
    exports
}

/// A tiny SSA/CFG builder for the hand-synthesized MULTI-block runtime bodies (`Delegate.Combine`, and
/// later `Remove`) -- the single-block `synthesize_runtime_reader` style does not scale to loops/branches.
/// Values are numbered as created; blocks accumulate instructions + a terminator. A value defined in a
/// block is usable in any later block it dominates (verify is define-once/use-anywhere with no dominance
/// test, regalloc is liveness-based), so a value is threaded as a block PARAM only where it MERGES with a
/// different value at a join; elsewhere a dominated block reads it directly.
struct MirBuilder {
    value_types: Vec<MirType>,
    blocks: Vec<BasicBlock>,
    cur: usize,
    n_params: usize,
}

impl MirBuilder {
    fn new(params: &[MirType]) -> (Self, Vec<ValueId>) {
        let ids: Vec<ValueId> = (0..params.len()).map(|i| ValueId(i as u32)).collect();
        let entry = BasicBlock {
            params: ids.clone(),
            insts: Vec::new(),
            terminator: None,
        };
        (
            Self {
                value_types: params.to_vec(),
                blocks: vec![entry],
                cur: 0,
                n_params: params.len(),
            },
            ids,
        )
    }
    /// A fresh value of `ty`.
    fn val(&mut self, ty: MirType) -> ValueId {
        self.value_types.push(ty);
        ValueId((self.value_types.len() - 1) as u32)
    }
    /// A new EMPTY block (no params yet); returns its index. Blocks are created up front so branch targets
    /// resolve, but their PARAMS are created lazily by [`Self::enter`] as blocks are filled in execution
    /// order -- so a param's ValueId follows (not precedes) the values of the earlier blocks that jump to
    /// it, keeping ValueIds in definition order the way the rest of the pipeline expects.
    fn block(&mut self) -> usize {
        self.blocks.push(BasicBlock {
            params: Vec::new(),
            insts: Vec::new(),
            terminator: None,
        });
        self.blocks.len() - 1
    }
    /// Enter block `b` to fill it, creating its parameters NOW (fresh ValueIds, in execution order); a
    /// preceding `jump` supplies their values positionally. Returns the parameter values.
    fn enter(&mut self, b: usize, param_types: &[MirType]) -> Vec<ValueId> {
        let ids: Vec<ValueId> = param_types.iter().map(|t| self.val(*t)).collect();
        self.blocks[b].params = ids.clone();
        self.cur = b;
        ids
    }
    /// Enter a param-less block to fill it.
    fn at(&mut self, b: usize) {
        self.cur = b;
    }
    /// Append a value-defining instruction of result type `ty`; returns its value.
    fn emit(&mut self, ty: MirType, inst: Inst) -> ValueId {
        let v = self.val(ty);
        self.blocks[self.cur].insts.push((v, inst));
        v
    }
    /// Append a side-effecting instruction (`Store`/`ArrayStore`/`CopyBlock`); its result is an ignored i32.
    fn side(&mut self, inst: Inst) {
        let v = self.val(MirType::I32);
        self.blocks[self.cur].insts.push((v, inst));
    }
    fn ret(&mut self, v: ValueId) {
        self.blocks[self.cur].terminator = Some(Terminator::Return(Some(v)));
    }
    fn ret_void(&mut self) {
        self.blocks[self.cur].terminator = Some(Terminator::Return(None));
    }
    fn jump(&mut self, target: usize, args: Vec<ValueId>) {
        self.blocks[self.cur].terminator = Some(Terminator::Jump {
            target: BlockId(target as u32),
            args,
        });
    }
    /// A conditional branch. Its targets take NO arguments, which is not a shortcut of this builder
    /// but the backends' rule: both ARM object paths refuse a `Branch` carrying arguments outright
    /// (`ControlFlowUnsupported` -- "merges must go through Jump"). A synthesized body that needs to
    /// hand different values to a join branches to a one-line block of its own that `jump`s with
    /// them; the branching block dominates it, so it reads those values directly.
    fn branch(&mut self, cond: ValueId, if_true: usize, if_false: usize) {
        self.blocks[self.cur].terminator = Some(Terminator::Branch {
            cond,
            if_true: BlockId(if_true as u32),
            true_args: Vec::new(),
            if_false: BlockId(if_false as u32),
            false_args: Vec::new(),
        });
    }
    /// A block that traps if reached -- the answer for a case the body cannot compute rather than
    /// cannot decline (a `void` seam has no way to say "not me").
    fn unreachable(&mut self) {
        self.blocks[self.cur].terminator = Some(Terminator::Unreachable);
    }
    fn finish(self, ret: Option<MirType>) -> Function {
        Function {
            params: self.value_types[..self.n_params].to_vec(),
            ret,
            value_types: self.value_types,
            entry: BlockId(0),
            blocks: self.blocks,
        }
    }
}

/// A synthesized MIR body for a delegate type's `Invoke` -- the thunk form of the dispatch a
/// `callvirt` of it already lowers to inline. `params[0]` is the delegate receiver; the rest are
/// `Invoke`'s own signature parameters.
///
/// WHY IT HAS TO EXIST AT ALL, since `callvirt Invoke` never reaches it: `Invoke` is Runtime-
/// implemented, so it has no CIL body and the lowering skipped it, leaving the emitted `f<rid>` a
/// four-byte `bx lr` stub. That is invisible while the only reference is a call -- those are
/// intercepted into [`Inst::InvokeDelegate`] at the site. **`ldftn` is not intercepted.** csc (and
/// lcsc, identically -- a delegate type is sealed, so no `ldvirtftn` is needed) lowers
/// `new D(otherDelegate)` to `ldftn D::Invoke; newobj D::.ctor`, which hands the new delegate the
/// STUB as its `_methodPtr`. Calling it then returns whatever was in `r0`, so
/// `delegate-from-delegate` computed `x - x + 2` and exited 2 instead of 42.
///
/// Emitting the real dispatch here makes the address mean what the call already meant. It costs
/// nothing where it is unused: no `callvirt` references the symbol, so a program that never takes
/// `Invoke`'s address dead-strips it exactly as it dead-stripped the stub.
/// The body a delegate's `Invoke` gets, or `None` when this method is not one -- **the decision and
/// the construction together, so a lowering path cannot take one without the other.** Every path
/// that lowers a program asks this rather than restating the rule, because a delegate's `Invoke`
/// carries no CIL and each path decides separately what to do with a body-less method.
///
/// `is_delegate` is passed rather than derived because the question is about the TYPE and the caller
/// holds it: the whole-assembly path asks [`crate::resolver::is_delegate_type_of`] once per
/// `TypeDef` and would otherwise walk an extends chain once per method.
#[cfg(any(feature = "arm32", feature = "riscv32", feature = "wasm"))]
fn delegate_invoke_synthesis<'x>(
    assembly: &'x Assembly<'x>,
    references: &[&'x Assembly<'x>],
    is_delegate: bool,
    method_name: Option<&str>,
    signature: &Option<lamella_metadata::MethodSig>,
) -> Result<Option<Function>, BuildError> {
    if !is_delegate || method_name != Some("Invoke") {
        return Ok(None);
    }
    let Some(sig) = signature else {
        return Ok(None);
    };
    let mut params = alloc::vec![MirType::ObjectRef];
    for parameter in &sig.parameters {
        params.push(mir_type(parameter, assembly, None, references)?);
    }
    let ret = if sig.return_type == SigType::Void {
        None
    } else {
        Some(mir_type(&sig.return_type, assembly, None, references)?)
    };
    Ok(Some(delegate_invoke_body(&params, ret)))
}

fn delegate_invoke_body(params: &[MirType], ret: Option<MirType>) -> Function {
    let (mut mb, ids) = MirBuilder::new(params);
    let invoke = Inst::InvokeDelegate {
        delegate: ids[0],
        args: ids[1..].to_vec(),
        returns_value: ret.is_some(),
    };
    match ret {
        Some(ty) => {
            let result = mb.emit(ty, invoke);
            mb.ret(result);
        }
        None => {
            mb.side(invoke);
            mb.ret_void();
        }
    }
    mb.finish(ret)
}

/// A synthesized MIR body for `[RuntimeProvided] System.Delegate.Combine(a, b)` -- the immutable multicast
/// concatenation the AOT builds over the `{_target@0, _methodPtr@4, _invocationList@8}` delegate layout (the
/// interpreter uses its native rep; both implement the same contract, so parity holds by same-semantics).
/// `Combine(a, null) == a`, `Combine(null, b) == b`; otherwise a NEW `MulticastDelegate` -- cloning a's
/// CONCRETE descriptor via [`Inst::AllocLike`] so the caller's `castclass` to the concrete delegate type
/// still passes -- whose `_invocationList` is the FLATTENED concatenation of a's entries then b's. Each
/// operand contributes its own `_invocationList` (a `Delegate[count][entries]`) when multicast, or itself
/// when single-cast; `Invoke` walks the list and returns the last value (the invoke port is already on
/// every backend).
///
/// GC: with the bump allocator no object moves, so the freshly built list + result are stable. When a
/// precise MOVING collector lands it must trace `Delegate[]` elements (the deferred array-tracing contract);
/// the list is filled BEFORE the result's `AllocLike` safepoint and the result's ref fields are then set
/// with no intervening safepoint, so only that array-element tracing is owed. The `list` is a live root at
/// the `AllocLike` safepoint (used just after) and `a` is the live prototype, so both survive a collection.
///
/// Public so a per-target verifier example (`qemu-riscv-combine`, `wasm-combine`) can lower + run the
/// SAME synthesized body the corlib build substitutes -- proving the target-agnostic MIR end to end.
pub fn delegate_combine_body() -> Function {
    const DELEGATE_SIZE: u32 = 12;
    const INVLIST_OFF: i64 = 8;
    let i32t = MirType::I32;
    let objt = MirType::ObjectRef;
    let (mut mb, params) = MirBuilder::new(&[objt, objt]);
    let (a, b) = (params[0], params[1]);

    let ret_b = mb.block();
    let chk_b = mb.block();
    let ret_a = mb.block();
    let body = mb.block();
    let na_one = mb.block();
    let na_list = mb.block();
    let nb_head = mb.block();
    let nb_one = mb.block();
    let nb_list = mb.block();
    let alloc = mb.block();
    let copy_a1 = mb.block();
    let copy_am = mb.block();
    let copy_b = mb.block();
    let copy_b1 = mb.block();
    let copy_bm = mb.block();
    let fin = mb.block();

    let c = |v: i64| Inst::ConstInt { ty: i32t, value: v };
    let refint = |value| Inst::Convert {
        value,
        kind: ConvKind::RefToInt,
    };
    let add = |lhs, rhs| Inst::Binary {
        op: BinOp::Add,
        lhs,
        rhs,
    };
    let load = |address| Inst::Load {
        address,
        width: 4,
        signed: false,
    };
    let eq = |lhs, rhs| Inst::Compare {
        op: CmpOp::Eq,
        lhs,
        rhs,
    };

    mb.at(0);
    let zero = mb.emit(i32t, c(0));
    let ai = mb.emit(i32t, refint(a));
    let a_null = mb.emit(i32t, eq(ai, zero));
    mb.branch(a_null, ret_b, chk_b);
    mb.at(ret_b);
    mb.ret(b);

    mb.at(chk_b);
    let bi = mb.emit(i32t, refint(b));
    let b_null = mb.emit(i32t, eq(bi, zero));
    mb.branch(b_null, ret_a, body);
    mb.at(ret_a);
    mb.ret(a);

    mb.at(body);
    let off8 = mb.emit(i32t, c(INVLIST_OFF));
    let a8 = mb.emit(i32t, add(ai, off8));
    let la = mb.emit(objt, load(a8));
    let b8 = mb.emit(i32t, add(bi, off8));
    let lb = mb.emit(objt, load(b8));
    let lai = mb.emit(i32t, refint(la));
    let la_null = mb.emit(i32t, eq(lai, zero));
    mb.branch(la_null, na_one, na_list);
    mb.at(na_one);
    let one_a = mb.emit(i32t, c(1));
    mb.jump(nb_head, vec![one_a]);
    mb.at(na_list);
    let la_count = mb.emit(i32t, load(lai));
    mb.jump(nb_head, vec![la_count]);

    let na_in = mb.enter(nb_head, &[i32t])[0];
    let lbi = mb.emit(i32t, refint(lb));
    let lb_null = mb.emit(i32t, eq(lbi, zero));
    mb.branch(lb_null, nb_one, nb_list);
    mb.at(nb_one);
    let one_b = mb.emit(i32t, c(1));
    mb.jump(alloc, vec![na_in, one_b]);
    mb.at(nb_list);
    let lb_count = mb.emit(i32t, load(lbi));
    mb.jump(alloc, vec![na_in, lb_count]);

    let alloc_p = mb.enter(alloc, &[i32t, i32t]);
    let (na, nb) = (alloc_p[0], alloc_p[1]);
    let total = mb.emit(i32t, add(na, nb));
    let list = mb.emit(
        objt,
        Inst::AllocArray {
            handle: lamella_ir::synthetic_array_handle(ELEMENT_KIND_REFERENCE),
            element: None,
            length: total,
            element_size: 4,
            element_kind: ELEMENT_KIND_REFERENCE,
            element_cast_class: crate::resolver::ARRAY_CAST_CLASS_NONE,
        },
    );
    let listi = mb.emit(i32t, refint(list));
    let four = mb.emit(i32t, c(4));
    let list_e = mb.emit(i32t, add(listi, four));
    mb.branch(la_null, copy_a1, copy_am);
    mb.at(copy_a1);
    let idx0 = mb.emit(i32t, c(0));
    mb.side(Inst::ArrayStore {
        array: list,
        index: idx0,
        value: a,
        element_size: 4,
    });
    mb.jump(copy_b, vec![]);
    mb.at(copy_am);
    let la_e = mb.emit(i32t, add(lai, four));
    let na_bytes = mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::Mul,
            lhs: na,
            rhs: four,
        },
    );
    mb.side(Inst::CopyBlock {
        dst: list_e,
        src: la_e,
        size: na_bytes,
    });
    mb.jump(copy_b, vec![]);

    mb.at(copy_b);
    mb.branch(lb_null, copy_b1, copy_bm);
    mb.at(copy_b1);
    mb.side(Inst::ArrayStore {
        array: list,
        index: na,
        value: b,
        element_size: 4,
    });
    mb.jump(fin, vec![]);
    mb.at(copy_bm);
    let na_off = mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::Mul,
            lhs: na,
            rhs: four,
        },
    );
    let dst_b = mb.emit(i32t, add(list_e, na_off));
    let lb_e = mb.emit(i32t, add(lbi, four));
    let nb_bytes = mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::Mul,
            lhs: nb,
            rhs: four,
        },
    );
    mb.side(Inst::CopyBlock {
        dst: dst_b,
        src: lb_e,
        size: nb_bytes,
    });
    mb.jump(fin, vec![]);

    mb.at(fin);
    let result = mb.emit(
        objt,
        Inst::AllocLike {
            proto: a,
            payload_size: DELEGATE_SIZE,
        },
    );
    let ri = mb.emit(i32t, refint(result));
    let z = mb.emit(i32t, c(0));
    mb.side(Inst::Store {
        address: ri,
        value: z,
        width: 4,
    });
    let r4 = mb.emit(i32t, add(ri, four));
    mb.side(Inst::Store {
        address: r4,
        value: z,
        width: 4,
    });
    let r8 = mb.emit(i32t, add(ri, off8));
    let listw = mb.emit(i32t, refint(list));
    mb.side(Inst::Store {
        address: r8,
        value: listw,
        width: 4,
    });
    mb.ret(result);

    mb.finish(Some(objt))
}

/// A synthesized MIR body for `[RuntimeProvided] System.Delegate.Remove(source, value)` -- the immutable
/// `a -= b`. Removes the LAST entry of `source`'s invocation list that equals `value` (same `_target@0`
/// AND `_methodPtr@4`), returning: `source` unchanged if `value` is not found; `null` if the list becomes
/// empty; the bare remaining single-cast delegate if one entry is left; else a new `MulticastDelegate`
/// (cloning `source`'s descriptor via [`Inst::AllocLike`]) over the shortened list.
///
/// A MULTICAST `value` (`a -= (b + c)`) removes the LAST contiguous SUBSEQUENCE of `source`'s list that
/// equals `value`'s list -- a nested scan (outer over candidate start positions from the end, inner over
/// the value list; the `multi_val`/`sub_*` blocks), matching .NET's `MulticastDelegate.RemoveImpl`. Not
/// found (or a single-cast source that cannot contain a >=2-entry subsequence) returns `source` unchanged.
///
/// GC: mirrors [`delegate_combine_body`] -- the only safepoints are the new list's `AllocArray` and the
/// result's `AllocLike`; the list is filled between them with no intervening alloc, and the result's ref
/// fields are set before RETURN, so a moving collector need only trace the deferred `Delegate[]` elements.
pub fn delegate_remove_body() -> Function {
    const DELEGATE_SIZE: u32 = 12;
    const INVLIST_OFF: i64 = 8;
    let i32t = MirType::I32;
    let objt = MirType::ObjectRef;
    let (mut mb, params) = MirBuilder::new(&[objt, objt]);
    let (source, value) = (params[0], params[1]);

    let ret_null = mb.block();
    let ret_source = mb.block();
    let b1 = mb.block();
    let b2 = mb.block();
    let b3 = mb.block();
    let single_src = mb.block();
    let multi_src = mb.block();
    let match_hdr = mb.block();
    let cmp_k = mb.block();
    let matched = mb.block();
    let match_next = mb.block();
    let decide = mb.block();
    let decide1 = mb.block();
    let single_remain = mb.block();
    let rebuild = mb.block();
    let multi_val = mb.block();
    let sub_setup = mb.block();
    let sub_hdr = mb.block();
    let sub_body = mb.block();
    let sub_inner = mb.block();
    let sub_cmp = mb.block();
    let sub_cont = mb.block();
    let sub_next = mb.block();
    let sub_matched = mb.block();
    let sub_decide1 = mb.block();
    let sub_single = mb.block();
    let sub_single_i0 = mb.block();
    let sub_single_pos = mb.block();
    let sub_rebuild = mb.block();

    let c = |v: i64| Inst::ConstInt { ty: i32t, value: v };
    let refint = |value| Inst::Convert { value, kind: ConvKind::RefToInt };
    let intref = |value| Inst::Convert { value, kind: ConvKind::IntToRef };
    let add = |lhs, rhs| Inst::Binary { op: BinOp::Add, lhs, rhs };
    let sub = |lhs, rhs| Inst::Binary { op: BinOp::Sub, lhs, rhs };
    let mul = |lhs, rhs| Inst::Binary { op: BinOp::Mul, lhs, rhs };
    let band = |lhs, rhs| Inst::Binary { op: BinOp::And, lhs, rhs };
    let load = |address| Inst::Load { address, width: 4, signed: false };
    let eq = |lhs, rhs| Inst::Compare { op: CmpOp::Eq, lhs, rhs };
    let lt = |lhs, rhs| Inst::Compare { op: CmpOp::SignedLt, lhs, rhs };
    let lt0 = |lhs, zero| Inst::Compare { op: CmpOp::SignedLt, lhs, rhs: zero };

    mb.at(0);
    let zero = mb.emit(i32t, c(0));
    let si = mb.emit(i32t, refint(source));
    let s_null = mb.emit(i32t, eq(si, zero));
    mb.branch(s_null, ret_null, b1);
    mb.at(ret_null);
    let nz = mb.emit(i32t, c(0));
    let nref = mb.emit(objt, intref(nz));
    mb.ret(nref);
    mb.at(ret_source);
    mb.ret(source);

    mb.at(b1);
    let vi = mb.emit(i32t, refint(value));
    let v_null = mb.emit(i32t, eq(vi, zero));
    mb.branch(v_null, ret_source, b2);

    mb.at(b2);
    let off8 = mb.emit(i32t, c(INVLIST_OFF));
    let v8 = mb.emit(i32t, add(vi, off8));
    let vl = mb.emit(objt, load(v8));
    let vli = mb.emit(i32t, refint(vl));
    let vl_zero = mb.emit(i32t, eq(vli, zero));
    mb.branch(vl_zero, b3, multi_val);

    mb.at(b3);
    let four = mb.emit(i32t, c(4));
    let vt = mb.emit(i32t, load(vi));
    let vm4 = mb.emit(i32t, add(vi, four));
    let vm = mb.emit(i32t, load(vm4));
    let s8 = mb.emit(i32t, add(si, off8));
    let sl = mb.emit(objt, load(s8));
    let sli = mb.emit(i32t, refint(sl));
    let sl_null = mb.emit(i32t, eq(sli, zero));
    mb.branch(sl_null, single_src, multi_src);

    mb.at(single_src);
    let st = mb.emit(i32t, load(si));
    let t_eq = mb.emit(i32t, eq(st, vt));
    let sm4 = mb.emit(i32t, add(si, four));
    let sm = mb.emit(i32t, load(sm4));
    let m_eq = mb.emit(i32t, eq(sm, vm));
    let s_match = mb.emit(i32t, band(t_eq, m_eq));
    mb.branch(s_match, ret_null, ret_source);

    mb.at(multi_src);
    let sn = mb.emit(i32t, load(sli));
    let se = mb.emit(i32t, add(sli, four));
    let one = mb.emit(i32t, c(1));
    let kstart = mb.emit(i32t, sub(sn, one));
    mb.jump(match_hdr, vec![kstart]);

    let k = mb.enter(match_hdr, &[i32t])[0];
    let k_neg = mb.emit(i32t, lt0(k, zero));
    mb.branch(k_neg, ret_source, cmp_k);
    mb.at(cmp_k);
    let koff = mb.emit(i32t, mul(k, four));
    let ek_addr = mb.emit(i32t, add(se, koff));
    let e = mb.emit(i32t, load(ek_addr));
    let et = mb.emit(i32t, load(e));
    let te = mb.emit(i32t, eq(et, vt));
    let e4 = mb.emit(i32t, add(e, four));
    let em = mb.emit(i32t, load(e4));
    let me = mb.emit(i32t, eq(em, vm));
    let e_match = mb.emit(i32t, band(te, me));
    mb.branch(e_match, matched, match_next);
    mb.at(matched);
    mb.jump(decide, vec![k]);
    mb.at(match_next);
    let k1 = mb.emit(i32t, sub(k, one));
    mb.jump(match_hdr, vec![k1]);

    let found = mb.enter(decide, &[i32t])[0];
    let new_n = mb.emit(i32t, sub(sn, one));
    let n_zero = mb.emit(i32t, eq(new_n, zero));
    mb.branch(n_zero, ret_null, decide1);
    mb.at(decide1);
    let n_one = mb.emit(i32t, eq(new_n, one));
    mb.branch(n_one, single_remain, rebuild);

    mb.at(single_remain);
    let rem_idx = mb.emit(i32t, sub(one, found));
    let rem_off = mb.emit(i32t, mul(rem_idx, four));
    let rem_addr = mb.emit(i32t, add(se, rem_off));
    let rem_e = mb.emit(i32t, load(rem_addr));
    let rem_ref = mb.emit(objt, intref(rem_e));
    mb.ret(rem_ref);

    mb.at(rebuild);
    let nl = mb.emit(
        objt,
        Inst::AllocArray {
            handle: lamella_ir::synthetic_array_handle(ELEMENT_KIND_REFERENCE),
            element: None,
            length: new_n,
            element_size: 4,
            element_kind: ELEMENT_KIND_REFERENCE,
            element_cast_class: crate::resolver::ARRAY_CAST_CLASS_NONE,
        },
    );
    let nli = mb.emit(i32t, refint(nl));
    let ne = mb.emit(i32t, add(nli, four));
    let found4 = mb.emit(i32t, mul(found, four));
    mb.side(Inst::CopyBlock { dst: ne, src: se, size: found4 });
    let fp1 = mb.emit(i32t, add(found, one));
    let fp1_4 = mb.emit(i32t, mul(fp1, four));
    let src2 = mb.emit(i32t, add(se, fp1_4));
    let dst2 = mb.emit(i32t, add(ne, found4));
    let rem_count = mb.emit(i32t, sub(new_n, found));
    let rem_bytes = mb.emit(i32t, mul(rem_count, four));
    mb.side(Inst::CopyBlock { dst: dst2, src: src2, size: rem_bytes });
    let result = mb.emit(
        objt,
        Inst::AllocLike { proto: source, payload_size: DELEGATE_SIZE },
    );
    let ri = mb.emit(i32t, refint(result));
    let rz = mb.emit(i32t, c(0));
    mb.side(Inst::Store { address: ri, value: rz, width: 4 });
    let r4 = mb.emit(i32t, add(ri, four));
    mb.side(Inst::Store { address: r4, value: rz, width: 4 });
    let r8 = mb.emit(i32t, add(ri, off8));
    let listw = mb.emit(i32t, refint(nl));
    mb.side(Inst::Store { address: r8, value: listw, width: 4 });
    mb.ret(result);

    mb.at(multi_val);
    let s8b = mb.emit(i32t, add(si, off8));
    let slb = mb.emit(objt, load(s8b));
    let slib = mb.emit(i32t, refint(slb));
    let sl_null_b = mb.emit(i32t, eq(slib, zero));
    mb.branch(sl_null_b, ret_source, sub_setup);

    mb.at(sub_setup);
    let four2 = mb.emit(i32t, c(4));
    let snb = mb.emit(i32t, load(slib));
    let seb = mb.emit(i32t, add(slib, four2));
    let vnb = mb.emit(i32t, load(vli));
    let veb = mb.emit(i32t, add(vli, four2));
    let istart = mb.emit(i32t, sub(snb, vnb));
    mb.jump(sub_hdr, vec![istart]);

    let i = mb.enter(sub_hdr, &[i32t])[0];
    let i_neg = mb.emit(i32t, lt0(i, zero));
    mb.branch(i_neg, ret_source, sub_body);
    mb.at(sub_body);
    let j0 = mb.emit(i32t, c(0));
    mb.jump(sub_inner, vec![i, j0]);

    let inner = mb.enter(sub_inner, &[i32t, i32t]);
    let (ii, jj) = (inner[0], inner[1]);
    let j_lt = mb.emit(i32t, lt(jj, vnb));
    mb.branch(j_lt, sub_cmp, sub_matched);
    mb.at(sub_cmp);
    let ij = mb.emit(i32t, add(ii, jj));
    let ij4 = mb.emit(i32t, mul(ij, four2));
    let s_ea = mb.emit(i32t, add(seb, ij4));
    let s_e = mb.emit(i32t, load(s_ea));
    let vj4 = mb.emit(i32t, mul(jj, four2));
    let v_ea = mb.emit(i32t, add(veb, vj4));
    let v_e = mb.emit(i32t, load(v_ea));
    let s_t = mb.emit(i32t, load(s_e));
    let v_t = mb.emit(i32t, load(v_e));
    let t_ok = mb.emit(i32t, eq(s_t, v_t));
    let s_ma = mb.emit(i32t, add(s_e, four2));
    let s_m = mb.emit(i32t, load(s_ma));
    let v_ma = mb.emit(i32t, add(v_e, four2));
    let v_m = mb.emit(i32t, load(v_ma));
    let m_ok = mb.emit(i32t, eq(s_m, v_m));
    let both = mb.emit(i32t, band(t_ok, m_ok));
    mb.branch(both, sub_cont, sub_next);
    mb.at(sub_cont);
    let one3 = mb.emit(i32t, c(1));
    let jn = mb.emit(i32t, add(jj, one3));
    mb.jump(sub_inner, vec![ii, jn]);
    mb.at(sub_next);
    let one4 = mb.emit(i32t, c(1));
    let ip = mb.emit(i32t, sub(ii, one4));
    mb.jump(sub_hdr, vec![ip]);

    mb.at(sub_matched);
    let new_nb = mb.emit(i32t, sub(snb, vnb));
    let nb_zero = mb.emit(i32t, eq(new_nb, zero));
    mb.branch(nb_zero, ret_null, sub_decide1);
    mb.at(sub_decide1);
    let one5 = mb.emit(i32t, c(1));
    let nb_one = mb.emit(i32t, eq(new_nb, one5));
    mb.branch(nb_one, sub_single, sub_rebuild);

    mb.at(sub_single);
    let i_is0 = mb.emit(i32t, eq(ii, zero));
    mb.branch(i_is0, sub_single_i0, sub_single_pos);
    mb.at(sub_single_i0);
    let four3 = mb.emit(i32t, c(4));
    let vn4 = mb.emit(i32t, mul(vnb, four3));
    let surv0_a = mb.emit(i32t, add(seb, vn4));
    let surv0 = mb.emit(i32t, load(surv0_a));
    let surv0_ref = mb.emit(objt, intref(surv0));
    mb.ret(surv0_ref);
    mb.at(sub_single_pos);
    let surv1 = mb.emit(i32t, load(seb));
    let surv1_ref = mb.emit(objt, intref(surv1));
    mb.ret(surv1_ref);

    mb.at(sub_rebuild);
    let nlb = mb.emit(
        objt,
        Inst::AllocArray {
            handle: lamella_ir::synthetic_array_handle(ELEMENT_KIND_REFERENCE),
            element: None,
            length: new_nb,
            element_size: 4,
            element_kind: ELEMENT_KIND_REFERENCE,
            element_cast_class: crate::resolver::ARRAY_CAST_CLASS_NONE,
        },
    );
    let nlib = mb.emit(i32t, refint(nlb));
    let four4 = mb.emit(i32t, c(4));
    let neb = mb.emit(i32t, add(nlib, four4));
    let i4b = mb.emit(i32t, mul(ii, four4));
    mb.side(Inst::CopyBlock { dst: neb, src: seb, size: i4b });
    let ivn = mb.emit(i32t, add(ii, vnb));
    let ivn4 = mb.emit(i32t, mul(ivn, four4));
    let src2b = mb.emit(i32t, add(seb, ivn4));
    let dst2b = mb.emit(i32t, add(neb, i4b));
    let tail_c = mb.emit(i32t, sub(new_nb, ii));
    let tail_b = mb.emit(i32t, mul(tail_c, four4));
    mb.side(Inst::CopyBlock { dst: dst2b, src: src2b, size: tail_b });
    let result2 = mb.emit(
        objt,
        Inst::AllocLike { proto: source, payload_size: DELEGATE_SIZE },
    );
    let ri2 = mb.emit(i32t, refint(result2));
    let rz2 = mb.emit(i32t, c(0));
    mb.side(Inst::Store { address: ri2, value: rz2, width: 4 });
    let r4b = mb.emit(i32t, add(ri2, four4));
    mb.side(Inst::Store { address: r4b, value: rz2, width: 4 });
    let off8b = mb.emit(i32t, c(INVLIST_OFF));
    let r8b = mb.emit(i32t, add(ri2, off8b));
    let listw2 = mb.emit(i32t, refint(nlb));
    mb.side(Inst::Store { address: r8b, value: listw2, width: 4 });
    mb.ret(result2);

    mb.finish(Some(objt))
}

/// `log2` of each element kind's byte width, packed two bits per kind at bit `2*kind` -- the run-time
/// half of the frozen element-kind code space (`resolver::primitive_element_kind`, mirroring the
/// interpreter's `PrimKind`). A synthesized `System.Array` body reads word 1 of the array's descriptor
/// and needs the STRIDE, and the descriptor carries only the kind; two bits reach every width the code
/// space has (1/2/4/8), so the whole table fits in one immediate and costs a shift and a mask rather
/// than a nine-way branch chain. Built from the width list here rather than written as a literal, so it
/// cannot drift from it.
const ELEMENT_WIDTH_SHIFTS: u32 = {
    let shifts = [2u32, 0, 0, 1, 1, 2, 3, 2, 3];
    let mut table = 0u32;
    let mut kind = 0;
    while kind < shifts.len() {
        table |= shifts[kind] << (2 * kind);
        kind += 1;
    }
    table
};

/// The highest element kind [`ELEMENT_WIDTH_SHIFTS`] describes. Anything above it -- `ELEMENT_KIND_OPAQUE`
/// (a struct element, whose width word 1 deliberately does not carry) or a code from a future format --
/// is not stridable here, and a body that met one must decline rather than guess.
const MAX_STRIDABLE_ELEMENT_KIND: i64 = 8;

/// Emits the common prologue of an untyped `System.Array` seam: from an array reference, the byte-width
/// SHIFT of its elements, having already branched to `decline` unless the array is a well-formed VECTOR
/// whose element kind carries a width.
///
/// The two guards are what make the rest safe. Word 0 must be exactly `ARRAY_DESC_MARK | 1`: a rank-2+
/// descriptor is all zeroes today, so reading its word 1 as an element kind would invent a stride, and a
/// class descriptor's word 0 is a payload size. Word 1 must be a kind the table covers, which excludes
/// the struct-element arrays whose stride is real but unrepresented. Every kind that passes strides by
/// exactly the `element_size` the allocation used -- a reference element is 4 and every primitive's size
/// is `1 << shift` -- so the byte range computed from it is the range the object actually occupies.
#[cfg(any(feature = "arm32", feature = "riscv32", feature = "wasm"))]
fn array_element_shift(
    mb: &mut MirBuilder,
    array: ValueId,
    decline: usize,
) -> (ValueId, ValueId) {
    let i32t = MirType::I32;
    let c = |v: i64| Inst::ConstInt { ty: i32t, value: v };
    let base = mb.emit(
        i32t,
        Inst::Convert {
            value: array,
            kind: ConvKind::RefToInt,
        },
    );
    let desc = mb.emit(i32t, Inst::LoadTypeDesc { object: array });
    let word0 = mb.emit(
        i32t,
        Inst::Load {
            address: desc,
            width: 4,
            signed: false,
        },
    );
    #[allow(clippy::cast_possible_wrap)]
    let mark = (crate::resolver::ARRAY_DESC_MARK | 1) as i32;
    let vector_mark = mb.emit(i32t, c(i64::from(mark)));
    let is_vector = mb.emit(
        i32t,
        Inst::Compare {
            op: CmpOp::Eq,
            lhs: word0,
            rhs: vector_mark,
        },
    );
    let kind_block = mb.block();
    mb.branch(is_vector, kind_block, decline);

    mb.at(kind_block);
    let four = mb.emit(i32t, c(4));
    let kind_slot = mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::Add,
            lhs: desc,
            rhs: four,
        },
    );
    let kind = mb.emit(
        i32t,
        Inst::Load {
            address: kind_slot,
            width: 4,
            signed: false,
        },
    );
    let max_kind = mb.emit(i32t, c(MAX_STRIDABLE_ELEMENT_KIND));
    let unstridable = mb.emit(
        i32t,
        Inst::Compare {
            op: CmpOp::UnsignedGt,
            lhs: kind,
            rhs: max_kind,
        },
    );
    let shift_block = mb.block();
    mb.branch(unstridable, decline, shift_block);

    mb.at(shift_block);
    let table = mb.emit(i32t, c(i64::from(ELEMENT_WIDTH_SHIFTS)));
    let one = mb.emit(i32t, c(1));
    let bit = mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::Shl,
            lhs: kind,
            rhs: one,
        },
    );
    let field = mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::ShrUnsigned,
            lhs: table,
            rhs: bit,
        },
    );
    let three = mb.emit(i32t, c(3));
    let shift = mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::And,
            lhs: field,
            rhs: three,
        },
    );
    (base, shift)
}

/// The ELEMENT type's descriptor address, read out of the array descriptor's `element_desc@16` --
/// having already branched to `decline` if the word is ABSENT.
///
/// The word is a REL_DESC: it holds `element_words - desc`, read exactly as `base_ptr@12` is, so the
/// element descriptor is `desc + word`. 0 means ABSENT -- the emitter lays it wherever it has no
/// descriptor that can answer for the element -- and the ratified contract says a consumer must refuse
/// a zero rather than treat it as an address, which is what the branch here is.
#[cfg(any(feature = "arm32", feature = "riscv32", feature = "wasm"))]
fn array_element_descriptor(mb: &mut MirBuilder, desc: ValueId, decline: usize) -> ValueId {
    let i32t = MirType::I32;
    let c = |v: i64| Inst::ConstInt { ty: i32t, value: v };
    let sixteen = mb.emit(i32t, c(16));
    let element_slot = mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::Add,
            lhs: desc,
            rhs: sixteen,
        },
    );
    let element_rel = mb.emit(
        i32t,
        Inst::Load {
            address: element_slot,
            width: 4,
            signed: false,
        },
    );
    let zero = mb.emit(i32t, c(0));
    let named = mb.emit(
        i32t,
        Inst::Compare {
            op: CmpOp::Ne,
            lhs: element_rel,
            rhs: zero,
        },
    );
    let named_block = mb.block();
    mb.branch(named, named_block, decline);

    mb.at(named_block);
    mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::Add,
            lhs: desc,
            rhs: element_rel,
        },
    )
}

/// The byte address of element `index` of the array whose object address is `base`, striding by
/// `shift`: `base + 4 + (index << shift)`. The `+4` steps over the length word an ObjectRef points at.
#[cfg(any(feature = "arm32", feature = "riscv32", feature = "wasm"))]
fn array_element_address(
    mb: &mut MirBuilder,
    base: ValueId,
    index: ValueId,
    shift: ValueId,
) -> ValueId {
    let i32t = MirType::I32;
    let offset = mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::Shl,
            lhs: index,
            rhs: shift,
        },
    );
    let four = mb.emit(
        i32t,
        Inst::ConstInt {
            ty: i32t,
            value: 4,
        },
    );
    let data = mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::Add,
            lhs: base,
            rhs: four,
        },
    );
    mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::Add,
            lhs: data,
            rhs: offset,
        },
    )
}

/// A synthesized MIR body for `[RuntimeProvided] System.Array.CopyCore(src, srcIndex, dst, dstIndex,
/// length)` -- the bulk element move behind `Array.Copy`, and the reason a copy is one operation instead
/// of one boxed read and one unboxed write per element.
///
/// It moves the range VERBATIM, so it only accepts a pair it can move that way, and answers `false` for
/// anything else -- which is the seam's declared contract, not a shortcut: the managed caller then runs
/// its own per-element loop, keeping the checked path for array covariance and the widening primitive
/// conversions. The test for "movable verbatim" is that both arrays have the SAME DESCRIPTOR, one pointer
/// compare, and it is exactly right: two arrays share a descriptor only when they are the same array
/// type, which makes the elements identical in representation AND makes a covariant store impossible
/// (`object[]` and `string[]` have different descriptors, so that pair declines and takes the checked
/// loop). A widening pair (`int[]` into `long[]`) declines for the same reason.
///
/// OVERLAP is handled rather than declined, because the contract says the range moves "as if through a
/// temporary" and the only source of overlap is one array copied onto itself. `CopyBlock` is a FORWARD
/// byte loop -- a `memcpy` -- so it is correct only when the destination does not start above the source;
/// the other direction gets a descending byte loop here. Disjoint ranges are correct either way, so the
/// single address compare decides without needing to test for overlap.
///
/// GC: no allocation and no safepoint, so nothing can move under it. A reference-element move copies
/// pointers verbatim, which is what an ordinary `stfld` of a reference already does on this backend -- if
/// a generational collector ever needs a store barrier, this body and `stfld` grow one together.
#[cfg(any(feature = "arm32", feature = "riscv32", feature = "wasm"))]
#[must_use]
pub fn array_copy_core_body() -> Function {
    let i32t = MirType::I32;
    let objt = MirType::ObjectRef;
    let (mut mb, params) = MirBuilder::new(&[objt, i32t, objt, i32t, i32t]);
    let (src, src_index, dst, dst_index, length) = (
        params[0], params[1], params[2], params[3], params[4],
    );
    let c = |v: i64| Inst::ConstInt { ty: i32t, value: v };

    let decline = mb.block();
    let accept = mb.block();

    mb.at(0);
    let dst_base = mb.emit(
        i32t,
        Inst::Convert {
            value: dst,
            kind: ConvKind::RefToInt,
        },
    );
    let src_desc = mb.emit(i32t, Inst::LoadTypeDesc { object: src });
    let dst_desc = mb.emit(i32t, Inst::LoadTypeDesc { object: dst });
    let same_type = mb.emit(
        i32t,
        Inst::Compare {
            op: CmpOp::Eq,
            lhs: src_desc,
            rhs: dst_desc,
        },
    );
    mb.branch(same_type, accept, decline);

    mb.at(decline);
    let zero = mb.emit(i32t, c(0));
    mb.ret(zero);

    mb.at(accept);
    let (base, shift) = array_element_shift(&mut mb, src, decline);
    let bytes = mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::Shl,
            lhs: length,
            rhs: shift,
        },
    );
    let nothing_to_do = mb.block();
    let move_range = mb.block();
    let zero2 = mb.emit(i32t, c(0));
    let empty = mb.emit(
        i32t,
        Inst::Compare {
            op: CmpOp::SignedLe,
            lhs: bytes,
            rhs: zero2,
        },
    );
    mb.branch(empty, nothing_to_do, move_range);

    mb.at(nothing_to_do);
    let one = mb.emit(i32t, c(1));
    mb.ret(one);

    mb.at(move_range);
    let src_addr = array_element_address(&mut mb, base, src_index, shift);
    let dst_addr = array_element_address(&mut mb, dst_base, dst_index, shift);
    let overlaps_forward = mb.emit(
        i32t,
        Inst::Compare {
            op: CmpOp::UnsignedGt,
            lhs: dst_addr,
            rhs: src_addr,
        },
    );
    let backward = mb.block();
    let forward = mb.block();
    mb.branch(overlaps_forward, backward, forward);

    mb.at(forward);
    mb.side(Inst::CopyBlock {
        dst: dst_addr,
        src: src_addr,
        size: bytes,
    });
    let ok_forward = mb.emit(i32t, c(1));
    mb.ret(ok_forward);

    mb.at(backward);
    let one_b = mb.emit(i32t, c(1));
    let last = mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::Sub,
            lhs: bytes,
            rhs: one_b,
        },
    );
    let loop_head = mb.block();
    let latch = mb.block();
    let done_backward = mb.block();
    mb.jump(loop_head, alloc::vec![last]);

    let index = mb.enter(loop_head, &[i32t])[0];
    let src_at = mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::Add,
            lhs: src_addr,
            rhs: index,
        },
    );
    let byte = mb.emit(
        i32t,
        Inst::Load {
            address: src_at,
            width: 1,
            signed: false,
        },
    );
    let dst_at = mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::Add,
            lhs: dst_addr,
            rhs: index,
        },
    );
    mb.side(Inst::Store {
        address: dst_at,
        value: byte,
        width: 1,
    });
    let one_l = mb.emit(i32t, c(1));
    let next = mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::Sub,
            lhs: index,
            rhs: one_l,
        },
    );
    let zero_l = mb.emit(i32t, c(0));
    let more = mb.emit(
        i32t,
        Inst::Compare {
            op: CmpOp::SignedGe,
            lhs: next,
            rhs: zero_l,
        },
    );
    mb.branch(more, latch, done_backward);

    mb.at(latch);
    mb.jump(loop_head, alloc::vec![next]);

    mb.at(done_backward);
    let ok_backward = mb.emit(i32t, c(1));
    mb.ret(ok_backward);

    mb.finish(Some(i32t))
}

/// A synthesized MIR body for `[RuntimeProvided] System.Array.get_Rank` -- the dimension count, which the
/// array's descriptor carries in the LOW bits of word 0 under [`crate::resolver::ARRAY_DESC_MARK`]. Read
/// from the mark rather than assumed to be 1, so it stays right when rank-N descriptors are marked; an
/// UNMARKED descriptor (which is every rank-2+ array today) traps rather than answering 1, because
/// answering the wrong rank is how a caller ends up indexing a rectangular array as a vector.
#[cfg(any(feature = "arm32", feature = "riscv32", feature = "wasm"))]
#[must_use]
pub fn array_rank_body() -> Function {
    let i32t = MirType::I32;
    let objt = MirType::ObjectRef;
    let (mut mb, params) = MirBuilder::new(&[objt]);
    let array = params[0];
    let trap = mb.block();
    mb.at(0);
    let (_, rank) = array_descriptor_rank(&mut mb, array, trap);
    mb.ret(rank);
    mb.at(trap);
    mb.unreachable();
    mb.finish(Some(i32t))
}

/// From an array reference: its object address and the RANK its descriptor states, having branched to
/// `trap` unless word 0 carries [`crate::resolver::ARRAY_DESC_MARK`]. The mark is what makes the rest of
/// word 0 a rank at all -- on a class descriptor that word is a payload size, and on the rank-2+ array
/// descriptors emitted today it is zero.
#[cfg(any(feature = "arm32", feature = "riscv32", feature = "wasm"))]
fn array_descriptor_rank(
    mb: &mut MirBuilder,
    array: ValueId,
    trap: usize,
) -> (ValueId, ValueId) {
    let i32t = MirType::I32;
    let c = |v: i64| Inst::ConstInt { ty: i32t, value: v };
    let base = mb.emit(
        i32t,
        Inst::Convert {
            value: array,
            kind: ConvKind::RefToInt,
        },
    );
    let desc = mb.emit(i32t, Inst::LoadTypeDesc { object: array });
    let word0 = mb.emit(
        i32t,
        Inst::Load {
            address: desc,
            width: 4,
            signed: false,
        },
    );
    #[allow(clippy::cast_possible_wrap)]
    let mask = c(i64::from(crate::resolver::ARRAY_DESC_MARK_MASK as i32));
    #[allow(clippy::cast_possible_wrap)]
    let mark = c(i64::from(crate::resolver::ARRAY_DESC_MARK as i32));
    let mask_v = mb.emit(i32t, mask);
    let marked_bits = mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::And,
            lhs: word0,
            rhs: mask_v,
        },
    );
    let mark_v = mb.emit(i32t, mark);
    let is_array = mb.emit(
        i32t,
        Inst::Compare {
            op: CmpOp::Eq,
            lhs: marked_bits,
            rhs: mark_v,
        },
    );
    let rank_block = mb.block();
    mb.branch(is_array, rank_block, trap);

    mb.at(rank_block);
    let rank = mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::Sub,
            lhs: word0,
            rhs: marked_bits,
        },
    );
    (base, rank)
}

/// A synthesized MIR body for `[RuntimeProvided] System.Array.GetValue(index)` -- the REFERENCE-element
/// case, which is the half of it that needs no box: the element IS an `object`, so reading the slot IS
/// the answer. The split is deliberate: a primitive or struct
/// element has to be BOXED to be returned as `object`, and boxing against a descriptor known only at RUN
/// TIME (the array's `element_desc@16`) needs an allocate-with-this-descriptor form the IR does not have.
///
/// A PRIMITIVE element is BOXED against the descriptor `element_desc@16` names -- the fifth word's first
/// consumer, and the reason it exists: the box's type and size are both run-time facts read out of the
/// array's own descriptor, which is what [`lamella_ir::Inst::AllocDescribed`] was added for. A STRUCT
/// element still traps, one gate earlier: `ELEMENT_KIND_OPAQUE` carries no width, so the shared
/// rank-and-kind gate declines it before this body ever asks about a descriptor.
///
/// Returning the raw element bits retyped as a reference would hand a caller an integer to dereference,
/// so every case this cannot answer TRAPS instead.
///
/// It also traps on an out-of-range index. .NET throws `IndexOutOfRangeException` there; we do not throw
/// where .NET throws yet (a known family), and the choice here is only between trapping and reading off
/// the end of the object, which is not a choice. The check is against the element count the array carries
/// at its own offset 0 -- the word `newarr` stored and `ldlen` reads.
#[cfg(any(feature = "arm32", feature = "riscv32", feature = "wasm"))]
#[must_use]
pub fn array_get_value_body() -> Function {
    let i32t = MirType::I32;
    let objt = MirType::ObjectRef;
    let (mut mb, params) = MirBuilder::new(&[objt, i32t]);
    let (array, index) = (params[0], params[1]);
    let c = |v: i64| Inst::ConstInt { ty: i32t, value: v };

    let trap = mb.block();
    mb.at(0);
    let (base, shift) = array_element_shift(&mut mb, array, trap);

    let length = mb.emit(
        i32t,
        Inst::Load {
            address: base,
            width: 4,
            signed: false,
        },
    );
    let in_range = mb.emit(
        i32t,
        Inst::Compare {
            op: CmpOp::UnsignedLt,
            lhs: index,
            rhs: length,
        },
    );
    let kind_block = mb.block();
    mb.branch(in_range, kind_block, trap);

    mb.at(kind_block);
    let desc = mb.emit(i32t, Inst::LoadTypeDesc { object: array });
    let four = mb.emit(i32t, c(4));
    let kind_slot = mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::Add,
            lhs: desc,
            rhs: four,
        },
    );
    let kind = mb.emit(
        i32t,
        Inst::Load {
            address: kind_slot,
            width: 4,
            signed: false,
        },
    );
    let reference = mb.emit(i32t, c(i64::from(crate::resolver::ELEMENT_KIND_REFERENCE)));
    let is_reference = mb.emit(
        i32t,
        Inst::Compare {
            op: CmpOp::Eq,
            lhs: kind,
            rhs: reference,
        },
    );
    let read_block = mb.block();
    let box_block = mb.block();
    mb.branch(is_reference, read_block, box_block);

    mb.at(read_block);
    let addr = array_element_address(&mut mb, base, index, shift);
    let slot = mb.emit(
        i32t,
        Inst::Load {
            address: addr,
            width: 4,
            signed: false,
        },
    );
    let element = mb.emit(
        objt,
        Inst::Convert {
            value: slot,
            kind: ConvKind::IntToRef,
        },
    );
    mb.ret(element);

    mb.at(box_block);
    let element_desc = array_element_descriptor(&mut mb, desc, trap);
    let payload = mb.emit(
        i32t,
        Inst::Load {
            address: element_desc,
            width: 4,
            signed: false,
        },
    );
    let one = mb.emit(i32t, c(1));
    let width = mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::Shl,
            lhs: one,
            rhs: shift,
        },
    );
    let agree = mb.emit(
        i32t,
        Inst::Compare {
            op: CmpOp::Eq,
            lhs: payload,
            rhs: width,
        },
    );
    let alloc_block = mb.block();
    mb.branch(agree, alloc_block, trap);

    mb.at(alloc_block);
    let boxed = mb.emit(
        objt,
        Inst::AllocDescribed {
            descriptor: element_desc,
            payload_size: payload,
        },
    );
    let moved_base = mb.emit(
        i32t,
        Inst::Convert {
            value: array,
            kind: ConvKind::RefToInt,
        },
    );
    let source = array_element_address(&mut mb, moved_base, index, shift);
    let destination = mb.emit(
        i32t,
        Inst::Convert {
            value: boxed,
            kind: ConvKind::RefToInt,
        },
    );
    mb.side(Inst::CopyBlock {
        dst: destination,
        src: source,
        size: payload,
    });
    mb.ret(boxed);

    mb.at(trap);
    mb.unreachable();

    mb.finish(Some(objt))
}

/// A synthesized MIR body for `[RuntimeProvided] System.Array.SetValue(value, index)` -- the WRITE half of
/// the untyped element accessor, and the last live `System.Array` trap row: `Array.Reverse`, `Array.Sort`
/// and `Array.BinarySearch` are all managed loops over this seam and [`array_get_value_body`], so they
/// follow it rather than needing bodies of their own.
///
/// The mirror image of `GetValue` in every part. The same rank-and-kind gate
/// ([`array_element_shift`]) and the same unsigned bounds test against the length word decide whether
/// there is an element at all; the element KIND then decides what the incoming `object` has to be:
///
/// - a REFERENCE element takes the reference itself, once the value is shown ASSIGNABLE to the element
///   type -- a `CastClassScan` from the value's own descriptor to the one `element_desc@16` names, the
///   same base_ptr@12 walk `castclass`/`isinst` use. `null` is accepted without asking, since it is
///   assignable to every reference type and has no descriptor to ask about.
/// - a PRIMITIVE element takes the BOX apart: the value's descriptor must be EXACTLY the element's (a
///   box carries its type's canonical descriptor, which is how `GetValue`'s box unboxes and passes
///   `is int`), and then the payload copies in -- the reverse of `GetValue`'s boxing path, behind the
///   same payload/width agreement guard.
///
/// The seam is `void`, so like [`array_clear_core_body`] it has no way to hand a verdict back: the two
/// outcomes are STORE and TRAP. **That is deliberate, and it is why no managed `throw` sits on top of a
/// declining return.** The scan cannot answer for an INTERFACE element -- the base_ptr chain carries
/// classes only, and interface implementation lives in the itable, which is keyed per interface METHOD
/// and so cannot answer "does this type implement `IFoo`" at all -- so for `IFoo[] a; a.SetValue(impl, 0)`
/// the scan reads 0 on a store .NET performs. Trapping there says "this runtime could not do it", which
/// is true. A `bool` decline turned into an `InvalidCastException` by the managed wrapper would instead
/// say "your program was wrong", which is false, and it would be indistinguishable from the genuinely
/// non-assignable case. Named as a limitation rather than dressed up as a diagnosis; answering it needs
/// an interface SET in the descriptor format, which is a ratification, not a body.
///
/// Two more cases trap for reasons worth stating, because .NET does something else in each: a WIDENING
/// primitive store (a boxed `int` into a `long[]`, which .NET converts) declines on the exact-descriptor
/// test, and a `null` into a primitive element (.NET throws) has no payload to copy. An out-of-range
/// index traps as `GetValue`'s does -- the known "we do not throw where .NET throws" family, where the
/// only alternative is writing past the end of the object.
///
/// GC: nothing here allocates, so no safepoint lands inside it and the base address taken up front stays
/// valid -- the opposite of `GetValue`, whose box allocation forces the array address to be recomputed
/// after it. A reference element is stored as a plain word, exactly as `stfld` of a reference is; if a
/// generational collector ever needs a store barrier, this body, `stfld` and [`array_copy_core_body`]
/// grow one together.
#[cfg(any(feature = "arm32", feature = "riscv32", feature = "wasm"))]
#[must_use]
pub fn array_set_value_body() -> Function {
    let i32t = MirType::I32;
    let objt = MirType::ObjectRef;
    let (mut mb, params) = MirBuilder::new(&[objt, objt, i32t]);
    let (array, value, index) = (params[0], params[1], params[2]);
    let c = |v: i64| Inst::ConstInt { ty: i32t, value: v };

    let trap = mb.block();
    mb.at(0);
    let (base, shift) = array_element_shift(&mut mb, array, trap);

    let length = mb.emit(
        i32t,
        Inst::Load {
            address: base,
            width: 4,
            signed: false,
        },
    );
    let in_range = mb.emit(
        i32t,
        Inst::Compare {
            op: CmpOp::UnsignedLt,
            lhs: index,
            rhs: length,
        },
    );
    let kind_block = mb.block();
    mb.branch(in_range, kind_block, trap);

    mb.at(kind_block);
    let desc = mb.emit(i32t, Inst::LoadTypeDesc { object: array });
    let four = mb.emit(i32t, c(4));
    let kind_slot = mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::Add,
            lhs: desc,
            rhs: four,
        },
    );
    let kind = mb.emit(
        i32t,
        Inst::Load {
            address: kind_slot,
            width: 4,
            signed: false,
        },
    );
    let reference = mb.emit(i32t, c(i64::from(crate::resolver::ELEMENT_KIND_REFERENCE)));
    let is_reference = mb.emit(
        i32t,
        Inst::Compare {
            op: CmpOp::Eq,
            lhs: kind,
            rhs: reference,
        },
    );
    let reference_block = mb.block();
    let primitive_block = mb.block();
    mb.branch(is_reference, reference_block, primitive_block);

    mb.at(reference_block);
    let value_bits = mb.emit(
        i32t,
        Inst::Convert {
            value,
            kind: ConvKind::RefToInt,
        },
    );
    let null = mb.emit(i32t, c(0));
    let is_null = mb.emit(
        i32t,
        Inst::Compare {
            op: CmpOp::Eq,
            lhs: value_bits,
            rhs: null,
        },
    );
    let null_block = mb.block();
    let typed_block = mb.block();
    mb.branch(is_null, null_block, typed_block);

    mb.at(null_block);
    let null_addr = array_element_address(&mut mb, base, index, shift);
    mb.side(Inst::Store {
        address: null_addr,
        value: value_bits,
        width: 4,
    });
    mb.ret_void();

    mb.at(typed_block);
    let element_desc = array_element_descriptor(&mut mb, desc, trap);
    let value_desc = mb.emit(i32t, Inst::LoadTypeDesc { object: value });
    let assignable = mb.emit(
        i32t,
        Inst::CastClassScan {
            args: alloc::vec![value_desc, element_desc],
        },
    );
    let store_block = mb.block();
    mb.branch(assignable, store_block, trap);

    mb.at(store_block);
    let addr = array_element_address(&mut mb, base, index, shift);
    mb.side(Inst::Store {
        address: addr,
        value: value_bits,
        width: 4,
    });
    mb.ret_void();

    mb.at(primitive_block);
    let box_bits = mb.emit(
        i32t,
        Inst::Convert {
            value,
            kind: ConvKind::RefToInt,
        },
    );
    let no_box = mb.emit(i32t, c(0));
    let boxed = mb.emit(
        i32t,
        Inst::Compare {
            op: CmpOp::Ne,
            lhs: box_bits,
            rhs: no_box,
        },
    );
    let unbox_block = mb.block();
    mb.branch(boxed, unbox_block, trap);

    mb.at(unbox_block);
    let element_desc_p = array_element_descriptor(&mut mb, desc, trap);
    let box_desc = mb.emit(i32t, Inst::LoadTypeDesc { object: value });
    let same_type = mb.emit(
        i32t,
        Inst::Compare {
            op: CmpOp::Eq,
            lhs: box_desc,
            rhs: element_desc_p,
        },
    );
    let size_block = mb.block();
    mb.branch(same_type, size_block, trap);

    mb.at(size_block);
    let payload = mb.emit(
        i32t,
        Inst::Load {
            address: element_desc_p,
            width: 4,
            signed: false,
        },
    );
    let one = mb.emit(i32t, c(1));
    let width = mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::Shl,
            lhs: one,
            rhs: shift,
        },
    );
    let agree = mb.emit(
        i32t,
        Inst::Compare {
            op: CmpOp::Eq,
            lhs: payload,
            rhs: width,
        },
    );
    let copy_block = mb.block();
    mb.branch(agree, copy_block, trap);

    mb.at(copy_block);
    let destination = array_element_address(&mut mb, base, index, shift);
    mb.side(Inst::CopyBlock {
        dst: destination,
        src: box_bits,
        size: payload,
    });
    mb.ret_void();

    mb.at(trap);
    mb.unreachable();

    mb.finish(None)
}

/// A synthesized MIR body for `[RuntimeProvided] System.Array.Clone()` -- `ICloneable` on an array: a new
/// array of the SAME element type and length holding the same elements, shallow (reference elements
/// shared, value-type elements copied byte for byte), which is .NET's contract.
///
/// The untyped `Array` base cannot `newarr` its own element type in managed code, which is why this is a
/// seam at all -- but the heap array carries that type in its descriptor, so the duplicate is an
/// allocation against a RUN-TIME descriptor: [`Inst::AllocDescribed`] with the array's OWN descriptor
/// ([`Inst::LoadTypeDesc`]) and a payload of `4 + length * (1 << shift)`. That is the whole body. It was
/// blocked on "a variable-size allocation whose type is a run-time fact" having no IR form; `GetValue`'s
/// boxing half added exactly that form, and the second consumer costs a dozen instructions.
///
/// The length word is INSIDE the payload at offset 0 -- the word `newarr` stores and `ldlen` reads -- so
/// the single `CopyBlock` of the whole payload carries the length and the elements together, and the
/// clone needs no separate header write.
///
/// THE ORDERING IS THE SAME GC ARGUMENT `GetValue` makes, and it bites harder here because BOTH ends move:
/// allocating can move the source array, so its address is recomputed from the array REFERENCE after the
/// allocation rather than reused from the integer taken before it. The length and the byte count are
/// plain integers, so they survive a move; an address does not.
///
/// Declines (traps) exactly where the other untyped seams do: a rank-2+ array, and a struct element,
/// whose `ELEMENT_KIND_OPAQUE` carries no width to compute a payload size from. .NET clones both; this
/// one cannot size them yet, and sizing a clone wrong means allocating short and copying past the end.
///
/// **A VIRTUAL CALL CANNOT REACH IT, AND THE REASON IS NOT IN THIS BODY.** `Clone` is an implicit
/// `ICloneable` implementation, so it is VIRTUAL, so a call site lowers to a `CallVirtual` that reads a
/// slot from the receiver's descriptor -- and an ARRAY descriptor is emitted with an EMPTY vtable
/// (`TypeDescLiteral`'s `vtable` is `[]` at both array-allocation sites). The slot read lands before the
/// descriptor's words and dispatch jumps to whatever is there. So this is a gap in ARRAY DISPATCH rather
/// than in this body: every virtual call on an array receiver (`Clone`, `ToString`, `GetHashCode`,
/// `Equals`, `GetEnumerator`) has nowhere to dispatch through. `GetValue`/`SetValue` are unaffected
/// because they are NON-virtual, so their `callvirt` devirtualizes to a direct call.
///
/// The body is landed rather than held because it is complete and pinned on its own terms, and it starts
/// working the moment either fix lands: give an array descriptor `System.Array`'s vtable (the emitter
/// change, and the one that fixes the whole family), or devirtualize a `callvirt` to a FINAL method into
/// a direct call (sound in general -- a final method has exactly one implementation -- and it fixes only
/// this member). Neither is in this change; both are named in the census.
#[cfg(any(feature = "arm32", feature = "riscv32", feature = "wasm"))]
#[must_use]
pub fn array_clone_body() -> Function {
    let i32t = MirType::I32;
    let objt = MirType::ObjectRef;
    let (mut mb, params) = MirBuilder::new(&[objt]);
    let array = params[0];
    let c = |v: i64| Inst::ConstInt { ty: i32t, value: v };

    let trap = mb.block();
    mb.at(0);
    let (base, shift) = array_element_shift(&mut mb, array, trap);

    let length = mb.emit(
        i32t,
        Inst::Load {
            address: base,
            width: 4,
            signed: false,
        },
    );
    let bytes = mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::Shl,
            lhs: length,
            rhs: shift,
        },
    );
    let four = mb.emit(i32t, c(4));
    let payload = mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::Add,
            lhs: bytes,
            rhs: four,
        },
    );
    let desc = mb.emit(i32t, Inst::LoadTypeDesc { object: array });
    let clone = mb.emit(
        objt,
        Inst::AllocDescribed {
            descriptor: desc,
            payload_size: payload,
        },
    );
    let source = mb.emit(
        i32t,
        Inst::Convert {
            value: array,
            kind: ConvKind::RefToInt,
        },
    );
    let destination = mb.emit(
        i32t,
        Inst::Convert {
            value: clone,
            kind: ConvKind::RefToInt,
        },
    );
    mb.side(Inst::CopyBlock {
        dst: destination,
        src: source,
        size: payload,
    });
    mb.ret(clone);

    mb.at(trap);
    mb.unreachable();

    mb.finish(Some(objt))
}

/// A synthesized MIR body for `[RuntimeProvided] System.Array.ClearCore(array, index, length)` -- zeroing
/// an element range to its element default, which for every kind this descriptor scheme can name is a
/// zeroed byte range (`0` / `0.0` / `false` / `null` are all all-zero bits).
///
/// `void`, so unlike [`array_copy_core_body`] it has no way to DECLINE: a range it cannot compute a stride
/// for -- a rank-2+ array, or a struct element whose width word 1 does not carry -- TRAPS. That is the
/// honest end of the only two options, since the alternative is returning as though the range were
/// cleared.
#[cfg(any(feature = "arm32", feature = "riscv32", feature = "wasm"))]
#[must_use]
pub fn array_clear_core_body() -> Function {
    let i32t = MirType::I32;
    let objt = MirType::ObjectRef;
    let (mut mb, params) = MirBuilder::new(&[objt, i32t, i32t]);
    let (array, index, length) = (params[0], params[1], params[2]);

    let trap = mb.block();
    mb.at(0);
    let (base, shift) = array_element_shift(&mut mb, array, trap);
    let addr = array_element_address(&mut mb, base, index, shift);
    let bytes = mb.emit(
        i32t,
        Inst::Binary {
            op: BinOp::Shl,
            lhs: length,
            rhs: shift,
        },
    );
    let zero = mb.emit(
        i32t,
        Inst::ConstInt {
            ty: i32t,
            value: 0,
        },
    );
    mb.side(Inst::FillBlock {
        dst: addr,
        value: zero,
        size: bytes,
    });
    mb.ret_void();

    mb.at(trap);
    mb.unreachable();

    mb.finish(None)
}

/// Maps a `[RuntimeProvided]` `System.Net.Sockets.Socket` / `System.Net.Security.TlsNative` seam static
/// to the C-ABI extern the AOT links against in `lamella-runtime-support-net` (the no_std staticlib
/// wrapping the SAME lamella-net-smoltcp + lamella-tls-mbedtls crates the interpreter binds). Returns
/// `None` for any other method (it keeps its normal lowering). The names are PROVISIONAL, chosen to
/// mirror the managed method names 1:1 -- the whole table reconciles in one place against the
/// staticlib's exact export list; the marshalling ([`runtime_seam_body`]) is name-independent.
fn net_seam_import(namespace: &str, type_name: &str, method: Option<&str>) -> Option<&'static str> {
    Some(match (namespace, type_name, method?) {
        ("System.Net.Sockets", "Socket", "ConnectStart") => "lamella_net_connect_start",
        ("System.Net.Sockets", "Socket", "ConnectPoll") => "lamella_thread_connect_poll",
        ("System.Net.Sockets", "Socket", "ListenStart") => "lamella_net_listen_start",
        ("System.Net.Sockets", "Socket", "AcceptPoll") => "lamella_thread_accept_poll",
        ("System.Net.Sockets", "Socket", "SendPoll") => "lamella_thread_send_poll",
        ("System.Net.Sockets", "Socket", "ReceivePoll") => "lamella_thread_recv_poll",
        ("System.Net.Sockets", "Socket", "LocalPort") => "lamella_net_local_port",
        ("System.Net.Sockets", "Socket", "CloseSocket") => "lamella_net_close",
        ("System.Net.Security", "TlsNative", "ClientConfig") => "lamella_tls_client_config",
        ("System.Net.Security", "TlsNative", "ClientNew") => "lamella_tls_client_new",
        ("System.Net.Security", "TlsNative", "Process") => "lamella_tls_process",
        ("System.Net.Security", "TlsNative", "WantsWrite") => "lamella_tls_wants_write",
        ("System.Net.Security", "TlsNative", "WriteTls") => "lamella_tls_write_tls",
        ("System.Net.Security", "TlsNative", "ReadTls") => "lamella_tls_read_tls",
        ("System.Net.Security", "TlsNative", "ReadPlain") => "lamella_tls_read_plain",
        ("System.Net.Security", "TlsNative", "WritePlain") => "lamella_tls_write_plain",
        ("System.Net.Security", "TlsNative", "PeerCert") => "lamella_tls_peer_cert",
        ("System.Net.Security", "TlsNative", "CloseTls") => "lamella_tls_close",
        ("System.Net.Security", "TlsNative", "DefaultStack") => "lamella_tls_default_stack",
        _ => return None,
    })
}

/// Whether a net/TLS seam's `byte[]` buffer is a SLICE `(byte[] buffer, int offset, int count)` -- the
/// Send/Receive/Write/Read family, whose native ABI is `(h, buf*, len)` with the offset folded into `buf*`
/// and `len = count`. The address/config/cert seams pass a WHOLE array instead (`(arr + 4, arr.Length)`),
/// so they fold nothing. Keyed by method name because a byte[]-then-two-ints signature is ambiguous by
/// type alone (`ListenStart(addr, port, backlog)` is a whole array followed by two unrelated scalars).
fn net_seam_folds_buffer(method: Option<&str>) -> bool {
    matches!(
        method,
        Some("SendPoll" | "ReceivePoll" | "WriteTls" | "ReadTls" | "ReadPlain" | "WritePlain")
    )
}

/// A net/TLS seam (UDP + server-side TLS) with no native extern: the net staticlib exports none for it.
/// It cannot be a link error: `Socket.Bind` STATICALLY references `UdpBind` (its `Dgram` branch, never taken
/// for a TCP program), so a TCP program would otherwise fail to link over an unreachable path. Instead the
/// AOT synthesizes it to return the `SockError` (-2) sentinel -- the corlib LINKS, and a program that
/// actually uses UDP / server TLS throws `SocketException` at runtime (loud), not a wrong value.
fn net_seam_deferred(namespace: &str, type_name: &str, method: Option<&str>) -> bool {
    matches!(
        (namespace, type_name, method),
        ("System.Net.Sockets", "Socket", Some("UdpBind" | "UdpSendTo" | "UdpReceiveFrom"))
            | ("System.Net.Security", "TlsNative", Some("ServerConfig" | "ServerNew"))
    )
}

/// The C-ABI import for a `System.Threading.Thread` `[RuntimeProvided]` scheduler primitive whose
/// managed arguments pass straight through (YieldThread / JoinThread / SleepThread -> the
/// runtime-support cooperative scheduler). StartThread is deliberately NOT here: its body also passes
/// the compiled `ThreadEntry` helper's address, so it synthesizes via [`thread_start_body`].
fn thread_seam_import(namespace: &str, type_name: &str, method: Option<&str>) -> Option<&'static str> {
    if (namespace, type_name) != ("System.Threading", "Thread") {
        return None;
    }
    Some(match method? {
        "YieldThread" => "lamella_thread_yield",
        "JoinThread" => "lamella_thread_join",
        "SleepThread" => "lamella_thread_sleep",
        _ => return None,
    })
}

/// The C-ABI import for a `System.Threading.Monitor` `[RuntimeProvided]` lock intrinsic (threading
/// Tier 3): the runtime-support per-object lock table behind C#'s `lock` statement and the
/// Wait/Pulse condition layer. Each takes the lock OBJECT, which [`runtime_seam_body`] marshals as
/// its raw address (`RefToInt`) -- the table keys on object addresses, so a locked object joins
/// the GC pin/relocate contract alongside `SCHED.entry_args`. A contended `lamella_monitor_enter`
/// BLOCKS its green thread (runnable bit cleared, `NotParked` -- the hand-off wake, invisible to
/// the reactor), so the seam is a park site like the `lamella_thread_*_poll` wrappers.
fn monitor_seam_import(
    namespace: &str,
    type_name: &str,
    method: Option<&str>,
) -> Option<&'static str> {
    if (namespace, type_name) != ("System.Threading", "Monitor") {
        return None;
    }
    Some(match method? {
        "EnterLock" => "lamella_monitor_enter",
        "ExitLock" => "lamella_monitor_exit",
        "TryEnterLock" => "lamella_monitor_try_enter",
        "WaitLock" => "lamella_monitor_wait",
        "PulseLock" => "lamella_monitor_pulse",
        "PulseAllLock" => "lamella_monitor_pulse_all",
        _ => return None,
    })
}

/// A synthesized MIR body for `System.Threading.Thread.StartThread(ThreadStart, bool)` (the
/// `[RuntimeProvided]` spawn). The native scheduler cannot invoke a managed delegate, so the body
/// passes THREE things to `lamella_thread_start`: the code address of the compiled managed entry
/// helper `Thread.ThreadEntry` (`entry_func`'s [`Inst::FuncAddr`] -- the same pool-word-with-Thumb-bit
/// mechanism a delegate's `_methodPtr` uses, so the native trampoline calls it directly), the
/// ThreadStart delegate as a raw pointer (`RefToInt`; opaque to the native side, handed back as
/// ThreadEntry's argument), and `isBackground` through. Returns the new thread's scheduler id. The
/// delegate stays reachable from the caller's frame across the call (a call site is a safepoint;
/// the cooperative tier collects only at safepoints), and the scheduler slot holding it is a de-facto
/// root until Tier 3 GC rooting -- fine for the no-collection tier this bridges.
fn thread_start_body(param_types: &[MirType], entry_func: u32) -> Function {
    let i32t = MirType::I32;
    let (mut b, params) = MirBuilder::new(param_types);
    let entry = b.emit(i32t, Inst::FuncAddr { func: entry_func });
    let delegate = b.emit(
        i32t,
        Inst::Convert {
            value: params[0],
            kind: ConvKind::RefToInt,
        },
    );
    let id = b.emit(
        i32t,
        Inst::PInvoke {
            import: "lamella_thread_start".into(),
            args: vec![entry, delegate, params[1]],
        },
    );
    b.ret(id);
    b.finish(Some(i32t))
}

/// The MethodDef rid of `namespace.type_name::method` in `assembly` -- which IS its function index in
/// the rid-keyed function vec (the resolver's identity mapping), usable as an [`Inst::FuncAddr`]
/// target. `None` when the assembly predates the method (the caller degrades to the placeholder body).
fn find_method_rid(
    assembly: &Assembly,
    namespace: &str,
    type_name: &str,
    method: &str,
) -> Option<u32> {
    for type_def in assembly.type_defs() {
        if type_def.name().map(|n| (n.namespace, n.name)) == Some((namespace, type_name)) {
            for m in type_def.methods() {
                if m.name() == Some(method) {
                    return Some(m.rid());
                }
            }
        }
    }
    None
}

/// The synthesized body for a `Lamella.Hardware.Mmio.Read{8,16,32}(uint address)`: one `width`-byte
/// [`Inst::Load`] at the argument address (the IR's memory-mapped-I/O read primitive), ZERO-EXTENDED
/// to i32 and returned. Compiles to `ldrb`/`ldrh`/`ldr [r0]; bx lr` -- the interpreter's volatile-seam
/// binding and this body realize the same contract, so a driver proven on the interpreter reads the
/// same register at the same WIDTH (the sub-word forms reach an 8/16-bit register at an unaligned
/// address a 32-bit access would fault on an M0+/ARMv6-M part -- SAMD21 pinmux/clock control).
fn mmio_read_body(width: u32) -> Function {
    let i32t = MirType::I32;
    let (mut b, params) = MirBuilder::new(&[i32t]);
    let value = b.emit(
        i32t,
        Inst::Load {
            address: params[0],
            width,
            signed: false,
        },
    );
    b.ret(value);
    b.finish(Some(i32t))
}

/// The synthesized body for a `Lamella.Hardware.Mmio.Write{8,16,32}(uint address, T value)`: one
/// `width`-byte [`Inst::Store`] of the value argument (its low byte/halfword at a sub-word width) at
/// the address argument, then return -- `strb`/`strh`/`str`.
fn mmio_write_body(width: u32) -> Function {
    let i32t = MirType::I32;
    let (mut b, params) = MirBuilder::new(&[i32t, i32t]);
    b.side(Inst::Store {
        address: params[0],
        value: params[1],
        width,
    });
    b.ret_void();
    b.finish(None)
}

/// A single-block body returning the `i32` constant `value` (a deferred seam -> `SockError`). Takes the
/// managed `param_types` so its ABI signature matches the caller; the arguments are ignored.
fn net_deferred_body(param_types: &[MirType], value: i64) -> Function {
    let (mut b, _params) = MirBuilder::new(param_types);
    let v = b.emit(
        MirType::I32,
        Inst::ConstInt {
            ty: MirType::I32,
            value,
        },
    );
    b.ret(v);
    b.finish(Some(MirType::I32))
}

/// A synthesized MIR body for a `System.Net` / `System.Net.Security` `[RuntimeProvided]` seam static (the
/// socket + TLS primitives). It MARSHALS the managed arguments to the C-ABI that the linked
/// `lamella-runtime-support-net` exports, calls it, and returns the result. The marshalling mirrors the
/// runtime's C-ABI (`939f993933`) exactly -- the same shape the interpreter's binding passes:
/// * a BUFFER SLICE (`fold_buffer`: a `byte[] buffer, int offset, int count` triple, as in Send/Receive/
///   Write/Read) crosses as ONE (ptr, len) pair with the offset FOLDED IN: `ptr = &buffer[offset]` (the
///   ObjectRef + 4 + offset) and `len = count`. The offset/count args are consumed, NOT passed separately --
///   the native `send_poll(h, buf*, len)` sends exactly the `[offset, offset+count)` slice.
/// * a WHOLE `byte[]` / `string` (an address, a cert buffer, a hostname) crosses as `(ptr = arr + 4, len =
///   arr.Length)` -- past the 4-byte header, the element/unit count at `arr + 0`.
/// * a scalar (`int`) passes straight through. (An extra managed arg the native side ignores -- e.g.
///   `ClientConfig`'s `rootsPem`, absent from `client_config(stack, verify)` -- is harmless: it lands in a
///   register the callee never reads.)
/// Only the FIRST array of a `fold_buffer` seam is a slice; every seam has at most one such buffer.
///
/// `import` is the extern the object path resolves through the linker: ARM `lower_runtime_calls` and
/// RISC-V `rewrite_pinvoke` rewrite the emitted `PInvoke` to a `CallNative`, and WASM binds it to a
/// `lamella_native` host import. Phase A (single-threaded blocking): the seam is an alloc-free leaf call
/// with no safepoint inside, and the managed busy-poll loop (`while (Poll() == WouldBlock) {}`) re-passes
/// the buffer each call -- so no GC pin is needed until Phase B.
///
/// `param_types` are the managed argument MirTypes (the caller resolves them once through the canonical
/// [`mir_type`]); `parameters` are the same arguments' `SigType`s, which classify the marshalling.
pub fn runtime_seam_body(
    param_types: &[MirType],
    parameters: &[SigType],
    returns_value: bool,
    fold_buffer: bool,
    import: &str,
) -> Function {
    let i32t = MirType::I32;
    let (mut b, params) = MirBuilder::new(param_types);
    let mut native_args: Vec<ValueId> = Vec::new();
    let mut folded = false;
    let mut index = 0;
    while index < parameters.len() {
        let arg = params[index];
        match &parameters[index] {
            SigType::SzArray(_) if fold_buffer && !folded => {
                let offset = params[index + 1];
                let count = params[index + 2];
                let addr = b.emit(i32t, Inst::Convert { value: arg, kind: ConvKind::RefToInt });
                let four = b.emit(i32t, Inst::ConstInt { ty: i32t, value: 4 });
                let base = b.emit(i32t, Inst::Binary { op: BinOp::Add, lhs: addr, rhs: four });
                let ptr = b.emit(i32t, Inst::Binary { op: BinOp::Add, lhs: base, rhs: offset });
                native_args.push(ptr);
                native_args.push(count);
                folded = true;
                index += 3;
                continue;
            }
            SigType::SzArray(_) | SigType::String => {
                let addr = b.emit(i32t, Inst::Convert { value: arg, kind: ConvKind::RefToInt });
                let four = b.emit(i32t, Inst::ConstInt { ty: i32t, value: 4 });
                let ptr = b.emit(i32t, Inst::Binary { op: BinOp::Add, lhs: addr, rhs: four });
                let len = b.emit(i32t, Inst::Load { address: addr, width: 4, signed: false });
                native_args.push(ptr);
                native_args.push(len);
            }
            SigType::Object | SigType::Class(_) => {
                let addr = b.emit(i32t, Inst::Convert { value: arg, kind: ConvKind::RefToInt });
                native_args.push(addr);
            }
            _ => native_args.push(arg),
        }
        index += 1;
    }
    if returns_value {
        let result = b.emit(
            i32t,
            Inst::PInvoke {
                import: import.into(),
                args: native_args,
            },
        );
        b.ret(result);
        b.finish(Some(i32t))
    } else {
        b.side(Inst::PInvoke {
            import: import.into(),
            args: native_args,
        });
        b.ret_void();
        b.finish(None)
    }
}

#[cfg(all(test, feature = "arm32"))]
mod tests {
    use super::*;

    /// THE POISONING SHAPE, PINNED WHERE THE QEMU ROW CANNOT REACH. `static-init-throws` scores this
    /// on emulated ARM silicon and is the better evidence, but it scores ONE code model: the same
    /// `type_init_thunk_body` feeds the RISC-V object path and the flat/wasm path, and neither has a
    /// scored program that runs a failing initializer. Asserting the MIR covers all three by
    /// construction, because there is one body and three lowerings of it.
    ///
    /// NOTE: every claim here is checked against a DERIVED value rather than a restated one -- the tag
    /// comes from `exception_tag_for_name`, the same function a `catch` clause's tag comes from, so
    /// a change to the hash moves both sides together and this test stays about the STRUCTURE.
    #[cfg(any(feature = "arm32", feature = "riscv32", feature = "wasm"))]
    #[test]
    fn a_failing_initializer_raises_the_same_wrapper_tag_at_every_later_access() {
        let func = type_init_thunk_body(16, 7);
        lamella_ir::verify(&func).expect("the thunk is well-formed MIR");

        let tie = i64::from(lamella_metadata::exception_tag_for_name(
            "System",
            "TypeInitializationException",
        ));
        let const_of = |value: ValueId| {
            func.blocks
                .iter()
                .flat_map(|b| &b.insts)
                .find_map(|(id, inst)| match inst {
                    Inst::ConstInt { value: v, .. } if *id == value => Some(*v),
                    _ => None,
                })
        };

        let raising: Vec<(usize, i64)> = func
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(b, block)| {
                block.insts.iter().find_map(|(_, inst)| match inst {
                    Inst::StaticStore {
                        owner: StaticOwner::Own,
                        offset: cil::G_EXCEPTION_TAG_OFFSET,
                        value,
                    } => Some((b, const_of(*value).expect("the raised tag is a constant"))),
                    _ => None,
                })
            })
            .collect();
        assert_eq!(raising.len(), 2, "one raise per failing access, and no more");
        let mut reached = alloc::vec![false; func.blocks.len()];
        let mut worklist = alloc::vec![func.entry.index()];
        while let Some(b) = worklist.pop() {
            if core::mem::replace(&mut reached[b], true) {
                continue;
            }
            match func.blocks[b].terminator.as_ref().expect("terminated") {
                Terminator::Jump { target, .. } => worklist.push(target.index()),
                Terminator::Branch {
                    if_true, if_false, ..
                } => {
                    worklist.push(if_true.index());
                    worklist.push(if_false.index());
                }
                Terminator::Return(_) | Terminator::Unreachable => {}
            }
        }
        assert!(
            raising.iter().all(|(b, _)| reached[*b]),
            "both raises are reachable from the entry, not merely present"
        );
        assert_eq!(
            raising[0].1, raising[1].1,
            "'rethrows the SAME exception' is the two raises writing one value"
        );
        assert_eq!(
            raising[0].1, tie,
            "the wrapper the CLI library specification names, not the constructor's own exception"
        );

        let calling: Vec<usize> = func
            .blocks
            .iter()
            .enumerate()
            .filter(|(_, block)| {
                block
                    .insts
                    .iter()
                    .any(|(_, inst)| matches!(inst, Inst::Call { callee: 7, .. }))
            })
            .map(|(b, _)| b)
            .collect();
        assert_eq!(calling.len(), 1, "one call site for the initializer");
        assert!(
            !raising.iter().any(|(b, _)| *b == calling[0]),
            "the call site is not a raising block -- a successful run leaves no tag in flight"
        );

        let mut written: Vec<i64> = func
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|(_, inst)| match inst {
                Inst::StaticStore {
                    owner: StaticOwner::Own,
                    offset: 16,
                    value,
                } => const_of(*value),
                _ => None,
            })
            .collect();
        written.sort_unstable();
        written.dedup();
        assert_eq!(written, alloc::vec![TYPE_INIT_RAN, TYPE_INIT_POISONED]);
        assert!(
            !written.contains(&0),
            "0 is 'not yet run' and is the zeroed region, never a written state"
        );
    }

    #[cfg(any(feature = "arm32", feature = "riscv32", feature = "wasm"))]
    #[test]
    fn the_element_width_table_gives_every_kind_its_allocation_stride() {
        let shift_of =
            |kind: u32| (ELEMENT_WIDTH_SHIFTS >> (2 * kind)) & 3;
        for (namespace, name, expected) in [
            ("System", "SByte", 1u32),
            ("System", "Byte", 1),
            ("System", "Boolean", 1),
            ("System", "Int16", 2),
            ("System", "UInt16", 2),
            ("System", "Char", 2),
            ("System", "Int32", 4),
            ("System", "UInt32", 4),
            ("System", "Single", 4),
            ("System", "Int64", 8),
            ("System", "UInt64", 8),
            ("System", "Double", 8),
        ] {
            let kind = crate::resolver::primitive_element_kind(namespace, name)
                .unwrap_or_else(|| panic!("{name} is a frozen primitive"));
            assert_eq!(
                1u32 << shift_of(kind),
                expected,
                "{name} (kind {kind}) strides by {expected}"
            );
        }
        assert_eq!(1u32 << shift_of(crate::resolver::ELEMENT_KIND_REFERENCE), 4);
        assert!(
            i64::from(crate::resolver::ELEMENT_KIND_OPAQUE) > MAX_STRIDABLE_ELEMENT_KIND,
            "OPAQUE must fall outside the stridable kinds"
        );
    }

    #[cfg(feature = "arm32")]
    #[test]
    fn the_array_range_cores_verify_and_lower_and_clear_traps_rather_than_lying() {
        let copy = array_copy_core_body();
        lamella_ir::verify(&copy).expect("CopyCore verifies");
        let clear = array_clear_core_body();
        lamella_ir::verify(&clear).expect("ClearCore verifies");
        let rank = array_rank_body();
        lamella_ir::verify(&rank).expect("get_Rank verifies");
        crate::arm32::lower_object(
            &[copy.clone(), clear.clone(), rank],
            &["Array_CopyCore", "Array_ClearCore", "Array_get_Rank"],
            &[],
        )
        .expect("all three lower on the object path");
        assert!(
            copy.blocks
                .iter()
                .all(|b| !matches!(b.terminator, Some(Terminator::Unreachable))),
            "CopyCore answers false instead of trapping"
        );
        assert!(
            clear
                .blocks
                .iter()
                .any(|b| matches!(b.terminator, Some(Terminator::Unreachable))),
            "ClearCore traps on a range it cannot stride"
        );
    }

    #[cfg(feature = "arm32")]
    #[test]
    fn get_value_reads_the_element_kind_and_traps_on_everything_but_a_reference() {
        let body = array_get_value_body();
        lamella_ir::verify(&body).expect("GetValue verifies");
        crate::arm32::lower_object(&[body.clone()], &["Array_GetValue"], &[])
            .expect("GetValue lowers on the object path");
        assert!(
            body.blocks
                .iter()
                .any(|b| matches!(b.terminator, Some(Terminator::Unreachable))),
            "GetValue must trap on an element it cannot answer for"
        );
        let insts: Vec<_> = body.blocks.iter().flat_map(|b| &b.insts).collect();
        let reference = insts
            .iter()
            .find(|(_, i)| {
                matches!(
                    i,
                    Inst::ConstInt { value, .. }
                        if *value == i64::from(crate::resolver::ELEMENT_KIND_REFERENCE)
                )
            })
            .map(|(v, _)| *v)
            .expect("GetValue names ELEMENT_KIND_REFERENCE");
        let kind_test = insts
            .iter()
            .find(|(_, i)| matches!(i, Inst::Compare { rhs, .. } if *rhs == reference))
            .map(|(v, _)| *v)
            .expect("GetValue compares the element kind against REFERENCE");
        assert!(
            body.blocks.iter().any(|b| matches!(
                b.terminator,
                Some(Terminator::Branch { cond, .. }) if cond == kind_test
            )),
            "GetValue must BRANCH on the element-kind test -- an unbranched compare hands an int              back as a reference, which is what dropping this guard does on silicon"
        );

        let described = insts
            .iter()
            .find_map(|(_, i)| match i {
                Inst::AllocDescribed {
                    descriptor,
                    payload_size,
                } => Some((*descriptor, *payload_size)),
                _ => None,
            })
            .expect("GetValue boxes a primitive element with AllocDescribed");
        let payload_is_a_load = insts
            .iter()
            .any(|(v, i)| *v == described.1 && matches!(i, Inst::Load { .. }));
        assert!(
            payload_is_a_load,
            "the boxed size must be READ from the element descriptor, not assumed"
        );
        assert!(
            insts.iter().any(|(_, i)| matches!(
                i,
                Inst::CopyBlock { size, .. } if *size == described.1
            )),
            "the element copy must be the size the box was allocated with"
        );
    }

    #[cfg(feature = "arm32")]
    #[test]
    fn set_value_checks_assignability_and_never_stores_on_a_verdict_it_did_not_branch_on() {
        let body = array_set_value_body();
        lamella_ir::verify(&body).expect("SetValue verifies");
        crate::arm32::lower_object(&[body.clone()], &["Array_SetValue"], &[])
            .expect("SetValue lowers on the object path");
        assert!(
            body.blocks
                .iter()
                .any(|b| matches!(b.terminator, Some(Terminator::Unreachable))),
            "SetValue must trap on a store it cannot perform"
        );
        let insts: Vec<_> = body.blocks.iter().flat_map(|b| &b.insts).collect();
        let branched_on = |cond: ValueId| {
            body.blocks.iter().any(|b| matches!(
                b.terminator,
                Some(Terminator::Branch { cond: c, .. }) if c == cond
            ))
        };

        let scan = insts
            .iter()
            .find_map(|(v, i)| match i {
                Inst::CastClassScan { args } => Some((*v, args.clone())),
                _ => None,
            })
            .expect("SetValue checks assignability with a CastClassScan");
        assert!(
            branched_on(scan.0),
            "SetValue must BRANCH on the assignability scan -- an unbranched scan stores a value of \
             any type into a reference array, which is what deleting this guard does on silicon"
        );
        assert!(
            insts
                .iter()
                .any(|(v, i)| *v == scan.1[0] && matches!(i, Inst::LoadTypeDesc { .. })),
            "the scan must start at the VALUE's own descriptor"
        );
        assert!(
            insts
                .iter()
                .any(|(v, i)| *v == scan.1[1] && matches!(i, Inst::Binary { op: BinOp::Add, .. })),
            "the scan must seek the ELEMENT descriptor, not the array's own"
        );

        let value_param = body.params.get(1).map(|_| ValueId(1)).expect("SetValue takes a value");
        let value_bits: Vec<ValueId> = insts
            .iter()
            .filter(|(_, i)| {
                matches!(i, Inst::Convert { value, kind: ConvKind::RefToInt } if *value == value_param)
            })
            .map(|(v, _)| *v)
            .collect();
        assert_eq!(
            value_bits.len(),
            2,
            "each half takes the value's bits exactly once"
        );
        for bits in value_bits {
            let null_test = insts
                .iter()
                .find(|(_, i)| matches!(i, Inst::Compare { lhs, rhs, .. }
                    if *lhs == bits
                        && insts.iter().any(|(v, c)| *v == *rhs
                            && matches!(c, Inst::ConstInt { value: 0, .. }))))
                .map(|(v, _)| *v)
                .expect("each half tests the value against null before reading its descriptor");
            assert!(
                branched_on(null_test),
                "SetValue must BRANCH on the null test -- an unbranched one loads a descriptor from \
                 the word before the heap and scans from whatever it holds"
            );
        }

        let reference = insts
            .iter()
            .find(|(_, i)| {
                matches!(
                    i,
                    Inst::ConstInt { value, .. }
                        if *value == i64::from(crate::resolver::ELEMENT_KIND_REFERENCE)
                )
            })
            .map(|(v, _)| *v)
            .expect("SetValue names ELEMENT_KIND_REFERENCE");
        let kind_test = insts
            .iter()
            .find(|(_, i)| matches!(i, Inst::Compare { rhs, .. } if *rhs == reference))
            .map(|(v, _)| *v)
            .expect("SetValue compares the element kind against REFERENCE");
        assert!(
            branched_on(kind_test),
            "SetValue must BRANCH on the element kind -- without it a boxed int's REFERENCE is stored \
             into an int[] as though it were the integer"
        );

        let copy = insts
            .iter()
            .find_map(|(_, i)| match i {
                Inst::CopyBlock { src, size, .. } => Some((*src, *size)),
                _ => None,
            })
            .expect("SetValue copies a primitive element's payload in");
        assert!(
            insts
                .iter()
                .any(|(v, i)| *v == copy.1 && matches!(i, Inst::Load { .. })),
            "the copied size must be READ from the element descriptor, not assumed -- a constant is \
             how a `char` would silently take four bytes"
        );
        let agreement = insts
            .iter()
            .find(|(_, i)| matches!(i, Inst::Compare { lhs, .. } if *lhs == copy.1))
            .map(|(v, _)| *v)
            .expect("SetValue compares the payload size against the array's stride");
        assert!(
            branched_on(agreement),
            "SetValue must BRANCH on the payload/width agreement"
        );
        let exact = insts
            .iter()
            .find(|(_, i)| matches!(
                i,
                Inst::Compare { op: CmpOp::Eq, lhs, rhs }
                    if insts.iter().any(|(v, d)| *v == *lhs && matches!(d, Inst::LoadTypeDesc { .. }))
                        && insts.iter().any(|(v, d)| *v == *rhs
                            && matches!(d, Inst::Binary { op: BinOp::Add, .. }))
            ))
            .map(|(v, _)| *v)
            .expect("SetValue compares the box's descriptor against the element's");
        assert!(
            branched_on(exact),
            "SetValue must BRANCH on the exact-descriptor test"
        );

        assert!(
            !insts
                .iter()
                .any(|(_, i)| crate::regalloc::is_safepoint(i)),
            "SetValue must not allocate -- a safepoint here would let the array MOVE under the \
             address computed before it"
        );
    }

    #[cfg(feature = "arm32")]
    #[test]
    fn clone_allocates_the_arrays_own_type_at_its_own_size_and_reloads_after_the_safepoint() {
        let body = array_clone_body();
        lamella_ir::verify(&body).expect("Clone verifies");
        crate::arm32::lower_object(&[body.clone()], &["Array_Clone"], &[])
            .expect("Clone lowers on the object path");
        assert!(
            body.blocks
                .iter()
                .any(|b| matches!(b.terminator, Some(Terminator::Unreachable))),
            "Clone must trap on an array it cannot size"
        );
        let insts: Vec<_> = body.blocks.iter().flat_map(|b| &b.insts).collect();

        let (descriptor, payload) = insts
            .iter()
            .find_map(|(_, i)| match i {
                Inst::AllocDescribed {
                    descriptor,
                    payload_size,
                } => Some((*descriptor, *payload_size)),
                _ => None,
            })
            .expect("Clone allocates with AllocDescribed");
        assert!(
            insts
                .iter()
                .any(|(v, i)| *v == descriptor && matches!(i, Inst::LoadTypeDesc { .. })),
            "the clone must be allocated against the SOURCE array's own descriptor"
        );
        assert!(
            insts
                .iter()
                .any(|(v, i)| *v == payload && matches!(i, Inst::Binary { op: BinOp::Add, .. })),
            "the clone's payload size must be computed (4 + length * stride), not assumed"
        );
        assert!(
            insts.iter().any(|(_, i)| matches!(
                i,
                Inst::CopyBlock { size, .. } if *size == payload
            )),
            "the copy must be the size the clone was allocated with"
        );

        let copy_src = insts
            .iter()
            .find_map(|(_, i)| match i {
                Inst::CopyBlock { src, .. } => Some(*src),
                _ => None,
            })
            .expect("Clone copies the payload");
        let alloc_block = body
            .blocks
            .iter()
            .find(|b| {
                b.insts
                    .iter()
                    .any(|(_, i)| matches!(i, Inst::AllocDescribed { .. }))
            })
            .expect("the allocation is in some block");
        let alloc_at = alloc_block
            .insts
            .iter()
            .position(|(_, i)| matches!(i, Inst::AllocDescribed { .. }))
            .expect("the allocation has a position");
        let src_at = alloc_block
            .insts
            .iter()
            .position(|(v, _)| *v == copy_src)
            .expect(
                "the copy's SOURCE must be recomputed in the allocating block -- reusing an address \
                 taken before the allocation reads the array's OLD location after a collection",
            );
        assert!(
            src_at > alloc_at,
            "the source address must be taken AFTER the allocation that can move the array"
        );
    }

    #[cfg(feature = "arm32")]
    #[test]
    fn every_console_overload_is_synthesized_in_write_writeline_pairs() {
        for param in [
            SigType::String,
            SigType::Char,
            SigType::I4,
            SigType::Boolean,
            SigType::I8,
            SigType::U4,
            SigType::U8,
        ] {
            for name in ["Write", "WriteLine"] {
                let f = console_seam_body(Some(name), core::slice::from_ref(&param))
                    .unwrap_or_else(|| panic!("Console.{name}({param:?}) is not synthesized"));
                lamella_ir::verify(&f)
                    .unwrap_or_else(|e| panic!("Console.{name}({param:?}) verify: {e:?}"));
            }
        }
        assert!(
            console_seam_body(Some("Write"), &[SigType::R8]).is_none(),
            "Console.Write(double) is managed, not a synthesized seam"
        );
    }

    #[cfg(feature = "arm32")]
    #[test]
    fn synthesized_string_readers_verify_and_lower() {
        for (type_name, name, params) in [
            ("String", "get_Length", 0usize),
            ("String", "get_Chars", 1usize),
            ("Array", "get_Length", 0usize),
        ] {
            let f = synthesize_runtime_reader("System", type_name, Some(name), params)
                .unwrap_or_else(|| panic!("{type_name}.{name} not synthesized"));
            lamella_ir::verify(&f).unwrap_or_else(|e| panic!("{type_name}.{name} verify: {e:?}"));
            crate::arm32::lower(&f).unwrap_or_else(|e| panic!("{type_name}.{name} lower: {e:?}"));
        }
        assert!(
            synthesize_runtime_reader("System", "Array", Some("get_Chars"), 1).is_none(),
            "get_Chars synthesizes for String only"
        );
    }

    #[cfg(feature = "arm32")]
    #[test]
    fn synthesized_type_seams_verify_and_lower() {
        for (name, params) in [("GetTypeFromHandle", 1usize), ("HandleEquals", 2usize)] {
            let f = synthesize_type_seam("System", "Type", Some(name), params)
                .unwrap_or_else(|| panic!("Type.{name} not synthesized"));
            lamella_ir::verify(&f).unwrap_or_else(|e| panic!("Type.{name} verify: {e:?}"));
            crate::arm32::lower(&f).unwrap_or_else(|e| panic!("Type.{name} lower: {e:?}"));
        }
        let g = synthesize_type_seam("System", "Type", Some("GetTypeFromHandle"), 1).unwrap();
        assert_eq!(g.params.len(), 1, "the handle is the single argument");
        assert_eq!(g.ret, Some(MirType::ObjectRef), "a Type is a reference");
        assert!(
            synthesize_type_seam("System", "Type", Some("GetHashCode"), 0).is_none(),
            "unlisted Type members keep their placeholder body"
        );
        assert!(
            synthesize_type_seam("System", "String", Some("HandleEquals"), 2).is_none(),
            "the seams are System.Type's alone"
        );
    }

    #[cfg(any(feature = "arm32", feature = "riscv32", feature = "wasm"))]
    #[test]
    fn alloc_like_lowers_on_every_backend() {
        let f = Function {
            params: vec![MirType::ObjectRef],
            ret: Some(MirType::ObjectRef),
            value_types: vec![MirType::ObjectRef, MirType::ObjectRef],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![(
                    ValueId(1),
                    Inst::AllocLike {
                        proto: ValueId(0),
                        payload_size: 12,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        lamella_ir::verify(&f).expect("AllocLike verifies");
        #[cfg(feature = "arm32")]
        crate::arm32::lower_object(&[f.clone()], &["clone_it"], &[]).expect("arm lowers AllocLike");
        #[cfg(feature = "riscv32")]
        crate::riscv32::lower_object(&[f.clone()], &["clone_it"], &[], &[])
            .expect("riscv lowers AllocLike");
        #[cfg(feature = "wasm")]
        crate::wasm::lower(&f).expect("wasm lowers AllocLike");
    }

    #[cfg(any(feature = "arm32", feature = "riscv32", feature = "wasm"))]
    #[test]
    fn delegate_combine_body_verifies_and_lowers() {
        let f = delegate_combine_body();
        lamella_ir::verify(&f).expect("Combine body verifies");
        #[cfg(feature = "arm32")]
        crate::arm32::lower_object(&[f.clone()], &["combine"], &[]).expect("arm lowers Combine");
        #[cfg(feature = "riscv32")]
        crate::riscv32::lower_object(&[f.clone()], &["combine"], &[], &[])
            .expect("riscv lowers Combine");
        #[cfg(feature = "wasm")]
        crate::wasm::lower(&f).expect("wasm lowers Combine");
    }

    #[test]
    fn a_delegate_invoke_thunk_dispatches_instead_of_returning() {
        for ret in [Some(MirType::I32), None] {
            let returns = ret.is_some();
            let f = delegate_invoke_body(&[MirType::ObjectRef, MirType::I32], ret);
            lamella_ir::verify(&f).expect("the Invoke thunk verifies");
            let dispatches = f.blocks.iter().flat_map(|b| &b.insts).any(|(_, inst)| {
                matches!(
                    inst,
                    Inst::InvokeDelegate { delegate, args, returns_value }
                        if *delegate == ValueId(0) && args == &[ValueId(1)] && *returns_value == returns
                )
            });
            assert!(
                dispatches,
                "the synthesized Invoke must dispatch through its receiver, not answer a constant"
            );
            #[cfg(feature = "arm32")]
            crate::arm32::lower_object(&[f.clone()], &["invoke"], &[]).expect("arm lowers Invoke");
            #[cfg(feature = "riscv32")]
            crate::riscv32::lower_object(&[f.clone()], &["invoke"], &[], &[])
                .expect("riscv lowers Invoke");
            #[cfg(feature = "wasm")]
            crate::wasm::lower(&f).expect("wasm lowers Invoke");
        }
    }

    #[cfg(any(feature = "arm32", feature = "riscv32", feature = "wasm"))]
    #[test]
    fn delegate_remove_body_verifies_and_lowers() {
        let f = delegate_remove_body();
        lamella_ir::verify(&f).expect("Remove body verifies");
        #[cfg(feature = "arm32")]
        crate::arm32::lower_object(&[f.clone()], &["remove"], &[]).expect("arm lowers Remove");
        #[cfg(feature = "riscv32")]
        crate::riscv32::lower_object(&[f.clone()], &["remove"], &[], &[])
            .expect("riscv lowers Remove");
        #[cfg(feature = "wasm")]
        crate::wasm::lower(&f).expect("wasm lowers Remove");
    }

    #[cfg(any(feature = "arm32", feature = "riscv32", feature = "wasm"))]
    #[test]
    fn net_seam_bodies_marshal_and_lower() {
        use alloc::boxed::Box;
        let obj = MirType::ObjectRef;
        let i32t = MirType::I32;
        let bytes = || SigType::SzArray(Box::new(SigType::U1));
        let ints = || SigType::SzArray(Box::new(SigType::I4));
        let cases: [(&str, alloc::vec::Vec<MirType>, alloc::vec::Vec<SigType>, bool, bool); 5] = [
            (
                "lamella_net_send_poll",
                vec![i32t, obj, i32t, i32t],
                vec![SigType::I4, bytes(), SigType::I4, SigType::I4],
                true,
                true,
            ),
            (
                "lamella_net_udp_send_to",
                vec![i32t, obj, i32t, i32t, obj, i32t],
                vec![SigType::I4, bytes(), SigType::I4, SigType::I4, bytes(), SigType::I4],
                true,
                true,
            ),
            (
                "lamella_net_udp_receive_from",
                vec![i32t, obj, i32t, i32t, obj, obj],
                vec![SigType::I4, bytes(), SigType::I4, SigType::I4, bytes(), ints()],
                true,
                true,
            ),
            (
                "lamella_tls_client_new",
                vec![i32t, obj],
                vec![SigType::I4, SigType::String],
                true,
                false,
            ),
            ("lamella_net_close", vec![i32t], vec![SigType::I4], false, false),
        ];
        for (label, param_types, parameters, returns_value, fold_buffer) in &cases {
            let label = *label;
            let f = runtime_seam_body(param_types, parameters, *returns_value, *fold_buffer, label);
            lamella_ir::verify(&f).unwrap_or_else(|e| panic!("{label} verify: {e:?}"));
            #[cfg(feature = "arm32")]
            crate::arm32::lower_object(&[f.clone()], &[label], &[])
                .unwrap_or_else(|e| panic!("{label} arm lower: {e:?}"));
            #[cfg(feature = "riscv32")]
            crate::riscv32::lower_object(&[f.clone()], &[label], &[], &[])
                .unwrap_or_else(|e| panic!("{label} riscv lower: {e:?}"));
            #[cfg(feature = "wasm")]
            crate::wasm::lower(&f).unwrap_or_else(|e| panic!("{label} wasm lower: {e:?}"));
        }
    }

    #[test]
    fn net_seam_maps_the_poll_drivers_to_the_parking_wrappers() {
        let socket = |m| net_seam_import("System.Net.Sockets", "Socket", Some(m));
        assert_eq!(socket("ConnectPoll"), Some("lamella_thread_connect_poll"));
        assert_eq!(socket("AcceptPoll"), Some("lamella_thread_accept_poll"));
        assert_eq!(socket("SendPoll"), Some("lamella_thread_send_poll"));
        assert_eq!(socket("ReceivePoll"), Some("lamella_thread_recv_poll"));
        assert_eq!(socket("ConnectStart"), Some("lamella_net_connect_start"));
        assert_eq!(socket("ListenStart"), Some("lamella_net_listen_start"));
        assert_eq!(socket("LocalPort"), Some("lamella_net_local_port"));
        assert_eq!(socket("CloseSocket"), Some("lamella_net_close"));
    }

    #[test]
    fn monitor_seam_maps_the_lock_intrinsics_to_the_lock_table() {
        let monitor = |m| monitor_seam_import("System.Threading", "Monitor", Some(m));
        assert_eq!(monitor("EnterLock"), Some("lamella_monitor_enter"));
        assert_eq!(monitor("ExitLock"), Some("lamella_monitor_exit"));
        assert_eq!(monitor("TryEnterLock"), Some("lamella_monitor_try_enter"));
        assert_eq!(monitor("WaitLock"), Some("lamella_monitor_wait"));
        assert_eq!(monitor("PulseLock"), Some("lamella_monitor_pulse"));
        assert_eq!(monitor("PulseAllLock"), Some("lamella_monitor_pulse_all"));
        assert_eq!(monitor("Enter"), None, "the managed wrappers lower normally");
        assert_eq!(
            monitor_seam_import("System.Threading", "Thread", Some("EnterLock")),
            None
        );
    }

    #[cfg(any(feature = "arm32", feature = "riscv32", feature = "wasm"))]
    #[test]
    fn monitor_seam_bodies_marshal_and_lower() {
        let obj = MirType::ObjectRef;
        for (label, returns_value) in [
            ("lamella_monitor_enter", false),
            ("lamella_monitor_exit", false),
            ("lamella_monitor_try_enter", true),
            ("lamella_monitor_wait", false),
            ("lamella_monitor_pulse", false),
            ("lamella_monitor_pulse_all", false),
        ] {
            let f = runtime_seam_body(
                core::slice::from_ref(&obj),
                &[SigType::Object],
                returns_value,
                false,
                label,
            );
            lamella_ir::verify(&f).unwrap_or_else(|e| panic!("{label} verify: {e:?}"));
            #[cfg(feature = "arm32")]
            crate::arm32::lower_object(&[f.clone()], &[label], &[])
                .unwrap_or_else(|e| panic!("{label} arm lower: {e:?}"));
            #[cfg(feature = "riscv32")]
            crate::riscv32::lower_object(&[f.clone()], &[label], &[], &[])
                .unwrap_or_else(|e| panic!("{label} riscv lower: {e:?}"));
            #[cfg(feature = "wasm")]
            crate::wasm::lower(&f).unwrap_or_else(|e| panic!("{label} wasm lower: {e:?}"));
        }
    }

    #[cfg(feature = "arm32")]
    #[test]
    fn deferred_trap_body_verifies_and_lowers_to_a_trap() {
        let f = deferred_trap_body();
        lamella_ir::verify(&f).expect("deferred trap body verifies");
        assert!(
            matches!(f.blocks[0].terminator, Some(Terminator::Unreachable)),
            "the deferred body is a trap, never a return"
        );
        crate::arm32::lower_object(&[f], &["deferred"], &[]).expect("it lowers on the object path");
    }

    #[cfg(any(feature = "arm32", feature = "riscv32", feature = "wasm"))]
    #[test]
    fn mmio_seam_bodies_are_inline_memory_ops() {
        for width in [1u32, 2, 4] {
            let read = mmio_read_body(width);
            let load_width = read.blocks.iter().flat_map(|b| &b.insts).find_map(|(_, i)| match i {
                Inst::Load { width: w, .. } => Some(*w),
                _ => None,
            });
            assert_eq!(load_width, Some(width), "Read{} loads exactly {width} bytes", width * 8);
            assert!(
                read.blocks.iter().flat_map(|b| &b.insts).all(|(_, i)| matches!(
                    i,
                    Inst::Load { .. } | Inst::ConstInt { .. }
                )),
                "the read must lower to a bare Load, not a call"
            );
            let write = mmio_write_body(width);
            let store_width = write.blocks.iter().flat_map(|b| &b.insts).find_map(|(_, i)| match i {
                Inst::Store { width: w, .. } => Some(*w),
                _ => None,
            });
            assert_eq!(store_width, Some(width), "Write{} stores exactly {width} bytes", width * 8);
            assert!(
                write.blocks.iter().flat_map(|b| &b.insts).all(|(_, i)| matches!(
                    i,
                    Inst::Store { .. } | Inst::ConstInt { .. }
                )),
                "the write must lower to a bare Store, not a call"
            );
            for (label, f) in [("mmio_read", read), ("mmio_write", write)] {
                lamella_ir::verify(&f).unwrap_or_else(|e| panic!("{label}{width} verify: {e:?}"));
                #[cfg(feature = "arm32")]
                crate::arm32::lower_object(&[f.clone()], &[label], &[])
                    .unwrap_or_else(|e| panic!("{label}{width} arm lower: {e:?}"));
                #[cfg(feature = "riscv32")]
                crate::riscv32::lower_object(&[f.clone()], &[label], &[], &[])
                    .unwrap_or_else(|e| panic!("{label}{width} riscv lower: {e:?}"));
                #[cfg(feature = "wasm")]
                crate::wasm::lower(&f).unwrap_or_else(|e| panic!("{label}{width} wasm lower: {e:?}"));
            }
        }
    }

    #[cfg(any(feature = "arm32", feature = "riscv32", feature = "wasm"))]
    #[test]
    fn thread_seam_bodies_marshal_and_lower() {
        let obj = MirType::ObjectRef;
        let i32t = MirType::I32;
        let entry_stub = || Function {
            params: vec![MirType::ObjectRef],
            ret: None,
            value_types: vec![MirType::ObjectRef],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: Vec::new(),
                terminator: Some(Terminator::Return(None)),
            }],
        };
        let start = thread_start_body(&[obj, i32t], 1);
        lamella_ir::verify(&start).expect("StartThread body verifies");
        #[cfg(feature = "arm32")]
        crate::arm32::lower_object(
            &[start.clone(), entry_stub()],
            &["start_thread", "thread_entry"],
            &[],
        )
        .expect("arm lowers StartThread");
        #[cfg(feature = "riscv32")]
        crate::riscv32::lower_object(
            &[start.clone(), entry_stub()],
            &["start_thread", "thread_entry"],
            &[],
            &[],
        )
        .expect("riscv lowers StartThread");
        #[cfg(feature = "wasm")]
        crate::wasm::lower_module(&[start.clone(), entry_stub()])
            .expect("wasm lowers StartThread");
        for (label, param_types, parameters) in [
            ("lamella_thread_yield", alloc::vec::Vec::new(), alloc::vec::Vec::new()),
            ("lamella_thread_join", vec![i32t], vec![SigType::I4]),
            ("lamella_thread_sleep", vec![i32t], vec![SigType::I4]),
        ] {
            let f = runtime_seam_body(&param_types, &parameters, false, false, label);
            lamella_ir::verify(&f).unwrap_or_else(|e| panic!("{label} verify: {e:?}"));
            #[cfg(feature = "arm32")]
            crate::arm32::lower_object(&[f.clone()], &[label], &[])
                .unwrap_or_else(|e| panic!("{label} arm lower: {e:?}"));
            #[cfg(feature = "riscv32")]
            crate::riscv32::lower_object(&[f.clone()], &[label], &[], &[])
                .unwrap_or_else(|e| panic!("{label} riscv lower: {e:?}"));
            #[cfg(feature = "wasm")]
            crate::wasm::lower(&f).unwrap_or_else(|e| panic!("{label} wasm lower: {e:?}"));
        }
    }

    #[cfg(feature = "arm32")]
    #[test]
    fn synthesized_console_writers_verify_and_lower() {
        let bodies = [
            console_body(
                Some(MirType::ObjectRef),
                true,
                Some("lamella_console_write"),
                true,
            ),
            console_body(
                Some(MirType::I32),
                false,
                Some("lamella_console_write_i32"),
                true,
            ),
            console_body(
                Some(MirType::I64),
                false,
                Some("lamella_console_write_i64"),
                true,
            ),
            console_body(
                Some(MirType::I32),
                false,
                Some("lamella_console_write_char"),
                false,
            ),
            console_body(None, false, None, true),
        ];
        for f in bodies {
            lamella_ir::verify(&f).expect("a synthesized console body verifies");
            crate::arm32::lower_object(&[f], &["c"], &[])
                .expect("a synthesized console body lowers");
        }
    }

    #[cfg(feature = "arm32")]
    #[test]
    fn synthesized_substring_and_char_tostring_verify_and_lower() {
        let bodies = [
            synthesize_runtime_reader("System", "String", Some("Substring"), 1).unwrap(),
            synthesize_runtime_reader("System", "String", Some("Substring"), 2).unwrap(),
            char_to_string_body(),
        ];
        for f in bodies {
            lamella_ir::verify(&f).expect("a synthesized string body verifies");
            crate::arm32::lower_object(&[f], &["s"], &[]).expect("it lowers on the object path");
        }
    }

    /// The host guard that `debug_assert!` could not be: an IMAGE reaching a string-allocating seam
    /// without a nameable `System.String` is refused at build time, naming the seam.
    ///
    /// **THIS IS THE ONLY PLACE THE GUARD CAN FIRE, WHICH IS WHY IT IS TESTED HERE.** The archive is
    /// built `--release` with `panic = "abort"` and a `loop {}` panic handler, so an assertion inside
    /// it either compiles out or turns a lockup at a garbage PC into a lockup at a known one -- the
    /// same silent expiry as the comment it replaces. On the host there is an exit code.
    ///
    /// The two arms are the whole point: WITH a descriptor table this is an image and the build is
    /// refused; WITHOUT one it is a bare object lowering that has no type world, nothing can dispatch
    /// on a string it makes, and the guard must stay out of the way -- which is exactly what the two
    /// seam-lowering tests beside this one do.
    #[cfg(feature = "arm32")]
    #[test]
    fn an_image_reaching_a_string_seam_without_a_string_descriptor_is_refused() {
        let body = char_to_string_body();
        lamella_ir::verify(&body).expect("the synthesized body verifies");

        crate::arm32::lower_object(&[body.clone()], &["c"], &[])
            .expect("a bare object lowering has no type world and is not guarded");

        let described = [crate::resolver::TypeMeta {
            handle: lamella_ir::TypeHandle(0x0200_0001),
            type_tag: 0,
            vtable: alloc::vec::Vec::new(),
            itable: alloc::vec::Vec::new(),
            base: None,
            words: None,
            exported: false,
            full_name: None,
        }];
        let refused = crate::arm32::lower_object_vtables(&[body], &["c"], &[], &described);
        match refused {
            Err(crate::arm32::LowerError::StringSeamWithoutDescriptor { seam }) => {
                assert_eq!(
                    seam, "lamella_char_to_string",
                    "the refusal names the seam that forced the descriptor, not a condition"
                );
            }
            other => panic!(
                "an image reaching a string seam with no `System.String` must be REFUSED on the \
                 host -- the device cannot report it: {other:?}"
            ),
        }
    }

    #[cfg(feature = "arm32")]
    #[test]
    fn synthesized_double_to_string_verifies_and_lowers() {
        let f = double_to_string_body();
        lamella_ir::verify(&f).expect("Double.ToString body verifies");
        let obj =
            lamella_elf::read_object(&crate::arm32::lower_object(&[f], &["dts"], &[]).unwrap())
                .unwrap();
        assert!(
            obj.symbols
                .iter()
                .any(|s| s.name == "lamella_double_to_string" && !s.defined),
            "the formatter is an undefined extern the link resolves against runtime-support"
        );
    }

    #[test]
    fn rejects_an_unknown_target() {
        assert!(matches!(
            build(b"any bytes", "no-such-chip"),
            Err(BuildError::UnsupportedTarget)
        ));
    }

    #[test]
    fn reports_malformed_cil_for_a_chip_target() {
        assert!(matches!(
            build(b"not a managed assembly", "microbit"),
            Err(BuildError::Parse)
        ));
    }

    /// A one-block Python function that returns its argument: no allocation, no `PyIntrinsic`, no
    /// float -- the shape the flat path is FOR, and the shape that has been on nRF51 silicon.
    fn py_identity() -> Function {
        Function {
            params: vec![MirType::PyValue],
            ret: Some(MirType::PyValue),
            value_types: vec![MirType::PyValue],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        }
    }

    /// EVERY chip the guard admits must reach an arm that produces an image. The guard and the
    /// boot-image match are two lists that could drift apart silently -- a chip accepted by the
    /// guard and unhandled by the match would fall to `UnsupportedTarget` AFTER lowering, reporting
    /// "unsupported" about a target that is supported. Walking the constant is what makes the two
    /// one list rather than two that happen to agree today.
    #[test]
    fn build_py_reaches_every_cortex_m_target_the_guard_admits() {
        for target in CORTEX_M_TARGETS {
            let image = build_py(&[py_identity()], target)
                .unwrap_or_else(|e| panic!("{target} must produce an image, got {e:?}"));
            assert!(
                image.len() > 8,
                "{target}: an image is more than a bare vector table"
            );
        }
    }

    /// The Python front end and the CIL front end get the SAME boot image, because there is only one
    /// that builds it. Checked on the Nordic shape, whose stack top is spelled out from the parts
    /// rather than imported from the implementation ([`CORTEX_M_TARGETS`]'s two Nordic entries are
    /// the nRF51's 16 KiB and the nRF52833's 128 KiB at `0x2000_0000`), so this is an answer key
    /// rather than a mirror.
    #[test]
    fn build_py_lays_the_same_nordic_vector_table_the_cil_path_does() {
        for (target, sp) in [("microbit", 0x2000_4000u32), ("nrf52833", 0x2002_0000)] {
            let image = build_py(&[py_identity()], target).expect("builds");
            assert_eq!(
                &image[0..4],
                &sp.to_le_bytes(),
                "{target}: word 0 is the initial stack pointer"
            );
            assert_eq!(
                &image[4..8],
                &0x0000_0009u32.to_le_bytes(),
                "{target}: word 1 is the reset vector -> offset 8, Thumb bit set"
            );
        }
    }

    /// `build_py` REACHES the Python backend rather than falling through: a `PyIntrinsic` with no
    /// `PySupport` address must report `CallUnsupported` FROM THE LOWERING, not `UnsupportedTarget`.
    /// An unwired entry point never lowers at all, so only a real arm can produce this error --
    /// the same separation `build_routes_ch32v003_to_the_rv32ec_backend` makes with `Parse`.
    ///
    /// It also pins the flat path's limit as a REFUSAL rather than a silent miscompile: the
    /// addresses `PySupport` carries cannot be resolved without a linker, and the backend says so.
    #[test]
    fn build_py_refuses_a_dynamic_op_the_flat_path_cannot_resolve() {
        let dynamic = Function {
            params: vec![MirType::PyValue],
            ret: Some(MirType::PyValue),
            value_types: vec![MirType::PyValue, MirType::PyValue],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![(
                    ValueId(1),
                    Inst::PyIntrinsic {
                        op: lamella_ir::PyOp::Getattr { name: 5 },
                        args: vec![ValueId(0)],
                        cache: 0,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        assert!(matches!(
            build_py(&[dynamic], "microbit"),
            Err(BuildError::LowerArm(arm32::LowerError::CallUnsupported))
        ));
    }

    /// The PUBLIC refusal. `"ch32v003"` is a real target of this crate's OTHER backend, so it is the
    /// case that would actually be tried: a RISC-V chip must not reach an ARM lowering.
    ///
    /// **THIS ASSERTION ALONE CANNOT SAY WHICH MECHANISM REFUSED.** Two stand in the way -- the
    /// up-front guard and [`cortex_m_boot_image`]'s own arm -- and both answer `UnsupportedTarget`,
    /// so deleting the guard leaves this test green. The next one is the control that separates them.
    #[test]
    fn build_py_refuses_a_chip_outside_the_supported_list() {
        assert!(matches!(
            build_py(&[py_identity()], "ch32v003"),
            Err(BuildError::UnsupportedTarget)
        ));
    }

    /// The guard refuses BEFORE lowering, which is the only thing it contributes that the boot-image
    /// arm does not -- and it is invisible to a return value unless the lowering would ALSO fail.
    /// So: an unsupported chip AND a function the lowering cannot compile. Guarded, the answer is
    /// `UnsupportedTarget`; unguarded, lowering runs first and answers `LowerArm(CallUnsupported)`,
    /// reporting a dynamic-op problem about a build whose real problem is the chip.
    ///
    /// The assertion above cannot do this job: it scores both candidates identically, so it stays
    /// green with the guard deleted. A control has to tell the two apart to be evidence about either.
    #[test]
    fn build_py_checks_the_chip_before_it_spends_the_lowering() {
        let dynamic = Function {
            params: vec![MirType::PyValue],
            ret: Some(MirType::PyValue),
            value_types: vec![MirType::PyValue, MirType::PyValue],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![(
                    ValueId(1),
                    Inst::PyIntrinsic {
                        op: lamella_ir::PyOp::Getattr { name: 5 },
                        args: vec![ValueId(0)],
                        cache: 0,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        assert!(
            matches!(
                build_py(&[dynamic], "ch32v003"),
                Err(BuildError::UnsupportedTarget)
            ),
            "the chip is reported, not the dynamic op the lowering would have tripped on"
        );
    }

    /// [`cortex_m_boot_image`]'s own refusal, which the public entry points can never reach because
    /// they filter first. It is tested directly rather than left to inspection: the arm is what keeps
    /// the function TOTAL, and without it an unlisted chip would fall through to the Nordic arm and
    /// silently receive a micro:bit vector table. A single guarded caller hides that; a second caller
    /// does not, and there is one.
    #[test]
    fn the_boot_image_builder_refuses_a_chip_rather_than_defaulting_to_nordic() {
        assert!(matches!(
            cortex_m_boot_image("ch32v003", &[0x70, 0x47]),
            Err(BuildError::UnsupportedTarget)
        ));
    }

    /// `build()` REACHES the RV32EC backend for `"ch32v003"`. Malformed CIL landing on `Parse`
    /// rather than `UnsupportedTarget` is what separates "the target string is wired" from "the
    /// string fell through to the catch-all": an unwired target never gets as far as reading the
    /// assembly, so `Parse` can only be reported by an arm that exists.
    #[test]
    #[cfg(feature = "riscv32")]
    fn build_routes_ch32v003_to_the_rv32ec_backend() {
        assert!(matches!(
            build(b"not a managed assembly", "ch32v003"),
            Err(BuildError::Parse)
        ));
    }

    /// The reset stub is PREPENDED and sets `sp` before anything opens a frame -- the CH32V003
    /// resets into flash `0x0000_0000` with no hardware SP init, so code placed first would run
    /// managed code on a garbage stack.
    ///
    /// The stack top is spelled out from the CH32V003RM here (2 KB SRAM at `0x2000_0000`) instead
    /// of importing [`CH32V003_SRAM_TOP`]: a test that imports the value it is checking agrees with
    /// the implementation by construction and cannot report a wrong constant.
    #[test]
    #[cfg(feature = "riscv32")]
    fn the_ch32v003_boot_stub_precedes_the_code_and_sets_sp() {
        use lamella_asm_riscv32::{Encoder, Reg};
        let code = [0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        let image = ch32v003_boot_image(&code);

        assert!(image.len() > code.len(), "a stub is prepended to the code");
        assert_eq!(
            &image[image.len() - code.len()..],
            &code,
            "the lowered code follows the stub verbatim, so function 0 is what the stub calls"
        );

        let mut expect = Encoder::new();
        expect.li(Reg::SP, 0x2000_0800u32 as i32);
        let sp_init = expect.finish().expect("li sp assembles").bytes;
        assert_eq!(
            &image[..sp_init.len()],
            &sp_init[..],
            "the image opens by loading the top of the CH32V003's 2 KB SRAM into sp"
        );
    }

    #[test]
    fn startup_runs_cctors_before_main() {
        let f = startup(None, &[5, 7], 3);
        let callees = |g: &Function| -> Vec<u32> {
            g.blocks[0]
                .insts
                .iter()
                .filter_map(|(_, inst)| match inst {
                    Inst::Call { callee, .. } => Some(*callee),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(callees(&f), vec![5, 7, 3], "each .cctor, then Main");
        assert!(matches!(
            f.blocks[0].terminator,
            Some(Terminator::Return(Some(_)))
        ));
        assert_eq!(startup(None, &[], 3).blocks[0].insts.len(), 1);
        assert_eq!(
            callees(&startup(Some(9), &[5, 7], 3)),
            vec![9, 5, 7, 3],
            "init hook, then .cctors, then Main"
        );
        assert!(lamella_ir::verify(&f).is_ok());
    }

    #[test]
    fn startup_chains_reference_cctors_before_the_programs() {
        let refs = vec![
            alloc::string::String::from("Ldeadbeef.f12"),
            alloc::string::String::from("Ldeadbeef.f31"),
        ];
        let f = startup_with_references(Some(9), &refs, &[5, 7], 3);
        let order: Vec<alloc::string::String> = f.blocks[0]
            .insts
            .iter()
            .map(|(_, inst)| match inst {
                Inst::Call { callee, .. } => alloc::format!("f{callee}"),
                Inst::PInvoke { import, .. } => alloc::string::String::from(&**import),
                other => panic!("unexpected startup inst {other:?}"),
            })
            .collect();
        assert_eq!(
            order,
            vec!["f9", "Ldeadbeef.f12", "Ldeadbeef.f31", "f5", "f7", "f3"],
            "init hook, reference .cctors, program .cctors, then Main"
        );
        assert!(matches!(
            f.blocks[0].terminator,
            Some(Terminator::Return(Some(v))) if v.0 as usize == f.blocks[0].insts.len() - 1
        ));
        assert!(lamella_ir::verify(&f).is_ok());
    }

    /// THE DEFECT, STATED: a second body for one MethodDef row REPLACES the first rather than
    /// colliding, because `funcs` is indexed by rid. The assertion that matters is the middle one
    /// -- the surviving body is the SECOND, which is exactly the silent-overwrite behavior; the
    /// change is that the collision is now recorded instead of leaving no trace at all.
    #[test]
    fn a_second_body_for_one_rid_overwrites_and_is_recorded() {
        let body = |value: i64| Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::ConstInt {
                        ty: MirType::I32,
                        value,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let mut slots = BodySlots::new(4);
        slots.write(1, body(21));
        slots.write(2, body(7));
        assert!(
            slots.duplicates.is_empty(),
            "distinct rids are not a collision"
        );

        slots.write(1, body(99));
        assert_eq!(slots.duplicates, vec![1], "the second write to rid 1 is recorded");
        assert!(
            matches!(
                slots.funcs[1].blocks[0].insts[0].1,
                Inst::ConstInt { value: 99, .. }
            ),
            "the second body still wins; only its silence is gone"
        );
        assert!(
            matches!(
                slots.funcs[2].blocks[0].insts[0].1,
                Inst::ConstInt { value: 7, .. }
            ),
            "an untouched slot is unaffected"
        );
    }

    /// The recorded collision becomes a refusal, and it names the row. A build that tolerated it
    /// would ship an image built around whichever body won, with no diagnostic anywhere.
    #[test]
    fn a_recorded_duplicate_refuses_the_build() {
        assert!(refuse_duplicate_bodies(&[]).is_ok());
        assert!(matches!(
            refuse_duplicate_bodies(&[7, 9]),
            Err(BuildError::DuplicateMethodBody { rid: 7, total: 2 })
        ));
    }

}
