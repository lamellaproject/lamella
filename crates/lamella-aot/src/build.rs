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
    BasicBlock, BinOp, BlockId, CmpOp, ConvKind, Function, Inst, MirType, Terminator, TypeHandle,
    ValueId,
};
use lamella_metadata::tables::table;
use lamella_metadata::{Assembly, SigType, TargetLayout};
use lamella_token::Token;

#[cfg(feature = "arm32")]
use crate::arm32;
use crate::cil;
use crate::resolver::MetadataResolver;
#[cfg(feature = "riscv32")]
use crate::riscv32;
#[cfg(feature = "wasm")]
use crate::wasm;

/// Why an AOT build failed.
#[derive(Debug)]
pub enum BuildError {
    /// The CIL assembly's metadata could not be read.
    Parse,
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
}

/// Compiles a CIL assembly to native bytes for `target`. `target = "wasm"` emits a WebAssembly module
/// with the embedding ABI (per-method exports + `alloc`/`dealloc` + memory) -- the C# -> `.wasm`
/// widget. A chip `target` ("microbit" for the nRF51 Cortex-M0, "rp2350" for the Pico 2 / Pico 2 W
/// Cortex-M33) emits a flashable bare-metal image -- the flat, linker-free fast path.
pub fn build(cil: &[u8], target: &str) -> Result<Vec<u8>, BuildError> {
    match target {
        #[cfg(feature = "wasm")]
        "wasm" => build_wasm(cil),
        #[cfg(feature = "arm32")]
        "microbit" | "rp2350" => build_cortex_m(cil, target),
        _ => Err(BuildError::UnsupportedTarget),
    }
}

/// Compiles a CIL assembly to a WebAssembly module: every method lowered through the same
/// `resolver` + `cil` front end the ARM/RISC-V backends use, then `wasm::lower_module_with_exports`.
/// Exports every public static method by name (the widget surface) plus `main` for the entry, if any.
#[cfg(feature = "wasm")]
pub fn build_wasm(cil: &[u8]) -> Result<Vec<u8>, BuildError> {
    let assembly = Assembly::read(cil).map_err(|_| BuildError::Parse)?;
    let entry = find_main(&assembly);
    let funcs = lower_assembly(&assembly, entry, &[])?;
    let exports = method_exports(&assembly, entry.is_some());
    let export_refs: Vec<(&str, u32)> = exports.iter().map(|(n, i)| (n.as_str(), *i)).collect();
    let descriptors = MetadataResolver::new(&assembly).type_descriptors();
    wasm::lower_module_with_exports(&funcs, &export_refs, &descriptors)
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
    if !matches!(target, "microbit" | "rp2350") {
        return Err(BuildError::UnsupportedTarget);
    }
    let assembly = Assembly::read(cil).map_err(|_| BuildError::Parse)?;
    let entry = find_main(&assembly);
    let funcs = lower_assembly(&assembly, entry, &[])?;
    let code = arm32::lower_module(&funcs).map_err(BuildError::LowerArm)?;
    Ok(match target {
        "rp2350" => rp2350_boot_image(0, &code),
        _ => {
            let initial_sp: u32 = 0x2000_4000;
            let mut image = Vec::with_capacity(8 + code.len());
            image.extend_from_slice(&initial_sp.to_le_bytes());
            image.extend_from_slice(&0x0000_0009u32.to_le_bytes());
            image.extend_from_slice(&code);
            image
        }
    })
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
    let assembly = Assembly::read(cil).map_err(|_| BuildError::Parse)?;
    let entry = find_main(&assembly);
    let (funcs, maps, fails) = lower_assembly_debug(&assembly, entry, &[]);
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
/// and the `pico2-aot-image` object-path flasher agree on the boot layout + verdict mailbox.
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
    /// The M33 vector-table offset register: the stub points it at the flash base so a fault vectors
    /// through THIS table (the bootrom leaves VTOR on its own).
    const SCB_VTOR: u32 = 0xE000_ED08;
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
/// part -- a far branch/`adr` relaxes to `B.W`/`ADR.W` rather than deferring; pass `false` for a
/// v6-M target (byte-identical, the widen fires only for an out-of-reach ref).
#[cfg(feature = "arm32")]
pub fn build_object_with_libraries_report(
    cil: &[u8],
    corlib: &[u8],
    libraries: &[&[u8]],
    wide: bool,
) -> Result<(Vec<u8>, LibraryBuildReport), BuildError> {
    build_object_core(cil, Some(corlib), libraries, true, wide)
}

#[cfg(feature = "arm32")]
fn build_object_inner(
    cil: &[u8],
    corlib: Option<&[u8]>,
    libraries: &[&[u8]],
) -> Result<Vec<u8>, BuildError> {
    build_object_core(cil, corlib, libraries, false, false).map(|(bytes, _)| bytes)
}

#[cfg(feature = "arm32")]
fn build_object_core(
    cil: &[u8],
    corlib: Option<&[u8]>,
    libraries: &[&[u8]],
    defer: bool,
    wide: bool,
) -> Result<(Vec<u8>, LibraryBuildReport), BuildError> {
    let assembly = Assembly::read(cil).map_err(|_| BuildError::Parse)?;
    let reference = match corlib {
        Some(bytes) => Some(Assembly::read(bytes).map_err(|_| BuildError::Parse)?),
        None => None,
    };
    let entry = find_main(&assembly);
    let library_assemblies: Vec<Assembly> = libraries
        .iter()
        .map(|lib| Assembly::read(lib).map_err(|_| BuildError::Parse))
        .collect::<Result<_, _>>()?;
    let references: Vec<&Assembly> = reference.iter().chain(library_assemblies.iter()).collect();
    let qualifiers = arm32::DescQualifiers {
        own: None,
        references: corlib
            .iter()
            .copied()
            .chain(libraries.iter().copied())
            .map(|bytes| alloc::format!("{:08x}", lamella_metadata::fnv1a32(0x811c_9dc5, bytes)))
            .collect(),
    };
    let (mut funcs, _maps, cil_fails) = lower_assembly_debug(&assembly, entry, &references);
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
            reference_cctors
                .extend(find_cctors(assembly).into_iter().map(|rid| alloc::format!("{prefix}f{rid}")));
        };
        chain(bytes, reference);
        for (lib_bytes, lib_assembly) in libraries.iter().zip(&library_assemblies) {
            chain(lib_bytes, lib_assembly);
        }
        funcs[0] = startup_with_references(
            find_native_export(&assembly, "lamella_time_init"),
            &reference_cctors,
            &find_cctors(&assembly),
            entry_rid,
        );
    }
    let funcs = funcs;
    let names = object_symbol_names(&assembly, funcs.len());
    let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let resolver = MetadataResolver::new(&assembly).with_references(&references);
    let mut descriptors = resolver.type_descriptors();
    let mut added: Vec<u32> = Vec::new();
    for func in &funcs {
        for block in &func.blocks {
            for (_, inst) in &block.insts {
                if let Inst::Alloc { handle, .. } = inst {
                    let known = descriptors.iter().any(|d| d.handle == *handle)
                        || added.contains(&handle.0);
                    if !known {
                        if let Some(meta) = resolver.reference_type_meta(*handle) {
                            added.push(handle.0);
                            descriptors.push(meta);
                        }
                    }
                }
            }
        }
    }
    let statics = assembly_statics(cil, &assembly, true);
    if !defer {
        let bytes = arm32::lower_object_vtables_statics(
            &funcs,
            &name_refs,
            &[],
            &descriptors,
            &statics,
            &qualifiers,
        )
        .map_err(BuildError::LowerArm)?;
        return Ok((
            bytes,
            LibraryBuildReport {
                cil_fails: Vec::new(),
                emit_stubs: Vec::new(),
            },
        ));
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
    let display_names = method_display_names(&assembly, funcs.len());
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
    };
    Ok((bytes, report))
}

/// One assembly's [`arm32::AssemblyStatics`] (#15): its region-symbol identity (the fnv1a32 of the
/// CIL bytes -- the SAME hash that prefixes a library object's internal symbols, so the two views
/// of one assembly agree), its dense region size, and its GLOBAL-roots record rows -- every
/// ref-typed static field's dense slot (the resolver's [`static_field_slots`] layout; the record
/// and the `ldsfld`/`stsfld` lowering share that one source, or the collector would walk the wrong
/// words). `include_eh_row` adds word 0 for the PROGRAM assembly only: the linker aliases the
/// shared `__lamella_eh_tag` word to the ENTRY object's reserved word 0, and that word holds a
/// type TAG today (an integer; the no-GC exception model), so it is emitted `ManagedPtr` -- the
/// collector range-checks a maybe-heap word and skips a non-heap value, and when an
/// object-carrying exception model lands the same entry covers the in-flight exception reference.
/// A library's word 0 is dead (never aliased, never written), so its record claims no root there.
#[cfg(feature = "arm32")]
fn assembly_statics(
    cil: &[u8],
    assembly: &Assembly,
    include_eh_row: bool,
) -> arm32::AssemblyStatics {
    let slots = crate::resolver::static_field_slots(assembly);
    let mut roots = Vec::new();
    if include_eh_row {
        roots.push(arm32::STACKMAP_KIND_MANAGED_PTR << 14);
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
    for (row, slot) in &slots {
        if *slot < 0x4000 && ref_rows.contains(row) {
            roots.push((*slot as u16) | (arm32::STACKMAP_KIND_OBJECT_REF << 14));
        }
    }
    arm32::AssemblyStatics {
        suffix: alloc::format!("{:08x}", lamella_metadata::fnv1a32(0x811c_9dc5, cil)),
        region_bytes: (slots.len() as u32 + 1) * 4,
        roots,
    }
}

/// Compiles a self-contained CIL assembly to ONE RV32IM relocatable ELF object through the RELOCATING
/// path ([`riscv32::lower_object`]): every reachable method becomes an `f<rid>` `STT_FUNC` symbol
/// (`f0` is the entry trampoline -> `Main`), and each cross-method call becomes an `R_RISCV_CALL_PLT`
/// relocation OUR linker resolves. This is the RISC-V twin of the ARM [`build_object`] -- it proves the
/// object path handles real compiler output, and it is the substrate the linked-path bricks (native
/// calls, cross-assembly calls, the descriptor object lane) build on.
///
/// It is REACHABILITY-LIMITED: only methods reachable from `Main` (direct `Call` edges, the `.cctor`s
/// the startup chains, and every this-assembly vtable/itable dispatch target) are lowered; every other
/// rid -- notably the implicit `.ctor`, which calls `object::.ctor()` in corlib -- stays a stub. That
/// lets a SELF-CONTAINED program (no `/reference`) build with no external call to resolve, exactly as
/// the flat `lower_module_gc` driver does. Once the cross-assembly `Call` + gc-sections path lands this
/// converges to the lower-all shape of [`build_object`] (the implicit `.ctor` becomes an extern the
/// linker drops when unreached). A reachable method that fails to lower is reported, never silently
/// stubbed. Emitting the object stays linker-free (the driver/examples own the link + boot).
#[cfg(feature = "riscv32")]
pub fn build_object_riscv(cil: &[u8]) -> Result<Vec<u8>, BuildError> {
    build_object_riscv_inner(cil, None, riscv32::RiscvProfile::Rv32im)
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
    build_object_riscv_inner(cil, None, profile)
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
    build_object_riscv_inner(cil, Some(reference), riscv32::RiscvProfile::Rv32im)
}

#[cfg(feature = "riscv32")]
fn build_object_riscv_inner(
    cil: &[u8],
    reference_cil: Option<&[u8]>,
    profile: riscv32::RiscvProfile,
) -> Result<Vec<u8>, BuildError> {
    let assembly = Assembly::read(cil).map_err(|_| BuildError::Parse)?;
    let reference = match reference_cil {
        Some(bytes) => Some(Assembly::read(bytes).map_err(|_| BuildError::Parse)?),
        None => None,
    };
    let entry = find_main(&assembly).ok_or(BuildError::NoEntryPoint)?;
    let mut resolver = MetadataResolver::new(&assembly);
    if let Some(reference) = reference.as_ref() {
        resolver = resolver.with_reference(reference);
    }
    let mut descriptors = resolver.type_descriptors();
    let funcs = lower_reachable(&assembly, entry, &descriptors, reference.as_ref())?;
    let mut added: Vec<u32> = Vec::new();
    for func in &funcs {
        for (_, inst) in func.blocks.iter().flat_map(|b| &b.insts) {
            if let Inst::Alloc { handle, .. } = inst {
                let known =
                    descriptors.iter().any(|d| d.handle == *handle) || added.contains(&handle.0);
                if !known {
                    if let Some(meta) = resolver.reference_type_meta(*handle) {
                        added.push(handle.0);
                        descriptors.push(meta);
                    }
                }
            }
        }
    }
    let names: Vec<alloc::string::String> =
        (0..funcs.len()).map(|i| alloc::format!("f{i}")).collect();
    let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    riscv32::lower_object_profile(&funcs, &name_refs, &[], &descriptors, profile)
        .map_err(BuildError::LowerRiscv)
}

/// Lowers the methods of a self-contained assembly REACHABLE from `entry`, rid-indexed, into a dense
/// module for [`riscv32::lower_object`]. Index 0 is the entry [`startup`] (board-init hook, then each
/// `.cctor`, then `Main`); each reachable method sits at its `MethodDef` rid; every unreached rid is a
/// [`stub`]. Reachability is a BFS over direct `Call` edges seeded with `Main`, the `.cctor`s, the
/// board-init hook, and every this-assembly vtable/itable dispatch target (an indirect call has no
/// `Call` edge). Skipping the unreached rids keeps the implicit `.ctor`'s `object::.ctor()` corlib call
/// out of a self-contained build -- the flat driver relies on the same property.
#[cfg(feature = "riscv32")]
fn lower_reachable<'a>(
    assembly: &'a Assembly<'a>,
    entry: u32,
    descriptors: &[crate::resolver::TypeMeta],
    reference: Option<&'a Assembly<'a>>,
) -> Result<Vec<Function>, BuildError> {
    let mut max_rid = entry;
    for type_def in assembly.type_defs() {
        for method in type_def.methods() {
            max_rid = max_rid.max(method.rid());
        }
    }
    let mut funcs: Vec<Function> = (0..=max_rid).map(|_| stub()).collect();
    let mut lowered = vec![false; funcs.len()];
    let cctors = find_cctors(assembly);
    let init = find_native_export(assembly, "lamella_time_init");
    let resolver = match reference {
        Some(reference) => MetadataResolver::new(assembly).with_reference(reference),
        None => MetadataResolver::new(assembly),
    };
    let mut worklist: Vec<u32> = core::iter::once(entry)
        .chain(cctors.iter().copied())
        .chain(init)
        .collect();
    for meta in descriptors {
        for slot in &meta.vtable {
            if let crate::resolver::VtableEntry::Func(index) = slot {
                worklist.push(*index);
            }
        }
        for (_, index) in &meta.itable {
            worklist.push(*index);
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
        let Some(func) = lower_one_reachable(assembly, &resolver, rid)? else {
            continue;
        };
        for block in &func.blocks {
            for (_, inst) in &block.insts {
                if let Inst::Call { callee, .. } = inst {
                    if lowered.get(*callee as usize) == Some(&false) {
                        worklist.push(*callee);
                    }
                }
            }
        }
        funcs[rid as usize] = func;
    }
    funcs[0] = startup(init, &cctors, entry);
    Ok(funcs)
}

/// Lowers the method at `MethodDef` rid `rid` to MIR (its plain managed body -- the same path
/// [`lower_assembly_debug`] takes for an ordinary method), or `Ok(None)` if there is no such method or
/// it has no body (abstract/extern). A body that fails to lower is `Err(BuildError::LowerCil)` -- FAIL
/// LOUD, never a silent stub (a stubbed reachable method would miscompile the program).
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
            let Some(body) = method.body() else {
                return Ok(None);
            };
            let signature = method.signature();
            let mut arg_types = Vec::new();
            if let Some(sig) = &signature {
                if sig.has_this {
                    arg_types.push(MirType::ObjectRef);
                }
                for parameter in &sig.parameters {
                    arg_types.push(mir_type(parameter, assembly, resolver.references()));
                }
            }
            let local_types: Vec<MirType> = method
                .local_variables()
                .iter()
                .map(|sig| mir_type(sig, assembly, resolver.references()))
                .collect();
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
    let assembly = Assembly::read(cil).map_err(|_| BuildError::Parse)?;
    let (funcs, _maps, _fails) = lower_assembly_debug(&assembly, None, &[]);
    let prefix = alloc::format!("L{:08x}.", lamella_metadata::fnv1a32(0x811c_9dc5, cil));
    let names = library_symbol_names(&assembly, funcs.len(), &prefix);
    let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let descriptors = MetadataResolver::new(&assembly).type_descriptors();
    riscv32::lower_object_library(&funcs, &name_refs, &[], &descriptors).map_err(BuildError::LowerRiscv)
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
/// -- a NON-corlib library (`System.Net.NetworkInformation`, a #15 fixture lib) allocates and
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
/// (`B.W`/`ADR.W`) instead of hard-erroring at the ARMv6-M reach, so a big-branch method
/// (a Pico2 BSP's `SpiDriver::Configure`) encodes instead of deferring. Pass `false` for a
/// v6-M target -- it keeps the object byte-identical (the widen fires only for an out-of-reach ref).
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

/// What [`build_library_object_report`] observed while building: the two DISTINCT silent-demotion
/// layers a library method can fall through, each previously invisible (finding 4b).
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
}

/// As [`build_library_object`], but also returning the [`LibraryBuildReport`] -- the CIL->MIR fail
/// list `lower_assembly_debug` computes (previously discarded) and the object-emit stub set
/// (previously internal to the emit fixpoint). The object bytes are IDENTICAL to
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
    let assembly = Assembly::read(cil).map_err(|_| BuildError::Parse)?;
    let reference_assemblies: Vec<Assembly> = references
        .iter()
        .map(|bytes| Assembly::read(bytes).map_err(|_| BuildError::Parse))
        .collect::<Result<_, _>>()?;
    let reference_list: Vec<&Assembly> = reference_assemblies.iter().collect();
    let (funcs, _maps, fails) = lower_assembly_debug(&assembly, None, &reference_list);
    let prefix = alloc::format!("L{:08x}.", lamella_metadata::fnv1a32(0x811c_9dc5, cil));
    let names = library_symbol_names(&assembly, funcs.len(), &prefix);
    let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let qualifiers = arm32::DescQualifiers {
        own: Some(alloc::format!("{:08x}", lamella_metadata::fnv1a32(0x811c_9dc5, cil))),
        references: references
            .iter()
            .map(|bytes| alloc::format!("{:08x}", lamella_metadata::fnv1a32(0x811c_9dc5, bytes)))
            .collect(),
    };
    let resolver = MetadataResolver::new(&assembly).with_references(&reference_list);
    let descriptors = resolver.type_descriptors();
    let statics = assembly_statics(cil, &assembly, false);
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
    };
    Ok((bytes, report))
}

/// `rid -> "Namespace.Type::Method"` for every METHOD_DEF row (None for a rid no method occupies
/// -- the rid-indexed layout keeps gaps as placeholder functions).
#[cfg(feature = "arm32")]
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
#[cfg(feature = "arm32")]
fn library_symbol_names(
    assembly: &Assembly,
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
            let runtime_provided = has_runtime_provided_attribute(assembly, token);
            let is_synth_reader = runtime_provided
                && synthesize_runtime_reader(
                    type_name.namespace,
                    type_name.name,
                    method.name(),
                    method.signature().map(|s| s.parameters.len()).unwrap_or(0),
                )
                .is_some();
            let is_plain_instance = !method.is_static()
                && !method.is_virtual()
                && !runtime_provided
                && method.body().is_some();
            if (method.is_static() || method.is_virtual() || is_synth_reader || is_plain_instance)
                && matches!(method.flags() & 0x7, 0x4..=0x6)
            {
                if let Some(method_name) = method.name() {
                    let params = method.signature().map(|s| s.parameters).unwrap_or_default();
                    names[rid] = crate::resolver::extern_method_symbol(
                        type_name.namespace,
                        type_name.name,
                        method_name,
                        &params,
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

/// The MIR type the AOT lowers a metadata signature type as.
fn mir_type<'x>(sig: &SigType, assembly: &'x Assembly<'x>, references: &[&'x Assembly<'x>]) -> MirType {
    match sig {
        SigType::I8 | SigType::U8 => MirType::I64,
        SigType::R4 => MirType::F32,
        SigType::R8 => MirType::F64,
        SigType::String
        | SigType::Object
        | SigType::Class(_)
        | SigType::SzArray(_)
        | SigType::Array { .. } => MirType::ObjectRef,
        SigType::ValueType(token) => {
            if let Some(underlying) =
                crate::resolver::enum_underlying(assembly, *token, references, &TargetLayout::ilp32())
            {
                underlying
            } else {
                let size = assembly
                    .value_type_layout(*token, &TargetLayout::ilp32())
                    .map(|layout| layout.size)
                    .unwrap_or(0);
                MirType::ValueType {
                    handle: TypeHandle(token.0),
                    size,
                }
            }
        }
        _ => MirType::I32,
    }
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

/// The program startup at index 0 (exported as `main`): runs each type initializer (`.cctor`) for
/// its side effects, then `return entry()`. With no `.cctor`s this is just `return entry()` -- the
/// plain trampoline. Eager static init before `main` is spec-compliant for the `beforefieldinit`
/// types the C# compiler emits for field initializers; precise lazy (before-first-access) init is
/// unsupported.
fn startup(init: Option<u32>, cctors: &[u32], entry_rid: u32) -> Function {
    startup_with_references(init, &[], cctors, entry_rid)
}

/// As [`startup`], but also chaining REFERENCE-assembly `.cctor`s (as extern `PInvoke` calls to
/// the library object's internal `L<hash>.f<rid>` symbols) -- the #15 second half. ORDER:
/// board-init hook, then REFERENCE cctors, then the program's own, then the entry. Reference-FIRST
/// is the dependency direction: a program `.cctor` may call referenced-assembly surface that reads
/// referenced statics (`new AutoResetEvent(false)` reaches `WaitHandle.coordinator`), while a
/// reference `.cctor` cannot name program state at all -- corlib does not know the program. (The
/// #15 ruling's text sketched program-then-reference; this deliberately runs the defensible eager
/// order instead, flagged on the board for the lead's ack.) Each call before the entry is void
/// (its result is a dead placeholder).
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

/// Whether `method_token` carries `[Lamella.Runtime.RuntimeProvided]` -- the corlib's marker for a method
/// whose empty body is a placeholder for a runtime-provided intrinsic. Mirrors the interpreter's
/// `lamella_load::has_runtime_provided_attribute`: csc emits a REAL empty body (not an implflag), so the
/// AOT keys on the ATTRIBUTE, and for the readers it can it synthesizes a body ([`synthesize_runtime_reader`])
/// instead of lowering the placeholder.
fn has_runtime_provided_attribute(assembly: &Assembly, method_token: Token) -> bool {
    assembly.custom_attributes(method_token).any(|attribute| {
        assembly
            .resolve_method(attribute.constructor)
            .and_then(|ctor| ctor.declaring_type)
            .is_some_and(|name| {
                name.namespace == "Lamella.Runtime" && name.name == "RuntimeProvidedAttribute"
            })
    })
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

/// A synthesized MIR body for a `System.Console` output overload. It threads an optional argument
/// (`param`) -- reinterpreted from an ObjectRef to a raw pointer first when `ref_to_int` (the string
/// form) -- into a runtime-support value-writer (`writer`), then optionally a trailing newline. So
/// `Write(x)` = the writer, `WriteLine(x)` = the writer + newline, `WriteLine()` = just the newline.
/// All are void `[RuntimeProvided]` statics; the object path rewrites each `PInvoke` to a `CallNative`
/// the linker resolves against `tools/runtime-support`. The writer matches the interpreter / .NET
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
/// `CallNative` the linker resolves against `tools/runtime-support`. `this` is dead by the allocating call,
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

/// Lowers an assembly's methods to a `Vec<Function>` keyed by MethodDef row. Index 0 is a trampoline
/// to `entry` (if any) -- the `main` export -- or a stub. A method that does not lower stays a stub.
/// `references` are the referenced assemblies (corlib first) for cross-assembly vtable-slot
/// agreement, in resolution order; empty for this-assembly-relative numbering.
fn lower_assembly<'a>(
    assembly: &'a Assembly<'a>,
    entry: Option<u32>,
    references: &[&'a Assembly<'a>],
) -> Result<Vec<Function>, BuildError> {
    let (funcs, _maps, fails) = lower_assembly_debug(assembly, entry, references);
    if let Some((rid, error)) = fails.into_iter().next() {
        return Err(BuildError::LowerCil { rid, error });
    }
    Ok(funcs)
}

/// As [`lower_assembly`], but also returns each function's [`cil::CilSourceMap`] (rid-indexed, empty for
/// the trampoline and the stub gaps) -- so the SAME image build()'s chip path produces also carries debug
/// info, and a debugger's line tables match the flashed layout by construction.
fn lower_assembly_debug<'a>(
    assembly: &'a Assembly<'a>,
    entry: Option<u32>,
    references: &[&'a Assembly<'a>],
) -> (
    Vec<Function>,
    Vec<cil::CilSourceMap>,
    Vec<(u32, cil::CilError)>,
) {
    let mut methods = Vec::new();
    let mut max_rid = entry.unwrap_or(0);
    for type_def in assembly.type_defs() {
        let type_name = type_def.name();
        for method in type_def.methods() {
            let rid = method.rid();
            max_rid = max_rid.max(rid);
            methods.push((rid, method, type_name));
        }
    }
    let mut funcs: Vec<Function> = (0..=max_rid).map(|_| stub()).collect();
    let mut maps: Vec<cil::CilSourceMap> = (0..=max_rid)
        .map(|_| cil::CilSourceMap(Vec::new()))
        .collect();
    if let Some(entry_rid) = entry {
        funcs[0] = startup(
            find_native_export(assembly, "lamella_time_init"),
            &find_cctors(assembly),
            entry_rid,
        );
    }
    let resolver = MetadataResolver::new(assembly).with_references(references);
    let mut fails: Vec<(u32, cil::CilError)> = Vec::new();
    for (rid, method, type_name) in &methods {
        let Some(body) = method.body() else { continue };
        let signature = method.signature();
        if has_runtime_provided_attribute(assembly, Token::new(table::METHOD_DEF, method.rid())) {
            if let Some(type_name) = type_name {
                let params = signature.as_ref().map_or(0, |sig| sig.parameters.len());
                if let Some(func) = synthesize_runtime_reader(
                    type_name.namespace,
                    type_name.name,
                    method.name(),
                    params,
                ) {
                    funcs[*rid as usize] = func;
                    continue;
                }
                if (type_name.namespace, type_name.name) == ("System", "Console") {
                    let params = signature
                        .as_ref()
                        .map(|s| s.parameters.as_slice())
                        .unwrap_or(&[]);
                    let s = |sym| Some(sym);
                    let body = match (method.name(), params) {
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
                        (Some("WriteLine"), [SigType::Boolean]) => Some(console_body(
                            Some(MirType::I32),
                            false,
                            s("lamella_console_write_bool"),
                            true,
                        )),
                        (Some("WriteLine"), [SigType::I8]) => Some(console_body(
                            Some(MirType::I64),
                            false,
                            s("lamella_console_write_i64"),
                            true,
                        )),
                        (Some("WriteLine"), [SigType::U8]) => Some(console_body(
                            Some(MirType::I64),
                            false,
                            s("lamella_console_write_u64"),
                            true,
                        )),
                        _ => None,
                    };
                    if let Some(func) = body {
                        funcs[*rid as usize] = func;
                        continue;
                    }
                }
                if (type_name.namespace, type_name.name) == ("System", "Double")
                    && method.name() == Some("ToString")
                    && params == 0
                {
                    funcs[*rid as usize] = double_to_string_body();
                    continue;
                }
                if (type_name.namespace, type_name.name) == ("System", "Char")
                    && method.name() == Some("ToString")
                    && params == 0
                {
                    funcs[*rid as usize] = char_to_string_body();
                    continue;
                }
                if (type_name.namespace, type_name.name) == ("System", "Delegate") && params == 2 {
                    if method.name() == Some("Combine") {
                        funcs[*rid as usize] = delegate_combine_body();
                        continue;
                    }
                    if method.name() == Some("Remove") {
                        funcs[*rid as usize] = delegate_remove_body();
                        continue;
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
                        funcs[*rid as usize] = body;
                        continue;
                    }
                }
                if let Some(import) =
                    net_seam_import(type_name.namespace, type_name.name, method.name())
                {
                    if let Some(sig) = &signature {
                        let param_types: Vec<MirType> =
                            sig.parameters.iter().map(|p| mir_type(p, assembly, resolver.references())).collect();
                        funcs[*rid as usize] = runtime_seam_body(
                            &param_types,
                            &sig.parameters,
                            !matches!(sig.return_type, SigType::Void),
                            net_seam_folds_buffer(method.name()),
                            import,
                        );
                        continue;
                    }
                }
                if net_seam_deferred(type_name.namespace, type_name.name, method.name()) {
                    if let Some(sig) = &signature {
                        let param_types: Vec<MirType> =
                            sig.parameters.iter().map(|p| mir_type(p, assembly, resolver.references())).collect();
                        funcs[*rid as usize] = net_deferred_body(&param_types, -2);
                        continue;
                    }
                }
                if (type_name.namespace, type_name.name) == ("System.Threading", "Thread")
                    && method.name() == Some("StartThread")
                {
                    if let (Some(sig), Some(entry_rid)) = (
                        &signature,
                        find_method_rid(assembly, "System.Threading", "Thread", "ThreadEntry"),
                    ) {
                        let param_types: Vec<MirType> =
                            sig.parameters.iter().map(|p| mir_type(p, assembly, resolver.references())).collect();
                        funcs[*rid as usize] = thread_start_body(&param_types, entry_rid);
                        continue;
                    }
                }
                if let Some(import) =
                    thread_seam_import(type_name.namespace, type_name.name, method.name())
                {
                    if let Some(sig) = &signature {
                        let param_types: Vec<MirType> =
                            sig.parameters.iter().map(|p| mir_type(p, assembly, resolver.references())).collect();
                        funcs[*rid as usize] = runtime_seam_body(
                            &param_types,
                            &sig.parameters,
                            !matches!(sig.return_type, SigType::Void),
                            false,
                            import,
                        );
                        continue;
                    }
                }
                if let Some(import) =
                    monitor_seam_import(type_name.namespace, type_name.name, method.name())
                {
                    if let Some(sig) = &signature {
                        let param_types: Vec<MirType> =
                            sig.parameters.iter().map(|p| mir_type(p, assembly, resolver.references())).collect();
                        funcs[*rid as usize] = runtime_seam_body(
                            &param_types,
                            &sig.parameters,
                            !matches!(sig.return_type, SigType::Void),
                            false,
                            import,
                        );
                        continue;
                    }
                }
            }
        }
        let mut arg_types = Vec::new();
        if let Some(sig) = &signature {
            if sig.has_this {
                arg_types.push(MirType::ObjectRef);
            }
            for parameter in &sig.parameters {
                arg_types.push(mir_type(parameter, assembly, resolver.references()));
            }
        }
        let local_types: Vec<MirType> = method
            .local_variables()
            .iter()
            .map(|sig| mir_type(sig, assembly, resolver.references()))
            .collect();
        match cil::lower_method_typed(&body, &resolver, &arg_types, &local_types) {
            Ok((func, map)) => {
                funcs[*rid as usize] = func;
                maps[*rid as usize] = map;
            }
            Err(error) => fails.push((*rid, error)),
        }
    }
    (funcs, maps, fails)
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
                let arity = method.signature().map_or(0, |s| s.parameters.len());
                name = format!("{type_name}_{method_name}_{arity}");
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
    fn branch(&mut self, cond: ValueId, if_true: usize, if_false: usize) {
        self.blocks[self.cur].terminator = Some(Terminator::Branch {
            cond,
            if_true: BlockId(if_true as u32),
            true_args: Vec::new(),
            if_false: BlockId(if_false as u32),
            false_args: Vec::new(),
        });
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
            handle: TypeHandle(0),
            length: total,
            element_size: 4,
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
        Inst::AllocArray { handle: TypeHandle(0), length: new_n, element_size: 4 },
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
        Inst::AllocArray { handle: TypeHandle(0), length: new_nb, element_size: 4 },
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

}
