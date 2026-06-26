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

use lamella_ir::{BasicBlock, BlockId, Function, Inst, MirType, Terminator, TypeHandle, ValueId};
use lamella_metadata::{Assembly, SigType, TargetLayout};

#[cfg(feature = "arm32")]
use crate::arm32;
use crate::cil;
use crate::resolver::MetadataResolver;
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
/// widget. A chip `target` (e.g. "microbit") emits a flashable bare-metal Cortex-M image.
pub fn build(cil: &[u8], target: &str) -> Result<Vec<u8>, BuildError> {
    match target {
        #[cfg(feature = "wasm")]
        "wasm" => build_wasm(cil),
        #[cfg(feature = "arm32")]
        "microbit" => build_cortex_m(cil, target),
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
    let funcs = lower_assembly(&assembly, entry)?;
    let exports = method_exports(&assembly, entry.is_some());
    let export_refs: Vec<(&str, u32)> = exports.iter().map(|(n, i)| (n.as_str(), *i)).collect();
    wasm::lower_module_with_exports(&funcs, &export_refs).map_err(BuildError::LowerWasm)
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
    let initial_sp: u32 = match target {
        "microbit" => 0x2000_4000,
        _ => return Err(BuildError::UnsupportedTarget),
    };
    let assembly = Assembly::read(cil).map_err(|_| BuildError::Parse)?;
    let entry = find_main(&assembly);
    let funcs = lower_assembly(&assembly, entry)?;
    let code = arm32::lower_module(&funcs).map_err(BuildError::LowerArm)?;
    let mut image = Vec::with_capacity(8 + code.len());
    image.extend_from_slice(&initial_sp.to_le_bytes());
    image.extend_from_slice(&0x0000_0009u32.to_le_bytes());
    image.extend_from_slice(&code);
    Ok(image)
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
    let (funcs, maps, fails) = lower_assembly_debug(&assembly, entry);
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
    let assembly = Assembly::read(cil).map_err(|_| BuildError::Parse)?;
    let entry = find_main(&assembly);
    let funcs = lower_assembly(&assembly, entry)?;
    let names = object_symbol_names(&assembly, funcs.len());
    let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    arm32::lower_object(&funcs, &name_refs, &[]).map_err(BuildError::LowerArm)
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
fn mir_type(sig: &SigType, assembly: &Assembly) -> MirType {
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
            let size = assembly
                .value_type_layout(*token, &TargetLayout::ilp32())
                .map(|layout| layout.size)
                .unwrap_or(0);
            MirType::ValueType {
                handle: TypeHandle(token.0),
                size,
            }
        }
        _ => MirType::I32,
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
/// types the C# compiler emits for field initializers; precise lazy (before-first-access) init is a
/// follow-on.
fn startup(init: Option<u32>, cctors: &[u32], entry_rid: u32) -> Function {
    let callees: Vec<u32> = init
        .into_iter()
        .chain(cctors.iter().copied())
        .chain(core::iter::once(entry_rid))
        .collect();
    let insts: Vec<(ValueId, Inst)> = callees
        .iter()
        .enumerate()
        .map(|(i, &callee)| {
            (
                ValueId(i as u32),
                Inst::Call {
                    callee,
                    args: Vec::new(),
                },
            )
        })
        .collect();
    let result = ValueId((callees.len() - 1) as u32);
    Function {
        params: Vec::new(),
        ret: Some(MirType::I32),
        value_types: vec![MirType::I32; callees.len()],
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

/// Lowers an assembly's methods to a `Vec<Function>` keyed by MethodDef row. Index 0 is a trampoline
/// to `entry` (if any) -- the `main` export -- or a stub. A method that does not lower stays a stub.
fn lower_assembly(assembly: &Assembly, entry: Option<u32>) -> Result<Vec<Function>, BuildError> {
    let (funcs, _maps, fails) = lower_assembly_debug(assembly, entry);
    if let Some((rid, error)) = fails.into_iter().next() {
        return Err(BuildError::LowerCil { rid, error });
    }
    Ok(funcs)
}

/// As [`lower_assembly`], but also returns each function's [`cil::CilSourceMap`] (rid-indexed, empty for
/// the trampoline and the stub gaps) -- so the SAME image build()'s chip path produces also carries debug
/// info, and a debugger's line tables match the flashed layout by construction.
fn lower_assembly_debug(
    assembly: &Assembly,
    entry: Option<u32>,
) -> (
    Vec<Function>,
    Vec<cil::CilSourceMap>,
    Vec<(u32, cil::CilError)>,
) {
    let mut methods = Vec::new();
    let mut max_rid = entry.unwrap_or(0);
    for type_def in assembly.type_defs() {
        for method in type_def.methods() {
            let rid = method.rid();
            max_rid = max_rid.max(rid);
            methods.push((rid, method));
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
    let resolver = MetadataResolver::new(assembly);
    let mut fails: Vec<(u32, cil::CilError)> = Vec::new();
    for (rid, method) in &methods {
        let Some(body) = method.body() else { continue };
        let signature = method.signature();
        let mut arg_types = Vec::new();
        if let Some(sig) = &signature {
            if sig.has_this {
                arg_types.push(MirType::ObjectRef);
            }
            for parameter in &sig.parameters {
                arg_types.push(mir_type(parameter, assembly));
            }
        }
        let local_types: Vec<MirType> = method
            .local_variables()
            .iter()
            .map(|sig| mir_type(sig, assembly))
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

#[cfg(all(test, feature = "arm32"))]
mod tests {
    use super::*;

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

}
