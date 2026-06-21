//! The AOT build entry point: compile a CIL assembly to a target's native bytes in one call. This is
//! the library face of the pipeline the `wasm-program` example drives -- the website's client-side
//! `lamella_aot_build(cil, target)` widget exporter (a wasm binding around this) turns a C# assembly
//! into a `.wasm` widget in the browser. No filesystem or `std`: it takes the CIL bytes and returns
//! the output bytes, so it runs inside the browser's compile-only wasm.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use lamella_ir::{BasicBlock, BlockId, Function, Inst, MirType, Terminator, TypeHandle, ValueId};
use lamella_metadata::{Assembly, SigType, TargetLayout};

use crate::cil;
use crate::resolver::MetadataResolver;
use crate::wasm;

/// Why an AOT build failed.
#[derive(Debug)]
pub enum BuildError {
    /// The CIL assembly's metadata could not be read.
    Parse,
    /// The target string is not one this build supports.
    UnsupportedTarget,
    /// A function could not be lowered to the target.
    Lower(wasm::LowerError),
}

/// Compiles a CIL assembly to native bytes for `target`. `target = "wasm"` emits a WebAssembly module
/// with the embedding ABI (per-method exports + `alloc`/`dealloc` + memory) -- the C# -> `.wasm`
/// widget. Chip targets (an ARM boot image) are a follow-up.
pub fn build(cil: &[u8], target: &str) -> Result<Vec<u8>, BuildError> {
    match target {
        "wasm" => build_wasm(cil),
        _ => Err(BuildError::UnsupportedTarget),
    }
}

/// Compiles a CIL assembly to a WebAssembly module: every method lowered through the same
/// `resolver` + `cil` front end the ARM/RISC-V backends use, then `wasm::lower_module_with_exports`.
/// Exports every public static method by name (the widget surface) plus `main` for the entry, if any.
pub fn build_wasm(cil: &[u8]) -> Result<Vec<u8>, BuildError> {
    let assembly = Assembly::read(cil).map_err(|_| BuildError::Parse)?;
    let entry = find_main(&assembly);
    let funcs = lower_assembly(&assembly, entry);
    let exports = method_exports(&assembly, entry.is_some());
    let export_refs: Vec<(&str, u32)> = exports.iter().map(|(n, i)| (n.as_str(), *i)).collect();
    wasm::lower_module_with_exports(&funcs, &export_refs).map_err(BuildError::Lower)
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
                handle: TypeHandle(token.row()),
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

/// `fn() -> i32 { return entry(); }` at index 0, exported as `main`.
fn trampoline(entry_rid: u32) -> Function {
    Function {
        params: Vec::new(),
        ret: Some(MirType::I32),
        value_types: vec![MirType::I32],
        entry: BlockId(0),
        blocks: vec![BasicBlock {
            params: Vec::new(),
            insts: vec![(
                ValueId(0),
                Inst::Call {
                    callee: entry_rid,
                    args: Vec::new(),
                },
            )],
            terminator: Some(Terminator::Return(Some(ValueId(0)))),
        }],
    }
}

/// Lowers an assembly's methods to a `Vec<Function>` keyed by MethodDef row. Index 0 is a trampoline
/// to `entry` (if any) -- the `main` export -- or a stub. A method that does not lower stays a stub.
fn lower_assembly(assembly: &Assembly, entry: Option<u32>) -> Vec<Function> {
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
    if let Some(entry_rid) = entry {
        funcs[0] = trampoline(entry_rid);
    }
    let resolver = MetadataResolver::new(assembly);
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
        if let Ok((func, _)) = cil::lower_method_typed(&body, &resolver, &arg_types, &local_types) {
            funcs[*rid as usize] = func;
        }
    }
    funcs
}

/// The embedding ABI's export list: `main` (the entry trampoline at index 0, if there is an entry)
/// plus every public static method by a `<Type>_<Method>` name (overloads disambiguated by arity then
/// rid), each at its MethodDef row = WASM function index, so a page's JS calls them by name.
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
