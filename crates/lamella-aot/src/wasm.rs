//! The WebAssembly target code generator -- the third backend target, after ARM and RISC-V.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use lamella_asm_wasm::{BlockType, Func, FuncType, Limits, MemArg, Module, ValType};
use lamella_ir::{
    BinOp, BlockId, CmpOp, ConvKind, Function, Inst, MirType, Terminator, ValueId, VerifyError,
};

use crate::resolver::TypeMeta;
use crate::struct_storage::{StorageClasses, struct_bytes};

/// The smallest linear-memory size, in 64 KiB pages, when nothing pushes the heap base higher.
const HEAP_MIN_PAGES: u32 = 2;
/// The global index of the bump-allocator heap pointer -- the first global the backend defines, so it
/// is index 0 whenever the module uses memory.
const HEAP_POINTER: u32 = 0;
/// The global index of the shadow-stack pointer: the lowest address of the newest call's [`Frame`].
/// It follows [`HEAP_POINTER`], and a module has it only when some function has a frame.
const STACK_POINTER: u32 = 1;
/// The size of the shadow stack, which holds every live call's [`Frame`]. It lies between the
/// module's data and its heap and grows down toward the data; a call whose frame would cross its
/// bottom traps (`unreachable`) instead of writing over the descriptors below it.
const STACK_BYTES: u32 = 64 * 1024;
/// The base address of the static-field region: it follows the null guard at offset 0, so each
/// static field lives at `STATIC_BASE + its offset`. The string literals follow the region, which is
/// as long as the program's statics need -- see [`lower_module_with_exports`].
const STATIC_BASE: i32 = 8;

/// The ONE header word every managed object carries immediately before it, holding the address of
/// the object's TYPE DESCRIPTOR. A class instance, an array and a string all take it, so `[obj - 4]`
/// answers "what type is this" for every reference alike and [`Inst::LoadTypeDesc`] stays one load
/// rather than a per-shape special case.
///
/// It is `const` rather than a `4` at each site because the shapes were not uniform until they all
/// took it: arrays and string literals had NO header, so a type test on one read the previous
/// allocation's tail -- a wrong answer that reads like a working one, since the word it lands on is
/// usually a plausible small integer.
const OBJECT_HEADER: u32 = 4;

/// Why a function could not be lowered to WebAssembly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LowerError {
    /// The function did not pass IR verification.
    ///
    /// CARRIES THE VERIFIER'S OWN ERRORS rather than collapsing them: a bare "not well formed" names
    /// neither the malformed instruction nor the types that disagreed, so a refusing row reported no
    /// reason at all. The verifier has always returned a `Vec<VerifyError>`; only this boundary threw
    /// it away.
    NotWellFormed {
        /// What [`lamella_ir::verify`] rejected. Never empty -- it is constructed only from an `Err`.
        errors: Vec<VerifyError>,
    },
    /// An instruction or type the WASM backend does not lower yet, such as a call into native code or
    /// a static field of another assembly.
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
    /// A static field access reaches past the end of the static-field region the program declares.
    /// The string literals begin where the region ends, so the access would read or write one.
    StaticOutsideRegion {
        /// The first byte past the furthest-reaching static access, counted from the region's start.
        end: u32,
        /// The region's size in bytes, as the program declares it.
        region: u32,
    },
}

impl core::fmt::Display for LowerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            LowerError::NotWellFormed { errors } => match errors.split_first() {
                Some((first, rest)) if rest.is_empty() => {
                    write!(f, "the function does not verify: {first:?}")
                }
                Some((first, rest)) => write!(
                    f,
                    "the function does not verify: {first:?}, and {} further problem(s)",
                    rest.len(),
                ),
                None => write!(f, "the function does not verify"),
            },
            LowerError::Unsupported => write!(
                f,
                "the function uses an instruction or a type the WASM backend does not lower yet, \
                 such as a call into native code or a static field of another assembly",
            ),
            LowerError::ControlFlowUnsupported => write!(
                f,
                "the function has a control-flow shape this target does not lower: a branch \
                 edge carrying block-parameter arguments (a merge passes its parameters on a \
                 Jump), or an entry block whose parameters do not match the signature",
            ),
            LowerError::UnencodableStringUnit { unit, index } => write!(
                f,
                "a string literal holds the UTF-16 code unit 0x{unit:04X} at index {index}, \
                 which this build's string storage cannot represent -- a lone surrogate has no \
                 form under `string-utf8`",
            ),
            LowerError::StaticOutsideRegion { end, region } => write!(
                f,
                "a static field access reaches byte {end} of the static region, which the \
                 program declares as {region} bytes; the string literals begin where the region \
                 ends, so the access would read or write one",
            ),
        }
    }
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
    lower_module_inner(
        core::slice::from_ref(func),
        main_export,
        false,
        descriptors,
        None,
        None,
    )
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
    lower_module_inner(funcs, main_export, false, &[], None, None)
}

/// Lowers a module, exporting each `(name, function_index)` in `exports` (the index is into `funcs`),
/// plus `memory` and the embedding-ABI allocator (`alloc`/`dealloc`) when the module uses memory.
/// This is the web embedding ABI: a page's JS calls each exported method by name (e.g. `Program_Add`)
/// and uses `alloc`/`dealloc` to pass arrays/strings in. `lower_module` is the single-entry
/// `main`-only case. Export names must be unique (the caller mangles overloads); a function appended
/// by the string lowering keeps the original indices valid.
///
/// `string_handle` is `System.String`'s identity in this build, from
/// [`MetadataResolver::string_type_handle`](crate::resolver::MetadataResolver::string_type_handle).
/// It is what gives a string -- literal or built -- an object header naming the SAME descriptor a
/// `string[]`'s element word names. `None` leaves strings headerless, which is what every caller
/// that lowers hand-built MIR wants and what this backend did for every caller before.
///
/// `static_region` is the size in bytes of the program's static-field region, from the walk that
/// numbered its statics (`resolver::static_region_words`), and the string literals are laid where
/// it ends. `None` sizes the region from the module's own static accesses instead: right for
/// hand-built MIR, and short for a program that reaches its last static only through the static's
/// address, since an address names no width. A program's static access past the region it
/// declares is refused ([`LowerError::StaticOutsideRegion`]).
pub fn lower_module_with_exports(
    funcs: &[Function],
    exports: &[(&str, u32)],
    descriptors: &[TypeMeta],
    string_handle: Option<u32>,
    static_region: Option<u32>,
) -> Result<Vec<u8>, LowerError> {
    lower_module_inner(
        funcs,
        exports,
        true,
        descriptors,
        string_handle,
        static_region,
    )
}

fn lower_module_inner(
    funcs: &[Function],
    exports: &[(&str, u32)],
    with_allocator: bool,
    descriptors: &[TypeMeta],
    string_handle: Option<u32>,
    static_region: Option<u32>,
) -> Result<Vec<u8>, LowerError> {
    let mut program: Vec<Function> = funcs.to_vec();
    crate::stringgen::lower_string_equals(&mut program);
    crate::stringgen::lower_string_concat(&mut program, string_handle);
    crate::stringgen::lower_int_to_string(&mut program, string_handle);
    let reached = static_reach(&program);
    let region = match static_region {
        Some(region) if reached > region => {
            return Err(LowerError::StaticOutsideRegion {
                end: reached,
                region,
            });
        }
        Some(region) => region,
        None => reached,
    };
    let mut strings = layout_strings(
        &mut program,
        (STATIC_BASE as u32 + region).next_multiple_of(8),
    )?;

    for func in &program {
        if let Err(errors) = lamella_ir::verify(func) {
            return Err(LowerError::NotWellFormed { errors });
        }
    }
    let frames: Vec<Frame> = program.iter().map(frame_layout).collect();
    let uses_stack = frames.iter().any(|frame| frame.size > 0);

    let native_import_sigs = collect_native_imports(&program)?;
    let import_count = native_import_sigs.len() as u32;

    let (desc_segments, desc_addr, desc_end) = layout_descriptors(
        &program,
        descriptors,
        string_handle,
        strings.heap_base as u32,
        import_count,
    );
    patch_string_headers(
        &mut strings.segments,
        string_handle
            .and_then(|handle| desc_addr.get(&handle).copied())
            .unwrap_or(0),
    );

    let mut module = Module::new();
    let data_end = desc_end.next_multiple_of(8);
    let (stack_limit, heap_base) = if uses_stack {
        (data_end, data_end + STACK_BYTES)
    } else {
        (0, data_end)
    };

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

    let heap_alloc = import_count + program.len() as u32;
    let allocates = program
        .iter()
        .any(|func| raises_of(func).contains(&Raise::OutOfMemory));
    let ctx = WasmCtx {
        funcs: &program,
        desc_addr,
        sig_types,
        import_count,
        native_imports,
        stack_limit,
        heap_alloc: allocates.then_some(heap_alloc),
    };
    let mut bodies = Vec::with_capacity(program.len());
    for (func, frame) in program.iter().zip(&frames) {
        bodies.push((module.add_type(func_type(func)?), lower_function(func, &ctx, frame)?));
    }
    let has_memory = uses_stack
        || !strings.segments.is_empty()
        || !desc_segments.is_empty()
        || bodies
            .iter()
            .any(|(_, body)| body.uses_memory() || body.uses_globals());
    if has_memory {
        module.set_memory(Limits {
            min_pages: (heap_base.div_ceil(0x1_0000) + 1).max(HEAP_MIN_PAGES),
            max_pages: None,
        });
        module.export_memory("memory");
        module.add_global(ValType::I32, true, i64::from(heap_base));
        if uses_stack {
            module.add_global(ValType::I32, true, i64::from(heap_base));
        }
        for (offset, blob) in strings.segments {
            module.add_data(offset, blob);
        }
        for (offset, blob) in desc_segments {
            module.add_data(offset, blob);
        }
    }
    for (type_index, body) in bodies {
        module.add_function(type_index, body);
    }
    if has_memory && (allocates || with_allocator) {
        let heap_alloc_type = module.add_type(FuncType {
            params: alloc::vec![ValType::I64],
            results: alloc::vec![ValType::I32],
        });
        module.add_function(heap_alloc_type, build_heap_alloc());
    }
    if has_memory && with_allocator {
        let alloc_index = heap_alloc + 1;
        let alloc_type = module.add_type(FuncType {
            params: alloc::vec![ValType::I32],
            results: alloc::vec![ValType::I32],
        });
        module.add_function(alloc_type, build_alloc(heap_alloc));
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

/// The module's one allocator, `heap_alloc(bytes: i64) -> address`: takes `bytes`, rounded up to 8,
/// from the top of the heap and answers where they start, growing linear memory when they reach
/// past its end. Every program allocation and the embedding ABI's `alloc` come through here.
///
/// Every range it hands out is fresh and inside linear memory. A request it cannot serve answers 0,
/// an address no allocation can have since the heap starts past the data, and leaves the heap
/// pointer where it was: an end past the 32-bit address space, or a grow the engine refuses. A
/// pointer out of range, or one overlapping an earlier block, would let two objects alias inside the
/// one linear memory, where memory isolation cannot see it.
///
/// The byte count is 64-bit so that no size arithmetic wraps on the way in: an array's length times
/// its element size is exact in 64 bits, where in 32 a huge length wraps to a small block that the
/// array's stores then overrun. Every caller's count is below 2^33, so the rounding and the sum below
/// cannot wrap either.
fn build_heap_alloc() -> Func {
    let mut f = Func::new(1);
    let top = f.add_local(ValType::I32);
    let end = f.add_local(ValType::I64);
    let grow = f.add_local(ValType::I32);
    f.global_get(HEAP_POINTER);
    f.local_tee(top);
    f.i64_extend_i32_u();
    f.local_get(0);
    f.i64_const(7);
    f.i64_add();
    f.i64_const(!7);
    f.i64_and();
    f.i64_add();
    f.local_tee(end);
    f.i64_const(0xFFFF_FFF8);
    f.i64_gt_u();
    f.if_(BlockType::Empty);
    f.i32_const(0);
    f.return_();
    f.end();
    f.local_get(end);
    f.i64_const(1);
    f.i64_sub();
    f.i64_const(16);
    f.i64_shr_u();
    f.i32_wrap_i64();
    f.i32_const(1);
    f.i32_add();
    f.memory_size();
    f.i32_sub();
    f.local_tee(grow);
    f.i32_const(0);
    f.i32_gt_s();
    f.if_(BlockType::Empty);
    f.local_get(grow);
    f.memory_grow();
    f.i32_const(-1);
    f.i32_eq();
    f.if_(BlockType::Empty);
    f.i32_const(0);
    f.return_();
    f.end();
    f.end();
    f.local_get(end);
    f.i32_wrap_i64();
    f.global_set(HEAP_POINTER);
    f.local_get(top);
    f.end();
    f
}

/// The embedding ABI's `alloc(size) -> ptr`: reserve `size` bytes, rounded up to 8, for the host to
/// write into, and return where they start. JS reserves a buffer with this, writes `[len][bytes]`,
/// and passes the pointer into an exported method.
///
/// It takes the bytes through the module's allocator (`heap_alloc`, the function index given), so
/// every range is fresh and inside linear memory and memory grows when a request needs it. Where a
/// program's allocation raises OutOfMemoryException, this traps (`unreachable`) instead: the host
/// called it, so there is no handler in the program to raise to. A host that keeps a view of
/// `memory.buffer` must take it again after this returns, because a grow replaces that buffer.
fn build_alloc(heap_alloc: u32) -> Func {
    let mut f = Func::new(1);
    let address = f.add_local(ValType::I32);
    f.local_get(0);
    f.i64_extend_i32_u();
    f.call(heap_alloc);
    f.local_tee(address);
    f.i32_eqz();
    f.if_(BlockType::Empty);
    f.unreachable();
    f.end();
    f.local_get(address);
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

/// The first byte past every static access in `program`, counted from the region's start: the size
/// of the static region a module needs when no program declares one.
///
/// It is never less than the exception model's two reserved words, the in-flight tag and message,
/// because the lowering's own raises write them whether or not the program touches them. An access
/// through a static's address (`StaticAddr`) counts one word, which is all an address says about
/// what it points at. A static of another assembly is left out: the lowering refuses it.
fn static_reach(program: &[Function]) -> u32 {
    let mut end = crate::cil::RESERVED_STATIC_SLOTS * 4;
    for func in program {
        let bytes = |value: &ValueId| match func.value_types.get(value.index()) {
            Some(MirType::I64 | MirType::F64) => 8,
            Some(MirType::ValueType { size, .. }) => size.div_ceil(4) * 4,
            _ => 4,
        };
        for (result, inst) in func.blocks.iter().flat_map(|b| &b.insts) {
            let reach = match inst {
                Inst::StaticLoad {
                    owner: lamella_ir::StaticOwner::Own,
                    offset,
                } => offset + bytes(result),
                Inst::StaticStore {
                    owner: lamella_ir::StaticOwner::Own,
                    offset,
                    value,
                } => offset + bytes(value),
                Inst::StaticAddr {
                    owner: lamella_ir::StaticOwner::Own,
                    offset,
                } => offset + 4,
                _ => continue,
            };
            end = end.max(reach);
        }
    }
    end
}

/// Interns each `StringLiteral` to an OBJECT -- a [`OBJECT_HEADER`] word then the
/// `[u32 length][UTF-16LE]` blob -- at an offset from `base`, the first byte past the static region,
/// rewriting the instruction to a constant `ObjectRef` pointing PAST the header (at the length word,
/// where `String.Length` and the storage readers expect offset 0), and returns the segments + heap
/// base.
///
/// The header word is laid ZERO here and patched by [`patch_string_headers`] once the descriptors
/// have addresses. It cannot be filled in place: descriptors are laid FROM the heap base this
/// function computes, so the address does not exist yet. Reserving the word and patching it is what
/// makes that an ordering detail rather than a reason a literal has no header -- which is what it was.
fn layout_strings(program: &mut [Function], base: u32) -> Result<StringLayout, LowerError> {
    let mut interned: Vec<(Vec<u16>, u32)> = Vec::new();
    let mut segments: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut next = base;
    for func in program.iter_mut() {
        for block in &mut func.blocks {
            for (_, inst) in &mut block.insts {
                if let Inst::StringLiteral { utf16 } = inst {
                    let units: Vec<u16> = utf16.to_vec();
                    let object = match interned.iter().find(|(c, _)| *c == units) {
                        Some((_, object)) => *object,
                        None => {
                            let mut blob = alloc::vec![0u8; OBJECT_HEADER as usize];
                            blob.extend_from_slice(&string_blob(&units)?);
                            let base = next;
                            next = (next + blob.len() as u32).next_multiple_of(4);
                            segments.push((base, blob));
                            let object = base + OBJECT_HEADER;
                            interned.push((units, object));
                            object
                        }
                    };
                    *inst = Inst::ConstInt {
                        ty: MirType::ObjectRef,
                        value: i64::from(object),
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

/// Writes `desc` into the reserved header word of every literal segment [`layout_strings`] laid.
///
/// A build with no `System.String` descriptor to name leaves them ZERO, which is the same ABSENT
/// value a null reference's `LoadTypeDesc` answers -- so a type test on a literal MISSES rather than
/// reading whatever the word happens to hold. The literals are all of `segments`, so this walks it
/// whole rather than carrying a second list of which entries are strings.
fn patch_string_headers(segments: &mut [(u32, Vec<u8>)], desc: i32) {
    for (_, blob) in segments.iter_mut() {
        if let Some(header) = blob.get_mut(..OBJECT_HEADER as usize) {
            header.copy_from_slice(&desc.to_le_bytes());
        }
    }
}

/// Builds a string blob in this build's storage encoding -- the unit count as a little-endian `u32`,
/// then either the UTF-16LE units or a byte length and the UTF-8/WTF-8 bytes.
///
/// Delegates to the ONE place that owns the layout rather than spelling it out again: hardcoding
/// UTF-16 here would let a `--features wasm,string-utf8` build be accepted and silently produce a
/// UTF-16 image.
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
    /// The bottom of the shadow stack: a call whose frame would start below it traps. Meaningful only
    /// in a module where some function has a frame.
    stack_limit: u32,
    /// The function index of the module's allocator ([`build_heap_alloc`]), in a module where
    /// anything allocates.
    heap_alloc: Option<u32>,
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
            let (import, args) = match inst {
                Inst::PInvoke { import, args } => {
                    (alloc::string::String::from(&**import), &args[..])
                }
                Inst::WriteInt { value } => {
                    (console_write_int_import(), core::slice::from_ref(value))
                }
                _ => continue,
            };
            if imports.iter().any(|(name, _)| *name == import) {
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
            imports.push((import, FuncType { params, results }));
        }
    }
    Ok(imports)
}

/// The host import `Console.WriteLine(int)` is a call of: the corlib's console seam for that
/// overload, named by the same rule as the seams of every other `Console` output overload this target
/// imports (`System.Console.WriteLine.i.v`), so a host serving those serves this one by name.
///
/// The overload alone reaches this target as [`Inst::WriteInt`] rather than as a call, because it is
/// folded for ARM and RISC-V, which write it over semihosting. This target has no semihosting, so
/// the instruction becomes the call the fold replaced.
fn console_write_int_import() -> alloc::string::String {
    crate::resolver::extern_method_symbol(
        "System",
        "Console",
        "WriteLine",
        &[lamella_metadata::SigType::I4],
        &lamella_metadata::SigType::Void,
        &|_| None,
    )
}

/// The result of [`layout_descriptors`]: the linear-memory data segments (`(offset, bytes)`), the
/// `type handle -> descriptor address` map, and the next free linear-memory offset.
type DescriptorLayout = (Vec<(u32, Vec<u8>)>, BTreeMap<u32, i32>, u32);

/// What SHAPE a descriptor takes. The two forms differ in every word, which is why a consumer that
/// reads one as the other gets a plausible wrong answer rather than a fault -- see the MARK guards on
/// [`Inst::CastClassScan`], [`Inst::InterfaceHasTag`] and [`Inst::CallInterface`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum DescShape {
    /// A CLASS: `[id@0][base_ptr@4][itable_count@8][(tag, funcref-index)@12 ...]`, this target's own
    /// form, with the vtable laid before it.
    Class,
    /// An ARRAY, in the format shared with the ARM and RISC-V backends:
    /// `[MARK | rank@0][element_kind@4][type_tag@8][base_ptr@12][element_desc@16][cast_class@20]`.
    ///
    ///
    /// It is not this target's class form with extra words -- word 1 is the element KIND where a
    /// class has an itable count, and `element_desc@16` is the word the SHARED covariant-store check
    /// reads. That check lives in `cil.rs` and is written against the ratified offsets, so an array
    /// descriptor here has to be laid at them or the check reads two unrelated words and compares them.
    Array {
        /// Word 1: what one element IS, for a consumer that must stride or trace the payload.
        element_kind: u32,
        /// The ELEMENT type's handle, whose descriptor `element_desc@16` names. `None` lays the
        /// word `0`, the ABSENT value the store check refuses to check against rather than guess.
        element: Option<u32>,
        /// Word 5: ECMA-335 III.4.3's array-element cast class, `0` where only the element's own
        /// identity will do. It is carried HERE rather than derived from `element_kind` because the
        /// kind answers a WIDTH question and names two types per code.
        element_cast_class: u32,
    },
}

/// Lays each allocated or queried type's descriptor in linear memory from `base`. Per type the vtable
/// (function = table indices) is laid BEFORE a fixed metadata block so slot `k` is `[desc - 4 - k*4]`,
/// and `desc` (the address in the map -- the object header + `TypeDescAddr` value) points at the words
/// of its [`DescShape`]. A class's `base_ptr` is the base type's descriptor address (0 at the chain's
/// end), which a `castclass`/`isinst` scan walks; the base types are added transitively so the whole
/// chain is present. Returns the data segments, the `handle -> descriptor address` map, and the next
/// free offset. Two passes: assign every descriptor an address, then lay the bytes (so a `base_ptr` --
/// or an `element_desc` -- can reference an address assigned later).
///
/// `string_handle` is `System.String`'s identity, and it is passed rather than discovered because
/// NOTHING IN THE PROGRAM NAMES IT by the time this runs: `layout_strings` has already rewritten every
/// literal to a bare `ConstInt`. A program that only prints text reaches the type through no `Alloc`,
/// no cast and no `TypeDescAddr`, so without it laid here a literal's header word has no address to
/// carry and every type test on a literal misses.
fn layout_descriptors(
    program: &[Function],
    descriptors: &[TypeMeta],
    string_handle: Option<u32>,
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
    let tag_of = |h: u32| {
        descriptors
            .iter()
            .find(|m| m.handle.0 == h)
            .map_or(0, |m| m.type_tag)
    };
    let mut handles: Vec<(u32, DescShape)> = Vec::new();
    if let Some(handle) = string_handle {
        note_descriptor(
            &mut handles,
            handle,
            DescShape::Array {
                element_kind: crate::resolver::string_descriptor_words(0)[1],
                element: None,
                element_cast_class: crate::resolver::ARRAY_CAST_CLASS_NONE,
            },
        );
    }
    for (_, inst) in program
        .iter()
        .flat_map(|f| f.blocks.iter().flat_map(|b| &b.insts))
    {
        match inst {
            Inst::Alloc { handle, .. } | Inst::TypeDescAddr { handle } => {
                note_descriptor(&mut handles, handle.0, DescShape::Class);
            }
            Inst::AllocArray {
                handle,
                element,
                element_kind,
                element_cast_class,
                ..
            } => {
                note_descriptor(
                    &mut handles,
                    handle.0,
                    DescShape::Array {
                        element_kind: *element_kind,
                        element: element.map(|e| e.0),
                        element_cast_class: *element_cast_class,
                    },
                );
                if let Some(element) = element {
                    note_descriptor(&mut handles, element.0, DescShape::Class);
                }
            }
            Inst::AllocArray2D { handle, .. } | Inst::AllocArrayMD { handle, .. } => {
                note_descriptor(&mut handles, handle.0, DescShape::Class);
            }
            _ => continue,
        }
    }
    let mut i = 0;
    while i < handles.len() {
        if let Some(b) = base_of(handles[i].0) {
            note_descriptor(&mut handles, b, DescShape::Class);
        }
        i += 1;
    }

    let mut map = BTreeMap::new();
    let mut next = base;
    for &(handle, shape) in &handles {
        let meta = descriptors.iter().find(|m| m.handle.0 == handle);
        let vlen = meta.map(|m| m.vtable.len()).unwrap_or(0) as u32;
        let ilen = meta.map(|m| m.itable.len()).unwrap_or(0) as u32;
        map.insert(handle, (next + vlen * 4) as i32);
        next += vlen * 4
            + match shape {
                DescShape::Class => 12 + ilen * 8,
                DescShape::Array { .. } => crate::resolver::ARRAY_ITABLE_OFFSET + 4 + ilen * 8,
            };
    }

    let mut segments = Vec::new();
    for &(handle, shape) in &handles {
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
        match shape {
            DescShape::Class => {
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
            }
            DescShape::Array {
                element_kind,
                element,
                element_cast_class,
            } => {
                blob.extend_from_slice(&(crate::resolver::ARRAY_DESC_MARK | 1).to_le_bytes());
                blob.extend_from_slice(&element_kind.to_le_bytes());
                blob.extend_from_slice(&element.map_or_else(|| tag_of(handle), tag_of).to_le_bytes());
                blob.extend_from_slice(&0u32.to_le_bytes());
                let element_rel = element
                    .and_then(|e| map.get(&e).copied())
                    .map_or(0, |addr| (addr as u32).wrapping_sub(desc_addr));
                blob.extend_from_slice(&element_rel.to_le_bytes());
                blob.extend_from_slice(&element_cast_class.to_le_bytes());
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
            }
        }
        segments.push((desc_addr - (vtable.len() as u32) * 4, blob));
    }
    (segments, map, next)
}

/// Records that `handle` needs a descriptor of `shape`, keeping ONE entry per handle.
///
/// An ARRAY shape UPGRADES a class one and never the reverse. A handle can be reached both ways --
/// `System.String` is staged as an array before the walk and then met again as a `string[]`'s
/// element, which is a plain type reference -- and only the array form carries the element word the
/// covariant-store check reads, so the richer shape has to win regardless of which order they arrive in.
fn note_descriptor(handles: &mut Vec<(u32, DescShape)>, handle: u32, shape: DescShape) {
    if let Some(entry) = handles.iter_mut().find(|(h, _)| *h == handle) {
        if matches!(entry.1, DescShape::Class) {
            entry.1 = shape;
        }
        return;
    }
    handles.push((handle, shape));
}

/// Lowers one function body: the value-to-local map (the entry block's parameters are the
/// WebAssembly parameters at locals `0..n`; every other value gets a fresh local), the [`Frame`]
/// that holds its struct values, then the structured control flow emitted from the dominator tree.
fn lower_function(func: &Function, ctx: &WasmCtx, frame: &Frame) -> Result<Func, LowerError> {
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
    let homes = Homes {
        frame_base: (frame.size > 0).then(|| body.add_local(ValType::I32)),
        local_of,
        frame,
    };
    if let Some(base) = homes.frame_base {
        enter_frame(&mut body, ctx, func, &homes, base)?;
    }

    let cfg = Cfg::analyze(func);
    let mut scopes: Vec<Scope> = Vec::new();
    let raises = raises_of(func);
    for &raise in raises.iter().rev() {
        body.block(BlockType::Empty);
        scopes.push(Scope {
            kind: ScopeKind::Raise(raise),
            block: func.entry,
        });
    }
    emit_tree(&cfg, func, ctx, &homes, &mut body, &mut scopes, func.entry)?;
    if func.ret.is_some() || !raises.is_empty() {
        body.unreachable();
    }
    for &raise in &raises {
        scopes.pop();
        body.end();
        emit_raise(&mut body, &homes, func.ret, raise)?;
    }
    body.end();
    Ok(body)
}

/// Where each of a function's values lives while its body is emitted: a WebAssembly local per
/// value, and for a function with a [`Frame`], the local holding the frame's base address.
struct Homes<'a> {
    /// Each value's local. A struct value's local holds the address of its bytes.
    local_of: Vec<u32>,
    /// The local holding this call's frame base, for a function whose frame is not empty.
    frame_base: Option<u32>,
    frame: &'a Frame,
}

impl Homes<'_> {
    /// The frame base's local and `value`'s offset from it, for a value that owns a slot.
    fn slot(&self, value: ValueId) -> Option<(u32, u32)> {
        let offset = (*self.frame.slots.get(value.index())?)?;
        Some((self.frame_base?, offset))
    }

    /// Points `value`'s local at its slot.
    fn name_slot(&self, body: &mut Func, value: ValueId) -> Result<(), LowerError> {
        let (base, offset) = self.slot(value).ok_or(LowerError::Unsupported)?;
        body.local_get(base);
        if offset != 0 {
            body.i32_const(offset as i32);
            body.i32_add();
        }
        body.local_set(self.local_of[value.index()]);
        Ok(())
    }

    /// Points the local of a struct an instruction makes at the struct's storage: its slot, or, for
    /// a struct with no bytes (which owns none -- see [`Frame`]), the heap's top, an address with
    /// nothing behind it.
    fn name_made_struct(&self, body: &mut Func, value: ValueId) -> Result<(), LowerError> {
        if self.slot(value).is_some() {
            return self.name_slot(body, value);
        }
        body.global_get(HEAP_POINTER);
        body.local_set(self.local_of[value.index()]);
        Ok(())
    }

    /// Copies `words` words from the address in `source`'s local into `value`'s slot, then points
    /// `value`'s local at the slot.
    fn copy_into_slot(
        &self,
        body: &mut Func,
        value: ValueId,
        source: u32,
        words: u32,
    ) -> Result<(), LowerError> {
        let (base, offset) = self.slot(value).ok_or(LowerError::Unsupported)?;
        for word in 0..words {
            body.local_get(base);
            body.local_get(source);
            body.i32_load(MemArg::new(4, word * 4));
            body.i32_store(MemArg::new(4, offset + word * 4));
        }
        self.name_slot(body, value)
    }
}

/// Where a function keeps the struct values its WebAssembly locals cannot hold.
///
/// A WebAssembly local holds one scalar and has no address, so on this target a struct value is the
/// ADDRESS of its bytes in linear memory, and the bytes live in the function's frame: a block of the
/// shadow stack that a call takes on entry and gives back when it returns. Each value that owns
/// storage has one slot at a fixed offset, so code that runs a million times in a loop rewrites the
/// same slot rather than taking new memory each time -- the arrangement the ARM and RISC-V backends
/// give every value on the machine stack.
///
/// A struct value owns a slot when it is:
///
/// - the result of an instruction: `InitStruct`, `CopyStruct`, a struct read from a field or a
///   static, or a call's struct result, which is copied out of the callee's frame as soon as the call
///   returns;
/// - a parameter of the function, copied into the frame on entry -- which is what makes a struct
///   passed by value the callee's own copy (ECMA-335 Partition I, 12.4.1.5: for a user-defined value
///   type passed by value, "called method receives a copy") for every kind of call and every caller,
///   the host included;
/// - a block parameter that takes a copy of its argument rather than sharing its storage (see
///   [`StorageClasses`]).
///
/// An address-taken scalar local (the target of an `out` or `ref` argument) and an assigned
/// parameter reach this backend as one-word structs, so they live in the frame too and are released
/// with it.
///
/// A struct with NO bytes owns no slot. Such a value has nothing to copy or address, and it is how
/// a value type declared in an assembly the build does not attach reaches this backend: an enum from
/// another library arrives as a zero-size struct while the callers pass its integer value. So a
/// zero-size struct's value travels in its local unchanged, as a scalar's does.
struct Frame {
    /// The frame's size in bytes: a multiple of 8, and 0 for a function that keeps nothing in memory.
    size: u32,
    /// Each value's slot offset from the frame base, for a value that owns a slot.
    slots: Vec<Option<u32>>,
    /// Which block parameters share their argument's storage rather than take a copy of it.
    shared: Vec<bool>,
}

/// Lays out `func`'s [`Frame`]: one 8-aligned slot per struct value that owns storage, in value
/// order.
fn frame_layout(func: &Function) -> Frame {
    let shared = StorageClasses::of(func).shared();
    let mut slots = alloc::vec![None; func.value_types.len()];
    let mut size = 0u32;
    let mut give_slot = |value: ValueId| {
        if let Some(bytes) = struct_bytes(&func.value_types, value)
            && slots[value.index()].is_none()
        {
            slots[value.index()] = Some(size);
            size += bytes.next_multiple_of(8);
        }
    };
    for (index, block) in func.blocks.iter().enumerate() {
        for &param in &block.params {
            if index == func.entry.index() || !shared[param.index()] {
                give_slot(param);
            }
        }
        for &(result, _) in &block.insts {
            give_slot(result);
        }
    }
    Frame {
        size,
        slots,
        shared,
    }
}

/// Takes this call's frame from the shadow stack, copies each struct parameter into it, and points
/// every copying block parameter's local at its slot for the rest of the call.
///
/// The frame is the [`Frame::size`] bytes below the stack pointer. A frame that would start below the
/// stack's bottom traps here, before anything is written: the stack lies directly above the module's
/// descriptors and string data, and a frame past its bottom would overwrite them.
fn enter_frame(
    body: &mut Func,
    ctx: &WasmCtx,
    func: &Function,
    homes: &Homes,
    base: u32,
) -> Result<(), LowerError> {
    let frame_size = homes.frame.size;
    body.global_get(STACK_POINTER);
    body.i32_const(ctx.stack_limit.saturating_add(frame_size) as i32);
    body.i32_lt_u();
    body.if_(BlockType::Empty);
    body.unreachable();
    body.end();
    body.global_get(STACK_POINTER);
    body.i32_const(frame_size as i32);
    body.i32_sub();
    body.local_tee(base);
    body.global_set(STACK_POINTER);

    let entry = func.entry.index();
    for &param in &func.blocks[entry].params {
        if let Some(bytes) = struct_bytes(&func.value_types, param)
            && homes.slot(param).is_some()
        {
            let incoming = homes.local_of[param.index()];
            homes.copy_into_slot(body, param, incoming, bytes.div_ceil(4))?;
        }
    }
    for (index, block) in func.blocks.iter().enumerate() {
        if index == entry {
            continue;
        }
        for &param in &block.params {
            if homes.slot(param).is_some() {
                homes.name_slot(body, param)?;
            }
        }
    }
    Ok(())
}

/// Gives this call's frame back to the shadow stack, just before a `return`. Whatever value the
/// return carries stays on the operand stack beneath it.
///
/// A struct the function returns is an address inside the frame given back here. Nothing runs
/// between this and the caller's next instruction, which copies the struct into the caller's own
/// frame (see [`adopt_struct_result`]); a host calling an export that returns a struct reads it
/// before calling into the module again.
fn leave_frame(body: &mut Func, homes: &Homes) {
    if let Some(base) = homes.frame_base {
        body.local_get(base);
        body.i32_const(homes.frame.size as i32);
        body.i32_add();
        body.global_set(STACK_POINTER);
    }
}

/// Copies a call's struct result out of the callee's frame into the caller's own slot for it.
///
/// The callee answers with an address in a frame it has already given back, so the bytes are intact
/// only until the next call takes a frame: `Two(Make(1), Make(2))` would read the second result
/// through both addresses. Copying at once makes every struct result the caller's own value, which
/// is also what keeps a frame's lifetime its call's.
///
/// An instruction that writes its struct straight into its slot, a call with no result, and a
/// struct with no bytes are left alone; every other producer of a struct value is a call into code
/// that owns the bytes.
fn adopt_struct_result(
    body: &mut Func,
    ctx: &WasmCtx,
    homes: &Homes,
    value_types: &[MirType],
    result: ValueId,
    inst: &Inst,
) -> Result<(), LowerError> {
    let Some(bytes) = struct_bytes(value_types, result) else {
        return Ok(());
    };
    if homes.slot(result).is_none() {
        return Ok(());
    }
    let produced = match inst {
        Inst::InitStruct
        | Inst::CopyStruct { .. }
        | Inst::FieldLoad { .. }
        | Inst::StaticLoad { .. } => false,
        Inst::Call { callee, .. } => ctx
            .funcs
            .get(*callee as usize)
            .is_some_and(|f| f.ret.is_some()),
        Inst::CallVirtual { returns_value, .. }
        | Inst::CallInterface { returns_value, .. }
        | Inst::CallIndirect { returns_value, .. }
        | Inst::InvokeDelegate { returns_value, .. } => *returns_value,
        _ => true,
    };
    if produced {
        let local = homes.local_of[result.index()];
        homes.copy_into_slot(body, result, local, bytes.div_ceil(4))?;
    }
    Ok(())
}

/// Zeroes the `bytes` (a multiple of 8) at the address in local `address`.
fn emit_zero(body: &mut Func, address: u32, bytes: u32) {
    if bytes <= 64 {
        for offset in (0..bytes).step_by(8) {
            body.local_get(address);
            body.i64_const(0);
            body.i64_store(MemArg::new(8, offset));
        }
    } else {
        body.local_get(address);
        body.i32_const(0);
        body.i32_const(bytes as i32);
        body.memory_fill();
    }
}

/// An exception the lowering raises itself, from a check it emits rather than from a `throw` in the
/// program. It is raised as an unhandled `throw` leaves a function -- its tag stored in flight, the
/// message word cleared, a zero returned -- so the caller's post-call test routes it to a handler or
/// passes it on, exactly as the ARM and RISC-V check stubs do.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Raise {
    /// A field access through a null object reference.
    NullReference,
    /// An allocation the heap cannot hold, because linear memory could not grow to fit it.
    OutOfMemory,
}

impl Raise {
    /// Every kind, in the order a function lays its exits.
    const ALL: [Raise; 2] = [Raise::NullReference, Raise::OutOfMemory];

    /// The tag the raise puts in flight: the builtin exception's, the word a `catch` of that type
    /// matches. The null reference takes the ARM and RISC-V stubs' own, so one `catch` matches all
    /// three targets' checks.
    fn tag(self) -> u32 {
        match self {
            Raise::NullReference => crate::cil::InlineCheck::NullReference.tag(),
            Raise::OutOfMemory => {
                lamella_metadata::exception_tag_for_name("System", "OutOfMemoryException")
            }
        }
    }
}

/// The object reference a field instruction dereferences, when it needs a null test first: a field
/// access through an `ObjectRef`, as on ARM and RISC-V. A managed or unmanaged pointer is not tested
/// (only an object reference is null here), and a struct held in the frame cannot be.
fn null_tested_base(inst: &Inst, value_types: &[MirType]) -> Option<ValueId> {
    let base = match inst {
        Inst::FieldLoad { base, .. }
        | Inst::FieldStore { base, .. }
        | Inst::FieldLoadNarrow { base, .. }
        | Inst::FieldStoreNarrow { base, .. }
        | Inst::FieldAddr { base, .. } => *base,
        _ => return None,
    };
    matches!(value_types.get(base.index()), Some(MirType::ObjectRef)).then_some(base)
}

/// The exceptions `func`'s checks can raise, in [`Raise::ALL`]'s order.
fn raises_of(func: &Function) -> Vec<Raise> {
    let insts = || {
        func.blocks
            .iter()
            .flat_map(|b| &b.insts)
            .map(|(_, inst)| inst)
    };
    Raise::ALL
        .into_iter()
        .filter(|raise| match raise {
            Raise::NullReference => {
                insts().any(|inst| null_tested_base(inst, &func.value_types).is_some())
            }
            Raise::OutOfMemory => insts().any(|inst| {
                matches!(
                    inst,
                    Inst::Alloc { .. }
                        | Inst::AllocLike { .. }
                        | Inst::AllocDescribed { .. }
                        | Inst::AllocArray { .. }
                        | Inst::AllocArray2D { .. }
                        | Inst::AllocArrayMD { .. }
                )
            }),
        })
        .collect()
}

/// Raises `raise` and returns from the function: the tail laid after the function's exit block for
/// that kind, which a failing check branches to.
///
/// The message word is cleared as well, as the ARM path's stub does: a raised exception carries no
/// message, and the word is shared, so leaving it would hand the handler the text of the last `throw
/// new E("...")`. The zero returned stands in for the function's result, which no caller reads while
/// a tag is in flight; for a struct result it is an address, and the caller's copy of the bytes there
/// is never observed.
fn emit_raise(
    body: &mut Func,
    homes: &Homes,
    ret: Option<MirType>,
    raise: Raise,
) -> Result<(), LowerError> {
    body.i32_const(STATIC_BASE);
    body.i32_const(raise.tag() as i32);
    body.i32_store(MemArg::new(4, crate::cil::G_EXCEPTION_TAG_OFFSET));
    body.i32_const(STATIC_BASE);
    body.i32_const(0);
    body.i32_store(MemArg::new(4, crate::cil::G_EXCEPTION_MESSAGE_OFFSET));
    match ret.map(valtype).transpose()? {
        Some(ValType::I32) => body.i32_const(0),
        Some(ValType::I64) => body.i64_const(0),
        Some(ValType::F32) => body.f32_const_bits(0),
        Some(ValType::F64) => body.f64_const_bits(0),
        None => {}
    }
    leave_frame(body, homes);
    body.return_();
    Ok(())
}

/// The branch depth of the exit block that raises `raise`, from the current point in the body.
fn raise_depth(scopes: &[Scope], raise: Raise) -> Result<u32, LowerError> {
    scopes
        .iter()
        .rev()
        .position(|scope| scope.kind == ScopeKind::Raise(raise))
        .map(|depth| depth as u32)
        .ok_or(LowerError::ControlFlowUnsupported)
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
    /// A `block` around the whole body that a failing check branches out of, to the code after its
    /// `end` that raises the exception (see [`Raise`]).
    Raise(Raise),
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
    homes: &Homes,
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
        emit_branches(cfg, func, ctx, homes, body, scopes, x, &merges)?;
        scopes.pop();
        body.end();
    } else {
        let merges = cfg.merge_children(x);
        emit_branches(cfg, func, ctx, homes, body, scopes, x, &merges)?;
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
    homes: &Homes,
    body: &mut Func,
    scopes: &mut Vec<Scope>,
    x: BlockId,
    merges: &[BlockId],
) -> Result<(), LowerError> {
    match merges.split_last() {
        None => emit_node(cfg, func, ctx, homes, body, scopes, x),
        Some((&outer, rest)) => {
            body.block(BlockType::Empty);
            scopes.push(Scope {
                kind: ScopeKind::Block,
                block: outer,
            });
            emit_branches(cfg, func, ctx, homes, body, scopes, x, rest)?;
            scopes.pop();
            body.end();
            emit_tree(cfg, func, ctx, homes, body, scopes, outer)
        }
    }
}

/// Emits `x`'s instructions and its terminator (a return, a trap, or a branch resolved against the
/// open scopes).
fn emit_node(
    cfg: &Cfg,
    func: &Function,
    ctx: &WasmCtx,
    homes: &Homes,
    body: &mut Func,
    scopes: &mut Vec<Scope>,
    x: BlockId,
) -> Result<(), LowerError> {
    let block = &func.blocks[x.index()];
    let local = |v: ValueId| homes.local_of[v.index()];
    for (result, inst) in &block.insts {
        lower_inst(
            body,
            &local,
            &func.value_types,
            ctx,
            homes,
            scopes,
            *result,
            inst,
        )?;
        adopt_struct_result(body, ctx, homes, &func.value_types, *result, inst)?;
    }
    match block.terminator.as_ref() {
        Some(Terminator::Return(value)) => {
            if let Some(v) = value {
                body.local_get(local(*v));
            }
            leave_frame(body, homes);
            body.return_();
            Ok(())
        }
        Some(Terminator::Unreachable) => {
            body.unreachable();
            Ok(())
        }
        Some(Terminator::Jump { target, args }) => {
            emit_edge(cfg, func, ctx, homes, body, scopes, x, *target, args)
        }
        Some(Terminator::Branch {
            cond,
            if_true,
            true_args,
            if_false,
            false_args,
        }) => emit_cond_branch(
            cfg, func, ctx, homes, body, scopes, x, *cond, *if_true, true_args, *if_false,
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
    homes: &Homes,
    body: &mut Func,
    scopes: &mut Vec<Scope>,
    x: BlockId,
    target: BlockId,
    args: &[ValueId],
) -> Result<(), LowerError> {
    emit_parallel_copy(
        body,
        homes,
        &func.value_types,
        args,
        &func.blocks[target.index()].params,
    )?;
    match disposition(cfg, x, target) {
        Disposition::BackEdge => body.br(depth_of(scopes, ScopeKind::Loop, target)?),
        Disposition::ForwardMerge => body.br(depth_of(scopes, ScopeKind::Block, target)?),
        Disposition::Inline => emit_tree(cfg, func, ctx, homes, body, scopes, target)?,
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
    homes: &Homes,
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
    let cond_local = homes.local_of[cond.index()];
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
            emit_tree(cfg, func, ctx, homes, body, scopes, if_false)
        }
        (None, Some(df_depth)) => {
            body.local_get(cond_local);
            body.i32_eqz();
            body.br_if(df_depth?);
            emit_tree(cfg, func, ctx, homes, body, scopes, if_true)
        }
        (None, None) => {
            body.local_get(cond_local);
            body.if_(BlockType::Empty);
            scopes.push(Scope {
                kind: ScopeKind::If,
                block: x,
            });
            emit_tree(cfg, func, ctx, homes, body, scopes, if_true)?;
            body.else_();
            emit_tree(cfg, func, ctx, homes, body, scopes, if_false)?;
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

/// Emits the block-parameter copies of an edge as one simultaneous assignment.
///
/// A struct parameter with a slot of its own takes a COPY of its argument's bytes (see [`Frame`]):
/// each word of each such parameter is one move between two frame slots, and the moves are ordered by
/// [`crate::parallel_moves::schedule`] so none overwrites a word a later one still reads, with one
/// local saving a word where the moves form a cycle. Every other parameter -- a scalar, or a struct
/// parameter that shares its argument's storage -- takes its argument's local: push every
/// non-identity source local, then pop them into the destination locals in reverse, so every source
/// is read before any destination is written, a swap included.
fn emit_parallel_copy(
    body: &mut Func,
    homes: &Homes,
    value_types: &[MirType],
    args: &[ValueId],
    params: &[ValueId],
) -> Result<(), LowerError> {
    use crate::parallel_moves::{SlotWord, schedule};
    let mut moves: Vec<(SlotWord<u32>, SlotWord<u32>)> = Vec::new();
    let mut sources: Vec<u32> = Vec::new();
    let mut dests: Vec<u32> = Vec::new();
    for (param, arg) in params.iter().zip(args) {
        if let Some(MirType::ValueType { size, .. }) = value_types.get(param.index())
            && homes.slot(*param).is_some()
            && !homes.frame.shared[param.index()]
        {
            let (Some((_, dst)), Some((_, src))) = (homes.slot(*param), homes.slot(*arg)) else {
                return Err(LowerError::ControlFlowUnsupported);
            };
            for word in 0..size.div_ceil(4) {
                moves.push((SlotWord::At(dst + word * 4), SlotWord::At(src + word * 4)));
            }
            continue;
        }
        let dst = homes.local_of[param.index()];
        let src = homes.local_of[arg.index()];
        if dst != src {
            sources.push(src);
            dests.push(dst);
        }
    }
    if !moves.is_empty() {
        let base = homes.frame_base.ok_or(LowerError::ControlFlowUnsupported)?;
        let mut saved: Option<u32> = None;
        for (dst, src) in schedule(&moves, SlotWord::Saved) {
            match (dst, src) {
                (SlotWord::At(dst), SlotWord::At(src)) => {
                    body.local_get(base);
                    body.local_get(base);
                    body.i32_load(MemArg::new(4, src));
                    body.i32_store(MemArg::new(4, dst));
                }
                (SlotWord::Saved, SlotWord::At(src)) => {
                    let word = *saved.get_or_insert_with(|| body.add_local(ValType::I32));
                    body.local_get(base);
                    body.i32_load(MemArg::new(4, src));
                    body.local_set(word);
                }
                (SlotWord::At(dst), SlotWord::Saved) => {
                    let word = saved.ok_or(LowerError::ControlFlowUnsupported)?;
                    body.local_get(base);
                    body.local_get(word);
                    body.i32_store(MemArg::new(4, dst));
                }
                (SlotWord::Saved, SlotWord::Saved) => {
                    return Err(LowerError::ControlFlowUnsupported);
                }
            }
        }
    }
    for &src in &sources {
        body.local_get(src);
    }
    for &dst in dests.iter().rev() {
        body.local_set(dst);
    }
    Ok(())
}

/// Lowers one value-defining instruction: it pushes its operands with `local.get`, emits the
/// operation, and stores the result into its local with `local.set` (a void side-effecting
/// instruction stores nothing). `scopes` are the scopes open around it, where a check finds the
/// exit block it raises through.
#[allow(clippy::too_many_arguments)]
fn lower_inst(
    body: &mut Func,
    local: &impl Fn(ValueId) -> u32,
    value_types: &[MirType],
    ctx: &WasmCtx,
    homes: &Homes,
    scopes: &[Scope],
    result: ValueId,
    inst: &Inst,
) -> Result<(), LowerError> {
    if let Some(base) = null_tested_base(inst, value_types) {
        body.local_get(local(base));
        body.i32_eqz();
        body.br_if(raise_depth(scopes, Raise::NullReference)?);
    }
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
        Inst::WriteInt { value } => {
            body.local_get(local(*value));
            let index = *ctx
                .native_imports
                .get(&console_write_int_import())
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
            emit_sized_load(body, *width, *signed, value_types[result.index()])?;
            body.local_set(local(result));
        }
        Inst::Store {
            address,
            value,
            width,
        } => {
            body.local_get(local(*address));
            body.local_get(local(*value));
            emit_sized_store(body, *width, value_types[value.index()])?;
        }
        Inst::FieldLoad { base, offset } => {
            if !is_addressable(value_types, *base) {
                return Err(LowerError::Unsupported);
            }
            if let MirType::ValueType { size, .. } = value_types[result.index()] {
                homes.name_made_struct(body, result)?;
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
            body.i64_const(i64::from(OBJECT_HEADER + payload_size));
            emit_heap_alloc(body, ctx, scopes, local(result))?;
            body.local_get(local(result));
            body.i32_const(desc);
            body.i32_store(MemArg::new(4, 0));
            body.local_get(local(result));
            body.i32_const(OBJECT_HEADER as i32);
            body.i32_add();
            body.local_set(local(result));
        }
        Inst::AllocDescribed {
            descriptor,
            payload_size,
        } => {
            body.local_get(local(*payload_size));
            body.i64_extend_i32_u();
            body.i64_const(i64::from(OBJECT_HEADER));
            body.i64_add();
            emit_heap_alloc(body, ctx, scopes, local(result))?;
            body.local_get(local(result));
            body.local_get(local(*descriptor));
            body.i32_store(MemArg::new(4, 0));
            body.local_get(local(result));
            body.i32_const(OBJECT_HEADER as i32);
            body.i32_add();
            body.local_set(local(result));
        }
        Inst::AllocLike {
            proto,
            payload_size,
        } => {
            body.i64_const(i64::from(OBJECT_HEADER + payload_size));
            emit_heap_alloc(body, ctx, scopes, local(result))?;
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
            let size = struct_size(value_types[result.index()])?;
            homes.name_made_struct(body, result)?;
            emit_zero(body, local(result), size as u32);
        }
        Inst::CopyStruct { src } => {
            let size = struct_size(value_types[result.index()])?;
            homes.name_made_struct(body, result)?;
            for word in 0..(size as u32).div_ceil(4) {
                let offset = word * 4;
                body.local_get(local(result));
                body.local_get(local(*src));
                body.i32_load(MemArg::new(4, offset));
                body.i32_store(MemArg::new(4, offset));
            }
        }
        Inst::AllocArray {
            handle,
            length,
            element_size,
            ..
        } => {
            let desc = ctx
                .desc_addr
                .get(&handle.0)
                .copied()
                .unwrap_or(handle.0 as i32);
            emit_byte_count(body, local, &[*length], *element_size, OBJECT_HEADER + 4);
            emit_heap_alloc(body, ctx, scopes, local(result))?;
            body.local_get(local(result));
            body.i32_const(desc);
            body.i32_store(MemArg::new(4, 0));
            body.local_get(local(result));
            body.local_get(local(*length));
            body.i32_store(MemArg::new(4, OBJECT_HEADER));
            body.local_get(local(result));
            body.i32_const(OBJECT_HEADER as i32);
            body.i32_add();
            body.local_set(local(result));
        }
        Inst::ArrayLoad {
            array,
            index,
            element_size,
            signed,
        } => {
            emit_bounds_check(body, local, *array, *index);
            emit_element_address(body, local, *array, *index, *element_size);
            emit_sized_load(body, *element_size, *signed, value_types[result.index()])?;
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
            emit_sized_store(body, *element_size, value_types[value.index()])?;
        }
        Inst::StaticLoad { owner, offset } => {
            if !matches!(owner, lamella_ir::StaticOwner::Own) {
                return Err(LowerError::Unsupported);
            }
            if let MirType::ValueType { size, .. } = value_types[result.index()] {
                homes.name_made_struct(body, result)?;
                for word in 0..size.div_ceil(4) {
                    body.local_get(local(result));
                    body.i32_const(STATIC_BASE);
                    body.i32_load(MemArg::new(4, *offset + word * 4));
                    body.i32_store(MemArg::new(4, word * 4));
                }
            } else {
                body.i32_const(STATIC_BASE);
                emit_typed_load(body, value_types[result.index()], *offset)?;
                body.local_set(local(result));
            }
        }
        Inst::StaticStore {
            owner,
            offset,
            value,
        } => {
            if !matches!(owner, lamella_ir::StaticOwner::Own) {
                return Err(LowerError::Unsupported);
            }
            if let MirType::ValueType { size, .. } = value_types[value.index()] {
                for word in 0..size.div_ceil(4) {
                    body.i32_const(STATIC_BASE);
                    body.local_get(local(*value));
                    body.i32_load(MemArg::new(4, word * 4));
                    body.i32_store(MemArg::new(4, *offset + word * 4));
                }
            } else {
                body.i32_const(STATIC_BASE);
                body.local_get(local(*value));
                emit_typed_store(body, value_types[value.index()], *offset)?;
            }
        }
        Inst::StaticAddr { owner, offset } => {
            if !matches!(owner, lamella_ir::StaticOwner::Own) {
                return Err(LowerError::Unsupported);
            }
            body.i32_const(STATIC_BASE + *offset as i32);
            body.local_set(local(result));
        }
        Inst::AllocArray2D {
            handle,
            dim0,
            dim1,
            element_size,
            element_kind: _,
        } => {
            let desc = ctx
                .desc_addr
                .get(&handle.0)
                .copied()
                .unwrap_or(handle.0 as i32);
            emit_byte_count(
                body,
                local,
                &[*dim0, *dim1],
                *element_size,
                OBJECT_HEADER + 8,
            );
            emit_heap_alloc(body, ctx, scopes, local(result))?;
            body.local_get(local(result));
            body.i32_const(desc);
            body.i32_store(MemArg::new(4, 0));
            body.local_get(local(result));
            body.local_get(local(*dim0));
            body.i32_store(MemArg::new(4, OBJECT_HEADER));
            body.local_get(local(result));
            body.local_get(local(*dim1));
            body.i32_store(MemArg::new(4, OBJECT_HEADER + 4));
            body.local_get(local(result));
            body.i32_const(OBJECT_HEADER as i32);
            body.i32_add();
            body.local_set(local(result));
        }
        Inst::Array2DLoad {
            array,
            index0,
            index1,
            element_size,
            signed,
        } => {
            emit_2d_element_address(body, local, *array, *index0, *index1, *element_size);
            emit_sized_load(body, *element_size, *signed, value_types[result.index()])?;
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
            emit_sized_store(body, *element_size, value_types[value.index()])?;
        }
        Inst::FuncAddr { func } => {
            body.i32_const((ctx.import_count + *func) as i32);
            body.local_set(local(result));
        }
        Inst::VirtualFuncAddr { object, slot, .. } => {
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
            ..
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
            emit_itable_base(body, local(*descriptor));
            body.local_set(ptr);
            body.local_get(ptr);
            body.i32_load(MemArg::new(4, 0));
            body.local_set(count);
            body.local_get(ptr);
            body.i32_const(4);
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
            body.br(2);
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
            emit_itable_base(body, desc);
            body.local_set(ptr);
            body.local_get(ptr);
            body.i32_load(MemArg::new(4, 0));
            body.local_set(count);
            body.local_get(ptr);
            body.i32_const(4);
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
            emit_class_word_guarded(body, cur, 4);
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
            handle,
            dims,
            element_size,
            element_kind: _,
        } => {
            let desc = ctx
                .desc_addr
                .get(&handle.0)
                .copied()
                .unwrap_or(handle.0 as i32);
            let header = OBJECT_HEADER + 4 * dims.len() as u32;
            emit_byte_count(body, local, dims, *element_size, header);
            emit_heap_alloc(body, ctx, scopes, local(result))?;
            body.local_get(local(result));
            body.i32_const(desc);
            body.i32_store(MemArg::new(4, 0));
            for (k, d) in dims.iter().enumerate() {
                body.local_get(local(result));
                body.local_get(local(*d));
                body.i32_store(MemArg::new(4, OBJECT_HEADER + (4 * k) as u32));
            }
            body.local_get(local(result));
            body.i32_const(OBJECT_HEADER as i32);
            body.i32_add();
            body.local_set(local(result));
        }
        Inst::Array2DElemAddr {
            array,
            index0,
            index1,
            element_size,
        } => {
            emit_2d_element_address(body, local, *array, *index0, *index1, *element_size);
            body.local_set(local(result));
        }
        Inst::ArrayMDElemAddr {
            array,
            indices,
            element_size,
        } => {
            emit_md_element_address(body, local, *array, indices, *element_size);
            body.local_set(local(result));
        }
        Inst::ArrayMDLoad {
            array,
            indices,
            element_size,
            signed,
        } => {
            emit_md_element_address(body, local, *array, indices, *element_size);
            emit_sized_load(body, *element_size, *signed, value_types[result.index()])?;
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
            emit_sized_store(body, *element_size, value_types[value.index()])?;
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
/// `ObjectRef`, a managed pointer, an unmanaged pointer, or a value-type instance (its local is the
/// address of its slot).
///
/// A `NativeInt` base IS a pointer, and the distinction is not a performance one. An UNMANAGED
/// pointer (`T*`) is CIL's `native int`: `conv.u`/`conv.i` is where a managed pointer stops being
/// tracked, and a pointer local is declared `NativeInt` outright, so `p->f` through an `S* p`
/// arrives here with a `NativeInt` base. The two answers read DIFFERENT MEMORY -- a pointer base is
/// dereferenced, anything else is taken as an instance living in the value's own slot -- so
/// classifying one as inline answers the pointer itself where the field was asked for, and stores to
/// the pointer instead of through it. Nothing else can be a field base: a base is an address or an
/// inline struct, and an inline struct is a `ValueType`.
fn is_addressable(value_types: &[MirType], value: ValueId) -> bool {
    matches!(
        value_types.get(value.index()),
        Some(
            MirType::ObjectRef
                | MirType::ManagedPtr
                | MirType::NativeInt
                | MirType::ValueType { .. }
        )
    )
}

/// Pushes the word at `[descriptor + offset]` -- or `0` when `descriptor` is an ARRAY descriptor
/// rather than a class one, which every caller reads as "there is nothing here".
///
/// THE TWO SHAPES DISAGREE ABOUT EVERY WORD BUT THEIR LAST TWO, so a class reader let loose on an
/// array descriptor does not fault -- it gets a plausible number. `base_ptr@4` is the element KIND,
/// so a `castclass` on an array would walk to address 4 or 5 and read a "descriptor" out of the null
/// guard; `itable_count@8` is the TYPE TAG, so an interface search would scan tens of thousands of
/// entries past the end of the descriptor and could match a tag by accident and `call_indirect`
/// whatever word followed it. The MARK is in the top byte of word 0 precisely so ONE load and one
/// compare separates them, and no handle can collide: handle table bytes are `0x01`..`0x0D` and
/// `0x1B`, and [`ARRAY_DESC_MARK`](crate::resolver::ARRAY_DESC_MARK) is `0xA5`.
///
/// ONE READER TAKES THIS ANSWER NOW -- the base-chain scan -- because the two itable readers reach
/// their word through [`emit_itable_base`] instead: an array HAS an itable, at a different offset,
/// so "there is nothing here" stopped being true for them. It stays a function because the base
/// chain is the position where the answer is still a refusal, and a refusal written out inline is
/// the one that gets forgotten.
fn emit_class_word_guarded(body: &mut Func, descriptor: u32, offset: u32) {
    body.local_get(descriptor);
    body.i32_load(MemArg::new(4, 0));
    body.i32_const(24);
    body.i32_shr_u();
    body.i32_const((crate::resolver::ARRAY_DESC_MARK >> 24) as i32);
    body.i32_eq();
    body.if_(BlockType::Value(ValType::I32));
    body.i32_const(0);
    body.else_();
    body.local_get(descriptor);
    body.i32_load(MemArg::new(4, offset));
    body.end();
}

/// Pushes the ADDRESS of a descriptor's itable COUNT word, for EITHER shape.
///
/// The two shapes keep their itable in the same relative place -- immediately past their header
/// -- and disagree only about how long that header is. A class's varies
/// (`[id][base_ptr][count]`, so the count is at 8); an array's is the fixed
/// [`crate::resolver::ARRAY_DESC_WORDS`], so its count is at
/// [`crate::resolver::ARRAY_ITABLE_OFFSET`]. Reading a class's offset out of an array
/// descriptor lands on the TYPE TAG, which a search then treats as an entry count -- tens of
/// thousands of entries past the descriptor, able to match a tag by accident.
///
/// One function rather than the choice written out at each reader, for the reason its neighbour
/// above gives: they are two positions of one rule, and a third would otherwise be the third
/// place to forget it.
fn emit_itable_base(body: &mut Func, descriptor: u32) {
    body.local_get(descriptor);
    body.i32_load(MemArg::new(4, 0));
    body.i32_const(24);
    body.i32_shr_u();
    body.i32_const((crate::resolver::ARRAY_DESC_MARK >> 24) as i32);
    body.i32_eq();
    body.if_(BlockType::Value(ValType::I32));
    body.local_get(descriptor);
    body.i32_const(crate::resolver::ARRAY_ITABLE_OFFSET as i32);
    body.i32_add();
    body.else_();
    body.local_get(descriptor);
    body.i32_const(8);
    body.i32_add();
    body.end();
}

/// Takes the byte count on the stack (an i64) from the heap through the module's allocator, leaving
/// the block's address in local `into`. An allocation the heap cannot hold raises
/// OutOfMemoryException through the function's exit for it.
fn emit_heap_alloc(
    body: &mut Func,
    ctx: &WasmCtx,
    scopes: &[Scope],
    into: u32,
) -> Result<(), LowerError> {
    body.call(ctx.heap_alloc.ok_or(LowerError::Unsupported)?);
    body.local_tee(into);
    body.i32_eqz();
    body.br_if(raise_depth(scopes, Raise::OutOfMemory)?);
    Ok(())
}

/// Pushes, as an i64, the bytes an allocation of `header` bytes and then one `element_size` element
/// per count takes, the count being the product of `counts` (each an `i32` read unsigned).
///
/// No block past 2^32 bytes can be allocated, so each partial product is held to 2^32, which keeps
/// the arithmetic exact: a product of two numbers below 2^32 is exact in 64 bits, and so the size
/// that reaches the allocator is either the true one or one it refuses. A count is read unsigned
/// because the bounds checks compare indexes against it unsigned, so no array's block is shorter
/// than its length word says.
fn emit_byte_count(
    body: &mut Func,
    local: &impl Fn(ValueId) -> u32,
    counts: &[ValueId],
    element_size: u32,
    header: u32,
) {
    const CAP: i64 = 1 << 32;
    let partial = body.add_local(ValType::I64);
    body.i64_const(i64::from(element_size));
    for &count in counts {
        body.local_get(local(count));
        body.i64_extend_i32_u();
        body.i64_mul();
        body.local_set(partial);
        body.i64_const(CAP);
        body.local_get(partial);
        body.local_get(partial);
        body.i64_const(CAP);
        body.i64_gt_u();
        body.select();
    }
    body.i64_const(i64::from(header));
    body.i64_add();
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

/// Loads `width` bytes from the address on the stack as a value of `result_ty` -- an array element,
/// or an indirect load through a pointer -- sign- or zero-extending a sub-word value per `signed`.
///
/// The width and the type must agree, and a pair that does not is refused rather than emitted: an
/// access of one width into a value of another is a module no engine accepts.
fn emit_sized_load(
    body: &mut Func,
    width: u32,
    signed: bool,
    result_ty: MirType,
) -> Result<(), LowerError> {
    let m = MemArg::new(width, 0);
    match (width, valtype(result_ty)?) {
        (1, ValType::I32) if signed => body.i32_load8_s(m),
        (1, ValType::I32) => body.i32_load8_u(m),
        (2, ValType::I32) if signed => body.i32_load16_s(m),
        (2, ValType::I32) => body.i32_load16_u(m),
        (4, ValType::I32) => body.i32_load(m),
        (4, ValType::F32) => body.f32_load(m),
        (8, ValType::I64) => body.i64_load(m),
        (8, ValType::F64) => body.f64_load(m),
        _ => return Err(LowerError::Unsupported),
    }
    Ok(())
}

/// Stores the value of `value_ty` on the stack (under its address) as `width` bytes -- an array
/// element, or an indirect store through a pointer. The width and the type must agree, as for
/// [`emit_sized_load`].
fn emit_sized_store(body: &mut Func, width: u32, value_ty: MirType) -> Result<(), LowerError> {
    let m = MemArg::new(width, 0);
    match (width, valtype(value_ty)?) {
        (1, ValType::I32) => body.i32_store8(m),
        (2, ValType::I32) => body.i32_store16(m),
        (4, ValType::I32) => body.i32_store(m),
        (4, ValType::F32) => body.f32_store(m),
        (8, ValType::I64) => body.i64_store(m),
        (8, ValType::F64) => body.f64_store(m),
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
/// they stay within the WASM 1.0 (MVP) instruction set. A float-to-integer truncation takes the
/// saturating form, which is past the MVP: WebAssembly 2.0's non-trapping float-to-int, as
/// `memory.copy` and `memory.fill` are its bulk memory.
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
        ConvKind::Float32ToInt => body.i32_trunc_sat_f32_s(),
        ConvKind::IntToFloat32 => body.f32_convert_i32_s(),
        ConvKind::Float64ToInt => body.i32_trunc_sat_f64_s(),
        ConvKind::Float32ToLong => body.i64_trunc_sat_f32_s(),
        ConvKind::Float64ToLong => body.i64_trunc_sat_f64_s(),
        ConvKind::Float32ToULong => body.i64_trunc_sat_f32_u(),
        ConvKind::Float64ToULong => body.i64_trunc_sat_f64_u(),
        ConvKind::IntToFloat64 => body.f64_convert_i32_s(),
        ConvKind::LongToFloat64 => body.f64_convert_i64_s(),
        ConvKind::Float32ToFloat64 => body.f64_promote_f32(),
        ConvKind::Float64ToFloat32 => body.f32_demote_f64(),
        ConvKind::LongToFloat32 => body.f32_convert_i64_s(),
        ConvKind::UIntToFloat64 => body.f64_convert_i32_u(),
        ConvKind::ULongToFloat64 => body.f64_convert_i64_u(),
        ConvKind::IntToRef | ConvKind::RefToInt | ConvKind::ToNativeInt => {}
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

    /// A `NativeInt` base is an UNMANAGED POINTER and holds a linear-memory address like any other.
    ///
    /// `T*` is CIL's `native int`, so `p->f` on a struct pointer arrives with one. Rejecting it
    /// refuses a program this backend can lower -- and the same classification on ARM is a silent
    /// wrong answer rather than a refusal, which is why it is pinned in each backend that has one.
    #[test]
    fn an_unmanaged_pointer_is_an_addressable_field_base() {
        let types = [
            MirType::ObjectRef,
            MirType::ManagedPtr,
            MirType::NativeInt,
            MirType::I32,
        ];
        assert!(is_addressable(&types, ValueId(0)));
        assert!(is_addressable(&types, ValueId(1)));
        assert!(is_addressable(&types, ValueId(2)), "`T*` is `native int`");
        assert!(!is_addressable(&types, ValueId(3)));
    }

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

    /// Whether a module declares a linear memory -- a memory section, id 5 -- read off its bytes.
    fn declares_memory(module: &[u8]) -> bool {
        let mut at = 8;
        while at < module.len() {
            let id = module[at];
            at += 1;
            let (mut size, mut shift) = (0usize, 0);
            loop {
                let byte = module[at];
                at += 1;
                size |= usize::from(byte & 0x7F) << shift;
                shift += 7;
                if byte & 0x80 == 0 {
                    break;
                }
            }
            if id == 5 {
                return true;
            }
            at += size;
        }
        false
    }

    /// A module whose one memory access is a read of an object's type descriptor declares a memory.
    /// Which bodies need one is read off what each emitted, so it holds for every instruction that
    /// reads memory -- a type test, a virtual, interface or delegate call -- and not only for the
    /// instructions a list of them names.
    #[test]
    fn a_module_declares_memory_for_any_body_that_reads_it() {
        let func = Function {
            params: alloc::vec![MirType::ObjectRef],
            ret: Some(MirType::I32),
            value_types: alloc::vec![MirType::ObjectRef, MirType::I32],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: alloc::vec![ValueId(0)],
                insts: alloc::vec![(ValueId(1), Inst::LoadTypeDesc { object: ValueId(0) })],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        let bytes = lower(&func).expect("a type-descriptor read lowers");
        assert!(declares_memory(&bytes), "the body reads `[object - 4]`");
    }

    /// A static access past the region the program declares is refused rather than laid over the
    /// string literals, which begin where the region ends. Left undeclared, the region is the
    /// module's own reach, and the same function lowers.
    #[test]
    fn a_static_access_past_the_declared_region_is_refused() {
        let i32t = MirType::I32;
        let func = Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: alloc::vec![i32t],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: Vec::new(),
                insts: alloc::vec![(
                    ValueId(0),
                    Inst::StaticLoad {
                        owner: lamella_ir::StaticOwner::Own,
                        offset: 100,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let program = core::slice::from_ref(&func);
        assert_eq!(
            lower_module_with_exports(program, &[("main", 0)], &[], None, Some(64)),
            Err(LowerError::StaticOutsideRegion {
                end: 104,
                region: 64
            })
        );
        assert!(lower_module_with_exports(program, &[("main", 0)], &[], None, None).is_ok());
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
                            element_cast_class: 0,
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
            refs: lamella_ir::RefWords::NONE,
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
        assert!(declares_memory(&bytes));
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
        assert!(declares_memory(&bytes));
    }

    /// A `callvirt` lowers to a `call_indirect` through the funcref table: func0 allocates a type whose
    /// vtable slot 0 is func1, then dispatches -- so the module gains a table section, an element
    /// segment, and the indirect-call opcode.
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
                            declaring_type: None,
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
            full_name: None,
        }];
        let bytes = lower_module_with_exports(&[caller, target], &[("main", 0)], &descriptors, None, None)
            .expect("the callvirt lowers to WASM");
        assert_eq!(&bytes[0..4], &[0x00, 0x61, 0x73, 0x6D]);
        assert!(bytes.contains(&0x11), "the call_indirect opcode is emitted");
        assert!(
            bytes.windows(2).any(|w| w == [0x70, 0x00]),
            "a funcref table is declared"
        );
    }

    /// An interface `callvirt` lowers to an itable search + `call_indirect`: the descriptor carries the
    /// `(tag, funcref-index)` pair in a data segment, and the module gains the indirect-call opcode.
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
            full_name: None,
        }];
        let bytes = lower_module_with_exports(&[caller, target], &[("main", 0)], &descriptors, None, None)
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
                full_name: None,
            },
            TypeMeta {
                handle: lamella_ir::TypeHandle(1),
                type_tag: 0,
                vtable: Vec::new(),
                itable: Vec::new(),
                base: None,
                words: None,
                exported: true,
                full_name: None,
            },
        ];
        let bytes = lower_module_with_exports(&[main], &[("main", 0)], &descriptors, None, None)
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
    /// matches a `void` target; assuming an i32 result would trap as a type mismatch.
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
                            declaring_type: None,
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
            full_name: None,
        }];
        let bytes = lower_module_with_exports(&[caller, target], &[("main", 0)], &descriptors, None, None)
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
        assert!(!declares_memory(&lower(&add_constants()).expect("40 + 2 lowers")));
        assert!(declares_memory(&lower(&object_fields()).expect("field access lowers")));
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
        assert!(declares_memory(&bytes));
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
            refs: lamella_ir::RefWords::NONE,
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
        assert!(declares_memory(&bytes));
    }

    /// A function with no struct value keeps nothing in memory, so it takes no frame and its module
    /// carries no shadow stack.
    #[test]
    fn a_function_with_no_struct_value_has_no_frame() {
        assert_eq!(frame_layout(&add_constants()).size, 0);
        assert_eq!(frame_layout(&loop_sum()).size, 0);
        let frame = frame_layout(&struct_copy());
        assert_eq!(frame.size, 16, "two 8-byte structs, one slot each");
        assert_eq!(frame.slots[0], Some(0));
        assert_eq!(frame.slots[5], Some(8));
    }

    /// A struct with no bytes -- how a value type from an assembly the build does not attach arrives
    /// -- owns no slot, so the value a caller passes for it reaches the body unchanged.
    #[test]
    fn a_struct_with_no_bytes_owns_no_slot() {
        let opaque = MirType::ValueType {
            handle: lamella_ir::TypeHandle(7),
            size: 0,
            refs: lamella_ir::RefWords::NONE,
        };
        let pass_through = Function {
            params: alloc::vec![opaque],
            ret: Some(opaque),
            value_types: alloc::vec![opaque],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: alloc::vec![ValueId(0)],
                insts: Vec::new(),
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let frame = frame_layout(&pass_through);
        assert_eq!(frame.size, 0, "nothing to keep, so no frame");
        assert_eq!(frame.slots[0], None);
    }

    /// Two blocks: block 0 makes a struct (value 0) and takes a pointer into it (value 1), then jumps
    /// to block 1, whose struct parameter (value 2) receives the struct. `pass_pointer` also passes
    /// the pointer along the edge, into block 1's second parameter (value 3); `read_in_block_1` has
    /// block 1 read through the pointer instead (value 4).
    fn pointer_into_a_struct(pass_pointer: bool, read_in_block_1: bool) -> Function {
        let vt = MirType::ValueType {
            handle: lamella_ir::TypeHandle(1),
            size: 8,
            refs: lamella_ir::RefWords::NONE,
        };
        let mut args = alloc::vec![ValueId(0)];
        let mut params = alloc::vec![ValueId(2)];
        if pass_pointer {
            args.push(ValueId(1));
            params.push(ValueId(3));
        }
        let mut insts = Vec::new();
        if read_in_block_1 {
            insts.push((
                ValueId(4),
                Inst::Load {
                    address: ValueId(1),
                    width: 4,
                    signed: false,
                },
            ));
        }
        Function {
            params: Vec::new(),
            ret: None,
            value_types: alloc::vec![vt, MirType::ManagedPtr, vt, MirType::ManagedPtr, MirType::I32],
            entry: BlockId(0),
            blocks: alloc::vec![
                BasicBlock {
                    params: Vec::new(),
                    insts: alloc::vec![
                        (ValueId(0), Inst::InitStruct),
                        (
                            ValueId(1),
                            Inst::FieldAddr {
                                base: ValueId(0),
                                offset: 0,
                            },
                        ),
                    ],
                    terminator: Some(Terminator::Jump {
                        target: BlockId(1),
                        args,
                    }),
                },
                BasicBlock {
                    params,
                    insts,
                    terminator: Some(Terminator::Return(None)),
                },
            ],
        }
    }

    /// A struct block parameter takes a COPY of its argument unless a pointer into the argument's
    /// storage outlives the block that took it. Passed along an edge, or read in another block, the
    /// pointer names the same storage as the parameter, so the parameter shares that storage and takes
    /// no slot of its own; used only where it was taken, it does not, and the parameter copies.
    #[test]
    fn a_struct_parameter_shares_its_arguments_storage_only_when_a_pointer_into_it_escapes() {
        let kept_local = frame_layout(&pointer_into_a_struct(false, false));
        assert!(!kept_local.shared[2], "no pointer leaves block 0, so the parameter copies");
        assert!(kept_local.slots[2].is_some(), "a copying parameter owns a slot");

        let passed_along = frame_layout(&pointer_into_a_struct(true, false));
        assert!(passed_along.shared[2], "the pointer travels with the struct along the edge");
        assert!(passed_along.slots[2].is_none(), "a sharing parameter owns no slot");

        let read_later = frame_layout(&pointer_into_a_struct(false, true));
        assert!(read_later.shared[2], "block 1 reads through block 0's pointer");
        assert!(read_later.slots[2].is_none());

        assert!(
            kept_local.slots[0].is_some() && passed_along.slots[0].is_some(),
            "an instruction's struct result always owns its slot"
        );
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
                            element_kind: 5,
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
        assert!(declares_memory(&bytes));
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
                            element_kind: 5,
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
        assert!(declares_memory(&bytes));
    }


    /// `System.String` and `string[]` as this backend spells them: a TypeRef-keyed handle, which is
    /// the identity a reference-less build mints (`qualified_type_handle` keeps a TypeRef's raw token
    /// when no reference resolves it), lifted to the array's own by `array_handle`.
    fn string_handles() -> (u32, u32) {
        let string = lamella_ir::TypeHandle(0x0100_0008);
        (string.0, lamella_ir::array_handle(string).0)
    }

    /// One function allocating a `string[]`, plus a `StringLiteral` per entry in `literals`.
    fn string_array_program(literals: &[&str]) -> Function {
        let (string, array) = string_handles();
        let mut value_types = alloc::vec![MirType::I32, MirType::ObjectRef];
        let mut insts = alloc::vec![
            (
                ValueId(0),
                Inst::ConstInt {
                    ty: MirType::I32,
                    value: 1,
                },
            ),
            (
                ValueId(1),
                Inst::AllocArray {
                    handle: lamella_ir::TypeHandle(array),
                    element: Some(lamella_ir::TypeHandle(string)),
                    length: ValueId(0),
                    element_size: 4,
                    element_kind: crate::resolver::ELEMENT_KIND_REFERENCE,
                    element_cast_class: 0,
                },
            ),
        ];
        for text in literals {
            let id = ValueId(value_types.len() as u32);
            value_types.push(MirType::ObjectRef);
            insts.push((
                id,
                Inst::StringLiteral {
                    utf16: text.encode_utf16().collect::<Vec<u16>>().into_boxed_slice(),
                },
            ));
        }
        Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types,
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: Vec::new(),
                insts,
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        }
    }

    /// The address [`layout_strings`] rewrote the program's one literal to -- the OBJECT, past its
    /// header.
    fn literal_object(program: &[Function]) -> u32 {
        program
            .iter()
            .flat_map(|f| f.blocks.iter().flat_map(|b| &b.insts))
            .find_map(|(_, inst)| match inst {
                Inst::ConstInt {
                    ty: MirType::ObjectRef,
                    value,
                } => Some(*value as u32),
                _ => None,
            })
            .expect("the literal was rewritten to an ObjectRef constant")
    }

    /// Reads `count` words of the descriptor at `addr` out of the laid segments.
    fn descriptor_words(segments: &[(u32, Vec<u8>)], addr: u32, count: usize) -> Vec<u32> {
        let (base, blob) = segments
            .iter()
            .find(|(base, blob)| addr >= *base && addr < base + blob.len() as u32)
            .expect("the descriptor's address falls inside a laid segment");
        let start = (addr - base) as usize;
        blob[start..start + count * 4]
            .chunks(4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .collect()
    }

    /// An ARRAY descriptor takes the shared, ratified format --
    /// `[MARK | rank][element_kind][type_tag][base_ptr][element_desc]` -- and its fifth word is a
    /// RELATIVE displacement to the element type's descriptor, because the SHARED covariant-store
    /// check in `cil.rs` reads it as one: it loads the word and adds the descriptor's own address
    /// back on. An absolute address laid there would be read as a displacement and land on nothing.
    #[test]
    fn an_array_descriptor_takes_the_ratified_form_with_a_relative_element_edge() {
        let (string, array) = string_handles();
        let program = alloc::vec![string_array_program(&[])];
        let (segments, map, _end) = layout_descriptors(&program, &[], Some(string), 0x400, 0);
        let array_desc = map[&array] as u32;
        let string_desc = map[&string] as u32;
        let words = descriptor_words(&segments, array_desc, 5);
        assert_eq!(
            words[0],
            crate::resolver::ARRAY_DESC_MARK | 1,
            "word 0 is MARK | rank, which is what tells a class reader to stop"
        );
        assert_eq!(
            words[1],
            crate::resolver::ELEMENT_KIND_REFERENCE,
            "word 1 is the element KIND, where a class descriptor carries its itable count"
        );
        assert_eq!(
            words[3], 0,
            "no base chain is laid for an array on this target"
        );
        assert_ne!(words[4], 0, "the element edge is present, not the ABSENT zero");
        assert_eq!(
            array_desc.wrapping_add(words[4]),
            string_desc,
            "element_desc@16 is a displacement that lands on System.String's descriptor"
        );
    }

    /// A string LITERAL's object header and a `string[]`'s `element_desc@16` must name ONE address:
    /// the store check scans the value's descriptor against the element's, so two identities for
    /// `System.String` refuse a legal store, and a literal with no header at all reads a type out of
    /// whatever precedes its blob.
    #[test]
    fn a_string_literal_and_a_string_array_element_name_one_descriptor() {
        let (string, array) = string_handles();
        let mut program = alloc::vec![string_array_program(&["hello"])];
        let base = (STATIC_BASE as u32 + static_reach(&program)).next_multiple_of(8);
        let mut strings = layout_strings(&mut program, base).expect("the literal lays out");
        let (segments, map, _end) =
            layout_descriptors(&program, &[], Some(string), strings.heap_base as u32, 0);
        let string_desc = map[&string] as u32;
        patch_string_headers(&mut strings.segments, string_desc as i32);
        let (base, literal) = &strings.segments[0];
        let object = literal_object(&program);
        assert_eq!(
            object,
            base + OBJECT_HEADER,
            "the literal carries a header word before it"
        );
        let header = (object - OBJECT_HEADER - base) as usize;
        assert_eq!(
            u32::from_le_bytes([
                literal[header],
                literal[header + 1],
                literal[header + 2],
                literal[header + 3],
            ]),
            string_desc,
            "and it names System.String's descriptor"
        );
        let element = descriptor_words(&segments, map[&array] as u32, 5)[4];
        assert_eq!(
            (map[&array] as u32).wrapping_add(element),
            string_desc,
            "and so does the array's element word -- ONE identity, reached two ways"
        );
    }

    /// The literal's `ObjectRef` points PAST its header, at the length word -- so `String.Length`,
    /// the storage readers and `[obj - 4]` all address what they expect.
    #[test]
    fn a_literal_points_past_its_header_at_the_length_word() {
        let mut program = alloc::vec![string_array_program(&["hi"])];
        let base = (STATIC_BASE as u32 + static_reach(&program)).next_multiple_of(8);
        let strings = layout_strings(&mut program, base).expect("the literal lays out");
        let (base, blob) = &strings.segments[0];
        let object = literal_object(&program);
        assert_eq!(
            object,
            base + OBJECT_HEADER,
            "the object begins past the header"
        );
        let length = (object - base) as usize;
        assert_eq!(
            u32::from_le_bytes([
                blob[length],
                blob[length + 1],
                blob[length + 2],
                blob[length + 3],
            ]),
            2,
            "and offset 0 of the object is the unit count"
        );
    }

    /// A rank-2+ array is HEADED but NOT marked as an array, and that is a recorded gap rather than
    /// an accident: `AllocArray2D`/`AllocArrayMD` carry no element KIND, and word 1 of the array form
    /// is exactly that. Marking one and guessing the kind would tell a consumer to stride or trace by
    /// a width that is not the element's. The ARM backend makes the same choice; this pins it so that
    /// closing it is deliberate on both rather than silent on one.
    #[test]
    fn a_rank_2_array_is_headed_but_not_marked_as_an_array() {
        let handle = lamella_ir::TypeHandle(0x0500_0009);
        let program = alloc::vec![Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: alloc::vec![MirType::I32, MirType::ObjectRef],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: Vec::new(),
                insts: alloc::vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 1,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::AllocArray2D {
                            handle,
                            dim0: ValueId(0),
                            dim1: ValueId(0),
                            element_size: 4,
                            element_kind: 5,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        }];
        let (segments, map, _end) = layout_descriptors(&program, &[], None, 0x400, 0);
        let words = descriptor_words(&segments, map[&handle.0] as u32, 3);
        assert_eq!(words[0], handle.0, "word 0 is the type id -- the class form");
        assert_ne!(
            words[0] & crate::resolver::ARRAY_DESC_MARK_MASK,
            crate::resolver::ARRAY_DESC_MARK,
            "and it carries no array MARK, so no consumer strides it"
        );
    }

    /// Every reader of a CLASS descriptor's words guards on the MARK before reading one. The guard is
    /// ONE shared emitter, so this looks for its signature -- `i32.const 24; i32.shr_u`, the top-byte
    /// extraction it opens with -- rather than asserting three hand-written copies that could drift.
    #[test]
    fn a_class_word_reader_guards_on_the_array_mark() {
        let (string, _array) = string_handles();
        let mut program = string_array_program(&[]);
        let desc = ValueId(program.value_types.len() as u32);
        program.value_types.push(MirType::I32);
        let target = ValueId(program.value_types.len() as u32);
        program.value_types.push(MirType::I32);
        let scan = ValueId(program.value_types.len() as u32);
        program.value_types.push(MirType::I32);
        program.blocks[0]
            .insts
            .push((desc, Inst::LoadTypeDesc { object: ValueId(1) }));
        program.blocks[0].insts.push((
            target,
            Inst::TypeDescAddr {
                handle: lamella_ir::TypeHandle(string),
            },
        ));
        program.blocks[0].insts.push((
            scan,
            Inst::CastClassScan {
                args: alloc::vec![desc, target],
            },
        ));
        let bytes = lower_module_with_exports(
            core::slice::from_ref(&program),
            &[("main", 0)],
            &[],
            Some(string),
            None,
        )
        .expect("the scan lowers");
        assert!(
            bytes.windows(3).any(|w| w == [0x41, 0x18, 0x76]),
            "the castclass walk extracts the descriptor's top byte before reading base_ptr@4"
        );
    }

    /// A GENERATED HELPER EMITS LITERALS OF ITS OWN, so the string layout has to run AFTER the
    /// helpers are appended. `null_safe_concat_operands` substitutes the empty string for a null
    /// operand; laid first, that `StringLiteral` reached the per-instruction lowering un-interned and
    /// fell through to `Unsupported`.
    #[test]
    fn a_concat_lowers_and_the_helpers_own_literals_are_interned() {
        let (string, _array) = string_handles();
        let program = Function {
            params: Vec::new(),
            ret: Some(MirType::ObjectRef),
            value_types: alloc::vec![MirType::ObjectRef, MirType::ObjectRef, MirType::ObjectRef],
            entry: BlockId(0),
            blocks: alloc::vec![BasicBlock {
                params: Vec::new(),
                insts: alloc::vec![
                    (
                        ValueId(0),
                        Inst::StringLiteral {
                            utf16: alloc::vec![0x0061].into_boxed_slice(),
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::StringLiteral {
                            utf16: alloc::vec![0x0062].into_boxed_slice(),
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::StringConcat {
                            lhs: ValueId(0),
                            rhs: ValueId(1),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        let bytes = lower_module_with_exports(
            core::slice::from_ref(&program),
            &[("main", 0)],
            &[],
            Some(string),
            None,
        )
        .expect("a concat lowers to WASM -- it did not, at any string handle, before the reorder");
        assert_eq!(&bytes[0..4], &[0x00, 0x61, 0x73, 0x6D]);
    }
}
