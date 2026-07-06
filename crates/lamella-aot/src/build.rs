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
    BasicBlock, BinOp, BlockId, ConvKind, Function, Inst, MirType, Terminator, TypeHandle, ValueId,
};
use lamella_metadata::tables::table;
use lamella_metadata::{Assembly, SigType, TargetLayout};
use lamella_token::Token;

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
    let funcs = lower_assembly(&assembly, entry, None)?;
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
    let initial_sp: u32 = match target {
        "microbit" => 0x2000_4000,
        _ => return Err(BuildError::UnsupportedTarget),
    };
    let assembly = Assembly::read(cil).map_err(|_| BuildError::Parse)?;
    let entry = find_main(&assembly);
    let funcs = lower_assembly(&assembly, entry, None)?;
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
    let (funcs, maps, fails) = lower_assembly_debug(&assembly, entry, None);
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
    build_object_inner(cil, None)
}

/// As [`build_object`], but with the REFERENCED assembly (corlib) attached for cross-assembly
/// vtable-slot agreement: a program type extending a referenced base numbers its slots INCLUDING the
/// base's inherited virtuals (as the referenced assembly numbers them itself), an inherited slot is an
/// extern vtable entry the linker resolves against [`build_library_object`]'s export of it, and a
/// `callvirt` naming a referenced method (a `MemberRef`, e.g. `object.GetHashCode()` on a base-typed
/// receiver) dispatches through that shared slot instead of static-devirtualizing.
#[cfg(feature = "arm32")]
pub fn build_object_with_corlib(cil: &[u8], corlib: &[u8]) -> Result<Vec<u8>, BuildError> {
    build_object_inner(cil, Some(corlib))
}

#[cfg(feature = "arm32")]
fn build_object_inner(cil: &[u8], corlib: Option<&[u8]>) -> Result<Vec<u8>, BuildError> {
    let assembly = Assembly::read(cil).map_err(|_| BuildError::Parse)?;
    let reference = match corlib {
        Some(bytes) => Some(Assembly::read(bytes).map_err(|_| BuildError::Parse)?),
        None => None,
    };
    let entry = find_main(&assembly);
    let funcs = lower_assembly(&assembly, entry, reference.as_ref())?;
    let names = object_symbol_names(&assembly, funcs.len());
    let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let mut resolver = MetadataResolver::new(&assembly);
    if let Some(reference) = reference.as_ref() {
        resolver = resolver.with_reference(reference);
    }
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
    arm32::lower_object_vtables(&funcs, &name_refs, &[], &descriptors).map_err(BuildError::LowerArm)
}

/// AOT-lowers a whole assembly as a LINKABLE LIBRARY object (a corlib, a helper library): every public
/// static method becomes a global symbol (named by `extern_method_symbol`) a program's extern call
/// resolves against, and a method that does not lower yet becomes a STUB so the rest of the library
/// still builds -- gaps are fixed iteratively. No entry/startup ([`arm32::lower_object_library`]).
#[cfg(feature = "arm32")]
pub fn build_library_object(cil: &[u8]) -> Result<Vec<u8>, BuildError> {
    let assembly = Assembly::read(cil).map_err(|_| BuildError::Parse)?;
    let (funcs, _maps, _fails) = lower_assembly_debug(&assembly, None, None);
    let prefix = alloc::format!("L{:08x}.", lamella_metadata::fnv1a32(0x811c_9dc5, cil));
    let names = library_symbol_names(&assembly, funcs.len(), &prefix);
    let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let descriptors = MetadataResolver::new(&assembly).type_descriptors();
    arm32::lower_object_library_vtables(&funcs, &name_refs, &[], &descriptors)
        .map_err(BuildError::LowerArm)
}

/// The per-function symbol names for [`build_library_object`]: a public static method takes its stable
/// cross-assembly symbol (`extern_method_symbol`), so a program links its extern call against it; a
/// public VIRTUAL instance method likewise, so a program type inheriting it fills the vtable slot with
/// an extern entry the linker resolves here (cross-assembly dispatch of a never-overridden base
/// virtual, e.g. `System.Object.ToString.`); every other method keeps `f<rid>` (internal).
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
                && method.flags() & 0x7 == 0x6
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
            if let Some(underlying) =
                crate::resolver::enum_underlying(assembly, *token, &TargetLayout::ilp32())
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

/// A synthesized MIR body for a `[RuntimeProvided]` `System.String` reader, over the AOT string layout
/// `[len: u32][u16 code units ...]` (an `ldstr` ObjectRef points at the `len` word). `get_Length` loads the
/// len word at `this + 0`; `get_Chars(i)` loads the `u16` at `this + 4 + i*2`, zero-extended to i32. Both
/// are non-virtual now (the getter-virtual fix), so a program's `s.Length` / `s[i]` is a direct
/// cross-assembly call that links to corlib's copy of this. Returns `None` for a marked method this backend
/// does not synthesize (Substring/Concat/Console.*): it keeps its placeholder body, so a program calling one
/// fails to LINK loudly rather than binding a wrong value.
fn synthesize_runtime_reader(
    namespace: &str,
    type_name: &str,
    method_name: Option<&str>,
    param_count: usize,
) -> Option<Function> {
    if (namespace, type_name) != ("System", "String") {
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
        (Some("get_Chars"), 1) => Some(Function {
            params: vec![MirType::ObjectRef, MirType::I32],
            ret: Some(MirType::I32),
            value_types: vec![MirType::ObjectRef, MirType::I32, MirType::I32, MirType::I32, MirType::I32, MirType::I32, MirType::I32, MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0), ValueId(1)],
                insts: vec![
                    (ValueId(2), Inst::Convert { value: ValueId(0), kind: ConvKind::RefToInt }),
                    (ValueId(3), Inst::ConstInt { ty: MirType::I32, value: 2 }),
                    (ValueId(4), Inst::Binary { op: BinOp::Mul, lhs: ValueId(1), rhs: ValueId(3) }),
                    (ValueId(5), Inst::ConstInt { ty: MirType::I32, value: 4 }),
                    (ValueId(6), Inst::Binary { op: BinOp::Add, lhs: ValueId(2), rhs: ValueId(5) }),
                    (ValueId(7), Inst::Binary { op: BinOp::Add, lhs: ValueId(6), rhs: ValueId(4) }),
                    (ValueId(8), Inst::Load { address: ValueId(7), width: 2, signed: false }),
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
/// `reference` is the referenced assembly (corlib) for cross-assembly vtable-slot agreement, or `None`
/// for this-assembly-relative numbering.
fn lower_assembly<'a>(
    assembly: &'a Assembly<'a>,
    entry: Option<u32>,
    reference: Option<&'a Assembly<'a>>,
) -> Result<Vec<Function>, BuildError> {
    let (funcs, _maps, fails) = lower_assembly_debug(assembly, entry, reference);
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
    reference: Option<&'a Assembly<'a>>,
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
    let resolver = match reference {
        Some(reference) => MetadataResolver::new(assembly).with_reference(reference),
        None => MetadataResolver::new(assembly),
    };
    let mut fails: Vec<(u32, cil::CilError)> = Vec::new();
    for (rid, method, type_name) in &methods {
        let Some(body) = method.body() else { continue };
        let signature = method.signature();
        if has_runtime_provided_attribute(assembly, Token::new(table::METHOD_DEF, method.rid())) {
            if let Some(type_name) = type_name {
                let params = signature.as_ref().map_or(0, |sig| sig.parameters.len());
                if let Some(func) =
                    synthesize_runtime_reader(type_name.namespace, type_name.name, method.name(), params)
                {
                    funcs[*rid as usize] = func;
                    continue;
                }
                if (type_name.namespace, type_name.name) == ("System", "Console") {
                    let params = signature.as_ref().map(|s| s.parameters.as_slice()).unwrap_or(&[]);
                    let s = |sym| Some(sym);
                    let body = match (method.name(), params) {
                        (Some("Write"), [SigType::String]) => {
                            Some(console_body(Some(MirType::ObjectRef), true, s("lamella_console_write"), false))
                        }
                        (Some("WriteLine"), [SigType::String]) => {
                            Some(console_body(Some(MirType::ObjectRef), true, s("lamella_console_write"), true))
                        }
                        (Some("Write"), [SigType::I4]) => {
                            Some(console_body(Some(MirType::I32), false, s("lamella_console_write_i32"), false))
                        }
                        (Some("WriteLine"), [SigType::I4]) => {
                            Some(console_body(Some(MirType::I32), false, s("lamella_console_write_i32"), true))
                        }
                        (Some("WriteLine"), [SigType::U4]) => {
                            Some(console_body(Some(MirType::I32), false, s("lamella_console_write_u32"), true))
                        }
                        (Some("Write"), [SigType::Char]) => {
                            Some(console_body(Some(MirType::I32), false, s("lamella_console_write_char"), false))
                        }
                        (Some("WriteLine"), [SigType::Char]) => {
                            Some(console_body(Some(MirType::I32), false, s("lamella_console_write_char"), true))
                        }
                        (Some("WriteLine"), [SigType::Boolean]) => {
                            Some(console_body(Some(MirType::I32), false, s("lamella_console_write_bool"), true))
                        }
                        (Some("WriteLine"), [SigType::I8]) => {
                            Some(console_body(Some(MirType::I64), false, s("lamella_console_write_i64"), true))
                        }
                        (Some("WriteLine"), [SigType::U8]) => {
                            Some(console_body(Some(MirType::I64), false, s("lamella_console_write_u64"), true))
                        }
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
            }
        }
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

    #[cfg(feature = "arm32")]
    #[test]
    fn synthesized_string_readers_verify_and_lower() {
        for (name, params) in [("get_Length", 0usize), ("get_Chars", 1usize)] {
            let f = synthesize_runtime_reader("System", "String", Some(name), params)
                .unwrap_or_else(|| panic!("{name} not synthesized"));
            lamella_ir::verify(&f).unwrap_or_else(|e| panic!("{name} verify: {e:?}"));
            crate::arm32::lower(&f).unwrap_or_else(|e| panic!("{name} lower: {e:?}"));
        }
    }

    #[cfg(feature = "arm32")]
    #[test]
    fn synthesized_console_writers_verify_and_lower() {
        let bodies = [
            console_body(Some(MirType::ObjectRef), true, Some("lamella_console_write"), true),
            console_body(Some(MirType::I32), false, Some("lamella_console_write_i32"), true),
            console_body(Some(MirType::I64), false, Some("lamella_console_write_i64"), true),
            console_body(Some(MirType::I32), false, Some("lamella_console_write_char"), false),
            console_body(None, false, None, true),
        ];
        for f in bodies {
            lamella_ir::verify(&f).expect("a synthesized console body verifies");
            crate::arm32::lower_object(&[f], &["c"], &[]).expect("a synthesized console body lowers");
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
        let obj = lamella_elf::read_object(&crate::arm32::lower_object(&[f], &["dts"], &[]).unwrap())
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

}
