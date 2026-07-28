//! The WebAssembly target code generator -- the third backend target, after ARM and RISC-V.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use lamella_asm_wasm::{BlockType, Func, FuncType, Limits, MemArg, Module, ValType};
use lamella_ir::{
    BinOp, BlockId, CmpOp, ConvKind, Function, Inst, MirType, Terminator, ValueId,
};

use crate::resolver::TypeMeta;

/// The base address of the read-only string-literal region in linear memory: it follows the
/// static-field region, and the managed heap follows it. Addresses below it are reserved -- offset 0
/// is the managed null (an `ObjectRef`/`ManagedPtr` of 0).
const STRING_BASE: i64 = 1024;
/// The smallest linear-memory size, in 64 KiB pages, when nothing pushes the heap base higher.
const HEAP_MIN_PAGES: u32 = 2;
/// The global index of the bump-allocator heap pointer -- the only global the backend defines, so it
/// is index 0 whenever the module uses memory.
const HEAP_POINTER: u32 = 0;
/// The base address of the static-field region: it sits between the null guard at offset 0 and the
/// heap, so each static field lives at `STATIC_BASE + its offset`.
const STATIC_BASE: i32 = 8;

/// Why a function could not be lowered to WebAssembly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LowerError {
    /// The function did not pass IR verification.
    NotWellFormed,
    /// An instruction or type the WASM backend does not lower yet: a value type (no memory home in
    /// the local-per-value model yet), a static field, or a string literal.
    Unsupported,
    /// A control-flow shape not handled: a `Branch` edge that carries block-parameter arguments
    /// (merges pass their parameters on `Jump` edges instead), an entry block whose parameters do
    /// not match the signature, or an internal structuring inconsistency.
    ControlFlowUnsupported,
    /// A string literal holds a UTF-16 code unit this build's string storage cannot represent: a LONE
    /// surrogate under `string-utf8`. Refused rather than replaced with U+FFFD -- see
    /// `stringgen::encode_string_bytes` for why a compiler refuses where the interpreter throws.
    UnencodableStringUnit {
        /// The offending UTF-16 code unit.
        unit: u16,
        /// Its index in the literal, in UTF-16 code units.
        index: u32,
    },
}

/// Lowers a single [`Function`] to a WebAssembly module's bytes -- a one-function module exporting
/// the function as `main`.
pub fn lower(func: &Function) -> Result<Vec<u8>, LowerError> {
    lower_module(core::slice::from_ref(func))
}

/// As [`lower`], but for a single function with per-type vtables/TypeDescs (`descriptors`) available --
/// so a program whose `Main` dispatches virtually or through a delegate lowers correctly. The
/// vtable-aware single-function entry (mirrors [`lower_module_with_exports`]'s descriptors).
pub fn lower_vtables(func: &Function, descriptors: &[TypeMeta]) -> Result<Vec<u8>, LowerError> {
    let main_export: &[(&str, u32)] = &[("main", 0)];
    lower_module_inner(core::slice::from_ref(func), main_export, false, descriptors)
}

/// Lowers a module of [`Function`]s to WebAssembly. Module order fixes call indices -- function 0
/// is the entry and is exported as `main` for the host/JS to call; an `Inst::Call`'s `callee` is
/// the callee's index in this slice. A module with P/Invokes (host imports) offsets each defined
/// function by the import count, since imported functions occupy the low WebAssembly indices.
pub fn lower_module(funcs: &[Function]) -> Result<Vec<u8>, LowerError> {
    let main_export: &[(&str, u32)] = if funcs.is_empty() {
        &[]
    } else {
        &[("main", 0)]
    };
    lower_module_inner(funcs, main_export, false, &[])
}

/// Lowers a module, exporting each `(name, function_index)` in `exports` (the index is into `funcs`),
/// plus `memory` and the embedding-ABI allocator (`alloc`/`dealloc`) when the module uses memory.
/// This is the web embedding ABI: a page's JS calls each exported method by name (e.g. `Program_Add`)
/// and uses `alloc`/`dealloc` to pass arrays/strings in. `lower_module` is the single-entry
/// `main`-only case. Export names must be unique (the caller mangles overloads); a function appended
/// by the string lowering keeps the original indices valid.
pub fn lower_module_with_exports(
    funcs: &[Function],
    exports: &[(&str, u32)],
    descriptors: &[TypeMeta],
) -> Result<Vec<u8>, LowerError> {
    lower_module_inner(funcs, exports, true, descriptors)
}

fn lower_module_inner(
    funcs: &[Function],
    exports: &[(&str, u32)],
    with_allocator: bool,
    descriptors: &[TypeMeta],
) -> Result<Vec<u8>, LowerError> {
    let mut program: Vec<Function> = funcs.to_vec();
    let strings = layout_strings(&mut program)?;
    crate::stringgen::lower_string_equals(&mut program);
    crate::stringgen::lower_string_concat(&mut program, None);
    crate::stringgen::lower_int_to_string(&mut program, None);

    for func in &program {
        if lamella_ir::verify(func).is_err() {
            return Err(LowerError::NotWellFormed);
        }
    }

    let native_import_sigs = collect_native_imports(&program)?;
    let import_count = native_import_sigs.len() as u32;

    let (desc_segments, desc_addr, desc_end) =
        layout_descriptors(&program, descriptors, strings.heap_base as u32, import_count);

    let mut module = Module::new();
    let has_memory =
        uses_memory(&program) || !strings.segments.is_empty() || !desc_segments.is_empty();
    if has_memory {
        let heap_base = i64::from(desc_end.next_multiple_of(8));
        module.set_memory(Limits {
            min_pages: ((heap_base as u32).div_ceil(0x1_0000) + 1).max(HEAP_MIN_PAGES),
            max_pages: None,
        });
        module.export_memory("memory");
        module.add_global(ValType::I32, true, heap_base);
        for (offset, blob) in strings.segments {
            module.add_data(offset, blob);
        }
        for (offset, blob) in desc_segments {
            module.add_data(offset, blob);
        }
    }

    let mut sig_types: Vec<(FuncType, u32)> = Vec::new();
    let mut needs_table = false;
    for func in &program {
        for (result, inst) in func.blocks.iter().flat_map(|b| &b.insts) {
            match inst {
                Inst::FuncAddr { .. } | Inst::VirtualFuncAddr { .. } => needs_table = true,
                Inst::CallVirtual {
                    args, returns_value, ..
                }
                | Inst::CallInterface {
                    args, returns_value, ..
                }
                | Inst::CallIndirect {
                    args, returns_value, ..
                } => {
                    needs_table = true;
                    let result_ty = call_result_ty(*returns_value, *result, &func.value_types);
                    let sig = call_site_type(args, result_ty, &func.value_types)?;
                    intern_sig(&mut module, &mut sig_types, sig);
                }
                Inst::InvokeDelegate {
                    args, returns_value, ..
                } => {
                    needs_table = true;
                    let result_ty = call_result_ty(*returns_value, *result, &func.value_types);
                    let without = call_site_type(args, result_ty, &func.value_types)?;
                    let mut with = without.clone();
                    with.params.insert(0, ValType::I32);
                    intern_sig(&mut module, &mut sig_types, without);
                    intern_sig(&mut module, &mut sig_types, with);
                }
                _ => {}
            }
        }
    }
    if needs_table {
        module.enable_func_table();
    }

    let mut native_imports: BTreeMap<alloc::string::String, u32> = BTreeMap::new();
    for (name, sig) in native_import_sigs {
        let type_index = module.add_type(sig);
        let index = module.add_import_func("lamella_native", &name, type_index);
        native_imports.insert(name, index);
    }

    let ctx = WasmCtx {
        funcs: &program,
        desc_addr,
        sig_types,
        import_count,
        native_imports,
    };
    for func in &program {
        let type_index = module.add_type(func_type(func)?);
        let body = lower_function(func, &ctx)?;
        module.add_function(type_index, body);
    }
    if with_allocator && has_memory {
        let alloc_index = import_count + program.len() as u32;
        let alloc_type = module.add_type(FuncType {
            params: alloc::vec![ValType::I32],
            results: alloc::vec![ValType::I32],
        });
        module.add_function(alloc_type, build_alloc());
        let dealloc_type = module.add_type(FuncType {
            params: alloc::vec![ValType::I32, ValType::I32],
            results: Vec::new(),
        });
        module.add_function(dealloc_type, build_dealloc());
        module.export_func("alloc", alloc_index);
        module.export_func("dealloc", alloc_index + 1);
    }
    for (name, index) in exports {
        module.export_func(name, import_count + *index);
    }
    Ok(module.finish())
}

/// The embedding ABI's `alloc(size) -> ptr`: round `size` up to 8 bytes, bump the heap-pointer global
/// by it, and return the old top. JS reserves a buffer with this, writes `[len][bytes]`, and passes
/// the pointer into an exported method.
fn build_alloc() -> Func {
    let mut f = Func::new(1);
    f.global_get(HEAP_POINTER);
    f.global_get(HEAP_POINTER);
    f.local_get(0);
    f.i32_const(7);
    f.i32_add();
    f.i32_const(!7);
    f.i32_and();
    f.i32_add();
    f.global_set(HEAP_POINTER);
    f.end();
    f
}

/// The embedding ABI's `dealloc(ptr, size)`: a no-op for the bump allocator (it never frees; a single
/// result stays valid until the next call). Present so JS can use the standard alloc/dealloc pair.
fn build_dealloc() -> Func {
    let mut f = Func::new(2);
    f.end();
    f
}

/// The result of laying out a module's string literals: the read-only data segments to emit and the
/// heap base that follows them.
struct StringLayout {
    /// `(offset, blob)` for each distinct literal, the blob being `[u32 length][UTF-16LE units]`.
    segments: Vec<(u32, Vec<u8>)>,
    /// The bump-allocator heap base: just past the string data, 8-aligned.
    heap_base: i64,
}

/// Interns each `StringLiteral` to a `[u32 length][UTF-16LE]` blob at an offset from [`STRING_BASE`],
/// rewriting the instruction to a constant `ObjectRef` pointer, and returns the segments + heap base.
fn layout_strings(program: &mut [Function]) -> Result<StringLayout, LowerError> {
    let mut interned: Vec<(Vec<u16>, u32)> = Vec::new();
    let mut segments: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut next = STRING_BASE as u32;
    for func in program.iter_mut() {
        for block in &mut func.blocks {
            for (_, inst) in &mut block.insts {
                if let Inst::StringLiteral { utf16 } = inst {
                    let units: Vec<u16> = utf16.to_vec();
                    let offset = match interned.iter().find(|(c, _)| *c == units) {
                        Some((_, offset)) => *offset,
                        None => {
                            let blob = string_blob(&units)?;
                            let offset = next;
                            next = (next + blob.len() as u32).next_multiple_of(4);
                            interned.push((units, offset));
                            segments.push((offset, blob));
                            offset
                        }
                    };
                    *inst = Inst::ConstInt {
                        ty: MirType::ObjectRef,
                        value: i64::from(offset),
                    };
                }
            }
        }
    }
    Ok(StringLayout {
        segments,
        heap_base: i64::from(next.next_multiple_of(8)),
    })
}

/// Builds a string blob in this build's storage encoding -- the unit count as a little-endian `u32`,
/// then either the UTF-16LE units or a byte length and the UTF-8/WTF-8 bytes.
///
/// Delegates to the ONE place that owns the layout rather than spelling it out again: this function
/// used to hardcode UTF-16, so a `--features wasm,string-utf8` build was accepted and silently
/// produced a UTF-16 image.
fn string_blob(utf16: &[u16]) -> Result<Vec<u8>, LowerError> {
    crate::stringgen::string_blob_bytes(utf16).map_err(|e| LowerError::UnencodableStringUnit {
        unit: e.unit,
        index: e.index,
    })
}

/// The WebAssembly function type for `func`: its MIR parameter and return types mapped to value
/// types.
fn func_type(func: &Function) -> Result<FuncType, LowerError> {
    let mut params = Vec::with_capacity(func.params.len());
    for &p in &func.params {
        params.push(valtype(p)?);
    }
    let results = match func.ret {
        Some(t) => alloc::vec![valtype(t)?],
        None => Vec::new(),
    };
    Ok(FuncType { params, results })
}

/// Whether any function touches the managed heap or raw memory, so the module needs a linear memory
/// and the bump-allocator global.
fn uses_memory(funcs: &[Function]) -> bool {
    funcs
        .iter()
        .flat_map(|f| &f.blocks)
        .flat_map(|b| &b.insts)
        .any(|(_, inst)| {
            matches!(
                inst,
                Inst::Alloc { .. }
                    | Inst::AllocLike { .. }
                    | Inst::AllocDescribed { .. }
                    | Inst::AllocArray { .. }
                    | Inst::InitStruct
                    | Inst::CopyStruct { .. }
                    | Inst::Load { .. }
                    | Inst::Store { .. }
                    | Inst::FieldLoad { .. }
                    | Inst::FieldStore { .. }
                    | Inst::FieldLoadNarrow { .. }
                    | Inst::FieldStoreNarrow { .. }
                    | Inst::FieldAddr { .. }
                    | Inst::ArrayLoad { .. }
                    | Inst::ArrayStore { .. }
                    | Inst::AllocArray2D { .. }
                    | Inst::Array2DLoad { .. }
                    | Inst::Array2DStore { .. }
                    | Inst::AllocArrayMD { .. }
                    | Inst::ArrayMDLoad { .. }
                    | Inst::ArrayMDStore { .. }
                    | Inst::StaticLoad { .. }
                    | Inst::StaticStore { .. }
                    | Inst::StringLiteral { .. }
                    | Inst::StringEquals { .. }
                    | Inst::StringConcat { .. }
                    | Inst::IntToString { .. }
                    | Inst::CopyBlock { .. }
                    | Inst::FillBlock { .. }
                    | Inst::ArrayElemAddr { .. }
            )
        })
}

/// Lowers one function body: the value-to-local map (the entry block's parameters are the
/// WebAssembly parameters at locals `0..n`; every other value gets a fresh local), then the
/// structured control flow emitted from the dominator tree.
/// The module-level context threaded through function lowering: the program (a direct call's
/// arity/result), each type's descriptor address (the Alloc header + `TypeDescAddr` value), and the
/// interned indirect-call signatures (a call site's [`FuncType`] -> its module type index).
struct WasmCtx<'a> {
    funcs: &'a [Function],
    desc_addr: BTreeMap<u32, i32>,
    sig_types: Vec<(FuncType, u32)>,
    /// The number of imported functions. Imports occupy the low function indices, so every DEFINED
    /// function reference (a `Call` callee, a `FuncAddr`/vtable table index, an export) is
    /// `import_count + its index in `funcs``.
    import_count: u32,
    /// Each `Inst::PInvoke` import name -> its wasm function index (in the `"lamella_native"` module).
    native_imports: BTreeMap<alloc::string::String, u32>,
}

impl WasmCtx<'_> {
    /// The module type index for a `call_indirect` of signature `sig` (interned during module setup).
    fn indirect_type(&self, sig: &FuncType) -> Result<u32, LowerError> {
        self.sig_types
            .iter()
            .find(|(s, _)| s == sig)
            .map(|(_, i)| *i)
            .ok_or(LowerError::Unsupported)
    }
}

/// The `call_indirect` signature of a call site: each argument's value type as a parameter, the call's
/// result value type (if any) as the single result. `call_indirect` is signature-checked, so this must
/// exactly match the target function's own type.
fn call_site_type(
    args: &[ValueId],
    result_ty: Option<MirType>,
    value_types: &[MirType],
) -> Result<FuncType, LowerError> {
    let mut params = Vec::with_capacity(args.len());
    for &a in args {
        params.push(valtype(
            *value_types.get(a.index()).ok_or(LowerError::Unsupported)?,
        )?);
    }
    let results = match result_ty {
        Some(t) => alloc::vec![valtype(t)?],
        None => Vec::new(),
    };
    Ok(FuncType { params, results })
}

/// The result type of an indirect call's signature: the result value's type when the callee returns a
/// value, or `None` for a `void` callee (whose `call_indirect` signature must have no result). The
/// result value carries a placeholder type even for `void`, so `returns_value` is the authority.
fn call_result_ty(returns_value: bool, result: ValueId, value_types: &[MirType]) -> Option<MirType> {
    if returns_value {
        value_types.get(result.index()).copied()
    } else {
        None
    }
}

/// Interns `sig` in the module's type section (for `call_indirect`) and records it with its index.
fn intern_sig(module: &mut Module, sig_types: &mut Vec<(FuncType, u32)>, sig: FuncType) {
    if sig_types.iter().any(|(s, _)| *s == sig) {
        return;
    }
    let index = module.add_type(sig.clone());
    sig_types.push((sig, index));
}

/// The distinct host imports a module needs, one per `Inst::PInvoke` import name (in first-appearance
/// order), each with the marshaled signature: parameters from the call's argument value types, and a
/// single result from the call-result value's type. This is the WASM analog of the RISC-V/ARM
/// extern-by-name -- except on wasm the symbol binds to a HOST IMPORT (module `"lamella_native"`, per
/// the embedding ABI), not a linked object. A `[DllImport]` and the corlib's `Console.Write` (which
/// lowers to a P/Invoke of `lamella_console_write_*`) both resolve this way; a `void` call's result
/// value is a dead placeholder (typed `i32`), so its import returns `i32` too (the host ignores it).
fn collect_native_imports(
    program: &[Function],
) -> Result<Vec<(alloc::string::String, FuncType)>, LowerError> {
    let mut imports: Vec<(alloc::string::String, FuncType)> = Vec::new();
    for func in program {
        for (result, inst) in func.blocks.iter().flat_map(|b| &b.insts) {
            let Inst::PInvoke { import, args } = inst else {
                continue;
            };
            if imports.iter().any(|(name, _)| name.as_str() == &**import) {
                continue;
            }
            let mut params = Vec::with_capacity(args.len());
            for &a in args {
                params.push(valtype(
                    func.value_types
                        .get(a.index())
                        .copied()
                        .ok_or(LowerError::Unsupported)?,
                )?);
            }
            let results = alloc::vec![valtype(
                func.value_types
                    .get(result.index())
                    .copied()
                    .ok_or(LowerError::Unsupported)?,
            )?];
            imports.push((alloc::string::String::from(&**import), FuncType { params, results }));
        }
    }
    Ok(imports)
}

/// The result of [`layout_descriptors`]: the linear-memory data segments (`(offset, bytes)`), the
/// `type handle -> descriptor address` map, and the next free linear-memory offset.
type DescriptorLayout = (Vec<(u32, Vec<u8>)>, BTreeMap<u32, i32>, u32);

/// Lays each allocated or queried type's descriptor in linear memory from `base`. Per type the vtable
/// (function = table indices) is laid BEFORE a fixed metadata block so slot `k` is `[desc - 4 - k*4]`,
/// and `desc` (the address in the map -- the object header + `TypeDescAddr` value) points at:
/// `[id @ desc+0][base_ptr @ desc+4][itable_count @ desc+8][(tag, funcref-index) @ desc+12 ...]`. The
/// `base_ptr` is the base type's descriptor address (0 at the chain's end), which a `castclass`/`isinst`
/// scan walks; the base types are added transitively so the whole chain is present. Returns the data
/// segments, the `handle -> descriptor address` map, and the next free offset. Two passes: assign every
/// descriptor an address, then lay the bytes (so a `base_ptr` can reference an address assigned later).
fn layout_descriptors(
    program: &[Function],
    descriptors: &[TypeMeta],
    base: u32,
    import_count: u32,
) -> DescriptorLayout {
    let base_of = |h: u32| {
        descriptors
            .iter()
            .find(|m| m.handle.0 == h)
            .and_then(|m| m.base)
            .map(|b| b.0)
    };
    let mut handles: Vec<u32> = Vec::new();
    for (_, inst) in program
        .iter()
        .flat_map(|f| f.blocks.iter().flat_map(|b| &b.insts))
    {
        let handle = match inst {
            Inst::Alloc { handle, .. } | Inst::TypeDescAddr { handle } => handle.0,
            _ => continue,
        };
        if !handles.contains(&handle) {
            handles.push(handle);
        }
    }
    let mut i = 0;
    while i < handles.len() {
        if let Some(b) = base_of(handles[i]) {
            if !handles.contains(&b) {
                handles.push(b);
            }
        }
        i += 1;
    }

    let mut map = BTreeMap::new();
    let mut next = base;
    for &handle in &handles {
        let meta = descriptors.iter().find(|m| m.handle.0 == handle);
        let vlen = meta.map(|m| m.vtable.len()).unwrap_or(0) as u32;
        let ilen = meta.map(|m| m.itable.len()).unwrap_or(0) as u32;
        map.insert(handle, (next + vlen * 4) as i32);
        next += vlen * 4 + 12 + ilen * 8;
    }

    let mut segments = Vec::new();
    for &handle in &handles {
        let meta = descriptors.iter().find(|m| m.handle.0 == handle);
        let vtable = meta.map(|m| m.vtable.as_slice()).unwrap_or(&[]);
        let itable = meta.map(|m| m.itable.as_slice()).unwrap_or(&[]);
        let desc_addr = map[&handle] as u32;
        let base_ptr = base_of(handle)
            .and_then(|b| map.get(&b).copied())
            .unwrap_or(0) as u32;
        let mut blob = Vec::with_capacity((vtable.len() + 3 + itable.len() * 2) * 4);
        for slot in vtable.iter().rev() {
            let index = match slot {
                crate::resolver::VtableEntry::Func(index) => *index,
                crate::resolver::VtableEntry::Extern(_) => {
                    debug_assert!(false, "extern vtable slot on the wasm path");
                    0
                }
            };
            blob.extend_from_slice(&(index + import_count).to_le_bytes());
        }
        blob.extend_from_slice(&handle.to_le_bytes());
        blob.extend_from_slice(&base_ptr.to_le_bytes());
        blob.extend_from_slice(&(itable.len() as u32).to_le_bytes());
        for (tag, impl_) in itable {
            blob.extend_from_slice(&tag.to_le_bytes());
            let func_index = match impl_ {
                crate::resolver::VtableEntry::Func(index) => *index,
                crate::resolver::VtableEntry::Extern(_) => {
                    debug_assert!(false, "extern itable entry on the wasm path");
                    0
                }
            };
            blob.extend_from_slice(&(func_index + import_count).to_le_bytes());
        }
        segments.push((desc_addr - (vtable.len() as u32) * 4, blob));
    }
    (segments, map, next)
}

fn lower_function(func: &Function, ctx: &WasmCtx) -> Result<Func, LowerError> {
    let entry = &func.blocks[func.entry.index()];
    if entry.params.len() != func.params.len() {
        return Err(LowerError::ControlFlowUnsupported);
    }
    let param_count = func.params.len() as u32;
    let mut body = Func::new(param_count);

    let mut local_of = alloc::vec![u32::MAX; func.value_types.len()];
    for (position, &param) in entry.params.iter().enumerate() {
        local_of[param.index()] = position as u32;
    }
    for (value, &ty) in func.value_types.iter().enumerate() {
        if local_of[value] == u32::MAX {
            local_of[value] = body.add_local(valtype(ty)?);
        }
    }

    let cfg = Cfg::analyze(func);
    let mut scopes: Vec<Scope> = Vec::new();
    emit_tree(
        &cfg,
        func,
        ctx,
        &local_of,
        &mut body,
        &mut scopes,
        func.entry,
    )?;
    if func.ret.is_some() {
        body.unreachable();
    }
    body.end();
    Ok(body)
}

/// A control structure scope open at a point in the emitted body. WebAssembly branches name an
/// enclosing scope by its depth from the innermost (depth 0), so the structurer keeps the open
/// scopes on a stack and computes a branch's depth by searching it.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Scope {
    kind: ScopeKind,
    /// The MIR block this scope is labeled with: a `Block` ends just before that block's code (a
    /// forward branch target), a `Loop` begins at that block's code (a back-edge target). An `If`
    /// is never a branch target but still occupies a depth level.
    block: BlockId,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    Block,
    Loop,
    If,
}

/// The control-flow analysis a function's structuring needs: reverse postorder, the dominator tree,
/// and which blocks are loop headers / merge points.
struct Cfg {
    entry: BlockId,
    /// Each block's position in reverse postorder, or `u32::MAX` if unreachable.
    rpo_index: Vec<u32>,
    /// Each reachable block's immediate dominator (the entry's is itself).
    idom: Vec<u32>,
    /// Whether a block is the target of a back-edge (so it gets a `loop`).
    is_loop_header: Vec<bool>,
    /// Whether a block has two or more forward predecessors (so it gets a `block` and is branched
    /// to rather than emitted inline).
    is_merge: Vec<bool>,
    /// The dominator-tree children of each block.
    dom_children: Vec<Vec<BlockId>>,
}

impl Cfg {
    /// The successor blocks of `block` (by its terminator), in branch order.
    fn successors(func: &Function, block: usize) -> Vec<usize> {
        match func.blocks[block].terminator.as_ref() {
            Some(Terminator::Jump { target, .. }) => alloc::vec![target.index()],
            Some(Terminator::Branch {
                if_true, if_false, ..
            }) => alloc::vec![if_true.index(), if_false.index()],
            _ => Vec::new(),
        }
    }

    fn analyze(func: &Function) -> Cfg {
        let n = func.blocks.len();
        let entry = func.entry.index();

        let mut visited = alloc::vec![false; n];
        let mut postorder: Vec<usize> = Vec::new();
        let mut stack: Vec<(usize, usize)> = alloc::vec![(entry, 0)];
        visited[entry] = true;
        while let Some((b, i)) = stack.pop() {
            let succ = Cfg::successors(func, b);
            if i < succ.len() {
                stack.push((b, i + 1));
                let c = succ[i];
                if !visited[c] {
                    visited[c] = true;
                    stack.push((c, 0));
                }
            } else {
                postorder.push(b);
            }
        }
        let reachable = visited;
        let rpo: Vec<usize> = postorder.iter().rev().copied().collect();
        let mut rpo_index = alloc::vec![u32::MAX; n];
        for (i, &b) in rpo.iter().enumerate() {
            rpo_index[b] = i as u32;
        }

        let mut preds: Vec<Vec<usize>> = alloc::vec![Vec::new(); n];
        for (b, &is_reachable) in reachable.iter().enumerate() {
            if is_reachable {
                for c in Cfg::successors(func, b) {
                    preds[c].push(b);
                }
            }
        }

        let mut idom = alloc::vec![u32::MAX; n];
        idom[entry] = entry as u32;
        let mut changed = true;
        while changed {
            changed = false;
            for &b in &rpo {
                if b == entry {
                    continue;
                }
                let mut new_idom: Option<usize> = None;
                for &p in &preds[b] {
                    if idom[p] != u32::MAX {
                        new_idom = Some(match new_idom {
                            None => p,
                            Some(cur) => intersect(p, cur, &idom, &rpo_index),
                        });
                    }
                }
                if let Some(ni) = new_idom {
                    if idom[b] != ni as u32 {
                        idom[b] = ni as u32;
                        changed = true;
                    }
                }
            }
        }

        let mut is_loop_header = alloc::vec![false; n];
        let mut forward_preds = alloc::vec![0u32; n];
        for (b, &is_reachable) in reachable.iter().enumerate() {
            if !is_reachable {
                continue;
            }
            for c in Cfg::successors(func, b) {
                if dominates(c, b, &idom, entry) {
                    is_loop_header[c] = true;
                } else {
                    forward_preds[c] += 1;
                }
            }
        }
        let is_merge: Vec<bool> = forward_preds.iter().map(|&c| c >= 2).collect();

        let mut dom_children: Vec<Vec<BlockId>> = alloc::vec![Vec::new(); n];
        for b in 0..n {
            if reachable[b] && b != entry {
                dom_children[idom[b] as usize].push(BlockId(b as u32));
            }
        }

        Cfg {
            entry: func.entry,
            rpo_index,
            idom,
            is_loop_header,
            is_merge,
            dom_children,
        }
    }

    /// The dominator-tree children of `block` that are merge points, sorted by reverse postorder.
    fn merge_children(&self, block: BlockId) -> Vec<BlockId> {
        let mut children: Vec<BlockId> = self.dom_children[block.index()]
            .iter()
            .copied()
            .filter(|c| self.is_merge[c.index()])
            .collect();
        children.sort_by_key(|c| self.rpo_index[c.index()]);
        children
    }
}

/// Whether block `a` dominates block `b` (walks `b` up the dominator tree to the root).
fn dominates(a: usize, b: usize, idom: &[u32], entry: usize) -> bool {
    let mut x = b;
    loop {
        if x == a {
            return true;
        }
        if x == entry {
            return false;
        }
        x = idom[x] as usize;
    }
}

/// The dominator-tree intersection of `a` and `b`: their nearest common dominator (Cooper-Harvey-
/// Kennedy -- walk the one further from the root up until they meet, comparing reverse-postorder
/// numbers where a larger number is further from the root).
fn intersect(mut a: usize, mut b: usize, idom: &[u32], rpo_index: &[u32]) -> usize {
    while a != b {
        while rpo_index[a] > rpo_index[b] {
            a = idom[a] as usize;
        }
        while rpo_index[b] > rpo_index[a] {
            b = idom[b] as usize;
        }
    }
    a
}

/// Emits the dominator subtree rooted at `x`: a `loop` wrapper if `x` is a loop header, then `x`'s
/// merge-child blocks and `x`'s own code, nested so every branch resolves to an enclosing scope.
fn emit_tree(
    cfg: &Cfg,
    func: &Function,
    ctx: &WasmCtx,
    local_of: &[u32],
    body: &mut Func,
    scopes: &mut Vec<Scope>,
    x: BlockId,
) -> Result<(), LowerError> {
    if cfg.is_loop_header[x.index()] {
        body.loop_(BlockType::Empty);
        scopes.push(Scope {
            kind: ScopeKind::Loop,
            block: x,
        });
        let merges = cfg.merge_children(x);
        emit_branches(cfg, func, ctx, local_of, body, scopes, x, &merges)?;
        scopes.pop();
        body.end();
    } else {
        let merges = cfg.merge_children(x);
        emit_branches(cfg, func, ctx, local_of, body, scopes, x, &merges)?;
    }
    Ok(())
}

/// Wraps `x`'s code in a `block` for each merge child (the latest in reverse postorder outermost,
/// so a forward `br` to any merge child reaches its `end`), emitting each merge child's subtree
/// after its block closes.
#[allow(clippy::too_many_arguments)]
fn emit_branches(
    cfg: &Cfg,
    func: &Function,
    ctx: &WasmCtx,
    local_of: &[u32],
    body: &mut Func,
    scopes: &mut Vec<Scope>,
    x: BlockId,
    merges: &[BlockId],
) -> Result<(), LowerError> {
    match merges.split_last() {
        None => emit_node(cfg, func, ctx, local_of, body, scopes, x),
        Some((&outer, rest)) => {
            body.block(BlockType::Empty);
            scopes.push(Scope {
                kind: ScopeKind::Block,
                block: outer,
            });
            emit_branches(cfg, func, ctx, local_of, body, scopes, x, rest)?;
            scopes.pop();
            body.end();
            emit_tree(cfg, func, ctx, local_of, body, scopes, outer)
        }
    }
}

/// Emits `x`'s instructions and its terminator (a return, a trap, or a branch resolved against the
/// open scopes).
fn emit_node(
    cfg: &Cfg,
    func: &Function,
    ctx: &WasmCtx,
    local_of: &[u32],
    body: &mut Func,
    scopes: &mut Vec<Scope>,
    x: BlockId,
) -> Result<(), LowerError> {
    let block = &func.blocks[x.index()];
    let local = |v: ValueId| local_of[v.index()];
    for (result, inst) in &block.insts {
        lower_inst(body, &local, &func.value_types, ctx, *result, inst)?;
    }
    match block.terminator.as_ref() {
        Some(Terminator::Return(value)) => {
            if let Some(v) = value {
                body.local_get(local(*v));
            }
            body.return_();
            Ok(())
        }
        Some(Terminator::Unreachable) => {
            body.unreachable();
            Ok(())
        }
        Some(Terminator::Jump { target, args }) => {
            emit_edge(cfg, func, ctx, local_of, body, scopes, x, *target, args)
        }
        Some(Terminator::Branch {
            cond,
            if_true,
            true_args,
            if_false,
            false_args,
        }) => emit_cond_branch(
            cfg, func, ctx, local_of, body, scopes, x, *cond, *if_true, true_args, *if_false,
            false_args,
        ),
        None => Err(LowerError::ControlFlowUnsupported),
    }
}

/// How an edge `x -> t` is realized.
enum Disposition {
    /// A back-edge to a loop header: `br` to its `loop` scope.
    BackEdge,
    /// A forward edge to a merge point: `br` to its `block` scope.
    ForwardMerge,
    /// An edge to a block `x` solely dominates: emit it inline here.
    Inline,
}

fn disposition(cfg: &Cfg, x: BlockId, t: BlockId) -> Disposition {
    if dominates(t.index(), x.index(), &cfg.idom, cfg.entry.index()) {
        Disposition::BackEdge
    } else if cfg.is_merge[t.index()] {
        Disposition::ForwardMerge
    } else {
        Disposition::Inline
    }
}

/// Emits an unconditional edge: the block-parameter copies, then a `br` to the target's scope (a
/// back-edge or a forward merge) or its inline subtree.
#[allow(clippy::too_many_arguments)]
fn emit_edge(
    cfg: &Cfg,
    func: &Function,
    ctx: &WasmCtx,
    local_of: &[u32],
    body: &mut Func,
    scopes: &mut Vec<Scope>,
    x: BlockId,
    target: BlockId,
    args: &[ValueId],
) -> Result<(), LowerError> {
    emit_parallel_copy(body, local_of, args, &func.blocks[target.index()].params);
    match disposition(cfg, x, target) {
        Disposition::BackEdge => body.br(depth_of(scopes, ScopeKind::Loop, target)?),
        Disposition::ForwardMerge => body.br(depth_of(scopes, ScopeKind::Block, target)?),
        Disposition::Inline => emit_tree(cfg, func, ctx, local_of, body, scopes, target)?,
    }
    Ok(())
}

/// Emits a conditional branch. When a side is itself a branch target (a merge or a loop header) it
/// becomes a `br_if`/`br`; when both sides are inline subtrees the block owns, it becomes an
/// `if`/`else`. (Branch edges that pass block-parameter arguments are deferred -- merges carry their
/// parameters on the `Jump` edges instead.)
#[allow(clippy::too_many_arguments)]
fn emit_cond_branch(
    cfg: &Cfg,
    func: &Function,
    ctx: &WasmCtx,
    local_of: &[u32],
    body: &mut Func,
    scopes: &mut Vec<Scope>,
    x: BlockId,
    cond: ValueId,
    if_true: BlockId,
    true_args: &[ValueId],
    if_false: BlockId,
    false_args: &[ValueId],
) -> Result<(), LowerError> {
    if !true_args.is_empty() || !false_args.is_empty() {
        return Err(LowerError::ControlFlowUnsupported);
    }
    let cond_local = local_of[cond.index()];
    let dt = disposition(cfg, x, if_true);
    let df = disposition(cfg, x, if_false);
    match (
        target_depth(scopes, &dt, if_true),
        target_depth(scopes, &df, if_false),
    ) {
        (Some(dt_depth), Some(df_depth)) => {
            body.local_get(cond_local);
            body.br_if(dt_depth?);
            body.br(df_depth?);
            Ok(())
        }
        (Some(dt_depth), None) => {
            body.local_get(cond_local);
            body.br_if(dt_depth?);
            emit_tree(cfg, func, ctx, local_of, body, scopes, if_false)
        }
        (None, Some(df_depth)) => {
            body.local_get(cond_local);
            body.i32_eqz();
            body.br_if(df_depth?);
            emit_tree(cfg, func, ctx, local_of, body, scopes, if_true)
        }
        (None, None) => {
            body.local_get(cond_local);
            body.if_(BlockType::Empty);
            scopes.push(Scope {
                kind: ScopeKind::If,
                block: x,
            });
            emit_tree(cfg, func, ctx, local_of, body, scopes, if_true)?;
            body.else_();
            emit_tree(cfg, func, ctx, local_of, body, scopes, if_false)?;
            scopes.pop();
            body.end();
            Ok(())
        }
    }
}

/// The branch depth for a side of a conditional, or `None` if it is an inline (fall-through) side.
/// The inner `Result` carries a structuring failure (a target whose scope is unexpectedly absent).
fn target_depth(
    scopes: &[Scope],
    disposition: &Disposition,
    target: BlockId,
) -> Option<Result<u32, LowerError>> {
    match disposition {
        Disposition::BackEdge => Some(depth_of(scopes, ScopeKind::Loop, target)),
        Disposition::ForwardMerge => Some(depth_of(scopes, ScopeKind::Block, target)),
        Disposition::Inline => None,
    }
}

/// The relative depth of the innermost open scope of `kind` labeled `target` (the topmost scope is
/// depth 0). A missing scope is a structuring bug, reported rather than panicked.
fn depth_of(scopes: &[Scope], kind: ScopeKind, target: BlockId) -> Result<u32, LowerError> {
    for (depth, scope) in scopes.iter().rev().enumerate() {
        if scope.kind == kind && scope.block == target {
            return Ok(depth as u32);
        }
    }
    Err(LowerError::ControlFlowUnsupported)
}

/// Emits the block-parameter copies of an edge as a parallel move: push every (non-identity) source
/// local, then pop them into the destination locals in reverse. Reading all sources before writing
/// any destination makes it correct even when the moves form a cycle (a swap), with the operand
/// stack as the scratch space -- no temporary locals needed.
fn emit_parallel_copy(body: &mut Func, local_of: &[u32], args: &[ValueId], params: &[ValueId]) {
    let mut sources: Vec<u32> = Vec::new();
    let mut dests: Vec<u32> = Vec::new();
    for (param, arg) in params.iter().zip(args) {
        let dst = local_of[param.index()];
        let src = local_of[arg.index()];
        if dst != src {
            sources.push(src);
            dests.push(dst);
        }
    }
    for &src in &sources {
        body.local_get(src);
    }
    for &dst in dests.iter().rev() {
        body.local_set(dst);
    }
}

/// Lowers one value-defining instruction: it pushes its operands with `local.get`, emits the
/// operation, and stores the result into its local with `local.set` (a void side-effecting
/// instruction stores nothing).
fn lower_inst(
    body: &mut Func,
    local: &impl Fn(ValueId) -> u32,
    value_types: &[MirType],
    ctx: &WasmCtx,
    result: ValueId,
    inst: &Inst,
) -> Result<(), LowerError> {
    match inst {
        Inst::ConstInt { ty, value } => {
            match valtype(*ty)? {
                ValType::I32 => body.i32_const(*value as i32),
                ValType::I64 => body.i64_const(*value),
                ValType::F32 => body.f32_const_bits(*value as u32),
                ValType::F64 => body.f64_const_bits(*value as u64),
            }
            body.local_set(local(result));
        }
        Inst::Binary { op, lhs, rhs } => {
            body.local_get(local(*lhs));
            body.local_get(local(*rhs));
            emit_binary(
                body,
                value_types[lhs.index()],
                value_types[rhs.index()],
                *op,
            )?;
            body.local_set(local(result));
        }
        Inst::Compare { op, lhs, rhs } => {
            body.local_get(local(*lhs));
            body.local_get(local(*rhs));
            emit_compare(body, value_types[lhs.index()], *op)?;
            body.local_set(local(result));
        }
        Inst::Convert { value, kind } => {
            body.local_get(local(*value));
            emit_convert(body, *kind)?;
            body.local_set(local(result));
        }
        Inst::Widen { value, signed } => {
            body.local_get(local(*value));
            if *signed {
                body.i64_extend_i32_s();
            } else {
                body.i64_extend_i32_u();
            }
            body.local_set(local(result));
        }
        Inst::Truncate { value } => {
            body.local_get(local(*value));
            body.i32_wrap_i64();
            body.local_set(local(result));
        }
        Inst::Call { callee, args } => {
            for &arg in args {
                body.local_get(local(arg));
            }
            body.call(ctx.import_count + *callee);
            let returns_value = ctx
                .funcs
                .get(*callee as usize)
                .is_some_and(|f| f.ret.is_some());
            if returns_value {
                body.local_set(local(result));
            }
        }
        Inst::PInvoke { import, args } => {
            for &arg in args {
                body.local_get(local(arg));
            }
            let index = *ctx
                .native_imports
                .get(&**import)
                .ok_or(LowerError::Unsupported)?;
            body.call(index);
            body.local_set(local(result));
        }
        Inst::Load {
            address,
            width,
            signed,
        } => {
            body.local_get(local(*address));
            match (*width, *signed) {
                (1, true) => body.i32_load8_s(MemArg::new(1, 0)),
                (1, false) => body.i32_load8_u(MemArg::new(1, 0)),
                (2, true) => body.i32_load16_s(MemArg::new(2, 0)),
                (2, false) => body.i32_load16_u(MemArg::new(2, 0)),
                _ => body.i32_load(MemArg::new(4, 0)),
            }
            body.local_set(local(result));
        }
        Inst::Store {
            address,
            value,
            width,
        } => {
            body.local_get(local(*address));
            body.local_get(local(*value));
            match *width {
                1 => body.i32_store8(MemArg::new(1, 0)),
                2 => body.i32_store16(MemArg::new(2, 0)),
                _ => body.i32_store(MemArg::new(4, 0)),
            }
        }
        Inst::FieldLoad { base, offset } => {
            if !is_addressable(value_types, *base) {
                return Err(LowerError::Unsupported);
            }
            if let MirType::ValueType { size, .. } = value_types[result.index()] {
                emit_bump(body, size.next_multiple_of(8) as i32);
                body.local_set(local(result));
                for word in 0..size.div_ceil(4) {
                    body.local_get(local(result));
                    body.local_get(local(*base));
                    body.i32_load(MemArg::new(4, *offset + word * 4));
                    body.i32_store(MemArg::new(4, word * 4));
                }
            } else {
                body.local_get(local(*base));
                emit_typed_load(body, value_types[result.index()], *offset)?;
                body.local_set(local(result));
            }
        }
        Inst::FieldStore {
            base,
            offset,
            value,
        } => {
            if !is_addressable(value_types, *base) {
                return Err(LowerError::Unsupported);
            }
            if let MirType::ValueType { size, .. } = value_types[value.index()] {
                for word in 0..size.div_ceil(4) {
                    body.local_get(local(*base));
                    body.local_get(local(*value));
                    body.i32_load(MemArg::new(4, word * 4));
                    body.i32_store(MemArg::new(4, *offset + word * 4));
                }
            } else {
                body.local_get(local(*base));
                body.local_get(local(*value));
                emit_typed_store(body, value_types[value.index()], *offset)?;
            }
        }
        Inst::FieldLoadNarrow {
            base,
            offset,
            size,
            signed,
        } => {
            if !is_addressable(value_types, *base) {
                return Err(LowerError::Unsupported);
            }
            body.local_get(local(*base));
            match (*size, *signed) {
                (1, false) => body.i32_load8_u(MemArg::new(1, *offset)),
                (1, true) => body.i32_load8_s(MemArg::new(1, *offset)),
                (2, false) => body.i32_load16_u(MemArg::new(2, *offset)),
                (2, true) => body.i32_load16_s(MemArg::new(2, *offset)),
                _ => return Err(LowerError::Unsupported),
            }
            body.local_set(local(result));
        }
        Inst::FieldStoreNarrow {
            base,
            offset,
            value,
            size,
        } => {
            if !is_addressable(value_types, *base) {
                return Err(LowerError::Unsupported);
            }
            body.local_get(local(*base));
            body.local_get(local(*value));
            match *size {
                1 => body.i32_store8(MemArg::new(1, *offset)),
                2 => body.i32_store16(MemArg::new(2, *offset)),
                _ => return Err(LowerError::Unsupported),
            }
        }
        Inst::FieldAddr { base, offset } => {
            if !is_addressable(value_types, *base) {
                return Err(LowerError::Unsupported);
            }
            body.local_get(local(*base));
            if *offset != 0 {
                body.i32_const(*offset as i32);
                body.i32_add();
            }
            body.local_set(local(result));
        }
        Inst::Alloc {
            handle,
            payload_size,
            ..
        } => {
            let desc = ctx
                .desc_addr
                .get(&handle.0)
                .copied()
                .unwrap_or(handle.0 as i32);
            emit_bump(body, (4 + payload_size.next_multiple_of(8)) as i32);
            body.local_set(local(result));
            body.local_get(local(result));
            body.i32_const(desc);
            body.i32_store(MemArg::new(4, 0));
            body.local_get(local(result));
            body.i32_const(4);
            body.i32_add();
            body.local_set(local(result));
        }
        Inst::AllocLike {
            proto,
            payload_size,
        } => {
            emit_bump(body, (4 + payload_size.next_multiple_of(8)) as i32);
            body.local_set(local(result));
            body.local_get(local(result));
            body.local_get(local(*proto));
            body.i32_const(4);
            body.i32_sub();
            body.i32_load(MemArg::new(4, 0));
            body.i32_store(MemArg::new(4, 0));
            body.local_get(local(result));
            body.i32_const(4);
            body.i32_add();
            body.local_set(local(result));
        }
        Inst::TypeDescAddr { handle } => {
            let desc = ctx
                .desc_addr
                .get(&handle.0)
                .copied()
                .unwrap_or(handle.0 as i32);
            body.i32_const(desc);
            body.local_set(local(result));
        }
        Inst::LoadTypeDesc { object } => {
            body.local_get(local(*object));
            body.i32_eqz();
            body.if_(BlockType::Value(ValType::I32));
            body.i32_const(0);
            body.else_();
            body.local_get(local(*object));
            body.i32_const(4);
            body.i32_sub();
            body.i32_load(MemArg::new(4, 0));
            body.end();
            body.local_set(local(result));
        }
        Inst::InitStruct => {
            emit_bump(body, struct_size(value_types[result.index()])?);
            body.local_set(local(result));
        }
        Inst::CopyStruct { src } => {
            let size = struct_size(value_types[result.index()])?;
            emit_bump(body, size);
            body.local_set(local(result));
            for word in 0..(size as u32).div_ceil(4) {
                let offset = word * 4;
                body.local_get(local(result));
                body.local_get(local(*src));
                body.i32_load(MemArg::new(4, offset));
                body.i32_store(MemArg::new(4, offset));
            }
        }
        Inst::AllocArray {
            length,
            element_size,
            ..
        } => {
            body.local_get(local(*length));
            body.i32_const(*element_size as i32);
            body.i32_mul();
            body.i32_const(4);
            body.i32_add();
            body.i32_const(7);
            body.i32_add();
            body.i32_const(!7);
            body.i32_and();
            body.global_get(HEAP_POINTER);
            body.local_tee(local(result));
            body.i32_add();
            body.global_set(HEAP_POINTER);
            body.local_get(local(result));
            body.local_get(local(*length));
            body.i32_store(MemArg::new(4, 0));
        }
        Inst::ArrayLoad {
            array,
            index,
            element_size,
            signed,
        } => {
            emit_bounds_check(body, local, *array, *index);
            emit_element_address(body, local, *array, *index, *element_size);
            emit_array_load(body, *element_size, *signed, value_types[result.index()])?;
            body.local_set(local(result));
        }
        Inst::ArrayStore {
            array,
            index,
            value,
            element_size,
        } => {
            emit_bounds_check(body, local, *array, *index);
            emit_element_address(body, local, *array, *index, *element_size);
            body.local_get(local(*value));
            emit_array_store(body, *element_size, value_types[value.index()])?;
        }
        Inst::StaticLoad { owner, offset } => {
            if !matches!(owner, lamella_ir::StaticOwner::Own) {
                return Err(LowerError::Unsupported);
            }
            body.i32_const(STATIC_BASE);
            emit_typed_load(body, value_types[result.index()], *offset)?;
            body.local_set(local(result));
        }
        Inst::StaticStore {
            owner,
            offset,
            value,
        } => {
            if !matches!(owner, lamella_ir::StaticOwner::Own) {
                return Err(LowerError::Unsupported);
            }
            body.i32_const(STATIC_BASE);
            body.local_get(local(*value));
            emit_typed_store(body, value_types[value.index()], *offset)?;
        }
        Inst::AllocArray2D {
            dim0,
            dim1,
            element_size,
            ..
        } => {
            body.local_get(local(*dim0));
            body.local_get(local(*dim1));
            body.i32_mul();
            body.i32_const(*element_size as i32);
            body.i32_mul();
            body.i32_const(8);
            body.i32_add();
            body.i32_const(7);
            body.i32_add();
            body.i32_const(!7);
            body.i32_and();
            body.global_get(HEAP_POINTER);
            body.local_tee(local(result));
            body.i32_add();
            body.global_set(HEAP_POINTER);
            body.local_get(local(result));
            body.local_get(local(*dim0));
            body.i32_store(MemArg::new(4, 0));
            body.local_get(local(result));
            body.local_get(local(*dim1));
            body.i32_store(MemArg::new(4, 4));
        }
        Inst::Array2DLoad {
            array,
            index0,
            index1,
            element_size,
            signed,
        } => {
            emit_2d_element_address(body, local, *array, *index0, *index1, *element_size);
            emit_array_load(body, *element_size, *signed, value_types[result.index()])?;
            body.local_set(local(result));
        }
        Inst::Array2DStore {
            array,
            index0,
            index1,
            value,
            element_size,
        } => {
            emit_2d_element_address(body, local, *array, *index0, *index1, *element_size);
            body.local_get(local(*value));
            emit_array_store(body, *element_size, value_types[value.index()])?;
        }
        Inst::FuncAddr { func } => {
            body.i32_const((ctx.import_count + *func) as i32);
            body.local_set(local(result));
        }
        Inst::VirtualFuncAddr { object, slot } => {
            let offset = i32::try_from(4 + slot * 4).map_err(|_| LowerError::Unsupported)?;
            body.local_get(local(*object));
            body.i32_const(4);
            body.i32_sub();
            body.i32_load(MemArg::new(4, 0));
            body.i32_const(offset);
            body.i32_sub();
            body.i32_load(MemArg::new(4, 0));
            body.local_set(local(result));
        }
        Inst::CallVirtual {
            slot,
            args,
            returns_value,
        } => {
            let receiver = *args.first().ok_or(LowerError::Unsupported)?;
            let offset = i32::try_from(4 + slot * 4).map_err(|_| LowerError::Unsupported)?;
            let result_ty = call_result_ty(*returns_value, result, value_types);
            let sig = call_site_type(args, result_ty, value_types)?;
            let type_index = ctx.indirect_type(&sig)?;
            for &arg in args.iter() {
                body.local_get(local(arg));
            }
            body.local_get(local(receiver));
            body.i32_const(4);
            body.i32_sub();
            body.i32_load(MemArg::new(4, 0));
            body.i32_const(offset);
            body.i32_sub();
            body.i32_load(MemArg::new(4, 0));
            body.call_indirect(type_index, 0);
            if *returns_value {
                body.local_set(local(result));
            }
        }
        Inst::InterfaceHasTag { descriptor, tag } => {
            let ptr = body.add_local(ValType::I32);
            let count = body.add_local(ValType::I32);
            let present = body.add_local(ValType::I32);
            body.i32_const(0);
            body.local_set(present);
            body.local_get(local(*descriptor));
            body.if_(BlockType::Empty);
            body.local_get(local(*descriptor));
            body.i32_load(MemArg::new(4, 8));
            body.local_set(count);
            body.local_get(local(*descriptor));
            body.i32_const(12);
            body.i32_add();
            body.local_set(ptr);
            body.loop_(BlockType::Empty);
            body.local_get(count);
            body.i32_eqz();
            body.if_(BlockType::Empty);
            body.else_();
            body.local_get(ptr);
            body.i32_load(MemArg::new(4, 0));
            body.i32_const(*tag as i32);
            body.i32_eq();
            body.if_(BlockType::Empty);
            body.i32_const(1);
            body.local_set(present);
            body.else_();
            body.local_get(ptr);
            body.i32_const(8);
            body.i32_add();
            body.local_set(ptr);
            body.local_get(count);
            body.i32_const(1);
            body.i32_sub();
            body.local_set(count);
            body.br(3);
            body.end();
            body.end();
            body.end();
            body.end();
            body.local_get(present);
            body.local_set(local(result));
        }
        Inst::CallInterface {
            tag,
            args,
            returns_value,
        } => {
            let receiver = *args.first().ok_or(LowerError::Unsupported)?;
            let result_ty = call_result_ty(*returns_value, result, value_types);
            let sig = call_site_type(args, result_ty, value_types)?;
            let type_index = ctx.indirect_type(&sig)?;
            let desc = body.add_local(ValType::I32);
            let ptr = body.add_local(ValType::I32);
            let count = body.add_local(ValType::I32);
            let found = body.add_local(ValType::I32);
            for &arg in args.iter() {
                body.local_get(local(arg));
            }
            body.local_get(local(receiver));
            body.i32_const(4);
            body.i32_sub();
            body.i32_load(MemArg::new(4, 0));
            body.local_set(desc);
            body.local_get(desc);
            body.i32_load(MemArg::new(4, 8));
            body.local_set(count);
            body.local_get(desc);
            body.i32_const(12);
            body.i32_add();
            body.local_set(ptr);
            body.loop_(BlockType::Empty);
            body.local_get(count);
            body.i32_eqz();
            body.if_(BlockType::Empty);
            body.unreachable();
            body.end();
            body.local_get(ptr);
            body.i32_load(MemArg::new(4, 0));
            body.i32_const(*tag as i32);
            body.i32_eq();
            body.if_(BlockType::Empty);
            body.local_get(ptr);
            body.i32_load(MemArg::new(4, 4));
            body.local_set(found);
            body.else_();
            body.local_get(ptr);
            body.i32_const(8);
            body.i32_add();
            body.local_set(ptr);
            body.local_get(count);
            body.i32_const(1);
            body.i32_sub();
            body.local_set(count);
            body.br(1);
            body.end();
            body.end();
            body.local_get(found);
            body.call_indirect(type_index, 0);
            if *returns_value {
                body.local_set(local(result));
            }
        }
        Inst::CallIndirect {
            target,
            args,
            returns_value,
        } => {
            let result_ty = call_result_ty(*returns_value, result, value_types);
            let sig = call_site_type(args, result_ty, value_types)?;
            let type_index = ctx.indirect_type(&sig)?;
            for &arg in args.iter() {
                body.local_get(local(arg));
            }
            body.local_get(local(*target));
            body.call_indirect(type_index, 0);
            if *returns_value {
                body.local_set(local(result));
            }
        }
        Inst::InvokeDelegate {
            delegate,
            args,
            returns_value,
        } => {
            let result_ty = call_result_ty(*returns_value, result, value_types);
            let without = call_site_type(args, result_ty, value_types)?;
            let mut with = without.clone();
            with.params.insert(0, ValType::I32);
            let without_idx = ctx.indirect_type(&without)?;
            let with_idx = ctx.indirect_type(&with)?;
            let bt = match result_ty {
                Some(t) => BlockType::Value(valtype(t)?),
                None => BlockType::Empty,
            };
            let arg_locals: Vec<u32> = args.iter().map(|&a| local(a)).collect();
            let del = body.add_local(ValType::I32);
            let index = body.add_local(ValType::I32);
            let last = match result_ty {
                Some(t) => Some(body.add_local(valtype(t)?)),
                None => None,
            };
            body.i32_const(0);
            body.local_set(index);
            body.local_get(local(*delegate));
            body.i32_load(MemArg::new(4, 8));
            body.if_(BlockType::Empty);
            body.block(BlockType::Empty);
            body.loop_(BlockType::Empty);
            body.local_get(index);
            body.local_get(local(*delegate));
            body.i32_load(MemArg::new(4, 8));
            body.i32_load(MemArg::new(4, 0));
            body.i32_ge_u();
            body.br_if(1);
            body.local_get(local(*delegate));
            body.i32_load(MemArg::new(4, 8));
            body.i32_const(4);
            body.i32_add();
            body.local_get(index);
            body.i32_const(4);
            body.i32_mul();
            body.i32_add();
            body.i32_load(MemArg::new(4, 0));
            body.local_set(del);
            emit_delegate_dispatch(body, del, &arg_locals, bt, with_idx, without_idx);
            if let Some(l) = last {
                body.local_set(l);
            }
            body.local_get(index);
            body.i32_const(1);
            body.i32_add();
            body.local_set(index);
            body.br(0);
            body.end();
            body.end();
            body.else_();
            body.local_get(local(*delegate));
            body.local_set(del);
            emit_delegate_dispatch(body, del, &arg_locals, bt, with_idx, without_idx);
            if let Some(l) = last {
                body.local_set(l);
            }
            body.end();
            if let Some(l) = last {
                body.local_get(l);
                body.local_set(local(result));
            }
        }
        Inst::CastClassScan { args } => {
            let start = *args.first().ok_or(LowerError::Unsupported)?;
            let target = *args.get(1).ok_or(LowerError::Unsupported)?;
            let cur = body.add_local(ValType::I32);
            let res = body.add_local(ValType::I32);
            body.local_get(local(start));
            body.local_set(cur);
            body.i32_const(0);
            body.local_set(res);
            body.local_get(cur);
            body.if_(BlockType::Empty);
            body.loop_(BlockType::Empty);
            body.local_get(cur);
            body.local_get(local(target));
            body.i32_eq();
            body.if_(BlockType::Empty);
            body.i32_const(1);
            body.local_set(res);
            body.else_();
            body.local_get(cur);
            body.i32_load(MemArg::new(4, 4));
            body.local_set(cur);
            body.local_get(cur);
            body.i32_eqz();
            body.if_(BlockType::Empty);
            body.i32_const(0);
            body.local_set(res);
            body.else_();
            body.br(2);
            body.end();
            body.end();
            body.end();
            body.end();
            body.local_get(res);
            body.local_set(local(result));
        }
        Inst::CopyBlock { dst, src, size } => {
            body.local_get(local(*dst));
            body.local_get(local(*src));
            body.local_get(local(*size));
            body.memory_copy();
        }
        Inst::FillBlock { dst, value, size } => {
            body.local_get(local(*dst));
            body.local_get(local(*value));
            body.local_get(local(*size));
            body.memory_fill();
        }
        Inst::ArrayElemAddr {
            array,
            index,
            element_size,
        } => {
            body.local_get(local(*array));
            body.i32_const(4);
            body.i32_add();
            body.local_get(local(*index));
            body.i32_const(*element_size as i32);
            body.i32_mul();
            body.i32_add();
            body.local_set(local(result));
        }
        Inst::AllocArrayMD {
            dims,
            element_size,
            ..
        } => {
            body.local_get(local(dims[0]));
            for d in &dims[1..] {
                body.local_get(local(*d));
                body.i32_mul();
            }
            body.i32_const(*element_size as i32);
            body.i32_mul();
            body.i32_const(4 * dims.len() as i32);
            body.i32_add();
            body.i32_const(7);
            body.i32_add();
            body.i32_const(!7);
            body.i32_and();
            body.global_get(HEAP_POINTER);
            body.local_tee(local(result));
            body.i32_add();
            body.global_set(HEAP_POINTER);
            for (k, d) in dims.iter().enumerate() {
                body.local_get(local(result));
                body.local_get(local(*d));
                body.i32_store(MemArg::new(4, (4 * k) as u32));
            }
        }
        Inst::ArrayMDLoad {
            array,
            indices,
            element_size,
            signed,
        } => {
            emit_md_element_address(body, local, *array, indices, *element_size);
            emit_array_load(body, *element_size, *signed, value_types[result.index()])?;
            body.local_set(local(result));
        }
        Inst::ArrayMDStore {
            array,
            indices,
            value,
            element_size,
        } => {
            emit_md_element_address(body, local, *array, indices, *element_size);
            body.local_get(local(*value));
            emit_array_store(body, *element_size, value_types[value.index()])?;
        }
        _ => return Err(LowerError::Unsupported),
    }
    Ok(())
}

/// Per-dimension bounds-checks `(index0, index1)` against the dimensions at `[array+0]` / `[array+4]`
/// (trapping out of range), then pushes the element address `array + 8 + (index0*dim1 + index1)*size`
/// (row-major, the two dimension words skipped).
fn emit_2d_element_address(
    body: &mut Func,
    local: &impl Fn(ValueId) -> u32,
    array: ValueId,
    index0: ValueId,
    index1: ValueId,
    element_size: u32,
) {
    body.local_get(local(index0));
    body.local_get(local(array));
    body.i32_load(MemArg::new(4, 0));
    body.i32_ge_u();
    body.if_(BlockType::Empty);
    body.unreachable();
    body.end();
    body.local_get(local(index1));
    body.local_get(local(array));
    body.i32_load(MemArg::new(4, 4));
    body.i32_ge_u();
    body.if_(BlockType::Empty);
    body.unreachable();
    body.end();
    body.local_get(local(array));
    body.i32_const(8);
    body.i32_add();
    body.local_get(local(index0));
    body.local_get(local(array));
    body.i32_load(MemArg::new(4, 4));
    body.i32_mul();
    body.local_get(local(index1));
    body.i32_add();
    body.i32_const(element_size as i32);
    body.i32_mul();
    body.i32_add();
}

/// Leaves the address of rank-N element `(indices[0..N])` on the stack: bounds-check each index against
/// its dimension word `[array + 4*k]` (unsigned; `unreachable` on failure), then compute `array + 4*N +
/// flat*element_size` where the flat index is the Horner fold `((..(i0*dim1 + i1)*dim2 + i2)..) +
/// i(N-1)`. The stack-machine form nests naturally: each step multiplies the running flat by the next
/// dimension and adds the next index. Generalizes `emit_2d_element_address` from 2 dimensions to N.
fn emit_md_element_address(
    body: &mut Func,
    local: &impl Fn(ValueId) -> u32,
    array: ValueId,
    indices: &[ValueId],
    element_size: u32,
) {
    let n = indices.len();
    for (k, &idx) in indices.iter().enumerate() {
        body.local_get(local(idx));
        body.local_get(local(array));
        body.i32_load(MemArg::new(4, (4 * k) as u32));
        body.i32_ge_u();
        body.if_(BlockType::Empty);
        body.unreachable();
        body.end();
    }
    body.local_get(local(array));
    body.i32_const(4 * n as i32);
    body.i32_add();
    body.local_get(local(indices[0]));
    for (k, &idx) in indices.iter().enumerate().skip(1) {
        body.local_get(local(array));
        body.i32_load(MemArg::new(4, (4 * k) as u32));
        body.i32_mul();
        body.local_get(local(idx));
        body.i32_add();
    }
    body.i32_const(element_size as i32);
    body.i32_mul();
    body.i32_add();
}

/// Emits the dispatch of ONE delegate held in `del` (a linear-memory address): read `_target@0` -- a
/// non-null target is the instance receiver (pushed as arg0 ahead of the explicit args), a null target
/// is a static method -- then `call_indirect` the `_methodPtr@4` funcref table index against the matching
/// signature. Shared by the single-cast and multicast (per-element) paths of `InvokeDelegate`.
fn emit_delegate_dispatch(
    body: &mut Func,
    del: u32,
    arg_locals: &[u32],
    bt: BlockType,
    with_idx: u32,
    without_idx: u32,
) {
    body.local_get(del);
    body.i32_load(MemArg::new(4, 0));
    body.if_(bt);
    body.local_get(del);
    body.i32_load(MemArg::new(4, 0));
    for &a in arg_locals {
        body.local_get(a);
    }
    body.local_get(del);
    body.i32_load(MemArg::new(4, 4));
    body.call_indirect(with_idx, 0);
    body.else_();
    for &a in arg_locals {
        body.local_get(a);
    }
    body.local_get(del);
    body.i32_load(MemArg::new(4, 4));
    body.call_indirect(without_idx, 0);
    body.end();
}

/// Whether `value`'s local holds a linear-memory address that a field access can dereference: a heap
/// `ObjectRef`, a managed pointer, or a value-type instance (its local is the address of its slot).
fn is_addressable(value_types: &[MirType], value: ValueId) -> bool {
    matches!(
        value_types.get(value.index()),
        Some(MirType::ObjectRef | MirType::ManagedPtr | MirType::ValueType { .. })
    )
}

/// Emits an inline bump allocation of `size` bytes, leaving the allocated address on the stack: read
/// the heap pointer (the result), advance it by `size`, write it back.
fn emit_bump(body: &mut Func, size: i32) {
    body.global_get(HEAP_POINTER);
    body.global_get(HEAP_POINTER);
    body.i32_const(size);
    body.i32_add();
    body.global_set(HEAP_POINTER);
}

/// The slot size of a value type, rounded up to 8 bytes (the bump-allocator alignment), or
/// `Unsupported` for a non-value-type.
fn struct_size(ty: MirType) -> Result<i32, LowerError> {
    match ty {
        MirType::ValueType { size, .. } => Ok(size.next_multiple_of(8) as i32),
        _ => Err(LowerError::Unsupported),
    }
}

/// Loads a scalar of MIR type `ty` from the address on the stack, at static `offset`.
fn emit_typed_load(body: &mut Func, ty: MirType, offset: u32) -> Result<(), LowerError> {
    match ty {
        MirType::I32
        | MirType::NativeInt
        | MirType::ObjectRef
        | MirType::ManagedPtr
        | MirType::PyValue => {
            body.i32_load(MemArg::new(4, offset));
        }
        MirType::I64 => body.i64_load(MemArg::new(8, offset)),
        MirType::F32 => body.f32_load(MemArg::new(4, offset)),
        MirType::F64 => body.f64_load(MemArg::new(8, offset)),
        MirType::ValueType { .. } => return Err(LowerError::Unsupported),
    }
    Ok(())
}

/// Stores the scalar of MIR type `ty` on the stack (under its address) at static `offset`.
fn emit_typed_store(body: &mut Func, ty: MirType, offset: u32) -> Result<(), LowerError> {
    match ty {
        MirType::I32
        | MirType::NativeInt
        | MirType::ObjectRef
        | MirType::ManagedPtr
        | MirType::PyValue => {
            body.i32_store(MemArg::new(4, offset));
        }
        MirType::I64 => body.i64_store(MemArg::new(8, offset)),
        MirType::F32 => body.f32_store(MemArg::new(4, offset)),
        MirType::F64 => body.f64_store(MemArg::new(8, offset)),
        MirType::ValueType { .. } => return Err(LowerError::Unsupported),
    }
    Ok(())
}

/// Loads an array element of `element_size` bytes from the address on the stack, sign- or
/// zero-extending a sub-word element per `signed`; the 4- and 8-byte widths pick the float load when
/// the result type is a float.
fn emit_array_load(
    body: &mut Func,
    element_size: u32,
    signed: bool,
    result_ty: MirType,
) -> Result<(), LowerError> {
    let m = MemArg::new(element_size, 0);
    match element_size {
        1 if signed => body.i32_load8_s(m),
        1 => body.i32_load8_u(m),
        2 if signed => body.i32_load16_s(m),
        2 => body.i32_load16_u(m),
        4 if matches!(result_ty, MirType::F32) => body.f32_load(m),
        4 => body.i32_load(m),
        8 if matches!(result_ty, MirType::F64) => body.f64_load(m),
        8 => body.i64_load(m),
        _ => return Err(LowerError::Unsupported),
    }
    Ok(())
}

/// Stores an array element of `element_size` bytes (the value on the stack, under its address); the
/// 4- and 8-byte widths pick the float store when the value type is a float.
fn emit_array_store(
    body: &mut Func,
    element_size: u32,
    value_ty: MirType,
) -> Result<(), LowerError> {
    let m = MemArg::new(element_size, 0);
    match element_size {
        1 => body.i32_store8(m),
        2 => body.i32_store16(m),
        4 if matches!(value_ty, MirType::F32) => body.f32_store(m),
        4 => body.i32_store(m),
        8 if matches!(value_ty, MirType::F64) => body.f64_store(m),
        8 => body.i64_store(m),
        _ => return Err(LowerError::Unsupported),
    }
    Ok(())
}

/// Traps (`unreachable`) unless `index < length`, the length read from `[array + 0]`. The compare is
/// unsigned, so a negative index (a huge unsigned value) traps too -- matching IndexOutOfRange.
fn emit_bounds_check(
    body: &mut Func,
    local: &impl Fn(ValueId) -> u32,
    array: ValueId,
    index: ValueId,
) {
    body.local_get(local(index));
    body.local_get(local(array));
    body.i32_load(MemArg::new(4, 0));
    body.i32_ge_u();
    body.if_(BlockType::Empty);
    body.unreachable();
    body.end();
}

/// Pushes the address of element `index` of `array`: `array + 4 + index*element_size` (the +4 skips
/// the length word).
fn emit_element_address(
    body: &mut Func,
    local: &impl Fn(ValueId) -> u32,
    array: ValueId,
    index: ValueId,
    element_size: u32,
) {
    body.local_get(local(array));
    body.i32_const(4);
    body.i32_add();
    body.local_get(local(index));
    body.i32_const(element_size as i32);
    body.i32_mul();
    body.i32_add();
}

/// Emits a binary operator over operands of value type `val_ty` (the result and left-operand type;
/// `count_ty` is the right operand's type, which a shift count may have narrower). Integer only for
/// now -- WebAssembly has native float arithmetic, but it is deferred with the rest of the float
/// path.
fn emit_binary(
    body: &mut Func,
    val_ty: MirType,
    count_ty: MirType,
    op: BinOp,
) -> Result<(), LowerError> {
    if val_ty.is_float() {
        let is64 = matches!(val_ty, MirType::F64);
        match op {
            BinOp::Add => bin(body, is64, Func::f32_add, Func::f64_add),
            BinOp::Sub => bin(body, is64, Func::f32_sub, Func::f64_sub),
            BinOp::Mul => bin(body, is64, Func::f32_mul, Func::f64_mul),
            BinOp::DivSigned | BinOp::DivUnsigned => bin(body, is64, Func::f32_div, Func::f64_div),
            _ => return Err(LowerError::Unsupported),
        }
        return Ok(());
    }
    if !val_ty.is_integer() {
        return Err(LowerError::Unsupported);
    }
    let is64 = matches!(val_ty, MirType::I64);
    match op {
        BinOp::Add => bin(body, is64, Func::i32_add, Func::i64_add),
        BinOp::Sub => bin(body, is64, Func::i32_sub, Func::i64_sub),
        BinOp::Mul => bin(body, is64, Func::i32_mul, Func::i64_mul),
        BinOp::And => bin(body, is64, Func::i32_and, Func::i64_and),
        BinOp::Or => bin(body, is64, Func::i32_or, Func::i64_or),
        BinOp::Xor => bin(body, is64, Func::i32_xor, Func::i64_xor),
        BinOp::Shl | BinOp::ShrSigned | BinOp::ShrUnsigned => {
            if is64 && matches!(count_ty, MirType::I32 | MirType::NativeInt) {
                body.i64_extend_i32_u();
            }
            match op {
                BinOp::Shl => bin(body, is64, Func::i32_shl, Func::i64_shl),
                BinOp::ShrSigned => bin(body, is64, Func::i32_shr_s, Func::i64_shr_s),
                BinOp::ShrUnsigned => bin(body, is64, Func::i32_shr_u, Func::i64_shr_u),
                _ => unreachable!(),
            }
        }
        BinOp::DivSigned => bin(body, is64, Func::i32_div_s, Func::i64_div_s),
        BinOp::DivUnsigned => bin(body, is64, Func::i32_div_u, Func::i64_div_u),
        BinOp::RemSigned => bin(body, is64, Func::i32_rem_s, Func::i64_rem_s),
        BinOp::RemUnsigned => bin(body, is64, Func::i32_rem_u, Func::i64_rem_u),
    }
    Ok(())
}

/// Emits the i32-vs-i64 form of an operator: `wide` when the operands are 64-bit, `narrow`
/// otherwise.
fn bin(body: &mut Func, is64: bool, narrow: fn(&mut Func), wide: fn(&mut Func)) {
    if is64 {
        wide(body);
    } else {
        narrow(body);
    }
}

/// Emits a comparison over operands of value type `ty`, leaving a 0/1 i32 on the stack.
fn emit_compare(body: &mut Func, ty: MirType, op: CmpOp) -> Result<(), LowerError> {
    if ty.is_float() {
        let is64 = matches!(ty, MirType::F64);
        match op {
            CmpOp::Eq => bin(body, is64, Func::f32_eq, Func::f64_eq),
            CmpOp::Ne => bin(body, is64, Func::f32_ne, Func::f64_ne),
            CmpOp::SignedLt => bin(body, is64, Func::f32_lt, Func::f64_lt),
            CmpOp::SignedGt => bin(body, is64, Func::f32_gt, Func::f64_gt),
            CmpOp::SignedLe => bin(body, is64, Func::f32_le, Func::f64_le),
            CmpOp::SignedGe => bin(body, is64, Func::f32_ge, Func::f64_ge),
            CmpOp::UnsignedLt => {
                bin(body, is64, Func::f32_ge, Func::f64_ge);
                body.i32_eqz();
            }
            CmpOp::UnsignedGt => {
                bin(body, is64, Func::f32_le, Func::f64_le);
                body.i32_eqz();
            }
            CmpOp::UnsignedLe => {
                bin(body, is64, Func::f32_gt, Func::f64_gt);
                body.i32_eqz();
            }
            CmpOp::UnsignedGe => {
                bin(body, is64, Func::f32_lt, Func::f64_lt);
                body.i32_eqz();
            }
        }
        return Ok(());
    }
    if matches!(ty, MirType::ValueType { .. }) {
        return Err(LowerError::Unsupported);
    }
    let is64 = matches!(ty, MirType::I64);
    match op {
        CmpOp::Eq => bin(body, is64, Func::i32_eq, Func::i64_eq),
        CmpOp::Ne => bin(body, is64, Func::i32_ne, Func::i64_ne),
        CmpOp::SignedLt => bin(body, is64, Func::i32_lt_s, Func::i64_lt_s),
        CmpOp::SignedLe => bin(body, is64, Func::i32_le_s, Func::i64_le_s),
        CmpOp::SignedGt => bin(body, is64, Func::i32_gt_s, Func::i64_gt_s),
        CmpOp::SignedGe => bin(body, is64, Func::i32_ge_s, Func::i64_ge_s),
        CmpOp::UnsignedLt => bin(body, is64, Func::i32_lt_u, Func::i64_lt_u),
        CmpOp::UnsignedLe => bin(body, is64, Func::i32_le_u, Func::i64_le_u),
        CmpOp::UnsignedGt => bin(body, is64, Func::i32_gt_u, Func::i64_gt_u),
        CmpOp::UnsignedGe => bin(body, is64, Func::i32_ge_u, Func::i64_ge_u),
    }
    Ok(())
}

/// Emits a width conversion. The sub-word integer narrowings are synthesized from shifts/masks so
/// they stay within the WASM 1.0 (MVP) instruction set; the float conversions are deferred with the
/// rest of the float path.
fn emit_convert(body: &mut Func, kind: ConvKind) -> Result<(), LowerError> {
    match kind {
        ConvKind::SignExtend8 => sign_extend(body, 24),
        ConvKind::SignExtend16 => sign_extend(body, 16),
        ConvKind::ZeroExtend8 => {
            body.i32_const(0xFF);
            body.i32_and();
        }
        ConvKind::ZeroExtend16 => {
            body.i32_const(0xFFFF);
            body.i32_and();
        }
        ConvKind::Float32ToInt => body.i32_trunc_f32_s(),
        ConvKind::IntToFloat32 => body.f32_convert_i32_s(),
        ConvKind::Float64ToInt => body.i32_trunc_f64_s(),
        ConvKind::IntToFloat64 => body.f64_convert_i32_s(),
        ConvKind::LongToFloat64 => body.f64_convert_i64_s(),
        ConvKind::Float32ToFloat64 => body.f64_promote_f32(),
        ConvKind::Float64ToFloat32 => body.f32_demote_f64(),
        ConvKind::LongToFloat32 => body.f32_convert_i64_s(),
        ConvKind::UIntToFloat64 => body.f64_convert_i32_u(),
        ConvKind::ULongToFloat64 => body.f64_convert_i64_u(),
        ConvKind::IntToRef | ConvKind::RefToInt => {}
    }
    Ok(())
}

/// Sign-extends the low bits of the i32 on the stack by shifting them up to the sign bit and back
/// with an arithmetic right shift (`shift` is `32 - width`).
fn sign_extend(body: &mut Func, shift: i32) {
    body.i32_const(shift);
    body.i32_shl();
    body.i32_const(shift);
    body.i32_shr_s();
}

/// Maps a MIR value type to a WebAssembly value type. The reference types and `native int` are
/// 32-bit on wasm32 (a linear-memory address/index); a value-type instance is likewise an i32 -- the
/// address of its bytes in linear memory (the slot `Field*` dereferences and `InitStruct`/
/// `CopyStruct` allocate). The 64-bit scalars and floats map to their own value types.
fn valtype(ty: MirType) -> Result<ValType, LowerError> {
    Ok(match ty {
        MirType::I32
        | MirType::NativeInt
        | MirType::ObjectRef
        | MirType::ManagedPtr
        | MirType::PyValue
        | MirType::ValueType { .. } => ValType::I32,
        MirType::I64 => ValType::I64,
        MirType::F32 => ValType::F32,
        MirType::F64 => ValType::F64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_ir::{BasicBlock, BlockId};

    /// `fn() -> i32 { 40 + 2 }` -- the first-milestone straight-line function.
    fn add_constants() -> Function {
        let i32t = MirType::I32;
        Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: alloc::vec![i32t, i32t, i32t],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: Vec::new(),
                insts: alloc::vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: i32t,
                            value: 40,
                        },
                    ),
                    (ValueId(1), Inst::ConstInt { ty: i32t, value: 2 }),
                    (
                        ValueId(2),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(0),
                            rhs: ValueId(1),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        }
    }

    #[test]
    fn lowers_the_add_module_to_a_valid_header() {
        let bytes = lower(&add_constants()).expect("40 + 2 lowers to WASM");
        assert_eq!(
            &bytes[0..8],
            &[0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00]
        );
        assert!(bytes.len() > 16);
    }

    #[test]
    fn lowering_is_deterministic() {
        let func = add_constants();
        assert_eq!(lower(&func), lower(&func));
    }

    /// `fn() -> i32 { Add(40, 2) }` calling `fn Add(i32, i32) -> i32 { a + b }` -- a straight-line
    /// two-function module exercising the call path and the parameter-to-local mapping.
    #[test]
    fn lowers_a_straight_line_call() {
        let i32t = MirType::I32;
        let main = Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: alloc::vec![i32t, i32t, i32t],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: Vec::new(),
                insts: alloc::vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: i32t,
                            value: 40,
                        },
                    ),
                    (ValueId(1), Inst::ConstInt { ty: i32t, value: 2 }),
                    (
                        ValueId(2),
                        Inst::Call {
                            callee: 1,
                            args: alloc::vec![ValueId(0), ValueId(1)],
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        let add = Function {
            params: alloc::vec![i32t, i32t],
            ret: Some(i32t),
            value_types: alloc::vec![i32t, i32t, i32t],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: alloc::vec![ValueId(0), ValueId(1)],
                insts: alloc::vec![(
                    ValueId(2),
                    Inst::Binary {
                        op: BinOp::Add,
                        lhs: ValueId(0),
                        rhs: ValueId(1),
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        let bytes = lower_module(&[main, add]).expect("a module with a call lowers");
        assert_eq!(&bytes[0..4], &[0x00, 0x61, 0x73, 0x6D]);
    }

    /// A counting loop summing `i` for `i` in 1..=5 -> 15: block 0 sets up, block 1 (the loop header)
    /// compares the counter to the limit, block 2 (the body) accumulates and jumps back carrying the
    /// merge-block parameters, block 3 returns. Exercises a `loop` scope, an `if`/`else`, a back-edge
    /// `br`, and block-parameter copies.
    fn loop_sum() -> Function {
        let i32t = MirType::I32;
        Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: alloc::vec![i32t; 8],
            entry: BlockId(0),
            blocks: alloc::vec![
                BasicBlock {
                    params: Vec::new(),
                    insts: alloc::vec![
                        (ValueId(0), Inst::ConstInt { ty: i32t, value: 0 }),
                        (ValueId(1), Inst::ConstInt { ty: i32t, value: 1 }),
                        (ValueId(2), Inst::ConstInt { ty: i32t, value: 5 }),
                    ],
                    terminator: Some(Terminator::Jump {
                        target: BlockId(1),
                        args: alloc::vec![ValueId(0), ValueId(1)],
                    }),
                },
                BasicBlock {
                    params: alloc::vec![ValueId(3), ValueId(4)],
                    insts: alloc::vec![(
                        ValueId(5),
                        Inst::Compare {
                            op: CmpOp::SignedGt,
                            lhs: ValueId(4),
                            rhs: ValueId(2),
                        },
                    )],
                    terminator: Some(Terminator::Branch {
                        cond: ValueId(5),
                        if_true: BlockId(3),
                        true_args: Vec::new(),
                        if_false: BlockId(2),
                        false_args: Vec::new(),
                    }),
                },
                BasicBlock {
                    params: Vec::new(),
                    insts: alloc::vec![
                        (
                            ValueId(6),
                            Inst::Binary {
                                op: BinOp::Add,
                                lhs: ValueId(3),
                                rhs: ValueId(4),
                            },
                        ),
                        (
                            ValueId(7),
                            Inst::Binary {
                                op: BinOp::Add,
                                lhs: ValueId(4),
                                rhs: ValueId(1),
                            },
                        ),
                    ],
                    terminator: Some(Terminator::Jump {
                        target: BlockId(1),
                        args: alloc::vec![ValueId(6), ValueId(7)],
                    }),
                },
                BasicBlock {
                    params: Vec::new(),
                    insts: Vec::new(),
                    terminator: Some(Terminator::Return(Some(ValueId(3)))),
                },
            ],
        }
    }

    /// A diamond `if`/`else` whose arms feed a merge block via its parameter: `x = cond ? 42 : 0;
    /// return x;`. Exercises a merge node (a `block` scope branched to from both arms) and the
    /// parameter copy on each edge.
    fn if_else_merge() -> Function {
        let i32t = MirType::I32;
        Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: alloc::vec![i32t; 4],
            entry: BlockId(0),
            blocks: alloc::vec![
                BasicBlock {
                    params: Vec::new(),
                    insts: alloc::vec![(ValueId(0), Inst::ConstInt { ty: i32t, value: 1 })],
                    terminator: Some(Terminator::Branch {
                        cond: ValueId(0),
                        if_true: BlockId(1),
                        true_args: Vec::new(),
                        if_false: BlockId(2),
                        false_args: Vec::new(),
                    }),
                },
                BasicBlock {
                    params: Vec::new(),
                    insts: alloc::vec![(
                        ValueId(1),
                        Inst::ConstInt {
                            ty: i32t,
                            value: 42,
                        },
                    )],
                    terminator: Some(Terminator::Jump {
                        target: BlockId(3),
                        args: alloc::vec![ValueId(1)],
                    }),
                },
                BasicBlock {
                    params: Vec::new(),
                    insts: alloc::vec![(ValueId(2), Inst::ConstInt { ty: i32t, value: 0 })],
                    terminator: Some(Terminator::Jump {
                        target: BlockId(3),
                        args: alloc::vec![ValueId(2)],
                    }),
                },
                BasicBlock {
                    params: alloc::vec![ValueId(3)],
                    insts: Vec::new(),
                    terminator: Some(Terminator::Return(Some(ValueId(3)))),
                },
            ],
        }
    }

    #[test]
    fn lowers_a_loop_to_a_valid_header() {
        let bytes = lower(&loop_sum()).expect("the counting loop lowers to WASM");
        assert_eq!(&bytes[0..4], &[0x00, 0x61, 0x73, 0x6D]);
        assert!(bytes.len() > 16);
    }

    #[test]
    fn lowers_an_if_else_merge() {
        let bytes = lower(&if_else_merge()).expect("the if/else merge lowers to WASM");
        assert_eq!(&bytes[0..4], &[0x00, 0x61, 0x73, 0x6D]);
        assert!(bytes.len() > 16);
    }

    /// Allocates a two-field object, stores 40 and 2 into its fields, reads them back, and sums them
    /// -> 42. Exercises `Alloc` (the bump allocator) + `FieldStore`/`FieldLoad`.
    fn object_fields() -> Function {
        let i32t = MirType::I32;
        Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: alloc::vec![
                MirType::ObjectRef,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
            ],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: Vec::new(),
                insts: alloc::vec![
                    (
                        ValueId(0),
                        Inst::Alloc {
                            handle: lamella_ir::TypeHandle(1),
                            payload_size: 8,
                            ref_offsets: alloc::vec![].into_boxed_slice(),
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::ConstInt {
                            ty: i32t,
                            value: 40,
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::FieldStore {
                            base: ValueId(0),
                            offset: 0,
                            value: ValueId(1),
                        },
                    ),
                    (ValueId(3), Inst::ConstInt { ty: i32t, value: 2 }),
                    (
                        ValueId(4),
                        Inst::FieldStore {
                            base: ValueId(0),
                            offset: 4,
                            value: ValueId(3),
                        },
                    ),
                    (
                        ValueId(5),
                        Inst::FieldLoad {
                            base: ValueId(0),
                            offset: 0,
                        },
                    ),
                    (
                        ValueId(6),
                        Inst::FieldLoad {
                            base: ValueId(0),
                            offset: 4,
                        },
                    ),
                    (
                        ValueId(7),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(5),
                            rhs: ValueId(6),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(7)))),
            }],
        }
    }

    /// Allocates a two-element `int[]`, stores 20 and 22, reads them back (bounds-checked) and sums
    /// them -> 42. Exercises `AllocArray` + `ArrayStore`/`ArrayLoad` + the length word.
    fn array_sum() -> Function {
        let i32t = MirType::I32;
        let cint = |v: i64| Inst::ConstInt { ty: i32t, value: v };
        let store = |array, index, value| Inst::ArrayStore {
            array,
            index,
            value,
            element_size: 4,
        };
        let load = |array, index| Inst::ArrayLoad {
            array,
            index,
            element_size: 4,
            signed: false,
        };
        Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: alloc::vec![
                i32t,
                MirType::ObjectRef,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
            ],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: Vec::new(),
                insts: alloc::vec![
                    (ValueId(0), cint(2)),
                    (
                        ValueId(1),
                        Inst::AllocArray {
                            handle: lamella_ir::TypeHandle(1),
                            element: None,
                            length: ValueId(0),
                            element_size: 4,
                            element_kind: 5,
                        },
                    ),
                    (ValueId(2), cint(20)),
                    (ValueId(3), cint(0)),
                    (ValueId(4), store(ValueId(1), ValueId(3), ValueId(2))),
                    (ValueId(5), cint(22)),
                    (ValueId(6), cint(1)),
                    (ValueId(7), store(ValueId(1), ValueId(6), ValueId(5))),
                    (ValueId(8), load(ValueId(1), ValueId(3))),
                    (ValueId(9), load(ValueId(1), ValueId(6))),
                    (
                        ValueId(10),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(8),
                            rhs: ValueId(9),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(10)))),
            }],
        }
    }

    #[test]
    fn lowers_object_fields() {
        let bytes = lower(&object_fields()).expect("object field access lowers to WASM");
        assert_eq!(&bytes[0..4], &[0x00, 0x61, 0x73, 0x6D]);
        assert!(bytes.len() > 16);
    }

    /// A nested value-type field-access shape: a scalar-filled struct copied into a second struct by
    /// value (a struct-valued `FieldStore`), then read back by value (a struct-valued `FieldLoad`) and
    /// its fields summed -> 42. Exercises the struct-valued field-copy path a flat scalar load/store
    /// cannot express -- the regression guard for the nested/boxed field-access lowering.
    fn nested_struct_fields() -> Function {
        let i32t = MirType::I32;
        let vt = MirType::ValueType {
            handle: lamella_ir::TypeHandle(1),
            size: 8,
        };
        let cint = |v: i64| Inst::ConstInt { ty: i32t, value: v };
        let scalar_store = |base, offset, value| Inst::FieldStore {
            base,
            offset,
            value,
        };
        Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: alloc::vec![vt, i32t, i32t, i32t, i32t, vt, i32t, vt, i32t, i32t, i32t],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: Vec::new(),
                insts: alloc::vec![
                    (ValueId(0), Inst::InitStruct),
                    (ValueId(1), cint(40)),
                    (ValueId(2), scalar_store(ValueId(0), 0, ValueId(1))),
                    (ValueId(3), cint(2)),
                    (ValueId(4), scalar_store(ValueId(0), 4, ValueId(3))),
                    (ValueId(5), Inst::InitStruct),
                    (ValueId(6), scalar_store(ValueId(5), 0, ValueId(0))),
                    (
                        ValueId(7),
                        Inst::FieldLoad {
                            base: ValueId(5),
                            offset: 0,
                        },
                    ),
                    (
                        ValueId(8),
                        Inst::FieldLoad {
                            base: ValueId(7),
                            offset: 0,
                        },
                    ),
                    (
                        ValueId(9),
                        Inst::FieldLoad {
                            base: ValueId(7),
                            offset: 4,
                        },
                    ),
                    (
                        ValueId(10),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(8),
                            rhs: ValueId(9),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(10)))),
            }],
        }
    }

    #[test]
    fn lowers_struct_valued_field_access() {
        let bytes =
            lower(&nested_struct_fields()).expect("struct-valued field access lowers to WASM");
        assert_eq!(&bytes[0..4], &[0x00, 0x61, 0x73, 0x6D]);
        assert!(uses_memory(&[nested_struct_fields()]));
    }

    /// The box/unbox type-check shape: allocate a boxed object (a type-id header + payload), read the
    /// header back (`LoadTypeDesc`), and compare it to the type's descriptor (`TypeDescAddr`). Exercises
    /// the flat-wasm boxing path -- the header write plus the two type-descriptor ops.
    fn boxed_type_check() -> Function {
        let i32t = MirType::I32;
        Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: alloc::vec![MirType::ObjectRef, i32t, i32t, i32t],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: Vec::new(),
                insts: alloc::vec![
                    (
                        ValueId(0),
                        Inst::Alloc {
                            handle: lamella_ir::TypeHandle(7),
                            payload_size: 4,
                            ref_offsets: alloc::vec![].into_boxed_slice(),
                        },
                    ),
                    (ValueId(1), Inst::LoadTypeDesc { object: ValueId(0) }),
                    (
                        ValueId(2),
                        Inst::TypeDescAddr {
                            handle: lamella_ir::TypeHandle(7),
                        },
                    ),
                    (
                        ValueId(3),
                        Inst::Compare {
                            op: CmpOp::Eq,
                            lhs: ValueId(1),
                            rhs: ValueId(2),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(3)))),
            }],
        }
    }

    #[test]
    fn lowers_boxed_type_check() {
        let bytes = lower(&boxed_type_check()).expect("the boxing type-check lowers to WASM");
        assert_eq!(&bytes[0..4], &[0x00, 0x61, 0x73, 0x6D]);
        assert!(uses_memory(&[boxed_type_check()]));
    }

    /// A `callvirt` lowers to a `call_indirect` through the funcref table: func0 allocates a type whose
    /// vtable slot 0 is func1, then dispatches -- so the module gains a table section, an element
    /// segment, and the indirect-call opcode (previously `CallVirtual` was `Unsupported`).
    #[test]
    fn lowers_a_callvirt_to_call_indirect() {
        let caller = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: alloc::vec![MirType::ObjectRef, MirType::I32],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: Vec::new(),
                insts: alloc::vec![
                    (
                        ValueId(0),
                        Inst::Alloc {
                            handle: lamella_ir::TypeHandle(1),
                            payload_size: 4,
                            ref_offsets: alloc::vec![].into_boxed_slice(),
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::CallVirtual {
                            slot: 0,
                            args: alloc::vec![ValueId(0)],
                            returns_value: true,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        let target = Function {
            params: alloc::vec![MirType::ObjectRef],
            ret: Some(MirType::I32),
            value_types: alloc::vec![MirType::ObjectRef, MirType::I32],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: alloc::vec![ValueId(0)],
                insts: alloc::vec![(
                    ValueId(1),
                    Inst::ConstInt {
                        ty: MirType::I32,
                        value: 42,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        let descriptors = alloc::vec![TypeMeta {
            handle: lamella_ir::TypeHandle(1),
            type_tag: 0,
            vtable: alloc::vec![crate::resolver::VtableEntry::Func(1)],
            itable: Vec::new(),
            base: None,
            words: None,
            exported: true,
        }];
        let bytes = lower_module_with_exports(&[caller, target], &[("main", 0)], &descriptors)
            .expect("the callvirt lowers to WASM");
        assert_eq!(&bytes[0..4], &[0x00, 0x61, 0x73, 0x6D]);
        assert!(bytes.contains(&0x11), "the call_indirect opcode is emitted");
        assert!(
            bytes.windows(2).any(|w| w == [0x70, 0x00]),
            "a funcref table is declared"
        );
    }

    /// An interface `callvirt` lowers to an itable search + `call_indirect`: the descriptor carries the
    /// `(tag, funcref-index)` pair in a data segment, and the module gains the indirect-call opcode
    /// (previously `CallInterface` was `Unsupported` on wasm).
    #[test]
    fn lowers_a_callinterface_to_itable_search() {
        const TAG: u32 = 0x8000_1234;
        let caller = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: alloc::vec![MirType::ObjectRef, MirType::I32],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: Vec::new(),
                insts: alloc::vec![
                    (
                        ValueId(0),
                        Inst::Alloc {
                            handle: lamella_ir::TypeHandle(1),
                            payload_size: 4,
                            ref_offsets: alloc::vec![].into_boxed_slice(),
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::CallInterface {
                            tag: TAG,
                            args: alloc::vec![ValueId(0)],
                            returns_value: true,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        let target = Function {
            params: alloc::vec![MirType::ObjectRef],
            ret: Some(MirType::I32),
            value_types: alloc::vec![MirType::ObjectRef, MirType::I32],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: alloc::vec![ValueId(0)],
                insts: alloc::vec![(
                    ValueId(1),
                    Inst::ConstInt {
                        ty: MirType::I32,
                        value: 42,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        let descriptors = alloc::vec![TypeMeta {
            handle: lamella_ir::TypeHandle(1),
            type_tag: 0,
            vtable: Vec::new(),
            itable: alloc::vec![(TAG, crate::resolver::VtableEntry::Func(1))],
            base: None,
            words: None,
            exported: true,
        }];
        let bytes = lower_module_with_exports(&[caller, target], &[("main", 0)], &descriptors)
            .expect("the interface call lowers to WASM");
        assert!(bytes.contains(&0x11), "the call_indirect opcode is emitted");
        assert!(
            bytes.windows(4).any(|w| w == TAG.to_le_bytes()),
            "the interface tag is laid in the itable data"
        );
    }

    /// A `castclass`/`isinst` chain lowers (`CastClassScan` was `Unsupported`), and the descriptors are
    /// laid with a base_ptr chain: allocating a Dog (handle 2, base Animal handle 1) and scanning toward
    /// Animal lays BOTH descriptors -- Animal added transitively even though it is never allocated.
    #[test]
    fn lowers_a_castclass_chain_with_base_ptrs() {
        let main = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: alloc::vec![MirType::ObjectRef, MirType::I32, MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: Vec::new(),
                insts: alloc::vec![
                    (
                        ValueId(0),
                        Inst::Alloc {
                            handle: lamella_ir::TypeHandle(2),
                            payload_size: 4,
                            ref_offsets: alloc::vec![].into_boxed_slice(),
                        },
                    ),
                    (ValueId(1), Inst::LoadTypeDesc { object: ValueId(0) }),
                    (
                        ValueId(2),
                        Inst::TypeDescAddr {
                            handle: lamella_ir::TypeHandle(1),
                        },
                    ),
                    (
                        ValueId(3),
                        Inst::CastClassScan {
                            args: alloc::vec![ValueId(1), ValueId(2)],
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(3)))),
            }],
        };
        let descriptors = alloc::vec![
            TypeMeta {
                handle: lamella_ir::TypeHandle(2),
                type_tag: 0,
                vtable: Vec::new(),
                itable: Vec::new(),
                base: Some(lamella_ir::TypeHandle(1)),
                words: None,
                exported: true,
            },
            TypeMeta {
                handle: lamella_ir::TypeHandle(1),
                type_tag: 0,
                vtable: Vec::new(),
                itable: Vec::new(),
                base: None,
                words: None,
                exported: true,
            },
        ];
        let bytes = lower_module_with_exports(&[main], &[("main", 0)], &descriptors)
            .expect("the castclass chain lowers to WASM");
        assert!(
            bytes.windows(4).any(|w| w == 1u32.to_le_bytes()),
            "Animal's descriptor (added transitively) is laid"
        );
        assert!(
            bytes.windows(4).any(|w| w == 2u32.to_le_bytes()),
            "Dog's descriptor is laid"
        );
    }

    /// A VOID `callvirt` (`returns_value: false`) interns a no-result signature, so `call_indirect`
    /// matches a `void` target -- previously the assumed i32 result trapped as a type mismatch.
    #[test]
    fn lowers_a_void_callvirt_to_a_void_signature() {
        let caller = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: alloc::vec![MirType::ObjectRef, MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: Vec::new(),
                insts: alloc::vec![
                    (
                        ValueId(0),
                        Inst::Alloc {
                            handle: lamella_ir::TypeHandle(1),
                            payload_size: 4,
                            ref_offsets: alloc::vec![].into_boxed_slice(),
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::CallVirtual {
                            slot: 0,
                            args: alloc::vec![ValueId(0)],
                            returns_value: false,
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 42,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        let target = Function {
            params: alloc::vec![MirType::ObjectRef],
            ret: None,
            value_types: alloc::vec![MirType::ObjectRef],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: alloc::vec![ValueId(0)],
                insts: Vec::new(),
                terminator: Some(Terminator::Return(None)),
            }],
        };
        let descriptors = alloc::vec![TypeMeta {
            handle: lamella_ir::TypeHandle(1),
            type_tag: 0,
            vtable: alloc::vec![crate::resolver::VtableEntry::Func(1)],
            itable: Vec::new(),
            base: None,
            words: None,
            exported: true,
        }];
        let bytes = lower_module_with_exports(&[caller, target], &[("main", 0)], &descriptors)
            .expect("the void callvirt lowers to WASM");
        assert!(
            bytes.windows(4).any(|w| w == [0x60, 0x01, 0x7F, 0x00]),
            "a void (i32)->() signature is interned for the call_indirect"
        );
    }

    /// The `isinst` reinterpret round-trip lowers: an `ObjectRef` retyped to `i32` and back (the no-op
    /// `RefToInt`/`IntToRef` conversions) so a reference can pass through integer mask arithmetic.
    #[test]
    fn lowers_reinterpret_round_trip() {
        let func = Function {
            params: alloc::vec![MirType::ObjectRef],
            ret: Some(MirType::ObjectRef),
            value_types: alloc::vec![MirType::ObjectRef, MirType::I32, MirType::ObjectRef],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: alloc::vec![ValueId(0)],
                insts: alloc::vec![
                    (
                        ValueId(1),
                        Inst::Convert {
                            value: ValueId(0),
                            kind: lamella_ir::ConvKind::RefToInt,
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::Convert {
                            value: ValueId(1),
                            kind: lamella_ir::ConvKind::IntToRef,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        let bytes = lower(&func).expect("the reinterpret round-trip lowers to WASM");
        assert_eq!(&bytes[0..4], &[0x00, 0x61, 0x73, 0x6D]);
    }

    #[test]
    fn lowers_an_array_sum() {
        let bytes = lower(&array_sum()).expect("array access lowers to WASM");
        assert_eq!(&bytes[0..4], &[0x00, 0x61, 0x73, 0x6D]);
        assert!(bytes.len() > 16);
    }

    #[test]
    fn straight_line_module_has_no_memory() {
        assert!(!uses_memory(&[add_constants()]));
        assert!(uses_memory(&[object_fields()]));
    }

    /// `(int)((float)40 + 2.0f)` -> 42: int-to-float, a native f32 add, and float-to-int truncation.
    fn float_roundtrip() -> Function {
        let i32t = MirType::I32;
        let f32t = MirType::F32;
        Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: alloc::vec![i32t, f32t, f32t, f32t, i32t],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: Vec::new(),
                insts: alloc::vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: i32t,
                            value: 40,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::Convert {
                            value: ValueId(0),
                            kind: ConvKind::IntToFloat32,
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::ConstInt {
                            ty: f32t,
                            value: (2.0f32).to_bits() as i64,
                        },
                    ),
                    (
                        ValueId(3),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(1),
                            rhs: ValueId(2),
                        },
                    ),
                    (
                        ValueId(4),
                        Inst::Convert {
                            value: ValueId(3),
                            kind: ConvKind::Float32ToInt,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(4)))),
            }],
        }
    }

    #[test]
    fn lowers_float_arithmetic() {
        let bytes = lower(&float_roundtrip()).expect("float arithmetic lowers to WASM");
        assert_eq!(&bytes[0..4], &[0x00, 0x61, 0x73, 0x6D]);
        assert!(bytes.len() > 16);
    }

    /// Stores 40 and 2 into two static fields, reads them back and sums -> 42. Exercises
    /// `StaticStore`/`StaticLoad` over the static region of linear memory.
    fn static_fields() -> Function {
        let i32t = MirType::I32;
        Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: alloc::vec![i32t; 7],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: Vec::new(),
                insts: alloc::vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: i32t,
                            value: 40,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::StaticStore {
                            owner: lamella_ir::StaticOwner::Own,
                            offset: 0,
                            value: ValueId(0),
                        },
                    ),
                    (ValueId(2), Inst::ConstInt { ty: i32t, value: 2 }),
                    (
                        ValueId(3),
                        Inst::StaticStore {
                            owner: lamella_ir::StaticOwner::Own,
                            offset: 4,
                            value: ValueId(2),
                        },
                    ),
                    (
                        ValueId(4),
                        Inst::StaticLoad {
                            owner: lamella_ir::StaticOwner::Own,
                            offset: 0,
                        },
                    ),
                    (
                        ValueId(5),
                        Inst::StaticLoad {
                            owner: lamella_ir::StaticOwner::Own,
                            offset: 4,
                        },
                    ),
                    (
                        ValueId(6),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(4),
                            rhs: ValueId(5),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(6)))),
            }],
        }
    }

    #[test]
    fn lowers_static_fields() {
        let bytes = lower(&static_fields()).expect("static field access lowers to WASM");
        assert_eq!(&bytes[0..4], &[0x00, 0x61, 0x73, 0x6D]);
        assert!(uses_memory(&[static_fields()]));
    }

    /// Reads the `.Length` of a 42-unit string literal -> 42. Exercises `StringLiteral` (a read-only
    /// data segment + a constant pointer to it) and the length word at offset 0.
    fn string_length() -> Function {
        let i32t = MirType::I32;
        let text: alloc::boxed::Box<[u16]> = "Lamella compiles C# to WebAssembly bytes!!"
            .encode_utf16()
            .collect::<Vec<u16>>()
            .into_boxed_slice();
        Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: alloc::vec![MirType::ObjectRef, i32t],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: Vec::new(),
                insts: alloc::vec![
                    (ValueId(0), Inst::StringLiteral { utf16: text }),
                    (
                        ValueId(1),
                        Inst::FieldLoad {
                            base: ValueId(0),
                            offset: 0,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        }
    }

    #[test]
    fn lowers_string_literal_length() {
        let bytes = lower(&string_length()).expect("a string literal lowers to WASM");
        assert_eq!(&bytes[0..4], &[0x00, 0x61, 0x73, 0x6D]);
        assert!(bytes.len() > 16);
    }

    /// The emitted MODULE must carry the literal in THIS BUILD's storage encoding, so the check
    /// is on the bytes the backend actually emits rather than on `string_blob_bytes`.
    #[test]
    fn the_emitted_module_carries_the_literal_in_this_builds_storage_tier() {
        let text: alloc::boxed::Box<[u16]> = "Hi".encode_utf16().collect::<Vec<u16>>().into();
        let func = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: alloc::vec![MirType::ObjectRef, MirType::I32],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: Vec::new(),
                insts: alloc::vec![
                    (ValueId(0), Inst::StringLiteral { utf16: text }),
                    (
                        ValueId(1),
                        Inst::FieldLoad {
                            base: ValueId(0),
                            offset: 0,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        let bytes = lower(&func).expect("a string literal lowers to WASM");
        let expected = crate::stringgen::string_blob_bytes(&[0x48, 0x69])
            .expect("\"Hi\" encodes in every tier");
        assert!(
            bytes.windows(expected.len()).any(|w| w == expected.as_slice()),
            "the module must contain the literal blob for this tier ({expected:02x?})"
        );
    }

    #[test]
    fn lowers_string_equality() {
        let i32t = MirType::I32;
        let units = |s: &str| -> alloc::boxed::Box<[u16]> {
            s.encode_utf16().collect::<Vec<u16>>().into_boxed_slice()
        };
        let func = Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: alloc::vec![MirType::ObjectRef, MirType::ObjectRef, i32t],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: Vec::new(),
                insts: alloc::vec![
                    (
                        ValueId(0),
                        Inst::StringLiteral {
                            utf16: units("answer"),
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::StringLiteral {
                            utf16: units("answer"),
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::StringEquals {
                            lhs: ValueId(0),
                            rhs: ValueId(1),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        let bytes = lower(&func).expect("string equality lowers to WASM");
        assert_eq!(&bytes[0..4], &[0x00, 0x61, 0x73, 0x6D]);
        assert!(bytes.len() > 16);
    }

    /// A value-type round-trip with a by-value copy: a `Point` p {40, 2}, `q = p`, mutate `q.X = 100`,
    /// then `p.X + p.Y` -> 42 (the copy must leave p untouched). Exercises `InitStruct`, `CopyStruct`,
    /// and field access through a value-type base (its local holds the slot address).
    fn struct_copy() -> Function {
        let i32t = MirType::I32;
        let pt = MirType::ValueType {
            handle: lamella_ir::TypeHandle(1),
            size: 8,
        };
        let fs = |base, offset, value| Inst::FieldStore {
            base,
            offset,
            value,
        };
        Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: alloc::vec![pt, i32t, i32t, i32t, i32t, pt, i32t, i32t, i32t, i32t, i32t,],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: Vec::new(),
                insts: alloc::vec![
                    (ValueId(0), Inst::InitStruct),
                    (
                        ValueId(1),
                        Inst::ConstInt {
                            ty: i32t,
                            value: 40,
                        },
                    ),
                    (ValueId(2), fs(ValueId(0), 0, ValueId(1))),
                    (ValueId(3), Inst::ConstInt { ty: i32t, value: 2 }),
                    (ValueId(4), fs(ValueId(0), 4, ValueId(3))),
                    (ValueId(5), Inst::CopyStruct { src: ValueId(0) }),
                    (
                        ValueId(6),
                        Inst::ConstInt {
                            ty: i32t,
                            value: 100,
                        },
                    ),
                    (ValueId(7), fs(ValueId(5), 0, ValueId(6))),
                    (
                        ValueId(8),
                        Inst::FieldLoad {
                            base: ValueId(0),
                            offset: 0,
                        },
                    ),
                    (
                        ValueId(9),
                        Inst::FieldLoad {
                            base: ValueId(0),
                            offset: 4,
                        },
                    ),
                    (
                        ValueId(10),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(8),
                            rhs: ValueId(9),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(10)))),
            }],
        }
    }

    #[test]
    fn lowers_a_value_type_copy() {
        let bytes = lower(&struct_copy()).expect("a struct copy lowers to WASM");
        assert_eq!(&bytes[0..4], &[0x00, 0x61, 0x73, 0x6D]);
        assert!(uses_memory(&[struct_copy()]));
    }

    /// A 2-D rectangular array `int[1,1]`: `a[0,0] = 42; return a[0,0]`. Exercises `AllocArray2D` +
    /// `Array2DStore`/`Array2DLoad`.
    fn array_2d() -> Function {
        let i32t = MirType::I32;
        Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: alloc::vec![i32t, i32t, MirType::ObjectRef, i32t, i32t, i32t, i32t],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: Vec::new(),
                insts: alloc::vec![
                    (ValueId(0), Inst::ConstInt { ty: i32t, value: 1 }),
                    (ValueId(1), Inst::ConstInt { ty: i32t, value: 1 }),
                    (
                        ValueId(2),
                        Inst::AllocArray2D {
                            handle: lamella_ir::TypeHandle(1),
                            dim0: ValueId(0),
                            dim1: ValueId(1),
                            element_size: 4,
                        },
                    ),
                    (
                        ValueId(3),
                        Inst::ConstInt {
                            ty: i32t,
                            value: 42,
                        },
                    ),
                    (ValueId(4), Inst::ConstInt { ty: i32t, value: 0 }),
                    (
                        ValueId(5),
                        Inst::Array2DStore {
                            array: ValueId(2),
                            index0: ValueId(4),
                            index1: ValueId(4),
                            value: ValueId(3),
                            element_size: 4,
                        },
                    ),
                    (
                        ValueId(6),
                        Inst::Array2DLoad {
                            array: ValueId(2),
                            index0: ValueId(4),
                            index1: ValueId(4),
                            element_size: 4,
                            signed: false,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(6)))),
            }],
        }
    }

    #[test]
    fn lowers_a_2d_array() {
        let bytes = lower(&array_2d()).expect("a 2-D array lowers to WASM");
        assert_eq!(&bytes[0..4], &[0x00, 0x61, 0x73, 0x6D]);
        assert!(uses_memory(&[array_2d()]));
    }

    /// A rank-3 rectangular array `int[2,3,4]`: `a[1,2,3] = 42; return a[1,2,3]`. Exercises
    /// `AllocArrayMD` + `ArrayMDStore`/`ArrayMDLoad` (the Horner flat index 1*3*4 + 2*4 + 3 = 23).
    fn array_3d() -> Function {
        let i32t = MirType::I32;
        let n = ValueId;
        let c = |v: i64| Inst::ConstInt { ty: i32t, value: v };
        Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: alloc::vec![
                i32t,
                i32t,
                i32t,
                MirType::ObjectRef,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
            ],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: Vec::new(),
                insts: alloc::vec![
                    (n(0), c(2)),
                    (n(1), c(3)),
                    (n(2), c(4)),
                    (
                        n(3),
                        Inst::AllocArrayMD {
                            handle: lamella_ir::TypeHandle(1),
                            dims: alloc::vec![n(0), n(1), n(2)].into_boxed_slice(),
                            element_size: 4,
                        },
                    ),
                    (n(4), c(1)),
                    (n(5), c(2)),
                    (n(6), c(3)),
                    (n(7), c(42)),
                    (
                        n(8),
                        Inst::ArrayMDStore {
                            array: n(3),
                            indices: alloc::vec![n(4), n(5), n(6)].into_boxed_slice(),
                            value: n(7),
                            element_size: 4,
                        },
                    ),
                    (
                        n(9),
                        Inst::ArrayMDLoad {
                            array: n(3),
                            indices: alloc::vec![n(4), n(5), n(6)].into_boxed_slice(),
                            element_size: 4,
                            signed: false,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(n(9)))),
            }],
        }
    }

    #[test]
    fn lowers_a_3d_array() {
        let bytes = lower(&array_3d()).expect("a 3-D array lowers to WASM");
        assert_eq!(&bytes[0..4], &[0x00, 0x61, 0x73, 0x6D]);
        assert!(uses_memory(&[array_3d()]));
    }
}
