//! Lowering CIL method bodies to the middle IR by abstract interpretation.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use lamella_cil::{EhClause, EhKind, Instruction, MethodBodyImage, Opcode, Operand, OperandKind};
use lamella_ir::{
    BasicBlock, BinOp, BlockId, CmpOp, ConvKind, Function, Inst, MirType, Terminator, TypeHandle,
    ValueId,
};

/// The reserved static-region offset of `g_exception_tag`: the no-GC exception model's
/// in-flight tag word. A `throw` stores the thrown type's tag here; a catch dispatch loads it
/// and compares; zero means no exception is propagating. User statics start past it (the
/// resolver shifts them), so a throw/dispatch and an `ldsfld`/`stsfld` never alias.
const G_EXCEPTION_TAG_OFFSET: u32 = 0;

/// A single-cast delegate's field offsets within its heap object: `object _target` (a GC ref) first,
/// then `IntPtr _methodPtr` (the `ldftn` code address). The object pointer is the payload start, so
/// these are absolute offsets. The layout matches the managed `System.Delegate` the runtime defines.
const DELEGATE_TARGET_OFFSET: u32 = 0;
const DELEGATE_METHODPTR_OFFSET: u32 = 4;

/// Why a method body could not be lowered to MIR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CilError {
    /// An opcode needed more operands than the evaluation stack held.
    StackUnderflow,
    /// The method body did not end in `ret` (control flow is not lowered yet).
    NoReturn,
    /// An opcode's decoded operand was not the shape the opcode requires.
    BadOperand,
    /// A CIL opcode this lowering does not handle yet.
    Unsupported(Opcode),
    /// A `call` target token could not be resolved (no [`CallResolver`] mapping).
    UnresolvedCall,
    /// A control-flow shape this lowering does not handle yet: a conditional branch
    /// into a merge block (which would need its edge split), an entry block reached
    /// by a back-edge, or a block that runs off the end of the method.
    UnsupportedControlFlow,
    /// A method selected for module lowering has no CIL body (abstract or extern).
    MissingBody,
}

/// What a `call`'s target is, recovered from its metadata token by a [`CallResolver`].
pub enum CallTarget {
    /// A method within this program, by function index -- lowered to a direct call.
    Internal(u32),
    /// A recognized BCL method, lowered to a backend intrinsic instead of a call.
    Intrinsic(Intrinsic),
}

/// A BCL method the AOT lowers specially rather than as a managed call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intrinsic {
    /// `System.Diagnostics.Debug.WriteLine(string)` -> semihosting output.
    DebugWriteLine,
    /// `System.Console.WriteLine(int)` -> a decimal int written over semihosting.
    ConsoleWriteLineInt,
    /// `System.String::op_Equality(string, string)` -> an ordinal length-then-content compare.
    StringEquals,
    /// `System.Object::.ctor()` -- the implicit base constructor a derived constructor chains to.
    /// A no-op: the object header is the runtime allocator's, and there is no managed base state.
    ObjectCtor,
    /// `System.Array::GetLength(int)` -- a dimension's length, read from the array header
    /// (`[array + dim*4]`: dim0 at +0, dim1 at +4; for a 1-D array dimension 0 is the length).
    ArrayGetLength,
    /// `System.String::Concat(string, string)` -- what `a + b` emits; allocates and fills a new blob.
    StringConcat,
    /// `System.Int32::ToString()` -- formats the receiver int as its decimal string.
    IntToString,
}

/// What the lowering needs about a `call` target: how many arguments to pop, whether it
/// yields a value, and what it resolves to.
pub struct CallInfo {
    /// The number of arguments the callee takes (popped from the evaluation stack).
    pub args: usize,
    /// Whether the call pushes a return value onto the stack.
    pub has_result: bool,
    /// The result's [`MirType`] when `has_result` (so a value-type return types correctly);
    /// `None` falls back to `int32`.
    pub result_type: Option<MirType>,
    /// The resolved target.
    pub target: CallTarget,
}

/// What a `[DllImport]` P/Invoke `call` resolves to -- enough for the lowering to emit the import call
/// AND marshal its arguments (strings to native buffers, bool result normalized).
pub struct PInvokeCall {
    /// The unmanaged import symbol (the `DllImport` entry point).
    pub import: Box<str>,
    /// Per parameter (in order), whether it is a managed `string` -- so the lowering marshals it to a
    /// native `char*` via the runtime helper instead of passing the ObjectRef. Its length is the
    /// argument count.
    pub param_is_string: Vec<bool>,
    /// The result [`MirType`], or `None` for a `void` import.
    pub result_type: Option<MirType>,
    /// Whether the result is a `bool` (normalized to 0/1 after the call).
    pub result_is_bool: bool,
    /// The `CharSet` as the marshal helper's encoding selector: 0 = Ansi, 1 = Unicode.
    pub charset: u8,
}

/// The runtime helper that transcodes a managed string to a native NUL-terminated buffer (in a per-call
/// arena), selected by CharSet: `(objref, charset) -> char*`. Emitted as a P/Invoke (a `CallNative`).
const PINVOKE_STR_TO_NATIVE: &str = "lamella_pinvoke_str_to_native";
/// The runtime helper that frees the per-call arena of marshaled buffers, once, after the import returns.
const PINVOKE_FRAME_END: &str = "lamella_pinvoke_frame_end";

/// Resolves a `call`'s metadata token to a [`CallInfo`]. The lowering owns this seam; the
/// implementation (over `lamella-metadata`) lives in the caller, so CIL->MIR lowering
/// needs no metadata of its own and stays testable against a mock.
pub trait CallResolver {
    /// Resolves a `call`'s operand (its metadata token) to a [`CallInfo`], or `None` if
    /// the target is unknown or unsupported.
    fn resolve(&self, operand: &Operand) -> Option<CallInfo>;

    /// Resolves an `ldstr`'s operand (a `#US` user-string token) to the string's bytes,
    /// or `None`. Defaults to `None` for resolvers that handle only calls; the lowering
    /// adds the newline and NUL terminator semihosting needs.
    fn user_string(&self, _operand: &Operand) -> Option<Box<[u8]>> {
        None
    }

    /// The byte offset of a field (an `ldfld`/`stfld` operand token) within its declaring
    /// value type's layout. Defaults to `None`.
    fn field_offset(&self, _operand: &Operand) -> Option<u32> {
        None
    }

    /// The MIR type of a field (an `ldfld` operand token), so the lowering types the loaded
    /// value -- a reference field reads an `ObjectRef` (a chained `a.Next.V` then dereferences
    /// it), not an `int`. Defaults to `None` (the lowering falls back to `int32`).
    fn field_type(&self, _operand: &Operand) -> Option<MirType> {
        None
    }

    /// The size in bytes of a value type (an `initobj` type-operand token), from its layout.
    /// Defaults to `None`.
    fn value_type_size(&self, _operand: &Operand) -> Option<u32> {
        None
    }

    /// Whether a field (an `ldfld`/`stfld` operand token) is declared on a REFERENCE type, so its
    /// instance can be null and the access is a null-deref candidate. A field on a value type (a
    /// struct) is excluded -- the "object" is a struct value/address, never null. Defaults to
    /// `false` (a resolver with no type hierarchy makes no null-deref leaders).
    fn field_on_reference_type(&self, _operand: &Operand) -> bool {
        false
    }

    /// The value type a `newobj` constructs, named by its constructor token: the declaring
    /// type's [`MirType::ValueType`] (with size), so the lowering can allocate the instance.
    /// `None` for a reference type (use [`CallResolver::newobj_reference_layout`]).
    fn newobj_value_type(&self, _operand: &Operand) -> Option<MirType> {
        None
    }

    /// The layout of the reference type a `newobj` constructs, named by its constructor token:
    /// its identity, payload size, and reference-field offsets (the GC trace map), so the
    /// lowering can allocate the object on the heap. `None` for a value type or unresolved type.
    fn newobj_reference_layout(&self, _operand: &Operand) -> Option<ReferenceLayout> {
        None
    }

    /// The heap layout of a delegate type a `newobj` builds, or `None` if the operand is not a
    /// delegate constructor. A delegate (`_target` at offset 0, `_methodPtr` at offset 4) is
    /// special-cased: the lowering allocates it and stores the two ctor args directly, skipping the
    /// bodyless `Delegate(object, native int)` ctor. Defaults to `None`.
    fn newobj_delegate(&self, _operand: &Operand) -> Option<ReferenceLayout> {
        None
    }

    /// A delegate `Invoke` a `callvirt` names: its explicit argument count (the signature params,
    /// excluding the delegate receiver) and whether it returns a value, or `None` if the call is not a
    /// delegate `Invoke`. The lowering loads `_methodPtr` and calls it indirectly. Defaults to `None`.
    fn delegate_invoke_args(&self, _operand: &Operand) -> Option<(usize, bool)> {
        None
    }

    /// The [`PInvokeCall`] a `call` resolves to if its method is a `[DllImport]` P/Invoke (the import
    /// symbol, per-parameter string-ness, result type/bool-ness, and CharSet), or `None` for an ordinary
    /// call. The lowering emits an [`Inst::PInvoke`] (rewritten to a `CallNative` to the symbol),
    /// marshaling string arguments through the runtime helper. A P/Invoke method has no managed body, so
    /// it is intercepted here BEFORE the normal call resolution. Defaults to `None`.
    fn pinvoke_call(&self, _operand: &Operand) -> Option<PInvokeCall> {
        None
    }

    /// The element of an array a `newarr` allocates, named by its element-type token: the array
    /// type's identity (for the emitted TypeDesc) and the element's size in bytes (so the lowering
    /// sizes `4 + length*element_size`). Defaults to `None` (the lowering cannot allocate it).
    fn array_element(&self, _operand: &Operand) -> Option<ArrayElement> {
        None
    }

    /// The 2-D rectangular-array operation a `newobj`/`call` operand names -- the `int[,]::.ctor`,
    /// `Get`, or `Set` pseudo-method on an array TypeSpec of rank 2 (rectangular arrays go through
    /// `System.Array` calls, not the `szarray` opcodes), or `None` if the operand is not one. The
    /// resolver recognizes it by decoding the operand's MemberRef parent (a TypeSpec) as an array
    /// signature. Defaults to `None`.
    fn array_2d_op(&self, _operand: &Operand) -> Option<Array2DOp> {
        None
    }

    /// The byte offset of a static field (an `ldsfld`/`stsfld` operand token) within the module's
    /// static storage region. Defaults to `None`.
    fn static_field_offset(&self, _operand: &Operand) -> Option<u32> {
        None
    }

    /// The exception TAG of the type a token names, for the no-GC tag-dispatch model: the
    /// constructor token of a `newobj` (an exception built and thrown is lowered to its tag, not
    /// a heap object) or a `catch` clause's type token. `Some(tag)` only for an exception type,
    /// so a `newobj` of an ordinary class still allocates. The tag is identical wherever the same
    /// type is named -- throw site, catch, and runtime -- so the tiers never diverge. Defaults to
    /// `None`.
    fn exception_tag(&self, _operand: &Operand) -> Option<u32> {
        None
    }

    /// Whether a `catch` clause's type token is a universal catch -- `System.Exception` (or
    /// `System.Object` for a typeless `catch {}`) -- which matches any in-flight exception, so the
    /// dispatch tests the tag for nonzero rather than an exact value. Defaults to `false`.
    fn is_catch_all_type(&self, _operand: &Operand) -> bool {
        false
    }

    /// The set of tags a `catch (T)` clause matches: `T`'s own tag plus the tags of every type
    /// that derives from `T` (a thrown subtype the catch must take). The default is exact-match
    /// only -- just `T`'s tag -- so a resolver with no type hierarchy keeps the original behavior;
    /// the metadata resolver adds in-program subtypes by walking `extends`. (BCL subtypes live in
    /// another assembly and are added later from the compiler's emitted base-chain vectors.)
    fn subtype_tags(&self, operand: &Operand) -> Vec<u32> {
        self.exception_tag(operand).into_iter().collect()
    }

    /// The type HANDLES a `castclass T` accepts: `T`'s own handle plus the handles of every
    /// in-program type that derives from `T` (a subtype the cast must also accept). Each handle
    /// names a TypeDesc to compare the cast object's runtime type against. Empty by default; the
    /// metadata resolver walks `extends`. (BCL subtypes are not enumerable here, so a cast to a BCL
    /// base accepts only the exact type for now.)
    fn cast_subtype_handles(&self, _operand: &Operand) -> Vec<TypeHandle> {
        Vec::new()
    }

    /// For a `castclass T` where T is an in-program TypeDef, T's handle -- the cast lowers to a base-
    /// pointer chain scan of the object's TypeDesc instead of per-subtype compares. `None` for a BCL
    /// TypeRef target (which uses [`cast_subtype_handles`]). `None` by default; the metadata resolver fills it.
    fn cast_target_chain(&self, _operand: &Operand) -> Option<TypeHandle> {
        None
    }

    /// The exception tag of a BUILTIN throwable named directly (no token), so a synthesized check
    /// can raise it -- e.g. `System.IndexOutOfRangeException` from an array bounds check. The tag
    /// matches a `catch` that names the same type. Defaults to `None`.
    fn builtin_exception_tag(&self, _namespace: &str, _name: &str) -> Option<u32> {
        None
    }

    /// The heap layout for `box`/`unbox.any` of the value type named by the operand token: the
    /// boxed payload's size (the value's width) and reference-offset map, with the type's identity
    /// (its TypeDesc, which IS the box's type identity per the GC ABI -- payload is value bytes
    /// only, no type tag). Defaults to `None`.
    fn boxed_layout(&self, _operand: &Operand) -> Option<ReferenceLayout> {
        None
    }

    /// The MIR type a `box`/`unbox.any` value type lowers to: `i32` for a sub-word or 32-bit scalar,
    /// `f32`/`i64`/`f64` for the wider scalars, or a `ValueType` for a struct. Used to type the
    /// `unbox.any` result so a multi-word scalar reads its full width. Defaults to None.
    fn boxed_value_type(&self, _operand: &Operand) -> Option<MirType> {
        None
    }

    /// The vtable slot a virtual `callvirt` target dispatches through: its index in its declaring
    /// type's vtable (the same index across the hierarchy, so the receiver's runtime-type vtable at
    /// this slot is the override to call). `None` for a non-virtual target, or one this resolver
    /// cannot place (an interface/abstract method, or a cross-module ref) -- dispatched directly.
    /// Defaults to None (direct dispatch everywhere).
    fn virtual_slot(&self, _operand: &Operand) -> Option<usize> {
        None
    }

    /// The interface-method identity tag a `callvirt` on an INTERFACE method dispatches through (the
    /// receiver type's itable is searched for it). `None` for a non-interface target -- which then
    /// falls to [`virtual_slot`](Self::virtual_slot) or a direct call. Defaults to None.
    fn interface_call_tag(&self, _operand: &Operand) -> Option<u32> {
        None
    }
}

/// The layout of a reference type a `newobj` allocates: its identity ([`TypeHandle`]), payload
/// size in bytes, and the byte offsets of its reference fields within the payload (the GC map).
pub struct ReferenceLayout {
    /// The reference type's identity, for the emitted TypeDesc.
    pub handle: TypeHandle,
    /// The object's payload size in bytes.
    pub size: u32,
    /// The byte offsets of the fields holding an `ObjectRef`/`&`, for the collector to trace.
    pub reference_offsets: Vec<u32>,
}

/// The element of an array a `newarr` allocates: the array type's identity (for its TypeDesc) and
/// the size in bytes of one element.
pub struct ArrayElement {
    /// The array type's identity, for the emitted TypeDesc.
    pub handle: TypeHandle,
    /// The size in bytes of one element.
    pub element_size: u32,
}

/// A 2-D rectangular-array pseudo-method a `newobj`/`call` names (`int[,]::.ctor`/`Get`/`Set` on an
/// array TypeSpec of rank 2). The lowering pops the operands and emits the matching 2-D MIR primitive.
pub enum Array2DOp {
    /// `newobj int[,]::.ctor(dim0, dim1)` -- allocate; carries the array identity + element size.
    New {
        /// The array type's identity, for the emitted TypeDesc.
        handle: TypeHandle,
        /// The size in bytes of one element.
        element_size: u32,
    },
    /// `call int[,]::Get(i, j)` -- load; carries the element width/signedness and the loaded type.
    Get {
        /// The size in bytes of one element.
        element_size: u32,
        /// Whether a sub-word element is sign-extended (signed) or zero-extended.
        signed: bool,
        /// The MIR type of the loaded element (the `Get` result).
        element_type: MirType,
    },
    /// `call int[,]::Set(i, j, value)` -- store (the value's width comes from its stacked type).
    Set {
        /// The size in bytes of one element.
        element_size: u32,
    },
}

/// A [`CallResolver`] for call-free bodies: every resolution fails. The default for the
/// existing entry points, which lower methods that make no calls (the MMIO drivers).
pub struct NoCalls;

impl CallResolver for NoCalls {
    fn resolve(&self, _operand: &Operand) -> Option<CallInfo> {
        None
    }
}

/// Lowers an integer [`MethodBodyImage`] to a MIR [`Function`] by abstract
/// interpretation: the CIL is split into basic blocks, the evaluation stack and
/// locals are tracked per block, and join points (merges) become block parameters.
fn lower_with_source(
    body: &MethodBodyImage,
    resolver: &dyn CallResolver,
    arg_types: &[MirType],
    local_types: &[MirType],
) -> Result<(Function, CilSourceMap), CilError> {
    let code = &body.code;
    let widths = eval_stack_widths(code, arg_types, local_types, resolver);
    let mut byte_offsets: Vec<u32> = Vec::with_capacity(code.len());
    let mut running = 0u32;
    for instr in code.iter() {
        byte_offsets.push(running);
        let opcode = instr.opcode.encoding().byte_len() as u32;
        let operand = match instr.opcode.operand_kind() {
            OperandKind::Switch => match &instr.operand {
                Operand::Switch(targets) => 4 + targets.len() as u32 * 4,
                _ => 4,
            },
            kind => kind.fixed_operand_len().unwrap_or(0) as u32,
        };
        running = running.wrapping_add(opcode + operand);
    }
    let blocks = control_flow::discover_blocks(code, &body.handlers, &|op| {
        resolver.field_on_reference_type(op)
    });
    let preds = control_flow::predecessors(code, &blocks);
    let (arg_count, local_count) = scan_slots(code);

    let block_of = |instr: usize| blocks.iter().position(|&(s, e)| instr >= s && instr < e);
    let is_merge = |b: usize| preds.get(b).is_some_and(|p| p.len() > 1);

    let catch_clauses: Vec<&EhClause> = body
        .handlers
        .iter()
        .filter(|clause| matches!(clause.kind, EhKind::Catch(_)))
        .collect();
    let handler_block_of_clause: Vec<usize> = catch_clauses
        .iter()
        .map(|clause| {
            blocks
                .iter()
                .position(|&(start, _)| start == clause.handler_range.start as usize)
                .unwrap_or(0)
        })
        .collect();
    let handler_clause: Vec<Option<usize>> = blocks
        .iter()
        .map(|&(start, _)| {
            catch_clauses
                .iter()
                .position(|clause| clause.handler_range.start as usize == start)
        })
        .collect();
    let throw_clauses: Vec<Vec<usize>> = blocks
        .iter()
        .map(|&(start, end)| {
            let mut covering: Vec<usize> = catch_clauses
                .iter()
                .enumerate()
                .filter(|(_, clause)| {
                    clause.try_range.start as usize <= start && end <= clause.try_range.end as usize
                })
                .map(|(index, _)| index)
                .collect();
            covering.sort_by_key(|&i| {
                catch_clauses[i].try_range.end - catch_clauses[i].try_range.start
            });
            covering
        })
        .collect();

    let finally_clauses: Vec<&EhClause> = body
        .handlers
        .iter()
        .filter(|clause| matches!(clause.kind, EhKind::Finally))
        .collect();
    let finally_handler_block: Vec<usize> = finally_clauses
        .iter()
        .map(|clause| {
            blocks
                .iter()
                .position(|&(start, _)| start == clause.handler_range.start as usize)
                .unwrap_or(0)
        })
        .collect();
    let in_range = |idx: usize, range: lamella_cil::InstructionRange| {
        (range.start as usize) <= idx && idx < (range.end as usize)
    };
    let finally_continuation_block: Vec<Option<usize>> = finally_clauses
        .iter()
        .map(|clause| {
            (clause.try_range.start as usize..clause.try_range.end as usize).find_map(|i| {
                match (code[i].opcode, &code[i].operand) {
                    (Opcode::Leave | Opcode::LeaveS, Operand::Target(t))
                        if !in_range(*t as usize, clause.try_range) =>
                    {
                        block_of(*t as usize)
                    }
                    _ => None,
                }
            })
        })
        .collect();
    let finally_handler: Vec<Option<usize>> = blocks
        .iter()
        .map(|&(start, _)| {
            finally_clauses
                .iter()
                .position(|clause| clause.handler_range.start as usize == start)
        })
        .collect();
    let finally_continuation: Vec<bool> = (0..blocks.len())
        .map(|b| finally_continuation_block.contains(&Some(b)))
        .collect();
    let finally_protect: Vec<Option<usize>> = blocks
        .iter()
        .map(|&(start, end)| {
            finally_clauses
                .iter()
                .enumerate()
                .filter(|(_, clause)| {
                    clause.try_range.start as usize <= start && end <= clause.try_range.end as usize
                })
                .min_by_key(|(_, clause)| clause.try_range.end - clause.try_range.start)
                .map(|(index, _)| index)
        })
        .collect();
    let leave_exits: Vec<Option<usize>> = blocks
        .iter()
        .map(|&(_, end)| {
            let last = end.checked_sub(1)?;
            let inst = code.get(last)?;
            if !matches!(inst.opcode, Opcode::Leave | Opcode::LeaveS) {
                return None;
            }
            let Operand::Target(target) = &inst.operand else {
                return None;
            };
            let block = block_of(last)?;
            let clause = finally_protect[block]?;
            (!in_range(*target as usize, finally_clauses[clause].try_range)).then_some(clause)
        })
        .collect();
    let unwind_finally = |b: usize| finally_protect[b].map(|clause| finally_handler_block[clause]);
    let takes_local_params =
        |b: usize| is_merge(b) || finally_handler[b].is_some() || finally_continuation[b];
    let trap_access: Vec<Option<TrapKind>> = blocks
        .iter()
        .enumerate()
        .map(|(b, &(start, _))| {
            let kind = trap_kind_at(&code[start], resolver);
            if throw_clauses[b].is_empty()
                && !matches!(
                    kind,
                    Some(TrapKind::Cast(_))
                        | Some(TrapKind::CastClass(_))
                        | Some(TrapKind::CastClassChain(_))
                )
            {
                return None;
            }
            kind
        })
        .collect();

    if is_merge(0) {
        return Err(CilError::UnsupportedControlFlow);
    }

    let mut value_types: Vec<MirType> = Vec::new();
    let mut strings: Vec<(ValueId, Box<[u8]>)> = Vec::new();
    let args: Vec<ValueId> = (0..arg_count)
        .map(|i| {
            new_value(
                &mut value_types,
                arg_types.get(i).copied().unwrap_or(MirType::I32),
            )
        })
        .collect();

    let mut block_params: Vec<Vec<ValueId>> = Vec::with_capacity(blocks.len());
    for (b, handler) in handler_clause.iter().enumerate() {
        let params = if b == 0 {
            args.clone()
        } else if handler.is_some() {
            let mut params: Vec<ValueId> = (0..local_count)
                .map(|i| {
                    let ty = local_types.get(i).copied().unwrap_or(MirType::I32);
                    new_value(&mut value_types, ty)
                })
                .collect();
            params.push(new_value(&mut value_types, MirType::ObjectRef));
            params
        } else if trap_access[b].is_some() {
            let mut params: Vec<ValueId> = (0..local_count)
                .map(|i| {
                    let ty = local_types.get(i).copied().unwrap_or(MirType::I32);
                    new_value(&mut value_types, ty)
                })
                .collect();
            for ty in trap_operand_types(&code[blocks[b].0], widths[blocks[b].0], resolver) {
                params.push(new_value(&mut value_types, ty));
            }
            params
        } else if takes_local_params(b) {
            (0..local_count)
                .map(|i| {
                    let ty = local_types.get(i).copied().unwrap_or(MirType::I32);
                    new_value(&mut value_types, ty)
                })
                .collect()
        } else {
            Vec::new()
        };
        block_params.push(params);
    }

    let mut mir_blocks: Vec<BasicBlock> = Vec::with_capacity(blocks.len());
    let mut source_map: Vec<Vec<u32>> = Vec::with_capacity(blocks.len());
    let mut exit_locals: Vec<Vec<Option<ValueId>>> = vec![Vec::new(); blocks.len()];
    let mut exit_stack: Vec<Vec<ValueId>> = vec![Vec::new(); blocks.len()];
    let original_block_count = blocks.len();
    let mut split_blocks: Vec<BasicBlock> = Vec::new();
    let mut propagate_fixups: Vec<usize> = Vec::new();

    for (b, &(start, end)) in blocks.iter().enumerate() {
        let mut locals: Vec<Option<ValueId>> = if b == 0 {
            vec![None; local_count]
        } else if handler_clause[b].is_some() {
            block_params[b]
                .iter()
                .take(local_count)
                .map(|&p| Some(p))
                .collect()
        } else if trap_access[b].is_some() {
            block_params[b]
                .iter()
                .take(local_count)
                .map(|&p| Some(p))
                .collect()
        } else if takes_local_params(b) {
            block_params[b].iter().map(|&p| Some(p)).collect()
        } else {
            let pred = *preds[b].first().ok_or(CilError::UnsupportedControlFlow)?;
            if pred < b {
                exit_locals[pred].clone()
            } else if is_merge(pred) {
                block_params[pred].iter().map(|&p| Some(p)).collect()
            } else {
                return Err(CilError::UnsupportedControlFlow);
            }
        };
        locals.resize(local_count, None);

        let mut stack: Vec<ValueId> = Vec::new();
        let current_exception = handler_clause[b].and(block_params[b].get(local_count).copied());
        if let Some(exception) = current_exception {
            stack.push(exception);
        }
        if trap_access[b].is_some() {
            let operand_count = trap_operand_types(&code[start], widths[start], resolver).len();
            for k in 0..operand_count {
                stack.push(block_params[b][local_count + k]);
            }
        }
        if b != 0
            && handler_clause[b].is_none()
            && trap_access[b].is_none()
            && finally_handler[b].is_none()
            && !finally_continuation[b]
        {
            if is_merge(b) {
                let height = preds[b]
                    .iter()
                    .copied()
                    .filter(|&p| p < b)
                    .map(|p| exit_stack[p].len())
                    .max()
                    .unwrap_or(0);
                for k in 0..height {
                    let ty = preds[b]
                        .iter()
                        .copied()
                        .filter(|&p| p < b)
                        .find_map(|p| exit_stack[p].get(k).copied())
                        .map(|v| value_types[v.index()])
                        .unwrap_or(MirType::I32);
                    let param = new_value(&mut value_types, ty);
                    block_params[b].push(param);
                    stack.push(param);
                }
            } else if let Some(&pred) = preds[b].first() {
                if pred < b {
                    stack = exit_stack[pred].clone();
                }
            }
        }
        let mut insts: Vec<(ValueId, Inst)> = Vec::new();
        let mut il_index: Vec<u32> = Vec::new();
        let mut terminator: Option<Terminator> = None;
        let mut last_local_addr: Option<(AddrOf, u32)> = None;

        for i in start..end {
            let inst = &code[i];
            let is_last = i + 1 == end;
            let before = insts.len();
            if is_last && inst.opcode == Opcode::Ret {
                terminator = Some(Terminator::Return(stack.pop()));
            } else if is_last && matches!(inst.opcode, Opcode::Throw | Opcode::Rethrow) {
                if inst.opcode == Opcode::Rethrow {
                    let exception =
                        current_exception.ok_or(CilError::Unsupported(Opcode::Rethrow))?;
                    stack.push(exception);
                }
                terminator = Some(build_eh_throw(
                    &throw_clauses[b],
                    &catch_clauses,
                    &handler_block_of_clause,
                    finally_protect[b],
                    &finally_handler_block,
                    resolver,
                    &mut stack,
                    &locals,
                    local_count,
                    local_types,
                    &mut value_types,
                    &mut insts,
                    &mut split_blocks,
                    original_block_count,
                    &mut propagate_fixups,
                )?);
            } else if is_last && inst.opcode == Opcode::Endfinally {
                let clause = finally_handler[b].ok_or(CilError::UnsupportedControlFlow)?;
                terminator = Some(build_eh_endfinally(
                    finally_continuation_block[clause],
                    &locals,
                    local_count,
                    local_types,
                    &mut value_types,
                    &mut insts,
                    &mut split_blocks,
                    original_block_count,
                    &mut propagate_fixups,
                ));
            } else if is_last
                && matches!(inst.opcode, Opcode::Leave | Opcode::LeaveS)
                && leave_exits[b].is_some()
            {
                let clause = leave_exits[b].expect("checked is_some above");
                terminator = Some(build_eh_finally_leave(
                    finally_handler_block[clause],
                    &locals,
                    local_count,
                    local_types,
                    &mut value_types,
                    &mut insts,
                ));
            } else if is_last
                && matches!(inst.opcode, Opcode::Leave | Opcode::LeaveS)
                && !throw_clauses[b].is_empty()
            {
                let Operand::Target(target_instr) = &inst.operand else {
                    return Err(CilError::BadOperand);
                };
                let leave_target =
                    block_of(*target_instr as usize).ok_or(CilError::UnsupportedControlFlow)?;
                terminator = Some(build_eh_leave(
                    leave_target,
                    &throw_clauses[b],
                    &catch_clauses,
                    &handler_block_of_clause,
                    unwind_finally(b),
                    resolver,
                    &is_merge,
                    &locals,
                    local_count,
                    local_types,
                    &mut value_types,
                    &mut insts,
                    &mut split_blocks,
                    original_block_count,
                    &mut propagate_fixups,
                )?);
            } else if is_last && control_flow::branch_kind(inst.opcode).is_some() {
                terminator = Some(build_branch(
                    inst,
                    end,
                    &block_of,
                    &is_merge,
                    local_count,
                    &mut stack,
                    &locals,
                    local_types,
                    &mut value_types,
                    &mut insts,
                    &mut split_blocks,
                    original_block_count,
                )?);
            } else if is_last && inst.opcode == Opcode::Switch {
                terminator = Some(build_switch(
                    inst,
                    end,
                    &block_of,
                    &is_merge,
                    local_count,
                    &mut stack,
                    &locals,
                    local_types,
                    &mut value_types,
                    &mut insts,
                    &mut split_blocks,
                    original_block_count,
                )?);
            } else {
                apply_value_op(
                    inst,
                    &mut value_types,
                    &mut stack,
                    &mut locals,
                    local_types,
                    &args,
                    &mut insts,
                    &mut strings,
                    resolver,
                    &mut last_local_addr,
                )?;
            }
            for _ in before..insts.len() {
                il_index.push(byte_offsets[i]);
            }
        }

        let terminator = match terminator {
            Some(t) => t,
            None => {
                let next = b + 1;
                if next >= blocks.len() {
                    return Err(CilError::UnsupportedControlFlow);
                }
                if let Some(kind) = trap_access[next].clone() {
                    let operand_count =
                        trap_operand_types(&code[blocks[next].0], widths[blocks[next].0], resolver)
                            .len();
                    let mut operands = Vec::with_capacity(operand_count);
                    for _ in 0..operand_count {
                        operands.push(stack.pop().ok_or(CilError::StackUnderflow)?);
                    }
                    operands.reverse();
                    build_trap_access_check(
                        next,
                        kind,
                        &operands,
                        &throw_clauses[b],
                        &catch_clauses,
                        &handler_block_of_clause,
                        finally_protect[b],
                        &finally_handler_block,
                        resolver,
                        &locals,
                        local_count,
                        local_types,
                        &mut value_types,
                        &mut insts,
                        &mut split_blocks,
                        original_block_count,
                        &mut propagate_fixups,
                    )?
                } else {
                    let mut args = merge_args(
                        is_merge(next),
                        local_count,
                        &locals,
                        local_types,
                        &mut value_types,
                        &mut insts,
                    );
                    if is_merge(next) {
                        args.extend(stack.iter().copied());
                    }
                    Terminator::Jump {
                        target: BlockId(next as u32),
                        args,
                    }
                }
            }
        };

        while il_index.len() < insts.len() {
            il_index.push(
                byte_offsets
                    .get(end.saturating_sub(1))
                    .copied()
                    .unwrap_or(0),
            );
        }

        exit_locals[b] = locals.clone();
        exit_stack[b] = stack.clone();
        mir_blocks.push(BasicBlock {
            params: block_params[b].clone(),
            insts,
            terminator: Some(terminator),
        });
        source_map.push(il_index);
    }

    for split in split_blocks {
        mir_blocks.push(split);
        source_map.push(Vec::new());
    }

    let ret = mir_blocks.iter().find_map(|blk| match &blk.terminator {
        Some(Terminator::Return(Some(v))) => value_types.get(v.index()).copied(),
        _ => None,
    });

    if let Some(ret_ty) = ret {
        for &block_index in &propagate_fixups {
            let value = new_value(&mut value_types, ret_ty);
            let block = &mut mir_blocks[block_index];
            block.insts.push((value, zero_inst(ret_ty)));
            block.terminator = Some(Terminator::Return(Some(value)));
        }
    }

    let function = Function {
        params: (0..arg_count)
            .map(|i| arg_types.get(i).copied().unwrap_or(MirType::I32))
            .collect(),
        ret,
        value_types,
        entry: BlockId(0),
        blocks: mir_blocks,
    };
    Ok((function, CilSourceMap(source_map)))
}

/// The CIL byte offset each MIR instruction was lowered from, indexed by block then by
/// instruction within the block -- the lowering's half of the native-to-source mapping.
/// The target lowering pairs these with native code offsets to build a line table; the
/// compiler's sequence points then carry a CIL byte offset to a source line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CilSourceMap(pub Vec<Vec<u32>>);

/// Lowers an integer [`MethodBodyImage`] to a MIR [`Function`]. See
/// [`lower_method_debug`] for the accompanying [`CilSourceMap`].
pub fn lower_method(body: &MethodBodyImage) -> Result<Function, CilError> {
    lower_with_source(body, &NoCalls, &[], &[]).map(|(function, _)| function)
}

/// Lowers a method body, also returning the [`CilSourceMap`] tying each MIR
/// instruction back to the CIL instruction it came from.
pub fn lower_method_debug(body: &MethodBodyImage) -> Result<(Function, CilSourceMap), CilError> {
    lower_with_source(body, &NoCalls, &[], &[])
}

/// Lowers a method body that makes calls, using `resolver` to map each `call`'s token to
/// its target -- an internal callee or a recognized [`Intrinsic`] -- and returns the
/// [`CilSourceMap`] as well. See [`CallResolver`].
pub fn lower_method_debug_with(
    body: &MethodBodyImage,
    resolver: &dyn CallResolver,
) -> Result<(Function, CilSourceMap), CilError> {
    lower_with_source(body, resolver, &[], &[])
}

/// Lowers a method body with explicit parameter and local types (mapped from the method's
/// signature and local-variable signature), so `int64`, value-type, and other non-`int32`
/// slots type correctly instead of defaulting to `int32`. A slot with no supplied type
/// defaults to `int32`.
pub fn lower_method_typed(
    body: &MethodBodyImage,
    resolver: &dyn CallResolver,
    arg_types: &[MirType],
    local_types: &[MirType],
) -> Result<(Function, CilSourceMap), CilError> {
    lower_with_source(body, resolver, arg_types, local_types)
}

/// Defines a fresh MIR value of `ty` and returns its id.
fn new_value(value_types: &mut Vec<MirType>, ty: MirType) -> ValueId {
    let id = ValueId(value_types.len() as u32);
    value_types.push(ty);
    id
}

/// Pushes an integer constant: a new value defined by a `ConstInt`.
fn push_const(
    value_types: &mut Vec<MirType>,
    stack: &mut Vec<ValueId>,
    insts: &mut Vec<(ValueId, Inst)>,
    value: i64,
) {
    let result = new_value(value_types, MirType::I32);
    insts.push((
        result,
        Inst::ConstInt {
            ty: MirType::I32,
            value,
        },
    ));
    stack.push(result);
}

/// Pops two operands (CIL order: the top is the right operand) and pushes a
/// binary operation over them.
fn binary(
    value_types: &mut Vec<MirType>,
    stack: &mut Vec<ValueId>,
    insts: &mut Vec<(ValueId, Inst)>,
    op: BinOp,
) -> Result<(), CilError> {
    let rhs = stack.pop().ok_or(CilError::StackUnderflow)?;
    let lhs = stack.pop().ok_or(CilError::StackUnderflow)?;
    let ty = value_types
        .get(lhs.0 as usize)
        .copied()
        .unwrap_or(MirType::I32);
    let result = new_value(value_types, ty);
    insts.push((result, Inst::Binary { op, lhs, rhs }));
    stack.push(result);
    Ok(())
}

/// Lowers a `stind.i{1,2,4}`: `*(addr) = value`, a `width`-byte store (the value is on top of the
/// address). The store yields no stack value.
fn stind(
    value_types: &mut Vec<MirType>,
    stack: &mut Vec<ValueId>,
    insts: &mut Vec<(ValueId, Inst)>,
    width: u32,
) -> Result<(), CilError> {
    let value = stack.pop().ok_or(CilError::StackUnderflow)?;
    let address = stack.pop().ok_or(CilError::StackUnderflow)?;
    let result = new_value(value_types, MirType::I32);
    insts.push((
        result,
        Inst::Store {
            address,
            value,
            width,
        },
    ));
    Ok(())
}

/// Lowers a `ldind.{i,u}{1,2,4}`: `value = *(addr)`, a `width`-byte load sign- or zero-extended to
/// i32 per `signed` (the address is on top; the loaded value replaces it). A `width`-8 load (`ldind.i8`)
/// yields a full i64.
fn ldind(
    value_types: &mut Vec<MirType>,
    stack: &mut Vec<ValueId>,
    insts: &mut Vec<(ValueId, Inst)>,
    width: u32,
    signed: bool,
) -> Result<(), CilError> {
    let address = stack.pop().ok_or(CilError::StackUnderflow)?;
    let result_type = if width == 8 {
        MirType::I64
    } else {
        MirType::I32
    };
    let result = new_value(value_types, result_type);
    insts.push((
        result,
        Inst::Load {
            address,
            width,
            signed,
        },
    ));
    stack.push(result);
    Ok(())
}

/// Pops two operands and pushes a comparison yielding 0 or 1.
fn compare(
    value_types: &mut Vec<MirType>,
    stack: &mut Vec<ValueId>,
    insts: &mut Vec<(ValueId, Inst)>,
    op: CmpOp,
) -> Result<(), CilError> {
    let rhs = stack.pop().ok_or(CilError::StackUnderflow)?;
    let lhs = stack.pop().ok_or(CilError::StackUnderflow)?;
    let result = new_value(value_types, MirType::I32);
    insts.push((result, Inst::Compare { op, lhs, rhs }));
    stack.push(result);
    Ok(())
}

/// Pops one operand and pushes its sub-word width conversion (the CLI's `conv.i1`/
/// `conv.u1`/`conv.i2`/`conv.u2`); the result is `int32`.
fn convert(
    value_types: &mut Vec<MirType>,
    stack: &mut Vec<ValueId>,
    insts: &mut Vec<(ValueId, Inst)>,
    kind: ConvKind,
) -> Result<(), CilError> {
    let value = stack.pop().ok_or(CilError::StackUnderflow)?;
    let result = new_value(value_types, MirType::I32);
    insts.push((result, Inst::Convert { value, kind }));
    stack.push(result);
    Ok(())
}

/// Widens the top of stack to `int64` (sign- or zero-extended); a no-op if it is already
/// `int64` (the CLI's `conv.i8`/`conv.u8`).
fn widen(
    value_types: &mut Vec<MirType>,
    stack: &mut Vec<ValueId>,
    insts: &mut Vec<(ValueId, Inst)>,
    signed: bool,
) -> Result<(), CilError> {
    let value = stack.pop().ok_or(CilError::StackUnderflow)?;
    if value_types.get(value.0 as usize) == Some(&MirType::I64) {
        stack.push(value);
        return Ok(());
    }
    let result = new_value(value_types, MirType::I64);
    insts.push((result, Inst::Widen { value, signed }));
    stack.push(result);
    Ok(())
}

/// Narrows the top of stack to `int32`: truncates an `int64`, or a no-op on a 32-bit value
/// (the CLI's `conv.i4`/`conv.u4`).
fn narrow_to_i32(
    value_types: &mut Vec<MirType>,
    stack: &mut Vec<ValueId>,
    insts: &mut Vec<(ValueId, Inst)>,
) -> Result<(), CilError> {
    let value = stack.pop().ok_or(CilError::StackUnderflow)?;
    if value_types.get(value.0 as usize) != Some(&MirType::I64) {
        stack.push(value);
        return Ok(());
    }
    let result = new_value(value_types, MirType::I32);
    insts.push((result, Inst::Truncate { value }));
    stack.push(result);
    Ok(())
}

/// Where a pending `ldloca`/`ldarga` address points -- a local or an argument slot -- which
/// the next field-access or call op resolves to its MIR value.
#[derive(Clone, Copy)]
enum AddrOf {
    /// A local variable, by index.
    Local(usize),
    /// An argument, by index.
    Arg(usize),
}

impl AddrOf {
    /// The MIR value of the addressed local or argument, if defined.
    fn value(self, locals: &[Option<ValueId>], args: &[ValueId]) -> Option<ValueId> {
        match self {
            AddrOf::Local(n) => locals.get(n).and_then(|v| *v),
            AddrOf::Arg(n) => args.get(n).copied(),
        }
    }
}

/// Resolves a pending address source to the MIR value it points at, allocating an
/// uninitialized struct local's zeroed slot on demand: `Point a; a.X = 1;` (definite
/// assignment, no `initobj`) writes through `&a` before `a` is ever stored, and an in-place
/// `new A(...)` constructs into one. Arguments are always defined on entry, so the fill-in
/// is locals-only.
fn addr_base(
    source: AddrOf,
    locals: &mut [Option<ValueId>],
    local_types: &[MirType],
    args: &[ValueId],
    value_types: &mut Vec<MirType>,
    insts: &mut Vec<(ValueId, Inst)>,
) -> Result<ValueId, CilError> {
    if let Some(base) = source.value(locals, args) {
        return Ok(base);
    }
    let AddrOf::Local(n) = source else {
        return Err(CilError::BadOperand);
    };
    let ty = local_types.get(n).copied().unwrap_or(MirType::I32);
    let slot = new_value(value_types, ty);
    let init = if matches!(ty, MirType::ValueType { .. }) {
        Inst::InitStruct
    } else {
        Inst::ConstInt { ty, value: 0 }
    };
    insts.push((slot, init));
    *locals.get_mut(n).ok_or(CilError::BadOperand)? = Some(slot);
    Ok(slot)
}

/// Applies one value-producing CIL instruction to the abstract state. Control-flow
/// terminators (`ret` and the branches) are handled by the caller, not here.
#[allow(clippy::too_many_arguments)]
fn apply_value_op(
    inst: &Instruction,
    value_types: &mut Vec<MirType>,
    stack: &mut Vec<ValueId>,
    locals: &mut [Option<ValueId>],
    local_types: &[MirType],
    args: &[ValueId],
    insts: &mut Vec<(ValueId, Inst)>,
    strings: &mut Vec<(ValueId, Box<[u8]>)>,
    resolver: &dyn CallResolver,
    last_local_addr: &mut Option<(AddrOf, u32)>,
) -> Result<(), CilError> {
    match inst.opcode {
        Opcode::Nop => {}
        Opcode::Ldarg0 => push_arg(args, stack, 0)?,
        Opcode::Ldarg1 => push_arg(args, stack, 1)?,
        Opcode::Ldarg2 => push_arg(args, stack, 2)?,
        Opcode::Ldarg3 => push_arg(args, stack, 3)?,
        Opcode::LdargS | Opcode::Ldarg => {
            let Operand::Variable(n) = &inst.operand else {
                return Err(CilError::BadOperand);
            };
            push_arg(args, stack, *n as usize)?;
        }
        Opcode::Ldloc0 => push_local(value_types, locals, stack, insts, 0)?,
        Opcode::Ldloc1 => push_local(value_types, locals, stack, insts, 1)?,
        Opcode::Ldloc2 => push_local(value_types, locals, stack, insts, 2)?,
        Opcode::Ldloc3 => push_local(value_types, locals, stack, insts, 3)?,
        Opcode::LdlocS | Opcode::Ldloc => {
            let Operand::Variable(n) = &inst.operand else {
                return Err(CilError::BadOperand);
            };
            push_local(value_types, locals, stack, insts, *n as usize)?;
        }
        Opcode::Stloc0 => store_local(value_types, locals, stack, insts, 0)?,
        Opcode::Stloc1 => store_local(value_types, locals, stack, insts, 1)?,
        Opcode::Stloc2 => store_local(value_types, locals, stack, insts, 2)?,
        Opcode::Stloc3 => store_local(value_types, locals, stack, insts, 3)?,
        Opcode::StlocS | Opcode::Stloc => {
            let Operand::Variable(n) = &inst.operand else {
                return Err(CilError::BadOperand);
            };
            store_local(value_types, locals, stack, insts, *n as usize)?;
        }
        Opcode::Ldnull => {
            let result = new_value(value_types, MirType::ObjectRef);
            insts.push((
                result,
                Inst::ConstInt {
                    ty: MirType::ObjectRef,
                    value: 0,
                },
            ));
            stack.push(result);
        }
        Opcode::Ldftn => {
            let info = resolver
                .resolve(&inst.operand)
                .ok_or(CilError::UnresolvedCall)?;
            let CallTarget::Internal(func) = info.target else {
                return Err(CilError::UnresolvedCall);
            };
            let result = new_value(value_types, MirType::I32);
            insts.push((result, Inst::FuncAddr { func }));
            stack.push(result);
        }
        Opcode::LdcI4M1 => push_const(value_types, stack, insts, -1),
        Opcode::LdcI40 => push_const(value_types, stack, insts, 0),
        Opcode::LdcI41 => push_const(value_types, stack, insts, 1),
        Opcode::LdcI42 => push_const(value_types, stack, insts, 2),
        Opcode::LdcI43 => push_const(value_types, stack, insts, 3),
        Opcode::LdcI44 => push_const(value_types, stack, insts, 4),
        Opcode::LdcI45 => push_const(value_types, stack, insts, 5),
        Opcode::LdcI46 => push_const(value_types, stack, insts, 6),
        Opcode::LdcI47 => push_const(value_types, stack, insts, 7),
        Opcode::LdcI48 => push_const(value_types, stack, insts, 8),
        Opcode::LdcI4S => {
            let Operand::Int8(v) = &inst.operand else {
                return Err(CilError::BadOperand);
            };
            push_const(value_types, stack, insts, i64::from(*v));
        }
        Opcode::LdcI4 => {
            let Operand::Int32(v) = &inst.operand else {
                return Err(CilError::BadOperand);
            };
            push_const(value_types, stack, insts, i64::from(*v));
        }
        Opcode::LdcI8 => {
            let Operand::Int64(v) = &inst.operand else {
                return Err(CilError::BadOperand);
            };
            let result = new_value(value_types, MirType::I64);
            insts.push((
                result,
                Inst::ConstInt {
                    ty: MirType::I64,
                    value: *v,
                },
            ));
            stack.push(result);
        }
        Opcode::LdcR4 => {
            let Operand::Float32(v) = &inst.operand else {
                return Err(CilError::BadOperand);
            };
            let result = new_value(value_types, MirType::F32);
            insts.push((
                result,
                Inst::ConstInt {
                    ty: MirType::F32,
                    value: i64::from(v.to_bits()),
                },
            ));
            stack.push(result);
        }
        Opcode::Add | Opcode::AddOvf | Opcode::AddOvfUn => {
            binary(value_types, stack, insts, BinOp::Add)?
        }
        Opcode::Sub | Opcode::SubOvf | Opcode::SubOvfUn => {
            binary(value_types, stack, insts, BinOp::Sub)?
        }
        Opcode::Mul | Opcode::MulOvf | Opcode::MulOvfUn => {
            binary(value_types, stack, insts, BinOp::Mul)?
        }
        Opcode::And => binary(value_types, stack, insts, BinOp::And)?,
        Opcode::Or => binary(value_types, stack, insts, BinOp::Or)?,
        Opcode::Xor => binary(value_types, stack, insts, BinOp::Xor)?,
        Opcode::Shl => binary(value_types, stack, insts, BinOp::Shl)?,
        Opcode::Shr => binary(value_types, stack, insts, BinOp::ShrSigned)?,
        Opcode::ShrUn => binary(value_types, stack, insts, BinOp::ShrUnsigned)?,
        Opcode::Div => binary(value_types, stack, insts, BinOp::DivSigned)?,
        Opcode::DivUn => binary(value_types, stack, insts, BinOp::DivUnsigned)?,
        Opcode::Rem => binary(value_types, stack, insts, BinOp::RemSigned)?,
        Opcode::RemUn => binary(value_types, stack, insts, BinOp::RemUnsigned)?,
        Opcode::Ceq => compare(value_types, stack, insts, CmpOp::Eq)?,
        Opcode::Cgt => compare(value_types, stack, insts, CmpOp::SignedGt)?,
        Opcode::CgtUn => compare(value_types, stack, insts, CmpOp::UnsignedGt)?,
        Opcode::Clt => compare(value_types, stack, insts, CmpOp::SignedLt)?,
        Opcode::CltUn => compare(value_types, stack, insts, CmpOp::UnsignedLt)?,
        Opcode::Neg => {
            let x = stack.pop().ok_or(CilError::StackUnderflow)?;
            let zero = new_value(value_types, MirType::I32);
            insts.push((
                zero,
                Inst::ConstInt {
                    ty: MirType::I32,
                    value: 0,
                },
            ));
            let result = new_value(value_types, MirType::I32);
            insts.push((
                result,
                Inst::Binary {
                    op: BinOp::Sub,
                    lhs: zero,
                    rhs: x,
                },
            ));
            stack.push(result);
        }
        Opcode::Not => {
            let x = stack.pop().ok_or(CilError::StackUnderflow)?;
            let ones = new_value(value_types, MirType::I32);
            insts.push((
                ones,
                Inst::ConstInt {
                    ty: MirType::I32,
                    value: -1,
                },
            ));
            let result = new_value(value_types, MirType::I32);
            insts.push((
                result,
                Inst::Binary {
                    op: BinOp::Xor,
                    lhs: x,
                    rhs: ones,
                },
            ));
            stack.push(result);
        }
        Opcode::ConvI1 => convert(value_types, stack, insts, ConvKind::SignExtend8)?,
        Opcode::ConvU1 => convert(value_types, stack, insts, ConvKind::ZeroExtend8)?,
        Opcode::ConvI2 => convert(value_types, stack, insts, ConvKind::SignExtend16)?,
        Opcode::ConvU2 => convert(value_types, stack, insts, ConvKind::ZeroExtend16)?,
        Opcode::ConvOvfI1 => convert(value_types, stack, insts, ConvKind::SignExtend8)?,
        Opcode::ConvOvfU1 => convert(value_types, stack, insts, ConvKind::ZeroExtend8)?,
        Opcode::ConvOvfI2 => convert(value_types, stack, insts, ConvKind::SignExtend16)?,
        Opcode::ConvOvfU2 => convert(value_types, stack, insts, ConvKind::ZeroExtend16)?,
        Opcode::ConvI8 | Opcode::ConvOvfI8 => widen(value_types, stack, insts, true)?,
        Opcode::ConvU8 | Opcode::ConvOvfU8 => widen(value_types, stack, insts, false)?,
        Opcode::ConvI4 | Opcode::ConvU4 | Opcode::ConvOvfI4 | Opcode::ConvOvfU4 => {
            let top = *stack.last().ok_or(CilError::StackUnderflow)?;
            if value_types.get(top.index()) == Some(&MirType::F32) {
                let value = stack.pop().ok_or(CilError::StackUnderflow)?;
                let result = new_value(value_types, MirType::I32);
                insts.push((
                    result,
                    Inst::Convert {
                        value,
                        kind: ConvKind::Float32ToInt,
                    },
                ));
                stack.push(result);
            } else {
                narrow_to_i32(value_types, stack, insts)?;
            }
        }
        Opcode::ConvR4 => {
            let top = *stack.last().ok_or(CilError::StackUnderflow)?;
            match value_types.get(top.index()) {
                Some(MirType::I32) => {
                    let value = stack.pop().ok_or(CilError::StackUnderflow)?;
                    let result = new_value(value_types, MirType::F32);
                    insts.push((
                        result,
                        Inst::Convert {
                            value,
                            kind: ConvKind::IntToFloat32,
                        },
                    ));
                    stack.push(result);
                }
                Some(MirType::F32) => {}
                _ => return Err(CilError::Unsupported(inst.opcode)),
            }
        }
        Opcode::Pop => {
            stack.pop().ok_or(CilError::StackUnderflow)?;
        }
        Opcode::Dup => {
            let top = *stack.last().ok_or(CilError::StackUnderflow)?;
            stack.push(top);
        }
        Opcode::ConvI | Opcode::ConvU => {}
        Opcode::StindI1 => stind(value_types, stack, insts, 1)?,
        Opcode::StindI2 => stind(value_types, stack, insts, 2)?,
        Opcode::StindI4 => stind(value_types, stack, insts, 4)?,
        Opcode::StindI8 => stind(value_types, stack, insts, 8)?,
        Opcode::LdindI1 => ldind(value_types, stack, insts, 1, true)?,
        Opcode::LdindU1 => ldind(value_types, stack, insts, 1, false)?,
        Opcode::LdindI2 => ldind(value_types, stack, insts, 2, true)?,
        Opcode::LdindU2 => ldind(value_types, stack, insts, 2, false)?,
        Opcode::LdindI4 | Opcode::LdindU4 => ldind(value_types, stack, insts, 4, false)?,
        Opcode::LdindI8 => ldind(value_types, stack, insts, 8, false)?,
        Opcode::Cpblk => {
            let size = stack.pop().ok_or(CilError::StackUnderflow)?;
            let src = stack.pop().ok_or(CilError::StackUnderflow)?;
            let dst = stack.pop().ok_or(CilError::StackUnderflow)?;
            let result = new_value(value_types, MirType::I32);
            insts.push((result, Inst::CopyBlock { dst, src, size }));
        }
        Opcode::Initblk => {
            let size = stack.pop().ok_or(CilError::StackUnderflow)?;
            let value = stack.pop().ok_or(CilError::StackUnderflow)?;
            let dst = stack.pop().ok_or(CilError::StackUnderflow)?;
            let result = new_value(value_types, MirType::I32);
            insts.push((result, Inst::FillBlock { dst, value, size }));
        }
        Opcode::Ldstr => {
            let bytes = resolver
                .user_string(&inst.operand)
                .ok_or(CilError::UnresolvedCall)?;
            let utf16: Box<[u16]> = core::str::from_utf8(&bytes)
                .unwrap_or("")
                .encode_utf16()
                .collect::<Vec<u16>>()
                .into_boxed_slice();
            let value = new_value(value_types, MirType::ObjectRef);
            insts.push((value, Inst::StringLiteral { utf16 }));
            strings.push((value, bytes));
            stack.push(value);
        }
        Opcode::Call | Opcode::Callvirt => {
            if let Some((arg_count, has_result)) = resolver.delegate_invoke_args(&inst.operand) {
                let mut args = Vec::with_capacity(arg_count);
                for _ in 0..arg_count {
                    args.push(stack.pop().ok_or(CilError::StackUnderflow)?);
                }
                args.reverse();
                let delegate = stack.pop().ok_or(CilError::StackUnderflow)?;
                let result = new_value(value_types, MirType::I32);
                insts.push((result, Inst::InvokeDelegate { delegate, args }));
                if has_result {
                    stack.push(result);
                }
                return Ok(());
            }
            if let Some(call) = resolver.pinvoke_call(&inst.operand) {
                let PInvokeCall {
                    import,
                    param_is_string,
                    result_type,
                    result_is_bool,
                    charset,
                } = call;
                let arg_count = param_is_string.len();
                let by_ref = last_local_addr
                    .take()
                    .map(|(source, off)| -> Result<ValueId, CilError> {
                        let base =
                            addr_base(source, locals, local_types, args, value_types, insts)?;
                        let ptr = new_value(value_types, MirType::ManagedPtr);
                        insts.push((ptr, Inst::FieldAddr { base, offset: off }));
                        Ok(ptr)
                    })
                    .transpose()?;
                let explicit = arg_count.saturating_sub(by_ref.is_some() as usize);
                let mut args = Vec::with_capacity(arg_count);
                for _ in 0..explicit {
                    args.push(stack.pop().ok_or(CilError::StackUnderflow)?);
                }
                args.reverse();
                if let Some(ptr) = by_ref {
                    args.insert(0, ptr);
                }
                let mut marshaled_a_string = false;
                for (slot, &is_string) in args.iter_mut().zip(param_is_string.iter()) {
                    if is_string {
                        let charset_arg = new_value(value_types, MirType::I32);
                        insts.push((
                            charset_arg,
                            Inst::ConstInt {
                                ty: MirType::I32,
                                value: i64::from(charset),
                            },
                        ));
                        let native = new_value(value_types, MirType::I32);
                        insts.push((
                            native,
                            Inst::PInvoke {
                                import: PINVOKE_STR_TO_NATIVE.into(),
                                args: alloc::vec![*slot, charset_arg],
                            },
                        ));
                        *slot = native;
                        marshaled_a_string = true;
                    }
                }
                let result = new_value(value_types, result_type.unwrap_or(MirType::I32));
                insts.push((result, Inst::PInvoke { import, args }));
                if marshaled_a_string {
                    let freed = new_value(value_types, MirType::I32);
                    insts.push((
                        freed,
                        Inst::PInvoke {
                            import: PINVOKE_FRAME_END.into(),
                            args: Vec::new(),
                        },
                    ));
                }
                if result_is_bool {
                    let zero = new_value(value_types, MirType::I32);
                    insts.push((
                        zero,
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 0,
                        },
                    ));
                    let normalized = new_value(value_types, MirType::I32);
                    insts.push((
                        normalized,
                        Inst::Compare {
                            op: CmpOp::Ne,
                            lhs: result,
                            rhs: zero,
                        },
                    ));
                    stack.push(normalized);
                } else if result_type.is_some() {
                    stack.push(result);
                }
                return Ok(());
            }
            match resolver.array_2d_op(&inst.operand) {
                Some(Array2DOp::Get {
                    element_size,
                    signed,
                    element_type,
                }) => {
                    let index1 = stack.pop().ok_or(CilError::StackUnderflow)?;
                    let index0 = stack.pop().ok_or(CilError::StackUnderflow)?;
                    let array = stack.pop().ok_or(CilError::StackUnderflow)?;
                    let result = new_value(value_types, element_type);
                    insts.push((
                        result,
                        Inst::Array2DLoad {
                            array,
                            index0,
                            index1,
                            element_size,
                            signed,
                        },
                    ));
                    stack.push(result);
                    return Ok(());
                }
                Some(Array2DOp::Set { element_size }) => {
                    let value = stack.pop().ok_or(CilError::StackUnderflow)?;
                    let index1 = stack.pop().ok_or(CilError::StackUnderflow)?;
                    let index0 = stack.pop().ok_or(CilError::StackUnderflow)?;
                    let array = stack.pop().ok_or(CilError::StackUnderflow)?;
                    let result = new_value(value_types, MirType::I32);
                    insts.push((
                        result,
                        Inst::Array2DStore {
                            array,
                            index0,
                            index1,
                            value,
                            element_size,
                        },
                    ));
                    return Ok(());
                }
                _ => {}
            }
            let info = resolver
                .resolve(&inst.operand)
                .ok_or(CilError::UnresolvedCall)?;
            if matches!(info.target, CallTarget::Intrinsic(Intrinsic::IntToString)) {
                let (source, _) = last_local_addr
                    .take()
                    .ok_or(CilError::Unsupported(inst.opcode))?;
                let value = addr_base(source, locals, local_types, args, value_types, insts)?;
                let result = new_value(value_types, MirType::ObjectRef);
                insts.push((result, Inst::IntToString { value }));
                stack.push(result);
                return Ok(());
            }
            let this = last_local_addr
                .take()
                .map(|(source, off)| -> Result<ValueId, CilError> {
                    let base = addr_base(source, locals, local_types, args, value_types, insts)?;
                    let ptr = new_value(value_types, MirType::ManagedPtr);
                    insts.push((ptr, Inst::FieldAddr { base, offset: off }));
                    Ok(ptr)
                })
                .transpose()?;
            let explicit = info.args.saturating_sub(this.is_some() as usize);
            let mut call_args = Vec::with_capacity(info.args);
            for _ in 0..explicit {
                call_args.push(stack.pop().ok_or(CilError::StackUnderflow)?);
            }
            call_args.reverse();
            if let Some(this) = this {
                call_args.insert(0, this);
            }
            match info.target {
                CallTarget::Internal(callee) => {
                    let result = new_value(value_types, info.result_type.unwrap_or(MirType::I32));
                    let is_callvirt = inst.opcode == Opcode::Callvirt;
                    let interface_tag = is_callvirt
                        .then(|| resolver.interface_call_tag(&inst.operand))
                        .flatten();
                    let slot = is_callvirt
                        .then(|| resolver.virtual_slot(&inst.operand))
                        .flatten();
                    let call_inst = match (interface_tag, slot) {
                        (Some(tag), _) => Inst::CallInterface {
                            tag,
                            args: call_args,
                        },
                        (_, Some(slot)) => Inst::CallVirtual {
                            slot: slot as u32,
                            args: call_args,
                        },
                        (None, None) => Inst::Call {
                            callee,
                            args: call_args,
                        },
                    };
                    insts.push((result, call_inst));
                    if info.has_result {
                        stack.push(result);
                    }
                }
                CallTarget::Intrinsic(Intrinsic::DebugWriteLine) => {
                    let string_value = *call_args.first().ok_or(CilError::StackUnderflow)?;
                    let bytes = strings
                        .iter()
                        .rev()
                        .find(|(v, _)| *v == string_value)
                        .map(|(_, b)| b.clone())
                        .ok_or(CilError::UnresolvedCall)?;
                    let mut text = bytes.into_vec();
                    text.push(b'\n');
                    text.push(0);
                    let result = new_value(value_types, MirType::I32);
                    insts.push((
                        result,
                        Inst::SemihostWrite {
                            text: text.into_boxed_slice(),
                        },
                    ));
                }
                CallTarget::Intrinsic(Intrinsic::ConsoleWriteLineInt) => {
                    let value = *call_args.first().ok_or(CilError::StackUnderflow)?;
                    let result = new_value(value_types, MirType::I32);
                    insts.push((result, Inst::WriteInt { value }));
                }
                CallTarget::Intrinsic(Intrinsic::StringEquals) => {
                    let lhs = *call_args.first().ok_or(CilError::StackUnderflow)?;
                    let rhs = *call_args.get(1).ok_or(CilError::StackUnderflow)?;
                    let result = new_value(value_types, MirType::I32);
                    insts.push((result, Inst::StringEquals { lhs, rhs }));
                    stack.push(result);
                }
                CallTarget::Intrinsic(Intrinsic::StringConcat) => {
                    let mut acc = *call_args.first().ok_or(CilError::StackUnderflow)?;
                    for &next in &call_args[1..] {
                        let result = new_value(value_types, MirType::ObjectRef);
                        insts.push((
                            result,
                            Inst::StringConcat {
                                lhs: acc,
                                rhs: next,
                            },
                        ));
                        acc = result;
                    }
                    stack.push(acc);
                }
                CallTarget::Intrinsic(Intrinsic::IntToString) => {
                    unreachable!(
                        "Int32.ToString() is intercepted before the call-argument handling"
                    );
                }
                CallTarget::Intrinsic(Intrinsic::ObjectCtor) => {
                }
                CallTarget::Intrinsic(Intrinsic::ArrayGetLength) => {
                    let array = *call_args.first().ok_or(CilError::StackUnderflow)?;
                    let dim = *call_args.get(1).ok_or(CilError::StackUnderflow)?;
                    let dim_const = insts
                        .iter()
                        .rev()
                        .find(|(v, _)| *v == dim)
                        .and_then(|(_, i)| match i {
                            Inst::ConstInt { value, .. } => u32::try_from(*value).ok(),
                            _ => None,
                        })
                        .ok_or(CilError::Unsupported(inst.opcode))?;
                    let result = new_value(value_types, MirType::I32);
                    insts.push((
                        result,
                        Inst::FieldLoad {
                            base: array,
                            offset: dim_const * 4,
                        },
                    ));
                    stack.push(result);
                }
            }
        }
        Opcode::LdlocaS | Opcode::Ldloca => {
            let Operand::Variable(n) = &inst.operand else {
                return Err(CilError::BadOperand);
            };
            *last_local_addr = Some((AddrOf::Local(*n as usize), 0));
        }
        Opcode::LdargaS | Opcode::Ldarga => {
            let Operand::Variable(n) = &inst.operand else {
                return Err(CilError::BadOperand);
            };
            *last_local_addr = Some((AddrOf::Arg(*n as usize), 0));
        }
        Opcode::Ldflda => {
            let (source, offset) = last_local_addr.take().ok_or(CilError::BadOperand)?;
            let field = resolver
                .field_offset(&inst.operand)
                .ok_or(CilError::BadOperand)?;
            *last_local_addr = Some((source, offset + field));
        }
        Opcode::Initobj => {
            let (AddrOf::Local(n), _) = last_local_addr.take().ok_or(CilError::BadOperand)? else {
                return Err(CilError::BadOperand);
            };
            let size = resolver
                .value_type_size(&inst.operand)
                .ok_or(CilError::BadOperand)?;
            let handle = match &inst.operand {
                Operand::Token(token) => lamella_ir::TypeHandle(token.0),
                _ => lamella_ir::TypeHandle(0),
            };
            let zeroed = new_value(value_types, MirType::ValueType { handle, size });
            insts.push((zeroed, Inst::InitStruct));
            *locals.get_mut(n).ok_or(CilError::BadOperand)? = Some(zeroed);
        }
        Opcode::Ldfld => {
            let (base, base_offset) = match last_local_addr.take() {
                Some((source, off)) => (
                    addr_base(source, locals, local_types, args, value_types, insts)?,
                    off,
                ),
                None => (stack.pop().ok_or(CilError::StackUnderflow)?, 0),
            };
            let offset = base_offset
                + resolver
                    .field_offset(&inst.operand)
                    .ok_or(CilError::BadOperand)?;
            let field_ty = resolver.field_type(&inst.operand).unwrap_or(MirType::I32);
            let result = new_value(value_types, field_ty);
            insts.push((result, Inst::FieldLoad { base, offset }));
            stack.push(result);
        }
        Opcode::Stfld => {
            let value = stack.pop().ok_or(CilError::StackUnderflow)?;
            let (base, base_offset) = match last_local_addr.take() {
                Some((source, off)) => (
                    addr_base(source, locals, local_types, args, value_types, insts)?,
                    off,
                ),
                None => (stack.pop().ok_or(CilError::StackUnderflow)?, 0),
            };
            let offset = base_offset
                + resolver
                    .field_offset(&inst.operand)
                    .ok_or(CilError::BadOperand)?;
            let placeholder = new_value(value_types, MirType::I32);
            insts.push((
                placeholder,
                Inst::FieldStore {
                    base,
                    offset,
                    value,
                },
            ));
        }
        Opcode::Newarr => {
            let element = resolver
                .array_element(&inst.operand)
                .ok_or(CilError::BadOperand)?;
            let length = stack.pop().ok_or(CilError::StackUnderflow)?;
            let array = new_value(value_types, MirType::ObjectRef);
            insts.push((
                array,
                Inst::AllocArray {
                    handle: element.handle,
                    length,
                    element_size: element.element_size,
                },
            ));
            stack.push(array);
        }
        Opcode::LdelemI8 => {
            let index = stack.pop().ok_or(CilError::StackUnderflow)?;
            let array = stack.pop().ok_or(CilError::StackUnderflow)?;
            let result = new_value(value_types, MirType::I64);
            insts.push((
                result,
                Inst::ArrayLoad {
                    array,
                    index,
                    element_size: 8,
                    signed: true,
                },
            ));
            stack.push(result);
        }
        Opcode::StelemI8 => {
            let value = stack.pop().ok_or(CilError::StackUnderflow)?;
            let index = stack.pop().ok_or(CilError::StackUnderflow)?;
            let array = stack.pop().ok_or(CilError::StackUnderflow)?;
            let placeholder = new_value(value_types, MirType::I32);
            insts.push((
                placeholder,
                Inst::ArrayStore {
                    array,
                    index,
                    value,
                    element_size: 8,
                },
            ));
        }
        Opcode::LdelemI1
        | Opcode::LdelemU1
        | Opcode::LdelemI2
        | Opcode::LdelemU2
        | Opcode::LdelemI4
        | Opcode::LdelemU4 => {
            let (element_size, signed) = match inst.opcode {
                Opcode::LdelemI1 => (1, true),
                Opcode::LdelemU1 => (1, false),
                Opcode::LdelemI2 => (2, true),
                Opcode::LdelemU2 => (2, false),
                _ => (4, false),
            };
            let index = stack.pop().ok_or(CilError::StackUnderflow)?;
            let array = stack.pop().ok_or(CilError::StackUnderflow)?;
            let result = new_value(value_types, MirType::I32);
            insts.push((
                result,
                Inst::ArrayLoad {
                    array,
                    index,
                    element_size,
                    signed,
                },
            ));
            stack.push(result);
        }
        Opcode::StelemI1 | Opcode::StelemI2 | Opcode::StelemI4 => {
            let element_size = match inst.opcode {
                Opcode::StelemI1 => 1,
                Opcode::StelemI2 => 2,
                _ => 4,
            };
            let value = stack.pop().ok_or(CilError::StackUnderflow)?;
            let index = stack.pop().ok_or(CilError::StackUnderflow)?;
            let array = stack.pop().ok_or(CilError::StackUnderflow)?;
            let placeholder = new_value(value_types, MirType::I32);
            insts.push((
                placeholder,
                Inst::ArrayStore {
                    array,
                    index,
                    value,
                    element_size,
                },
            ));
        }
        Opcode::LdelemRef => {
            let index = stack.pop().ok_or(CilError::StackUnderflow)?;
            let array = stack.pop().ok_or(CilError::StackUnderflow)?;
            let result = new_value(value_types, MirType::ObjectRef);
            insts.push((
                result,
                Inst::ArrayLoad {
                    array,
                    index,
                    element_size: 4,
                    signed: false,
                },
            ));
            stack.push(result);
        }
        Opcode::StelemRef => {
            let value = stack.pop().ok_or(CilError::StackUnderflow)?;
            let index = stack.pop().ok_or(CilError::StackUnderflow)?;
            let array = stack.pop().ok_or(CilError::StackUnderflow)?;
            let placeholder = new_value(value_types, MirType::I32);
            insts.push((
                placeholder,
                Inst::ArrayStore {
                    array,
                    index,
                    value,
                    element_size: 4,
                },
            ));
        }
        Opcode::Ldlen => {
            let array = stack.pop().ok_or(CilError::StackUnderflow)?;
            let result = new_value(value_types, MirType::I32);
            insts.push((
                result,
                Inst::FieldLoad {
                    base: array,
                    offset: 0,
                },
            ));
            stack.push(result);
        }
        Opcode::Ldsfld => {
            let offset = resolver
                .static_field_offset(&inst.operand)
                .ok_or(CilError::BadOperand)?;
            let field_ty = resolver.field_type(&inst.operand).unwrap_or(MirType::I32);
            let result = new_value(value_types, field_ty);
            insts.push((result, Inst::StaticLoad { offset }));
            stack.push(result);
        }
        Opcode::Stsfld => {
            let value = stack.pop().ok_or(CilError::StackUnderflow)?;
            let offset = resolver
                .static_field_offset(&inst.operand)
                .ok_or(CilError::BadOperand)?;
            let placeholder = new_value(value_types, MirType::I32);
            insts.push((placeholder, Inst::StaticStore { offset, value }));
        }
        Opcode::Newobj => {
            if let Some(tag) = resolver.exception_tag(&inst.operand) {
                push_const(value_types, stack, insts, i64::from(tag));
                return Ok(());
            }
            if let Some(layout) = resolver.newobj_delegate(&inst.operand) {
                let method_ptr = stack.pop().ok_or(CilError::StackUnderflow)?;
                let target = stack.pop().ok_or(CilError::StackUnderflow)?;
                let obj = new_value(value_types, MirType::ObjectRef);
                insts.push((
                    obj,
                    Inst::Alloc {
                        handle: layout.handle,
                        payload_size: layout.size,
                        ref_offsets: layout.reference_offsets.into_boxed_slice(),
                    },
                ));
                let store =
                    |insts: &mut Vec<(ValueId, Inst)>, vts: &mut Vec<MirType>, offset, value| {
                        let placeholder = new_value(vts, MirType::I32);
                        insts.push((
                            placeholder,
                            Inst::FieldStore {
                                base: obj,
                                offset,
                                value,
                            },
                        ));
                    };
                store(insts, value_types, DELEGATE_TARGET_OFFSET, target);
                store(insts, value_types, DELEGATE_METHODPTR_OFFSET, method_ptr);
                stack.push(obj);
                return Ok(());
            }
            if let Some(Array2DOp::New {
                handle,
                element_size,
            }) = resolver.array_2d_op(&inst.operand)
            {
                let dim1 = stack.pop().ok_or(CilError::StackUnderflow)?;
                let dim0 = stack.pop().ok_or(CilError::StackUnderflow)?;
                let array = new_value(value_types, MirType::ObjectRef);
                insts.push((
                    array,
                    Inst::AllocArray2D {
                        handle,
                        dim0,
                        dim1,
                        element_size,
                    },
                ));
                stack.push(array);
                return Ok(());
            }
            let info = resolver
                .resolve(&inst.operand)
                .ok_or(CilError::UnresolvedCall)?;
            let (this, result_value) = match resolver.newobj_value_type(&inst.operand) {
                Some(ty) => {
                    let temp = new_value(value_types, ty);
                    insts.push((temp, Inst::InitStruct));
                    let this = new_value(value_types, MirType::ManagedPtr);
                    insts.push((
                        this,
                        Inst::FieldAddr {
                            base: temp,
                            offset: 0,
                        },
                    ));
                    (this, temp)
                }
                None => {
                    let layout = resolver
                        .newobj_reference_layout(&inst.operand)
                        .ok_or(CilError::BadOperand)?;
                    let obj = new_value(value_types, MirType::ObjectRef);
                    insts.push((
                        obj,
                        Inst::Alloc {
                            handle: layout.handle,
                            payload_size: layout.size,
                            ref_offsets: layout.reference_offsets.into_boxed_slice(),
                        },
                    ));
                    (obj, obj)
                }
            };
            let explicit = info.args.saturating_sub(1);
            let mut call_args = Vec::with_capacity(info.args);
            for _ in 0..explicit {
                call_args.push(stack.pop().ok_or(CilError::StackUnderflow)?);
            }
            call_args.reverse();
            call_args.insert(0, this);
            match info.target {
                CallTarget::Internal(callee) => {
                    let result = new_value(value_types, MirType::I32);
                    insts.push((
                        result,
                        Inst::Call {
                            callee,
                            args: call_args,
                        },
                    ));
                }
                CallTarget::Intrinsic(_) => return Err(CilError::UnresolvedCall),
            }
            stack.push(result_value);
        }
        Opcode::Box => {
            let layout = resolver
                .boxed_layout(&inst.operand)
                .ok_or(CilError::BadOperand)?;
            let value = stack.pop().ok_or(CilError::StackUnderflow)?;
            let obj = new_value(value_types, MirType::ObjectRef);
            insts.push((
                obj,
                Inst::Alloc {
                    handle: layout.handle,
                    payload_size: layout.size,
                    ref_offsets: layout.reference_offsets.into_boxed_slice(),
                },
            ));
            let placeholder = new_value(value_types, MirType::I32);
            insts.push((
                placeholder,
                Inst::FieldStore {
                    base: obj,
                    offset: 0,
                    value,
                },
            ));
            stack.push(obj);
        }
        Opcode::UnboxAny => {
            let result_ty = resolver
                .boxed_value_type(&inst.operand)
                .ok_or(CilError::BadOperand)?;
            let obj = stack.pop().ok_or(CilError::StackUnderflow)?;
            let result = new_value(value_types, result_ty);
            insts.push((
                result,
                Inst::FieldLoad {
                    base: obj,
                    offset: 0,
                },
            ));
            stack.push(result);
        }
        Opcode::Castclass => {
            let object = stack.pop().ok_or(CilError::StackUnderflow)?;
            stack.push(object);
        }
        other => return Err(CilError::Unsupported(other)),
    }
    Ok(())
}

/// The arguments a predecessor passes to a successor. A merge block takes a
/// parameter per local, so the predecessor passes its current locals, materializing
/// a zero for any never written along this path (CIL zero-initializes locals). A
/// non-merge successor inherits directly and receives no arguments.
fn merge_args(
    target_is_merge: bool,
    local_count: usize,
    locals: &[Option<ValueId>],
    local_types: &[MirType],
    value_types: &mut Vec<MirType>,
    insts: &mut Vec<(ValueId, Inst)>,
) -> Vec<ValueId> {
    if !target_is_merge {
        return Vec::new();
    }
    (0..local_count)
        .map(|slot| match locals.get(slot).copied().flatten() {
            Some(value) => value,
            None => {
                let ty = local_types.get(slot).copied().unwrap_or(MirType::I32);
                let zero = new_value(value_types, ty);
                let init = if matches!(ty, MirType::ValueType { .. }) {
                    Inst::InitStruct
                } else {
                    Inst::ConstInt { ty, value: 0 }
                };
                insts.push((zero, init));
                zero
            }
        })
        .collect()
}

/// The instruction that zero-initializes a value of `ty`: a zeroed struct for a value type, a
/// zero constant otherwise -- how an unwritten local or a propagation placeholder defaults.
fn zero_inst(ty: MirType) -> Inst {
    if matches!(ty, MirType::ValueType { .. }) {
        Inst::InitStruct
    } else {
        Inst::ConstInt { ty, value: 0 }
    }
}

/// How a synthesized dispatch decides a catch clause matches the in-flight exception.
#[derive(Clone)]
enum DispatchMatch {
    /// `catch (System.Exception)` / `catch {}`: any in-flight exception (a nonzero tag) matches.
    CatchAll,
    /// `catch (T)`: the in-flight tag matches any tag in this set -- `T`'s own tag (exact match)
    /// plus the tags of every in-program subtype of `T` (so a thrown subtype is caught). A single
    /// element is the plain exact-match case.
    Tags(Vec<u32>),
}

/// The match rule and handler block for the catch clause at `clause_index`.
fn catch_dispatch(
    resolver: &dyn CallResolver,
    catch_clauses: &[&EhClause],
    handler_block_of_clause: &[usize],
    clause_index: usize,
) -> Result<(DispatchMatch, usize), CilError> {
    let EhKind::Catch(catch_token) = &catch_clauses[clause_index].kind else {
        return Err(CilError::Unsupported(Opcode::Throw));
    };
    let operand = Operand::Token(*catch_token);
    let match_kind = if resolver.is_catch_all_type(&operand) {
        DispatchMatch::CatchAll
    } else {
        let tags = resolver.subtype_tags(&operand);
        if tags.is_empty() {
            return Err(CilError::Unsupported(Opcode::Throw));
        }
        DispatchMatch::Tags(tags)
    };
    Ok((match_kind, handler_block_of_clause[clause_index]))
}

/// Maps catch clause indices (in first-match order) to their (match rule, handler block) pairs, for
/// a dispatch cascade.
fn catch_dispatch_list(
    clause_indices: &[usize],
    resolver: &dyn CallResolver,
    catch_clauses: &[&EhClause],
    handler_block_of_clause: &[usize],
) -> Result<Vec<(DispatchMatch, usize)>, CilError> {
    clause_indices
        .iter()
        .map(|&c| catch_dispatch(resolver, catch_clauses, handler_block_of_clause, c))
        .collect()
}

/// Synthesizes a per-try dispatch cascade, appended after the originals with stable ids. Each catch
/// in `catches` (in first-match order) gets a dispatch block -- load `g_exception_tag`, test it
/// against the catch's `match_kind` -- whose no-match edge falls through to the next catch, and a
/// clear block that resets the tag and enters the handler carrying the throw-point `locals` plus the
/// caught exception (captured as an `ObjectRef` BEFORE clearing -- a catch variable is an exception
/// reference, so it stays typed `O` through any later merge). The final no-match runs an enclosing
/// `finally` first (the `unwind`) when one guards the try, else propagates (returns with the tag set,
/// filled once the return type is known). Returns the first dispatch block's id.
#[allow(clippy::too_many_arguments)]
fn synthesize_dispatch(
    catches: &[(DispatchMatch, usize)],
    unwind: Option<usize>,
    locals: &[Option<ValueId>],
    local_count: usize,
    local_types: &[MirType],
    value_types: &mut Vec<MirType>,
    split_blocks: &mut Vec<BasicBlock>,
    block_count: usize,
    propagate_fixups: &mut Vec<usize>,
) -> usize {
    let base = block_count + split_blocks.len();
    let no_match_block = base + 2 * catches.len();

    for (i, (match_kind, handler_block)) in catches.iter().enumerate() {
        let clear = base + 2 * i + 1;
        let next = if i + 1 < catches.len() {
            base + 2 * (i + 1)
        } else {
            no_match_block
        };

        let mut dispatch_insts: Vec<(ValueId, Inst)> = Vec::new();
        let loaded = new_value(value_types, MirType::I32);
        dispatch_insts.push((
            loaded,
            Inst::StaticLoad {
                offset: G_EXCEPTION_TAG_OFFSET,
            },
        ));
        let cond = match match_kind {
            DispatchMatch::CatchAll => loaded,
            DispatchMatch::Tags(tags) => {
                let mut cond: Option<ValueId> = None;
                for &tag in tags {
                    let expected = new_value(value_types, MirType::I32);
                    dispatch_insts.push((
                        expected,
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: i64::from(tag),
                        },
                    ));
                    let matched = new_value(value_types, MirType::I32);
                    dispatch_insts.push((
                        matched,
                        Inst::Compare {
                            op: CmpOp::Eq,
                            lhs: loaded,
                            rhs: expected,
                        },
                    ));
                    cond = Some(match cond {
                        None => matched,
                        Some(prev) => {
                            let combined = new_value(value_types, MirType::I32);
                            dispatch_insts.push((
                                combined,
                                Inst::Binary {
                                    op: BinOp::Or,
                                    lhs: prev,
                                    rhs: matched,
                                },
                            ));
                            combined
                        }
                    });
                }
                cond.unwrap_or(loaded)
            }
        };
        split_blocks.push(BasicBlock {
            params: Vec::new(),
            insts: dispatch_insts,
            terminator: Some(Terminator::Branch {
                cond,
                if_true: BlockId(clear as u32),
                true_args: Vec::new(),
                if_false: BlockId(next as u32),
                false_args: Vec::new(),
            }),
        });

        let mut clear_insts: Vec<(ValueId, Inst)> = Vec::new();
        let exception = new_value(value_types, MirType::ObjectRef);
        clear_insts.push((
            exception,
            Inst::StaticLoad {
                offset: G_EXCEPTION_TAG_OFFSET,
            },
        ));
        let zero = new_value(value_types, MirType::I32);
        clear_insts.push((
            zero,
            Inst::ConstInt {
                ty: MirType::I32,
                value: 0,
            },
        ));
        let cleared = new_value(value_types, MirType::I32);
        clear_insts.push((
            cleared,
            Inst::StaticStore {
                offset: G_EXCEPTION_TAG_OFFSET,
                value: zero,
            },
        ));
        let mut handler_args = merge_args(
            true,
            local_count,
            locals,
            local_types,
            value_types,
            &mut clear_insts,
        );
        handler_args.push(exception);
        split_blocks.push(BasicBlock {
            params: Vec::new(),
            insts: clear_insts,
            terminator: Some(Terminator::Jump {
                target: BlockId(*handler_block as u32),
                args: handler_args,
            }),
        });
    }

    let no_match = match unwind {
        Some(finally) => {
            let mut landing_insts: Vec<(ValueId, Inst)> = Vec::new();
            let args = merge_args(
                true,
                local_count,
                locals,
                local_types,
                value_types,
                &mut landing_insts,
            );
            let index = block_count + split_blocks.len();
            split_blocks.push(BasicBlock {
                params: Vec::new(),
                insts: landing_insts,
                terminator: Some(Terminator::Jump {
                    target: BlockId(finally as u32),
                    args,
                }),
            });
            index
        }
        None => push_propagate(split_blocks, block_count, propagate_fixups),
    };
    debug_assert_eq!(no_match, no_match_block);

    base
}

/// Builds the terminator for a block ending in `throw` (the no-GC tag model). The exception's
/// tag -- the value `newobj E` produced -- is stored into `g_exception_tag`, then control goes to a
/// synthesized dispatch ([`synthesize_dispatch`]) for the enclosing catch (`throw_clause`); else, if
/// the throw is inside a `finally`-protected try (`finally_protect`), to that finally (which runs and
/// then propagates); else it propagates directly (returns with the tag set, filled once the return
/// type is known via `propagate_fixups`).
#[allow(clippy::too_many_arguments)]
fn build_eh_throw(
    throw_clauses: &[usize],
    catch_clauses: &[&EhClause],
    handler_block_of_clause: &[usize],
    finally_protect: Option<usize>,
    finally_handler_block: &[usize],
    resolver: &dyn CallResolver,
    stack: &mut Vec<ValueId>,
    locals: &[Option<ValueId>],
    local_count: usize,
    local_types: &[MirType],
    value_types: &mut Vec<MirType>,
    insts: &mut Vec<(ValueId, Inst)>,
    split_blocks: &mut Vec<BasicBlock>,
    block_count: usize,
    propagate_fixups: &mut Vec<usize>,
) -> Result<Terminator, CilError> {
    let tag = stack.pop().ok_or(CilError::StackUnderflow)?;
    let stored = new_value(value_types, MirType::I32);
    insts.push((
        stored,
        Inst::StaticStore {
            offset: G_EXCEPTION_TAG_OFFSET,
            value: tag,
        },
    ));

    if !throw_clauses.is_empty() {
        let catches = catch_dispatch_list(
            throw_clauses,
            resolver,
            catch_clauses,
            handler_block_of_clause,
        )?;
        let unwind = finally_protect.map(|clause| finally_handler_block[clause]);
        let dispatch = synthesize_dispatch(
            &catches,
            unwind,
            locals,
            local_count,
            local_types,
            value_types,
            split_blocks,
            block_count,
            propagate_fixups,
        );
        return Ok(Terminator::Jump {
            target: BlockId(dispatch as u32),
            args: Vec::new(),
        });
    }

    if let Some(finally_clause) = finally_protect {
        let args = merge_args(true, local_count, locals, local_types, value_types, insts);
        return Ok(Terminator::Jump {
            target: BlockId(finally_handler_block[finally_clause] as u32),
            args,
        });
    }

    let propagate = push_propagate(split_blocks, block_count, propagate_fixups);
    Ok(Terminator::Jump {
        target: BlockId(propagate as u32),
        args: Vec::new(),
    })
}

/// Builds the terminator for a try-body block ending in `leave` that exits a caught try
/// (`throw_clause` names the catch): the cross-call propagation check. A may-throw call in the
/// body left `g_exception_tag` set if it propagated, so the leave loads the tag and, when it is
/// nonzero, branches to the catch's dispatch instead of leaving normally; when it is zero the try
/// completed, so it leaves to `leave_target` (through a landing block when that is a merge, since a
/// `Branch` edge carries no arguments). Scoped to one may-throw call per try body checked at the
/// exit -- a side effect between a throwing call and the leave is not yet modeled.
#[allow(clippy::too_many_arguments)]
fn build_eh_leave(
    leave_target: usize,
    throw_clauses: &[usize],
    catch_clauses: &[&EhClause],
    handler_block_of_clause: &[usize],
    unwind: Option<usize>,
    resolver: &dyn CallResolver,
    is_merge: &impl Fn(usize) -> bool,
    locals: &[Option<ValueId>],
    local_count: usize,
    local_types: &[MirType],
    value_types: &mut Vec<MirType>,
    insts: &mut Vec<(ValueId, Inst)>,
    split_blocks: &mut Vec<BasicBlock>,
    block_count: usize,
    propagate_fixups: &mut Vec<usize>,
) -> Result<Terminator, CilError> {
    let catches = catch_dispatch_list(
        throw_clauses,
        resolver,
        catch_clauses,
        handler_block_of_clause,
    )?;
    let in_flight = new_value(value_types, MirType::I32);
    insts.push((
        in_flight,
        Inst::StaticLoad {
            offset: G_EXCEPTION_TAG_OFFSET,
        },
    ));
    let dispatch = synthesize_dispatch(
        &catches,
        unwind,
        locals,
        local_count,
        local_types,
        value_types,
        split_blocks,
        block_count,
        propagate_fixups,
    );
    let landing = split_edge_to_merge(
        leave_target,
        is_merge,
        local_count,
        locals,
        local_types,
        &[],
        value_types,
        split_blocks,
        block_count,
    );
    Ok(Terminator::Branch {
        cond: in_flight,
        if_true: BlockId(dispatch as u32),
        true_args: Vec::new(),
        if_false: BlockId(landing as u32),
        false_args: Vec::new(),
    })
}

/// The kind of runtime check a may-trap access needs, and the builtin exception it raises.
#[derive(Clone)]
enum TrapKind {
    /// An array element access: `index >= length` (unsigned) -> `IndexOutOfRangeException`.
    Bounds,
    /// A field load on an object from the stack: `base == 0` -> `NullReferenceException`.
    NullRef,
    /// `unbox.any T`: the boxed value's TypeDesc must equal `&TypeDesc(T)` (the carried `handle`) --
    /// an exact type check, since a value type has no subtypes -> `InvalidCastException`.
    Cast(TypeHandle),
    /// `castclass T`: the object's runtime TypeDesc must be one of the target's accepted handles
    /// (`T` plus its in-program subtypes); if none match -> `InvalidCastException`.
    CastClass(Vec<TypeHandle>),
    /// `castclass T` for an in-program TypeDef T: walk the object's TypeDesc base_ptr chain for T's
    /// descriptor (the exact base-pointer scan). O(depth), no per-subtype compares -> `InvalidCastException`.
    CastClassChain(TypeHandle),
    /// An integer `div`/`rem`: the divisor (the second operand) being zero -> `DivideByZeroException`.
    DivByZero,
    /// A checked `add.ovf`/`sub.ovf` (and `.un`) whose result overflows -> `OverflowException`.
    Overflow(OverflowKind),
    /// A checked `conv.ovf.*` whose value lies outside the target type's range -> `OverflowException`.
    /// `lo` is the inclusive lower bound; `hi` the inclusive upper, or `None` for a `u64` target (no
    /// upper). Bounds + the value's compares run at the value's own width (i64 when narrowing a `long`);
    /// a `u32`/`u64` upper bound past i32::MAX is unreachable from an i32 source, so that compare is skipped.
    ConvOverflow { lo: i64, hi: Option<i64> },
}

/// Which checked arithmetic an [`TrapKind::Overflow`] guards, selecting the overflow test.
#[derive(Clone, Copy)]
enum OverflowKind {
    AddSigned,
    AddUnsigned,
    SubSigned,
    SubUnsigned,
    MulSigned,
    MulUnsigned,
}

/// Above this many accepted handles (the cast target plus its in-program subtypes), a `castclass` to an
/// in-program base switches from per-subtype TypeDesc compares to the base-pointer chain scan. A narrow
/// base stays on compares -- cheaper than the runtime walk, and needs no emitted base-pointer chain.
const CAST_CHAIN_THRESHOLD: usize = 4;

/// The trap a block's FIRST instruction needs, or `None` if it is not a trap-leader: a bounds check
/// for an array access, or a null check for a field access (`ldfld`/`stfld`) whose field is declared
/// on a REFERENCE type (so its object can be null). The reference-type gate is the same one
/// `discover_blocks` used to make the leader, so the two agree.
fn trap_kind_at(inst: &Instruction, resolver: &dyn CallResolver) -> Option<TrapKind> {
    let opcode = inst.opcode;
    if control_flow::is_may_trap_access(opcode) {
        return Some(TrapKind::Bounds);
    }
    if matches!(opcode, Opcode::Ldfld | Opcode::Stfld)
        && resolver.field_on_reference_type(&inst.operand)
    {
        return Some(TrapKind::NullRef);
    }
    if opcode == Opcode::UnboxAny {
        if let Some(layout) = resolver.boxed_layout(&inst.operand) {
            return Some(TrapKind::Cast(layout.handle));
        }
    }
    if opcode == Opcode::Castclass {
        let handles = resolver.cast_subtype_handles(&inst.operand);
        if handles.len() > CAST_CHAIN_THRESHOLD {
            if let Some(target) = resolver.cast_target_chain(&inst.operand) {
                return Some(TrapKind::CastClassChain(target));
            }
        }
        if !handles.is_empty() {
            return Some(TrapKind::CastClass(handles));
        }
    }
    if matches!(
        opcode,
        Opcode::Div | Opcode::DivUn | Opcode::Rem | Opcode::RemUn
    ) {
        return Some(TrapKind::DivByZero);
    }
    let overflow = match opcode {
        Opcode::AddOvf => Some(OverflowKind::AddSigned),
        Opcode::AddOvfUn => Some(OverflowKind::AddUnsigned),
        Opcode::SubOvf => Some(OverflowKind::SubSigned),
        Opcode::SubOvfUn => Some(OverflowKind::SubUnsigned),
        Opcode::MulOvf => Some(OverflowKind::MulSigned),
        Opcode::MulOvfUn => Some(OverflowKind::MulUnsigned),
        _ => None,
    };
    if let Some(kind) = overflow {
        return Some(TrapKind::Overflow(kind));
    }
    let conv_range: Option<(i64, Option<i64>)> = match opcode {
        Opcode::ConvOvfI1 => Some((-128, Some(127))),
        Opcode::ConvOvfU1 => Some((0, Some(255))),
        Opcode::ConvOvfI2 => Some((-32768, Some(32767))),
        Opcode::ConvOvfU2 => Some((0, Some(65535))),
        Opcode::ConvOvfI4 => Some((i64::from(i32::MIN), Some(i64::from(i32::MAX)))),
        Opcode::ConvOvfU4 => Some((0, Some(i64::from(u32::MAX)))),
        Opcode::ConvOvfU8 => Some((0, None)),
        _ => None,
    };
    if let Some((lo, hi)) = conv_range {
        return Some(TrapKind::ConvOverflow { lo, hi });
    }
    None
}

/// The operand types a may-trap access takes off the stack, so a trap-access block can receive them
/// as trailing parameters: `[array, index]` for an array load, `[array, index, value]` for an array
/// store (the value's width follows the element -- `i8` -> I64, `ref` -> O, else I32), `[object]` for
/// a field load, `[object, value]` for a field store (the value typed by the field). Empty for a
/// non-access opcode.
/// Per-instruction i64-WIDTH of the top eval-stack slot just before that instruction, by a linear sim.
/// A trap-leader types its integer operands at the STRUCTURAL block-params pass -- before any value
/// flows -- so it can't read the real types; this re-derives the width (i32 vs i64) so a checked op on
/// a 64-bit operand (a `long` divide/remainder or a `conv.ovf` from `long`) is checked at i64 width.
///
/// SAFE BY DESIGN: every produced slot defaults to NARROW; only the specific i64 PRODUCERS push WIDE
/// (ldc.i8, conv.i8/u8(.ovf), an i64 local/arg/field, ldelem.i8, an i64 binary result or call return),
/// and any opcode NOT modeled CLEARS the stack. So an i32 operand is never falsely widened (no regression
/// to the working i32 trap-leaders); at worst a 64-bit operand reached through an unmodeled op reads
/// narrow -- the prior behavior. The deeper stack may drift, but a checked op's operands are pushed
/// immediately before it, so the recorded TOP is correct.
fn eval_stack_widths(
    code: &[Instruction],
    arg_types: &[MirType],
    local_types: &[MirType],
    resolver: &dyn CallResolver,
) -> Vec<bool> {
    let i64w = |t: Option<MirType>| t == Some(MirType::I64);
    let mut stack: Vec<bool> = Vec::new();
    let mut widths = alloc::vec![false; code.len()];
    for (i, inst) in code.iter().enumerate() {
        widths[i] = stack.last().copied().unwrap_or(false);
        match inst.opcode {
            Opcode::LdcI8 => stack.push(true),
            Opcode::LdcI4M1
            | Opcode::LdcI40
            | Opcode::LdcI41
            | Opcode::LdcI42
            | Opcode::LdcI43
            | Opcode::LdcI44
            | Opcode::LdcI45
            | Opcode::LdcI46
            | Opcode::LdcI47
            | Opcode::LdcI48
            | Opcode::LdcI4S
            | Opcode::LdcI4
            | Opcode::LdcR4
            | Opcode::LdcR8
            | Opcode::Ldstr
            | Opcode::Ldnull => stack.push(false),
            Opcode::Ldarg0 => stack.push(i64w(arg_types.first().copied())),
            Opcode::Ldarg1 => stack.push(i64w(arg_types.get(1).copied())),
            Opcode::Ldarg2 => stack.push(i64w(arg_types.get(2).copied())),
            Opcode::Ldarg3 => stack.push(i64w(arg_types.get(3).copied())),
            Opcode::LdargS | Opcode::Ldarg => {
                let n = match inst.operand {
                    Operand::Variable(n) => n as usize,
                    _ => 0,
                };
                stack.push(i64w(arg_types.get(n).copied()));
            }
            Opcode::Ldloc0 => stack.push(i64w(local_types.first().copied())),
            Opcode::Ldloc1 => stack.push(i64w(local_types.get(1).copied())),
            Opcode::Ldloc2 => stack.push(i64w(local_types.get(2).copied())),
            Opcode::Ldloc3 => stack.push(i64w(local_types.get(3).copied())),
            Opcode::LdlocS | Opcode::Ldloc => {
                let n = match inst.operand {
                    Operand::Variable(n) => n as usize,
                    _ => 0,
                };
                stack.push(i64w(local_types.get(n).copied()));
            }
            Opcode::Ldsfld => stack.push(i64w(resolver.field_type(&inst.operand))),
            Opcode::Ldfld => {
                stack.pop();
                stack.push(i64w(resolver.field_type(&inst.operand)));
            }
            Opcode::LdelemI8 => {
                stack.pop();
                stack.pop();
                stack.push(true);
            }
            Opcode::LdelemI1
            | Opcode::LdelemU1
            | Opcode::LdelemI2
            | Opcode::LdelemU2
            | Opcode::LdelemI4
            | Opcode::LdelemU4
            | Opcode::LdelemRef => {
                stack.pop();
                stack.pop();
                stack.push(false);
            }
            Opcode::ConvI8 | Opcode::ConvU8 | Opcode::ConvOvfI8 | Opcode::ConvOvfU8 => {
                stack.pop();
                stack.push(true);
            }
            Opcode::ConvI1
            | Opcode::ConvU1
            | Opcode::ConvI2
            | Opcode::ConvU2
            | Opcode::ConvI4
            | Opcode::ConvU4
            | Opcode::ConvOvfI1
            | Opcode::ConvOvfU1
            | Opcode::ConvOvfI2
            | Opcode::ConvOvfU2
            | Opcode::ConvOvfI4
            | Opcode::ConvOvfU4
            | Opcode::ConvR4
            | Opcode::ConvR8 => {
                stack.pop();
                stack.push(false);
            }
            Opcode::Add
            | Opcode::AddOvf
            | Opcode::AddOvfUn
            | Opcode::Sub
            | Opcode::SubOvf
            | Opcode::SubOvfUn
            | Opcode::Mul
            | Opcode::MulOvf
            | Opcode::MulOvfUn
            | Opcode::And
            | Opcode::Or
            | Opcode::Xor
            | Opcode::Div
            | Opcode::DivUn
            | Opcode::Rem
            | Opcode::RemUn => {
                let w = stack.pop().unwrap_or(false);
                stack.pop();
                stack.push(w);
            }
            Opcode::Shl | Opcode::Shr | Opcode::ShrUn => {
                stack.pop();
                let w = stack.pop().unwrap_or(false);
                stack.push(w);
            }
            Opcode::Neg | Opcode::Not => {
                let w = stack.pop().unwrap_or(false);
                stack.push(w);
            }
            Opcode::Ceq | Opcode::Cgt | Opcode::CgtUn | Opcode::Clt | Opcode::CltUn => {
                stack.pop();
                stack.pop();
                stack.push(false);
            }
            Opcode::Dup => stack.push(stack.last().copied().unwrap_or(false)),
            Opcode::Pop => {
                stack.pop();
            }
            Opcode::Call | Opcode::Callvirt => match resolver.resolve(&inst.operand) {
                Some(info) => {
                    for _ in 0..info.args {
                        stack.pop();
                    }
                    if info.has_result {
                        stack.push(i64w(info.result_type));
                    }
                }
                None => stack.clear(),
            },
            Opcode::Nop => {}
            _ => stack.clear(),
        }
    }
    widths
}

fn trap_operand_types(inst: &Instruction, wide: bool, resolver: &dyn CallResolver) -> Vec<MirType> {
    let opcode = inst.opcode;
    if matches!(opcode, Opcode::Ldfld | Opcode::UnboxAny | Opcode::Castclass) {
        return vec![MirType::ObjectRef];
    }
    if opcode == Opcode::Stfld {
        let value = resolver.field_type(&inst.operand).unwrap_or(MirType::I32);
        return vec![MirType::ObjectRef, value];
    }
    if control_flow::is_may_trap_load(opcode) {
        return vec![MirType::ObjectRef, MirType::I32];
    }
    if matches!(
        opcode,
        Opcode::ConvOvfI1
            | Opcode::ConvOvfU1
            | Opcode::ConvOvfI2
            | Opcode::ConvOvfU2
            | Opcode::ConvOvfI4
            | Opcode::ConvOvfU4
            | Opcode::ConvOvfU8
    ) {
        return vec![if wide { MirType::I64 } else { MirType::I32 }];
    }
    if matches!(
        opcode,
        Opcode::Div
            | Opcode::DivUn
            | Opcode::Rem
            | Opcode::RemUn
            | Opcode::AddOvf
            | Opcode::AddOvfUn
            | Opcode::SubOvf
            | Opcode::SubOvfUn
            | Opcode::MulOvf
            | Opcode::MulOvfUn
    ) {
        let w = if wide { MirType::I64 } else { MirType::I32 };
        return vec![w, w];
    }
    let value = match opcode {
        Opcode::StelemI8 => MirType::I64,
        Opcode::StelemRef => MirType::ObjectRef,
        Opcode::StelemI1 | Opcode::StelemI2 | Opcode::StelemI4 => MirType::I32,
        _ => return Vec::new(),
    };
    vec![MirType::ObjectRef, MirType::I32, value]
}

/// Emits the overflow test for a checked add/sub into `insts`, returning the value that is nonzero on
/// overflow (a trap-leader's failure condition). Signed add/sub use the sign-bit identity; unsigned use
/// the carry/borrow. The op itself (a plain Add/Sub) runs after, recomputing the result -- a redundant
/// add a later MIR pass can fold.
fn emit_overflow_check(
    kind: OverflowKind,
    a: ValueId,
    b: ValueId,
    value_types: &mut Vec<MirType>,
    insts: &mut Vec<(ValueId, Inst)>,
) -> ValueId {
    let bin = |op: BinOp, lhs, rhs, vts: &mut Vec<MirType>, is: &mut Vec<(ValueId, Inst)>| {
        let v = new_value(vts, MirType::I32);
        is.push((v, Inst::Binary { op, lhs, rhs }));
        v
    };
    let cmp = |op: CmpOp, lhs, rhs, vts: &mut Vec<MirType>, is: &mut Vec<(ValueId, Inst)>| {
        let v = new_value(vts, MirType::I32);
        is.push((v, Inst::Compare { op, lhs, rhs }));
        v
    };
    let neg = |value, vts: &mut Vec<MirType>, is: &mut Vec<(ValueId, Inst)>| {
        let zero = new_value(vts, MirType::I32);
        is.push((
            zero,
            Inst::ConstInt {
                ty: MirType::I32,
                value: 0,
            },
        ));
        cmp_value(CmpOp::SignedLt, value, zero, vts, is)
    };
    match kind {
        OverflowKind::AddUnsigned => {
            let sum = bin(BinOp::Add, a, b, value_types, insts);
            cmp(CmpOp::UnsignedLt, sum, a, value_types, insts)
        }
        OverflowKind::SubUnsigned => cmp(CmpOp::UnsignedLt, a, b, value_types, insts),
        OverflowKind::AddSigned => {
            let sum = bin(BinOp::Add, a, b, value_types, insts);
            let ax = bin(BinOp::Xor, a, sum, value_types, insts);
            let bx = bin(BinOp::Xor, b, sum, value_types, insts);
            let and = bin(BinOp::And, ax, bx, value_types, insts);
            neg(and, value_types, insts)
        }
        OverflowKind::SubSigned => {
            let diff = bin(BinOp::Sub, a, b, value_types, insts);
            let ab = bin(BinOp::Xor, a, b, value_types, insts);
            let ad = bin(BinOp::Xor, a, diff, value_types, insts);
            let and = bin(BinOp::And, ab, ad, value_types, insts);
            neg(and, value_types, insts)
        }
        OverflowKind::MulSigned => {
            let a64 = new_value(value_types, MirType::I64);
            insts.push((
                a64,
                Inst::Widen {
                    value: a,
                    signed: true,
                },
            ));
            let b64 = new_value(value_types, MirType::I64);
            insts.push((
                b64,
                Inst::Widen {
                    value: b,
                    signed: true,
                },
            ));
            let prod = new_value(value_types, MirType::I64);
            insts.push((
                prod,
                Inst::Binary {
                    op: BinOp::Mul,
                    lhs: a64,
                    rhs: b64,
                },
            ));
            let min = new_value(value_types, MirType::I64);
            insts.push((
                min,
                Inst::ConstInt {
                    ty: MirType::I64,
                    value: i32::MIN as i64,
                },
            ));
            let max = new_value(value_types, MirType::I64);
            insts.push((
                max,
                Inst::ConstInt {
                    ty: MirType::I64,
                    value: i32::MAX as i64,
                },
            ));
            let below = cmp(CmpOp::SignedLt, prod, min, value_types, insts);
            let above = cmp(CmpOp::SignedGt, prod, max, value_types, insts);
            bin(BinOp::Or, below, above, value_types, insts)
        }
        OverflowKind::MulUnsigned => {
            let a64 = new_value(value_types, MirType::I64);
            insts.push((
                a64,
                Inst::Widen {
                    value: a,
                    signed: false,
                },
            ));
            let b64 = new_value(value_types, MirType::I64);
            insts.push((
                b64,
                Inst::Widen {
                    value: b,
                    signed: false,
                },
            ));
            let prod = new_value(value_types, MirType::I64);
            insts.push((
                prod,
                Inst::Binary {
                    op: BinOp::Mul,
                    lhs: a64,
                    rhs: b64,
                },
            ));
            let max = new_value(value_types, MirType::I64);
            insts.push((
                max,
                Inst::ConstInt {
                    ty: MirType::I64,
                    value: u32::MAX as i64,
                },
            ));
            cmp(CmpOp::UnsignedGt, prod, max, value_types, insts)
        }
    }
}

/// Pushes a `Compare` of `lhs`/`rhs` and returns its boolean value.
fn cmp_value(
    op: CmpOp,
    lhs: ValueId,
    rhs: ValueId,
    value_types: &mut Vec<MirType>,
    insts: &mut Vec<(ValueId, Inst)>,
) -> ValueId {
    let v = new_value(value_types, MirType::I32);
    insts.push((v, Inst::Compare { op, lhs, rhs }));
    v
}

/// Builds the terminator for a block that flows into a may-trap access (the next block): the
/// synthesized runtime check. `operands` is what the block left on its stack -- `[array, index]` for
/// an array load, `[array, index, value]` for a store, `[object]` for a field load. By `kind` it
/// computes the failure condition -- an out-of-range array index (unsigned `index >= length`, so a
/// negative index traps too) or a null field base (`object == 0`) -- and on failure routes to the
/// enclosing handler exactly as `throw new <builtin>()` would: storing the type's tag and going to
/// the catch dispatch / finally / propagation via [`build_eh_throw`]. Otherwise control falls to the
/// access block through a landing carrying the locals plus the operands, so the access runs with the
/// check passed.
#[allow(clippy::too_many_arguments)]
fn build_trap_access_check(
    access_block: usize,
    kind: TrapKind,
    operands: &[ValueId],
    throw_clauses: &[usize],
    catch_clauses: &[&EhClause],
    handler_block_of_clause: &[usize],
    finally_protect: Option<usize>,
    finally_handler_block: &[usize],
    resolver: &dyn CallResolver,
    locals: &[Option<ValueId>],
    local_count: usize,
    local_types: &[MirType],
    value_types: &mut Vec<MirType>,
    insts: &mut Vec<(ValueId, Inst)>,
    split_blocks: &mut Vec<BasicBlock>,
    block_count: usize,
    propagate_fixups: &mut Vec<usize>,
) -> Result<Terminator, CilError> {
    let (failed, exception_name) = match kind {
        TrapKind::Bounds => {
            let array = operands[0];
            let index = operands[1];
            let length = new_value(value_types, MirType::I32);
            insts.push((
                length,
                Inst::FieldLoad {
                    base: array,
                    offset: 0,
                },
            ));
            let oob = new_value(value_types, MirType::I32);
            insts.push((
                oob,
                Inst::Compare {
                    op: CmpOp::UnsignedGe,
                    lhs: index,
                    rhs: length,
                },
            ));
            (oob, "IndexOutOfRangeException")
        }
        TrapKind::NullRef => {
            let object = operands[0];
            let null = new_value(value_types, MirType::ObjectRef);
            insts.push((
                null,
                Inst::ConstInt {
                    ty: MirType::ObjectRef,
                    value: 0,
                },
            ));
            let is_null = new_value(value_types, MirType::I32);
            insts.push((
                is_null,
                Inst::Compare {
                    op: CmpOp::Eq,
                    lhs: object,
                    rhs: null,
                },
            ));
            (is_null, "NullReferenceException")
        }
        TrapKind::Cast(handle) => {
            let object = operands[0];
            let box_desc = new_value(value_types, MirType::I32);
            insts.push((box_desc, Inst::LoadTypeDesc { object }));
            let expected = new_value(value_types, MirType::I32);
            insts.push((expected, Inst::TypeDescAddr { handle }));
            let mismatch = new_value(value_types, MirType::I32);
            insts.push((
                mismatch,
                Inst::Compare {
                    op: CmpOp::Ne,
                    lhs: box_desc,
                    rhs: expected,
                },
            ));
            (mismatch, "InvalidCastException")
        }
        TrapKind::CastClass(handles) => {
            let object = operands[0];
            let object_desc = new_value(value_types, MirType::I32);
            insts.push((object_desc, Inst::LoadTypeDesc { object }));
            let mut matched: Option<ValueId> = None;
            for handle in &handles {
                let expected = new_value(value_types, MirType::I32);
                insts.push((expected, Inst::TypeDescAddr { handle: *handle }));
                let eq = new_value(value_types, MirType::I32);
                insts.push((
                    eq,
                    Inst::Compare {
                        op: CmpOp::Eq,
                        lhs: object_desc,
                        rhs: expected,
                    },
                ));
                matched = Some(match matched {
                    None => eq,
                    Some(prev) => {
                        let combined = new_value(value_types, MirType::I32);
                        insts.push((
                            combined,
                            Inst::Binary {
                                op: BinOp::Or,
                                lhs: prev,
                                rhs: eq,
                            },
                        ));
                        combined
                    }
                });
            }
            let matched = matched.unwrap_or(object_desc);
            let zero = new_value(value_types, MirType::I32);
            insts.push((
                zero,
                Inst::ConstInt {
                    ty: MirType::I32,
                    value: 0,
                },
            ));
            let mismatch = new_value(value_types, MirType::I32);
            insts.push((
                mismatch,
                Inst::Compare {
                    op: CmpOp::Eq,
                    lhs: matched,
                    rhs: zero,
                },
            ));
            (mismatch, "InvalidCastException")
        }
        TrapKind::CastClassChain(target) => {
            let object = operands[0];
            let object_desc = new_value(value_types, MirType::I32);
            insts.push((object_desc, Inst::LoadTypeDesc { object }));
            let target_desc = new_value(value_types, MirType::I32);
            insts.push((target_desc, Inst::TypeDescAddr { handle: target }));
            let matched = new_value(value_types, MirType::I32);
            insts.push((
                matched,
                Inst::CastClassScan {
                    args: alloc::vec![object_desc, target_desc],
                },
            ));
            let zero = new_value(value_types, MirType::I32);
            insts.push((
                zero,
                Inst::ConstInt {
                    ty: MirType::I32,
                    value: 0,
                },
            ));
            let mismatch = new_value(value_types, MirType::I32);
            insts.push((
                mismatch,
                Inst::Compare {
                    op: CmpOp::Eq,
                    lhs: matched,
                    rhs: zero,
                },
            ));
            (mismatch, "InvalidCastException")
        }
        TrapKind::DivByZero => {
            let divisor = operands[1];
            let divisor_ty = value_types
                .get(divisor.0 as usize)
                .copied()
                .unwrap_or(MirType::I32);
            let zero = new_value(value_types, divisor_ty);
            insts.push((
                zero,
                Inst::ConstInt {
                    ty: divisor_ty,
                    value: 0,
                },
            ));
            let is_zero = new_value(value_types, MirType::I32);
            insts.push((
                is_zero,
                Inst::Compare {
                    op: CmpOp::Eq,
                    lhs: divisor,
                    rhs: zero,
                },
            ));
            (is_zero, "DivideByZeroException")
        }
        TrapKind::Overflow(kind) => {
            let ovf = emit_overflow_check(kind, operands[0], operands[1], value_types, insts);
            (ovf, "OverflowException")
        }
        TrapKind::ConvOverflow { lo, hi } => {
            let value = operands[0];
            let ty = value_types
                .get(value.0 as usize)
                .copied()
                .unwrap_or(MirType::I32);
            let lo_c = new_value(value_types, ty);
            insts.push((lo_c, Inst::ConstInt { ty, value: lo }));
            let below = cmp_value(CmpOp::SignedLt, value, lo_c, value_types, insts);
            let source_max = if ty == MirType::I64 {
                i64::MAX
            } else {
                i64::from(i32::MAX)
            };
            match hi {
                Some(hi) if hi <= source_max => {
                    let hi_c = new_value(value_types, ty);
                    insts.push((hi_c, Inst::ConstInt { ty, value: hi }));
                    let above = cmp_value(CmpOp::SignedGt, value, hi_c, value_types, insts);
                    let ovf = new_value(value_types, MirType::I32);
                    insts.push((
                        ovf,
                        Inst::Binary {
                            op: BinOp::Or,
                            lhs: below,
                            rhs: above,
                        },
                    ));
                    (ovf, "OverflowException")
                }
                _ => (below, "OverflowException"),
            }
        }
    };

    let tag = resolver
        .builtin_exception_tag("System", exception_name)
        .ok_or(CilError::UnsupportedControlFlow)?;
    let mut trap_insts: Vec<(ValueId, Inst)> = Vec::new();
    let tag_value = new_value(value_types, MirType::I32);
    trap_insts.push((
        tag_value,
        Inst::ConstInt {
            ty: MirType::I32,
            value: i64::from(tag),
        },
    ));
    let mut trap_stack = vec![tag_value];
    let trap_terminator = build_eh_throw(
        throw_clauses,
        catch_clauses,
        handler_block_of_clause,
        finally_protect,
        finally_handler_block,
        resolver,
        &mut trap_stack,
        locals,
        local_count,
        local_types,
        value_types,
        &mut trap_insts,
        split_blocks,
        block_count,
        propagate_fixups,
    )?;
    let trap = block_count + split_blocks.len();
    split_blocks.push(BasicBlock {
        params: Vec::new(),
        insts: trap_insts,
        terminator: Some(trap_terminator),
    });

    let landing = block_count + split_blocks.len();
    let mut landing_insts: Vec<(ValueId, Inst)> = Vec::new();
    let mut access_args = merge_args(
        true,
        local_count,
        locals,
        local_types,
        value_types,
        &mut landing_insts,
    );
    access_args.extend_from_slice(operands);
    split_blocks.push(BasicBlock {
        params: Vec::new(),
        insts: landing_insts,
        terminator: Some(Terminator::Jump {
            target: BlockId(access_block as u32),
            args: access_args,
        }),
    });

    Ok(Terminator::Branch {
        cond: failed,
        if_true: BlockId(trap as u32),
        true_args: Vec::new(),
        if_false: BlockId(landing as u32),
        false_args: Vec::new(),
    })
}

/// Builds the terminator for a `leave` that exits a try with a `finally`: a plain jump to the
/// finally handler carrying the locals. The finally runs, then its `endfinally` epilogue resumes at
/// the leave target -- there is no tag check here (the `endfinally` decides resume vs propagate).
fn build_eh_finally_leave(
    finally_handler: usize,
    locals: &[Option<ValueId>],
    local_count: usize,
    local_types: &[MirType],
    value_types: &mut Vec<MirType>,
    insts: &mut Vec<(ValueId, Inst)>,
) -> Terminator {
    let args = merge_args(true, local_count, locals, local_types, value_types, insts);
    Terminator::Jump {
        target: BlockId(finally_handler as u32),
        args,
    }
}

/// Builds the terminator for a finally handler's `endfinally`: load the in-flight tag and, when it
/// is nonzero, propagate (the exception passed through the finally); otherwise resume at the
/// finally's normal continuation (the pending leave target), carrying the finally's locals through a
/// landing block (a `Branch` edge carries no arguments). When the try always throws there is no
/// normal continuation, so the finally simply propagates. The propagation return value is filled in
/// once the function's return type is known.
#[allow(clippy::too_many_arguments)]
fn build_eh_endfinally(
    continuation: Option<usize>,
    locals: &[Option<ValueId>],
    local_count: usize,
    local_types: &[MirType],
    value_types: &mut Vec<MirType>,
    insts: &mut Vec<(ValueId, Inst)>,
    split_blocks: &mut Vec<BasicBlock>,
    block_count: usize,
    propagate_fixups: &mut Vec<usize>,
) -> Terminator {
    let Some(continuation) = continuation else {
        let propagate = push_propagate(split_blocks, block_count, propagate_fixups);
        return Terminator::Jump {
            target: BlockId(propagate as u32),
            args: Vec::new(),
        };
    };
    let in_flight = new_value(value_types, MirType::I32);
    insts.push((
        in_flight,
        Inst::StaticLoad {
            offset: G_EXCEPTION_TAG_OFFSET,
        },
    ));
    let landing = block_count + split_blocks.len();
    let propagate = landing + 1;
    let mut landing_insts: Vec<(ValueId, Inst)> = Vec::new();
    let cont_args = merge_args(
        true,
        local_count,
        locals,
        local_types,
        value_types,
        &mut landing_insts,
    );
    split_blocks.push(BasicBlock {
        params: Vec::new(),
        insts: landing_insts,
        terminator: Some(Terminator::Jump {
            target: BlockId(continuation as u32),
            args: cont_args,
        }),
    });
    let propagate_actual = push_propagate(split_blocks, block_count, propagate_fixups);
    debug_assert_eq!(propagate_actual, propagate);
    Terminator::Branch {
        cond: in_flight,
        if_true: BlockId(propagate as u32),
        true_args: Vec::new(),
        if_false: BlockId(landing as u32),
        false_args: Vec::new(),
    }
}

/// Pushes a propagation block -- an exit that returns, leaving `g_exception_tag` set -- and
/// records it for return-value fill-in once the function's return type is known. Returns its
/// block index.
fn push_propagate(
    split_blocks: &mut Vec<BasicBlock>,
    block_count: usize,
    propagate_fixups: &mut Vec<usize>,
) -> usize {
    let index = block_count + split_blocks.len();
    split_blocks.push(BasicBlock {
        params: Vec::new(),
        insts: Vec::new(),
        terminator: Some(Terminator::Return(None)),
    });
    propagate_fixups.push(index);
    index
}

/// Builds the terminator for a block ending in a branch. `fallthrough` is the
/// instruction index immediately after the block (the not-taken successor).
#[allow(clippy::too_many_arguments)]
fn build_branch(
    inst: &Instruction,
    fallthrough: usize,
    block_of: &impl Fn(usize) -> Option<usize>,
    is_merge: &impl Fn(usize) -> bool,
    local_count: usize,
    stack: &mut Vec<ValueId>,
    locals: &[Option<ValueId>],
    local_types: &[MirType],
    value_types: &mut Vec<MirType>,
    insts: &mut Vec<(ValueId, Inst)>,
    split_blocks: &mut Vec<BasicBlock>,
    block_count: usize,
) -> Result<Terminator, CilError> {
    let Operand::Target(target_instr) = &inst.operand else {
        return Err(CilError::BadOperand);
    };
    let target = block_of(*target_instr as usize).ok_or(CilError::UnsupportedControlFlow)?;

    match control_flow::branch_kind(inst.opcode) {
        Some(control_flow::BranchKind::Unconditional) => {
            let mut args = merge_args(
                is_merge(target),
                local_count,
                locals,
                local_types,
                value_types,
                insts,
            );
            if is_merge(target) {
                args.extend(stack.iter().copied());
            }
            Ok(Terminator::Jump {
                target: BlockId(target as u32),
                args,
            })
        }
        Some(control_flow::BranchKind::Conditional) => {
            let other = block_of(fallthrough).ok_or(CilError::UnsupportedControlFlow)?;
            let (cond, if_true, if_false) =
                build_condition(inst.opcode, target, other, stack, value_types, insts)?;
            let stack_args: Vec<ValueId> = stack.clone();
            let mut split = |block: usize| {
                split_edge_to_merge(
                    block,
                    is_merge,
                    local_count,
                    locals,
                    local_types,
                    &stack_args,
                    value_types,
                    split_blocks,
                    block_count,
                )
            };
            let if_true = split(if_true);
            let if_false = split(if_false);
            Ok(Terminator::Branch {
                cond,
                if_true: BlockId(if_true as u32),
                true_args: Vec::new(),
                if_false: BlockId(if_false as u32),
                false_args: Vec::new(),
            })
        }
        None => Err(CilError::UnsupportedControlFlow),
    }
}

/// Builds the terminator for a block ending in `switch`: a cascade of equality tests that
/// realizes the jump table, since MIR has no switch terminator. CIL `switch (t0, .., t_{N-1})`
/// pops an index (typed int32 / native int, treated unsigned for the range check) and branches
/// to `t_index` when `index < N`, else falls through to `fallthrough` (the default). The
/// equality cascade `index == 0 -> t0; index == 1 -> t1; ...; else default` matches that exactly:
/// an index outside `[0, N-1]` equals no case and reaches the default. The case-0 test is this
/// block's own terminator; tests for cases 1..N-1 become fresh param-less blocks chained through
/// their not-matched edge. Every case target and the default route through `split_edge_to_merge`,
/// so a merge target is still reached by a `Jump` that carries the locals (the switch block's
/// locals, shared by every arm -- `switch` reads but does not write locals).
#[allow(clippy::too_many_arguments)]
fn build_switch(
    inst: &Instruction,
    fallthrough: usize,
    block_of: &impl Fn(usize) -> Option<usize>,
    is_merge: &impl Fn(usize) -> bool,
    local_count: usize,
    stack: &mut Vec<ValueId>,
    locals: &[Option<ValueId>],
    local_types: &[MirType],
    value_types: &mut Vec<MirType>,
    insts: &mut Vec<(ValueId, Inst)>,
    split_blocks: &mut Vec<BasicBlock>,
    block_count: usize,
) -> Result<Terminator, CilError> {
    let Operand::Switch(targets) = &inst.operand else {
        return Err(CilError::BadOperand);
    };
    let index = stack.pop().ok_or(CilError::StackUnderflow)?;
    let stack_args: Vec<ValueId> = stack.clone();

    let default = block_of(fallthrough).ok_or(CilError::UnsupportedControlFlow)?;
    let mut not_matched = split_edge_to_merge(
        default,
        is_merge,
        local_count,
        locals,
        local_types,
        &stack_args,
        value_types,
        split_blocks,
        block_count,
    );

    for case in (1..targets.len()).rev() {
        let target = block_of(targets[case] as usize).ok_or(CilError::UnsupportedControlFlow)?;
        let matched = split_edge_to_merge(
            target,
            is_merge,
            local_count,
            locals,
            local_types,
            &stack_args,
            value_types,
            split_blocks,
            block_count,
        );
        let test_block = block_count + split_blocks.len();
        let mut test_insts: Vec<(ValueId, Inst)> = Vec::new();
        let cond = emit_case_test(index, case as i64, value_types, &mut test_insts);
        split_blocks.push(BasicBlock {
            params: Vec::new(),
            insts: test_insts,
            terminator: Some(Terminator::Branch {
                cond,
                if_true: BlockId(matched as u32),
                true_args: Vec::new(),
                if_false: BlockId(not_matched as u32),
                false_args: Vec::new(),
            }),
        });
        not_matched = test_block;
    }

    let Some(&first) = targets.first() else {
        return Ok(Terminator::Jump {
            target: BlockId(not_matched as u32),
            args: Vec::new(),
        });
    };
    let target0 = block_of(first as usize).ok_or(CilError::UnsupportedControlFlow)?;
    let matched = split_edge_to_merge(
        target0,
        is_merge,
        local_count,
        locals,
        local_types,
        &stack_args,
        value_types,
        split_blocks,
        block_count,
    );
    let cond = emit_case_test(index, 0, value_types, insts);
    Ok(Terminator::Branch {
        cond,
        if_true: BlockId(matched as u32),
        true_args: Vec::new(),
        if_false: BlockId(not_matched as u32),
        false_args: Vec::new(),
    })
}

/// Emits `index == case` (one switch arm's selector equality) into `insts`, returning the
/// boolean result. The constant is typed to match the index so the compare's operands agree
/// (the index is int32 / native int, both `MirType::I32` on a 32-bit target).
fn emit_case_test(
    index: ValueId,
    case: i64,
    value_types: &mut Vec<MirType>,
    insts: &mut Vec<(ValueId, Inst)>,
) -> ValueId {
    let index_ty = value_types
        .get(index.index())
        .copied()
        .unwrap_or(MirType::I32);
    let konst = new_value(value_types, index_ty);
    insts.push((
        konst,
        Inst::ConstInt {
            ty: index_ty,
            value: case,
        },
    ));
    let cond = new_value(value_types, MirType::I32);
    insts.push((
        cond,
        Inst::Compare {
            op: CmpOp::Eq,
            lhs: index,
            rhs: konst,
        },
    ));
    cond
}

/// If `block` is a merge, splits the critical edge into it: appends a fresh param-less block
/// that jumps to the merge carrying the branching block's `locals`, and returns its index.
/// Otherwise returns `block` unchanged. So a conditional branch never targets a merge directly.
#[allow(clippy::too_many_arguments)]
fn split_edge_to_merge(
    block: usize,
    is_merge: &impl Fn(usize) -> bool,
    local_count: usize,
    locals: &[Option<ValueId>],
    local_types: &[MirType],
    stack: &[ValueId],
    value_types: &mut Vec<MirType>,
    split_blocks: &mut Vec<BasicBlock>,
    block_count: usize,
) -> usize {
    if !is_merge(block) {
        return block;
    }
    let mut insts: Vec<(ValueId, Inst)> = Vec::new();
    let mut args = merge_args(
        true,
        local_count,
        locals,
        local_types,
        value_types,
        &mut insts,
    );
    args.extend(stack.iter().copied());
    let index = block_count + split_blocks.len();
    split_blocks.push(BasicBlock {
        params: Vec::new(),
        insts,
        terminator: Some(Terminator::Jump {
            target: BlockId(block as u32),
            args,
        }),
    });
    index
}

/// Builds the condition value for a conditional branch and resolves which block is
/// the taken (`if_true`) and not-taken (`if_false`) successor. The compare-branches
/// (`beq`/`blt`/...) test two popped operands; `brtrue`/`brfalse` test one.
fn build_condition(
    op: Opcode,
    target: usize,
    fallthrough: usize,
    stack: &mut Vec<ValueId>,
    value_types: &mut Vec<MirType>,
    insts: &mut Vec<(ValueId, Inst)>,
) -> Result<(ValueId, usize, usize), CilError> {
    let compare_op = match op {
        Opcode::BeqS | Opcode::Beq => Some(CmpOp::Eq),
        Opcode::BneUnS | Opcode::BneUn => Some(CmpOp::Ne),
        Opcode::BgtS | Opcode::Bgt => Some(CmpOp::SignedGt),
        Opcode::BgtUnS | Opcode::BgtUn => Some(CmpOp::UnsignedGt),
        Opcode::BltS | Opcode::Blt => Some(CmpOp::SignedLt),
        Opcode::BltUnS | Opcode::BltUn => Some(CmpOp::UnsignedLt),
        Opcode::BgeS | Opcode::Bge => Some(CmpOp::SignedGe),
        Opcode::BgeUnS | Opcode::BgeUn => Some(CmpOp::UnsignedGe),
        Opcode::BleS | Opcode::Ble => Some(CmpOp::SignedLe),
        Opcode::BleUnS | Opcode::BleUn => Some(CmpOp::UnsignedLe),
        _ => None,
    };
    if let Some(cmpop) = compare_op {
        let rhs = stack.pop().ok_or(CilError::StackUnderflow)?;
        let lhs = stack.pop().ok_or(CilError::StackUnderflow)?;
        let cond = new_value(value_types, MirType::I32);
        insts.push((
            cond,
            Inst::Compare {
                op: cmpop,
                lhs,
                rhs,
            },
        ));
        return Ok((cond, target, fallthrough));
    }
    let value = stack.pop().ok_or(CilError::StackUnderflow)?;
    match op {
        Opcode::BrtrueS | Opcode::Brtrue => Ok((value, target, fallthrough)),
        Opcode::BrfalseS | Opcode::Brfalse => Ok((value, fallthrough, target)),
        _ => Err(CilError::Unsupported(op)),
    }
}

/// Scans a method body for the highest argument and local slot it references, to
/// size the entry parameters and the locals table.
fn scan_slots(code: &[Instruction]) -> (usize, usize) {
    let mut args = 0usize;
    let mut locals = 0usize;
    for instruction in code {
        match instruction.opcode {
            Opcode::Ldarg0 => args = args.max(1),
            Opcode::Ldarg1 => args = args.max(2),
            Opcode::Ldarg2 => args = args.max(3),
            Opcode::Ldarg3 => args = args.max(4),
            Opcode::LdargS
            | Opcode::Ldarg
            | Opcode::StargS
            | Opcode::Starg
            | Opcode::LdargaS
            | Opcode::Ldarga => {
                if let Operand::Variable(n) = &instruction.operand {
                    args = args.max(*n as usize + 1);
                }
            }
            Opcode::Ldloc0 | Opcode::Stloc0 => locals = locals.max(1),
            Opcode::Ldloc1 | Opcode::Stloc1 => locals = locals.max(2),
            Opcode::Ldloc2 | Opcode::Stloc2 => locals = locals.max(3),
            Opcode::Ldloc3 | Opcode::Stloc3 => locals = locals.max(4),
            Opcode::LdlocS
            | Opcode::Ldloc
            | Opcode::StlocS
            | Opcode::Stloc
            | Opcode::LdlocaS
            | Opcode::Ldloca => {
                if let Operand::Variable(n) = &instruction.operand {
                    locals = locals.max(*n as usize + 1);
                }
            }
            _ => {}
        }
    }
    (args, locals)
}

/// Pushes the value currently in argument slot `index`.
fn push_arg(args: &[ValueId], stack: &mut Vec<ValueId>, index: usize) -> Result<(), CilError> {
    let value = *args.get(index).ok_or(CilError::BadOperand)?;
    stack.push(value);
    Ok(())
}

/// Pushes the value in local slot `index`, materializing a zero for a local read
/// before it is stored (CIL zero-initializes locals).
fn push_local(
    value_types: &mut Vec<MirType>,
    locals: &mut [Option<ValueId>],
    stack: &mut Vec<ValueId>,
    insts: &mut Vec<(ValueId, Inst)>,
    index: usize,
) -> Result<(), CilError> {
    let slot = locals.get_mut(index).ok_or(CilError::BadOperand)?;
    let value = match *slot {
        Some(value) => value,
        None => {
            let zero = new_value(value_types, MirType::I32);
            insts.push((
                zero,
                Inst::ConstInt {
                    ty: MirType::I32,
                    value: 0,
                },
            ));
            *slot = Some(zero);
            zero
        }
    };
    stack.push(value);
    Ok(())
}

/// Stores the stack top into local slot `index`.
fn store_local(
    value_types: &mut Vec<MirType>,
    locals: &mut [Option<ValueId>],
    stack: &mut Vec<ValueId>,
    insts: &mut Vec<(ValueId, Inst)>,
    index: usize,
) -> Result<(), CilError> {
    let value = stack.pop().ok_or(CilError::StackUnderflow)?;
    let stored = if matches!(
        value_types.get(value.index()),
        Some(MirType::ValueType { .. })
    ) {
        let ty = value_types[value.index()];
        let copy = new_value(value_types, ty);
        insts.push((copy, Inst::CopyStruct { src: value }));
        copy
    } else {
        value
    };
    *locals.get_mut(index).ok_or(CilError::BadOperand)? = Some(stored);
    Ok(())
}

/// Control-flow graph analysis over a CIL instruction stream: basic-block
/// discovery and predecessors, consumed by the lowering's abstract interpreter.
mod control_flow {
    use alloc::vec;
    use alloc::vec::Vec;

    use alloc::collections::BTreeSet;
    use lamella_cil::{EhClause, EhKind, Instruction, Opcode, Operand};

    #[test]
    fn lowers_unsigned_less_than() {
        use super::*;
        let body = MethodBodyImage {
            max_stack: 2,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::simple(Opcode::LdcI4M1),
                Instruction::simple(Opcode::LdcI45),
                Instruction::simple(Opcode::CltUn),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let func = lower_method(&body).expect("lower clt.un");
        assert!(
            func.blocks
                .iter()
                .flat_map(|b| &b.insts)
                .any(|(_, i)| matches!(
                    i,
                    Inst::Compare {
                        op: CmpOp::UnsignedLt,
                        ..
                    }
                ))
        );
        assert!(crate::arm32::lower(&func).is_ok());
    }

    #[test]
    fn lowers_sub_word_indirect_load_store() {
        use super::*;
        let body = MethodBodyImage {
            max_stack: 2,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::simple(Opcode::LdcI40),
                Instruction::simple(Opcode::LdcI41),
                Instruction::simple(Opcode::StindI1),
                Instruction::simple(Opcode::LdcI40),
                Instruction::simple(Opcode::LdcI41),
                Instruction::simple(Opcode::StindI2),
                Instruction::simple(Opcode::LdcI40),
                Instruction::simple(Opcode::LdindI1),
                Instruction::simple(Opcode::LdcI40),
                Instruction::simple(Opcode::LdindU1),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::LdcI40),
                Instruction::simple(Opcode::LdindU2),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let func = lower_method(&body).expect("lower sub-word stind/ldind");
        let stores: Vec<u32> = func
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|(_, i)| match i {
                Inst::Store { width, .. } => Some(*width),
                _ => None,
            })
            .collect();
        let loads: Vec<(u32, bool)> = func
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter_map(|(_, i)| match i {
                Inst::Load { width, signed, .. } => Some((*width, *signed)),
                _ => None,
            })
            .collect();
        assert_eq!(stores, vec![1, 2]);
        assert_eq!(loads, vec![(1, true), (1, false), (2, false)]);
        assert!(crate::arm32::lower(&func).is_ok());
    }

    #[test]
    fn lowers_block_copy_and_fill() {
        use super::*;
        let body = MethodBodyImage {
            max_stack: 3,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::simple(Opcode::LdcI40),
                Instruction::simple(Opcode::LdcI40),
                Instruction::simple(Opcode::LdcI40),
                Instruction::simple(Opcode::Cpblk),
                Instruction::simple(Opcode::LdcI40),
                Instruction::simple(Opcode::LdcI40),
                Instruction::simple(Opcode::LdcI40),
                Instruction::simple(Opcode::Initblk),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let func = lower_method(&body).expect("lower cpblk/initblk");
        let insts: Vec<&Inst> = func
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .map(|(_, i)| i)
            .collect();
        assert!(
            insts.iter().any(|i| matches!(i, Inst::CopyBlock { .. })),
            "cpblk -> CopyBlock"
        );
        assert!(
            insts.iter().any(|i| matches!(i, Inst::FillBlock { .. })),
            "initblk -> FillBlock"
        );
        assert!(crate::arm32::lower(&func).is_ok());
    }

    #[test]
    fn lowers_struct_field_access() {
        use super::*;
        use lamella_token::Token;
        struct Fields;
        impl CallResolver for Fields {
            fn resolve(&self, _: &Operand) -> Option<CallInfo> {
                None
            }
            fn field_offset(&self, op: &Operand) -> Option<u32> {
                match op {
                    Operand::Token(t) if (t.0 & 0x00FF_FFFF) == 1 => Some(0),
                    Operand::Token(t) if (t.0 & 0x00FF_FFFF) == 2 => Some(4),
                    _ => None,
                }
            }
            fn value_type_size(&self, _: &Operand) -> Option<u32> {
                Some(8)
            }
        }
        let field = |row| Operand::Token(Token::new(0x04, row));
        let ty = Operand::Token(Token::new(0x02, 1));
        let body = MethodBodyImage {
            max_stack: 2,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
                Instruction::new(Opcode::Initobj, ty),
                Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
                Instruction::simple(Opcode::LdcI43),
                Instruction::new(Opcode::Stfld, field(1)),
                Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
                Instruction::simple(Opcode::LdcI45),
                Instruction::new(Opcode::Stfld, field(2)),
                Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
                Instruction::new(Opcode::Ldfld, field(1)),
                Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
                Instruction::new(Opcode::Ldfld, field(2)),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::LdcI48),
                Instruction::simple(Opcode::Ceq),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let point = MirType::ValueType {
            handle: lamella_ir::TypeHandle(0),
            size: 8,
        };
        let (func, _) = lower_method_typed(&body, &Fields, &[], &[point]).unwrap();
        let insts: Vec<_> = func.blocks[0].insts.iter().map(|(_, i)| i).collect();
        assert!(insts.iter().any(|i| matches!(i, Inst::InitStruct)));
        assert!(insts.iter().any(|i| matches!(i, Inst::FieldStore { .. })));
        assert!(insts.iter().any(|i| matches!(i, Inst::FieldLoad { .. })));
        assert!(crate::arm32::lower(&func).is_ok());
    }

    /// Whether an opcode is a branch and, if so, whether it also falls through.
    #[derive(Clone, Copy)]
    pub enum BranchKind {
        /// `br`/`leave`: control always leaves to the target.
        Unconditional,
        /// `brtrue`/`beq`/...: control goes to the target or falls through.
        Conditional,
    }

    /// Classifies an opcode's control flow; `None` if it is not a branch.
    pub fn branch_kind(op: Opcode) -> Option<BranchKind> {
        match op {
            Opcode::Br | Opcode::BrS | Opcode::Leave | Opcode::LeaveS => {
                Some(BranchKind::Unconditional)
            }
            Opcode::BrtrueS
            | Opcode::Brtrue
            | Opcode::BrfalseS
            | Opcode::Brfalse
            | Opcode::BeqS
            | Opcode::Beq
            | Opcode::BgeS
            | Opcode::Bge
            | Opcode::BgtS
            | Opcode::Bgt
            | Opcode::BleS
            | Opcode::Ble
            | Opcode::BltS
            | Opcode::Blt
            | Opcode::BneUnS
            | Opcode::BneUn
            | Opcode::BgeUnS
            | Opcode::BgeUn
            | Opcode::BgtUnS
            | Opcode::BgtUn
            | Opcode::BleUnS
            | Opcode::BleUn
            | Opcode::BltUnS
            | Opcode::BltUn => Some(BranchKind::Conditional),
            _ => None,
        }
    }

    /// Whether an opcode ends control flow with no fall-through. `endfinally` ends a finally
    /// handler -- control resumes at the pending leave target or the unwind, never the next
    /// instruction -- so it is a block terminator like `ret`/`throw`.
    fn is_return(op: Opcode) -> bool {
        matches!(
            op,
            Opcode::Ret | Opcode::Throw | Opcode::Rethrow | Opcode::Endfinally
        )
    }

    /// Whether an opcode is a bounds-checked array element LOAD -- one that raises
    /// `IndexOutOfRangeException` on an out-of-range index. The lowering hoists a bounds check ahead
    /// of these (inside a catch-protected try) so the trap can route to a handler.
    pub fn is_may_trap_load(op: Opcode) -> bool {
        matches!(
            op,
            Opcode::LdelemI1
                | Opcode::LdelemU1
                | Opcode::LdelemI2
                | Opcode::LdelemU2
                | Opcode::LdelemI4
                | Opcode::LdelemU4
                | Opcode::LdelemI8
                | Opcode::LdelemRef
        )
    }

    /// Whether an opcode is a bounds-checked array element STORE -- like a load it raises
    /// `IndexOutOfRangeException` out of range, but takes three operands (array, index, value) and
    /// leaves no result.
    pub fn is_may_trap_store(op: Opcode) -> bool {
        matches!(
            op,
            Opcode::StelemI1
                | Opcode::StelemI2
                | Opcode::StelemI4
                | Opcode::StelemI8
                | Opcode::StelemRef
        )
    }

    /// Whether an opcode is a bounds-checked array element access (load or store), so the lowering
    /// hoists a bounds check ahead of it.
    pub fn is_may_trap_access(op: Opcode) -> bool {
        is_may_trap_load(op) || is_may_trap_store(op)
    }

    /// The instruction indices control can reach from the terminator at `index`.
    pub fn successors(inst: &Instruction, index: usize) -> Vec<usize> {
        let mut out = Vec::new();
        match branch_kind(inst.opcode) {
            Some(BranchKind::Unconditional) => {
                if let Operand::Target(t) = &inst.operand {
                    out.push(*t as usize);
                }
            }
            Some(BranchKind::Conditional) => {
                if let Operand::Target(t) = &inst.operand {
                    out.push(*t as usize);
                }
                out.push(index + 1);
            }
            None => {
                if inst.opcode == Opcode::Switch {
                    if let Operand::Switch(targets) = &inst.operand {
                        for &t in targets.iter() {
                            out.push(t as usize);
                        }
                    }
                    out.push(index + 1);
                } else if !is_return(inst.opcode) {
                    out.push(index + 1);
                }
            }
        }
        out
    }

    /// Partitions a method's CIL into basic blocks, as `[start, end)` index ranges.
    /// Leaders are instruction 0, every branch target, the instruction after a branch or a
    /// return, and every exception-region boundary (a try/handler/filter start or end), so a
    /// protected region and its handler are clean block boundaries the EH lowering can map.
    pub fn discover_blocks(
        code: &[Instruction],
        handlers: &[EhClause],
        is_reference_field: &dyn Fn(&Operand) -> bool,
    ) -> Vec<(usize, usize)> {
        let mut leaders: BTreeSet<usize> = BTreeSet::new();
        leaders.insert(0);
        for (i, inst) in code.iter().enumerate() {
            if branch_kind(inst.opcode).is_some() {
                if let Operand::Target(t) = &inst.operand {
                    leaders.insert(*t as usize);
                }
                leaders.insert(i + 1);
            } else if inst.opcode == Opcode::Switch {
                if let Operand::Switch(targets) = &inst.operand {
                    for &t in targets.iter() {
                        leaders.insert(t as usize);
                    }
                }
                leaders.insert(i + 1);
            } else if is_return(inst.opcode) {
                leaders.insert(i + 1);
            }
        }
        for clause in handlers {
            leaders.insert(clause.try_range.start as usize);
            leaders.insert(clause.try_range.end as usize);
            leaders.insert(clause.handler_range.start as usize);
            leaders.insert(clause.handler_range.end as usize);
            if let EhKind::Filter { filter_start } = clause.kind {
                leaders.insert(filter_start as usize);
            }
        }
        for (i, inst) in code.iter().enumerate() {
            let is_field_null_deref = matches!(inst.opcode, Opcode::Ldfld | Opcode::Stfld)
                && is_reference_field(&inst.operand);
            let is_cast = matches!(inst.opcode, Opcode::UnboxAny | Opcode::Castclass);
            let is_div_rem = matches!(
                inst.opcode,
                Opcode::Div | Opcode::DivUn | Opcode::Rem | Opcode::RemUn
            );
            let is_overflow = matches!(
                inst.opcode,
                Opcode::AddOvf
                    | Opcode::AddOvfUn
                    | Opcode::SubOvf
                    | Opcode::SubOvfUn
                    | Opcode::MulOvf
                    | Opcode::MulOvfUn
            );
            let is_conv_ovf = matches!(
                inst.opcode,
                Opcode::ConvOvfI1
                    | Opcode::ConvOvfU1
                    | Opcode::ConvOvfI2
                    | Opcode::ConvOvfU2
                    | Opcode::ConvOvfI4
                    | Opcode::ConvOvfU4
                    | Opcode::ConvOvfU8
            );
            let in_catch_try = handlers.iter().any(|clause| {
                matches!(clause.kind, EhKind::Catch(_))
                    && (clause.try_range.start as usize..clause.try_range.end as usize).contains(&i)
            });
            if ((is_may_trap_access(inst.opcode)
                || is_field_null_deref
                || is_div_rem
                || is_overflow
                || is_conv_ovf)
                && in_catch_try)
                || is_cast
            {
                leaders.insert(i);
            }
        }
        let starts: Vec<usize> = leaders.into_iter().filter(|&l| l < code.len()).collect();
        starts
            .iter()
            .enumerate()
            .map(|(idx, &start)| (start, starts.get(idx + 1).copied().unwrap_or(code.len())))
            .collect()
    }

    /// The predecessor block indices of each block.
    pub fn predecessors(code: &[Instruction], blocks: &[(usize, usize)]) -> Vec<Vec<usize>> {
        let block_of = |instr: usize| blocks.iter().position(|&(s, e)| instr >= s && instr < e);
        let mut preds: Vec<Vec<usize>> = vec![Vec::new(); blocks.len()];
        for (b, &(_, end)) in blocks.iter().enumerate() {
            let Some(last) = end.checked_sub(1) else {
                continue;
            };
            let Some(inst) = code.get(last) else { continue };
            for succ in successors(inst, last) {
                if let Some(target) = block_of(succ) {
                    if !preds[target].contains(&b) {
                        preds[target].push(b);
                    }
                }
            }
        }
        preds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ldc.i4.s 40 ; ldc.i4.s 2 ; add ; ret`, the CIL of `fn() -> i32 { 40 + 2 }`.
    fn forty_plus_two() -> MethodBodyImage {
        MethodBodyImage {
            max_stack: 2,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::new(Opcode::LdcI4S, Operand::Int8(40)),
                Instruction::new(Opcode::LdcI4S, Operand::Int8(2)),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        }
    }

    #[test]
    fn lowers_ldc_add_ret_to_a_returning_function() {
        let func = lower_method(&forty_plus_two()).unwrap();
        assert!(lamella_ir::verify(&func).is_ok());
        assert_eq!(func.value_types.len(), 3);
        assert_eq!(func.ret, Some(MirType::I32));
    }

    #[test]
    fn lowers_a_call_through_the_resolver() {
        struct TwoArgReturning;
        impl CallResolver for TwoArgReturning {
            fn resolve(&self, _operand: &Operand) -> Option<CallInfo> {
                Some(CallInfo {
                    args: 2,
                    has_result: true,
                    result_type: None,
                    target: CallTarget::Internal(7),
                })
            }
        }
        let body = MethodBodyImage {
            max_stack: 2,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::simple(Opcode::LdcI43),
                Instruction::simple(Opcode::LdcI44),
                Instruction::new(Opcode::Call, Operand::None),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let (func, _) = lower_method_debug_with(&body, &TwoArgReturning).unwrap();
        assert!(lamella_ir::verify(&func).is_ok());
        let call = func.blocks[0].insts.iter().find_map(|(_, i)| match i {
            Inst::Call { callee, args } => Some((*callee, args.len())),
            _ => None,
        });
        assert_eq!(call, Some((7, 2)));
    }

    #[test]
    fn lowers_in_place_struct_ctor() {
        struct Ctor;
        impl CallResolver for Ctor {
            fn resolve(&self, _operand: &Operand) -> Option<CallInfo> {
                Some(CallInfo {
                    args: 3,
                    has_result: false,
                    result_type: None,
                    target: CallTarget::Internal(1),
                })
            }
        }
        let vec2 = MirType::ValueType {
            handle: lamella_ir::TypeHandle(0),
            size: 8,
        };
        let body = MethodBodyImage {
            max_stack: 3,
            init_locals: true,
            local_var_sig: None,
            code: vec![
                Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
                Instruction::new(Opcode::LdcI4S, Operand::Int8(13)),
                Instruction::simple(Opcode::LdcI48),
                Instruction::new(Opcode::Call, Operand::None),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let (func, _) = lower_method_typed(&body, &Ctor, &[], &[vec2]).unwrap();
        let insts: Vec<_> = func.blocks[0].insts.iter().map(|(_, i)| i).collect();
        assert!(insts.iter().any(|i| matches!(i, Inst::InitStruct)));
        assert!(
            insts
                .iter()
                .any(|i| matches!(i, Inst::FieldAddr { offset: 0, .. }))
        );
        let call = insts.iter().find_map(|i| match i {
            Inst::Call { callee, args } => Some((*callee, args.len())),
            _ => None,
        });
        assert_eq!(call, Some((1, 3)));
        assert!(lamella_ir::verify(&func).is_ok());
    }

    #[test]
    fn initobj_carries_its_type_handle() {
        use lamella_token::Token;
        struct Sized;
        impl CallResolver for Sized {
            fn resolve(&self, _operand: &Operand) -> Option<CallInfo> {
                None
            }
            fn value_type_size(&self, _operand: &Operand) -> Option<u32> {
                Some(8)
            }
        }
        let token = Token::new(0x02, 7);
        let body = MethodBodyImage {
            max_stack: 1,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
                Instruction::new(Opcode::Initobj, Operand::Token(token)),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let (func, _) = lower_method_debug_with(&body, &Sized).unwrap();
        assert!(func.value_types.iter().any(|t| matches!(
            t,
            MirType::ValueType { handle, size: 8 } if handle.0 == token.0
        )));
        assert!(lamella_ir::verify(&func).is_ok());
    }

    #[test]
    fn lowers_rvalue_newobj() {
        struct Ctor;
        impl CallResolver for Ctor {
            fn resolve(&self, _operand: &Operand) -> Option<CallInfo> {
                Some(CallInfo {
                    args: 3,
                    has_result: false,
                    result_type: None,
                    target: CallTarget::Internal(1),
                })
            }
            fn newobj_value_type(&self, _operand: &Operand) -> Option<MirType> {
                Some(MirType::ValueType {
                    handle: lamella_ir::TypeHandle(0),
                    size: 8,
                })
            }
        }
        let body = MethodBodyImage {
            max_stack: 3,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::new(Opcode::LdcI4S, Operand::Int8(40)),
                Instruction::simple(Opcode::LdcI42),
                Instruction::new(Opcode::Newobj, Operand::None),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let (func, _) = lower_method_debug_with(&body, &Ctor).unwrap();
        let insts: Vec<_> = func.blocks[0].insts.iter().map(|(_, i)| i).collect();
        assert!(insts.iter().any(|i| matches!(i, Inst::InitStruct)));
        assert!(
            insts
                .iter()
                .any(|i| matches!(i, Inst::FieldAddr { offset: 0, .. }))
        );
        let call = insts.iter().find_map(|i| match i {
            Inst::Call { callee, args } => Some((*callee, args.len())),
            _ => None,
        });
        assert_eq!(call, Some((1, 3)));
        assert!(matches!(func.ret, Some(MirType::ValueType { size: 8, .. })));
        assert!(lamella_ir::verify(&func).is_ok());
    }

    #[test]
    fn lowers_debug_writeline_to_semihosting() {
        struct DebugMock;
        impl CallResolver for DebugMock {
            fn resolve(&self, _operand: &Operand) -> Option<CallInfo> {
                Some(CallInfo {
                    args: 1,
                    has_result: false,
                    result_type: None,
                    target: CallTarget::Intrinsic(Intrinsic::DebugWriteLine),
                })
            }
            fn user_string(&self, _operand: &Operand) -> Option<Box<[u8]>> {
                Some(b"Hi".to_vec().into_boxed_slice())
            }
        }
        let body = MethodBodyImage {
            max_stack: 1,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::new(Opcode::Ldstr, Operand::None),
                Instruction::new(Opcode::Call, Operand::None),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let (func, _) = lower_method_debug_with(&body, &DebugMock).unwrap();
        assert!(lamella_ir::verify(&func).is_ok());
        let text = func.blocks[0].insts.iter().find_map(|(_, i)| match i {
            Inst::SemihostWrite { text } => Some(text.clone()),
            _ => None,
        });
        assert_eq!(text.as_deref(), Some(&b"Hi\n\0"[..]));
    }

    #[test]
    fn lowers_console_writeline_int_to_writeint() {
        struct ConsoleMock;
        impl CallResolver for ConsoleMock {
            fn resolve(&self, _operand: &Operand) -> Option<CallInfo> {
                Some(CallInfo {
                    args: 1,
                    has_result: false,
                    result_type: None,
                    target: CallTarget::Intrinsic(Intrinsic::ConsoleWriteLineInt),
                })
            }
        }
        let body = MethodBodyImage {
            max_stack: 1,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::new(Opcode::LdcI4S, Operand::Int8(42)),
                Instruction::new(Opcode::Call, Operand::None),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let (func, _) = lower_method_debug_with(&body, &ConsoleMock).unwrap();
        assert!(lamella_ir::verify(&func).is_ok());
        assert!(
            func.blocks[0]
                .insts
                .iter()
                .any(|(_, i)| matches!(i, Inst::WriteInt { .. }))
        );
        #[cfg(feature = "arm32")]
        assert!(crate::arm32::lower(&func).is_ok());
    }

    #[test]
    fn lowers_ldarga_for_a_struct_argument() {
        use lamella_token::Token;
        struct Fields;
        impl CallResolver for Fields {
            fn resolve(&self, _operand: &Operand) -> Option<CallInfo> {
                None
            }
            fn field_offset(&self, _operand: &Operand) -> Option<u32> {
                Some(0)
            }
        }
        let point = MirType::ValueType {
            handle: lamella_ir::TypeHandle(2),
            size: 8,
        };
        let body = MethodBodyImage {
            max_stack: 2,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::new(Opcode::LdargaS, Operand::Variable(0)),
                Instruction::new(Opcode::LdcI4S, Operand::Int8(99)),
                Instruction::new(Opcode::Stfld, Operand::Token(Token::new(0x04, 1))),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let (func, _) = lower_method_typed(&body, &Fields, &[point], &[]).unwrap();
        let arg0 = func.blocks[0].params[0];
        assert!(
            func.blocks[0]
                .insts
                .iter()
                .any(|(_, i)| matches!(i, Inst::FieldStore { base, .. } if *base == arg0))
        );
        assert!(lamella_ir::verify(&func).is_ok());
    }

    #[test]
    fn i64_local_through_a_merge_is_typed() {
        let i64s = [MirType::I64, MirType::I64];
        let body = MethodBodyImage {
            max_stack: 2,
            init_locals: true,
            local_var_sig: None,
            code: vec![
                Instruction::simple(Opcode::LdcI40),
                Instruction::simple(Opcode::ConvI8),
                Instruction::simple(Opcode::Stloc0),
                Instruction::new(Opcode::BrS, Operand::Target(9)),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::simple(Opcode::LdcI41),
                Instruction::simple(Opcode::ConvI8),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Stloc0),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::simple(Opcode::LdcI43),
                Instruction::simple(Opcode::ConvI8),
                Instruction::simple(Opcode::Clt),
                Instruction::new(Opcode::BrtrueS, Operand::Target(4)),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::simple(Opcode::Stloc1),
                Instruction::simple(Opcode::Ldloc1),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let (func, _) = lower_method_typed(&body, &NoCalls, &[], &i64s).unwrap();
        assert!(lamella_ir::verify(&func).is_ok());
        assert_eq!(func.ret, Some(MirType::I64));
    }

    #[test]
    fn lowers_string_op_equality_to_a_compare() {
        struct StringMock;
        impl CallResolver for StringMock {
            fn resolve(&self, _operand: &Operand) -> Option<CallInfo> {
                Some(CallInfo {
                    args: 2,
                    has_result: true,
                    result_type: Some(MirType::I32),
                    target: CallTarget::Intrinsic(Intrinsic::StringEquals),
                })
            }
            fn user_string(&self, _operand: &Operand) -> Option<Box<[u8]>> {
                Some(b"x".to_vec().into_boxed_slice())
            }
        }
        let body = MethodBodyImage {
            max_stack: 2,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::new(Opcode::Ldstr, Operand::None),
                Instruction::new(Opcode::Ldstr, Operand::None),
                Instruction::new(Opcode::Call, Operand::None),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let (func, _) = lower_method_debug_with(&body, &StringMock).unwrap();
        assert!(lamella_ir::verify(&func).is_ok());
        let insts = &func.blocks[0].insts;
        assert_eq!(
            insts
                .iter()
                .filter(|(_, i)| matches!(i, Inst::StringLiteral { .. }))
                .count(),
            2
        );
        assert!(
            insts
                .iter()
                .any(|(_, i)| matches!(i, Inst::StringEquals { .. }))
        );
        assert_eq!(func.ret, Some(MirType::I32));
        #[cfg(feature = "arm32")]
        assert!(crate::arm32::lower(&func).is_ok());
    }

    #[test]
    fn lowers_arguments_and_locals() {
        let body = MethodBodyImage {
            max_stack: 2,
            init_locals: true,
            local_var_sig: None,
            code: vec![
                Instruction::simple(Opcode::Ldarg0),
                Instruction::simple(Opcode::Ldarg1),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Stloc0),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let func = lower_method(&body).unwrap();
        assert!(lamella_ir::verify(&func).is_ok());
        assert_eq!(func.params.len(), 2);
        assert_eq!(func.ret, Some(MirType::I32));
    }

    #[test]
    fn lowers_neg() {
        let body = MethodBodyImage {
            max_stack: 1,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::simple(Opcode::LdcI45),
                Instruction::simple(Opcode::Neg),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let func = lower_method(&body).unwrap();
        assert!(lamella_ir::verify(&func).is_ok());
    }

    #[test]
    fn lowers_a_counting_loop() {
        let body = MethodBodyImage {
            max_stack: 2,
            init_locals: true,
            local_var_sig: None,
            code: vec![
                Instruction::simple(Opcode::LdcI40),
                Instruction::simple(Opcode::Stloc0),
                Instruction::simple(Opcode::LdcI41),
                Instruction::simple(Opcode::Stloc1),
                Instruction::new(Opcode::BrS, Operand::Target(13)),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::simple(Opcode::Ldloc1),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Stloc0),
                Instruction::simple(Opcode::Ldloc1),
                Instruction::simple(Opcode::LdcI41),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Stloc1),
                Instruction::simple(Opcode::Ldloc1),
                Instruction::simple(Opcode::LdcI45),
                Instruction::new(Opcode::BleS, Operand::Target(5)),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let func = lower_method(&body).unwrap();
        assert_eq!(func.blocks.len(), 4);
        assert!(lamella_ir::verify(&func).is_ok());
        assert!(func.value_types.len() > 8);
        #[cfg(feature = "arm32")]
        assert!(crate::arm32::lower(&func).is_ok());
    }

    #[test]
    fn lowers_an_if_else() {
        let body = MethodBodyImage {
            max_stack: 2,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::simple(Opcode::LdcI45),
                Instruction::simple(Opcode::LdcI43),
                Instruction::new(Opcode::BgtS, Operand::Target(5)),
                Instruction::new(Opcode::LdcI4S, Operand::Int8(9)),
                Instruction::simple(Opcode::Ret),
                Instruction::simple(Opcode::LdcI47),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let func = lower_method(&body).unwrap();
        assert_eq!(func.blocks.len(), 3);
        assert!(lamella_ir::verify(&func).is_ok());
        #[cfg(feature = "arm32")]
        assert!(crate::arm32::lower(&func).is_ok());
    }

    #[test]
    fn lowers_a_conditional_branch_into_a_merge() {
        let body = MethodBodyImage {
            max_stack: 2,
            init_locals: true,
            local_var_sig: None,
            code: vec![
                Instruction::simple(Opcode::LdcI45),
                Instruction::simple(Opcode::Stloc0),
                Instruction::simple(Opcode::Ldarg0),
                Instruction::simple(Opcode::LdcI40),
                Instruction::new(Opcode::BleS, Operand::Target(7)),
                Instruction::simple(Opcode::LdcI47),
                Instruction::simple(Opcode::Stloc0),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let func = lower_method(&body).unwrap();
        assert!(lamella_ir::verify(&func).is_ok());
        assert_eq!(func.blocks.len(), 4);
        #[cfg(feature = "arm32")]
        assert!(crate::arm32::lower(&func).is_ok());
    }

    #[test]
    fn discovers_an_if_else_cfg() {
        let code = [
            Instruction::simple(Opcode::Ldarg0),
            Instruction::simple(Opcode::LdcI40),
            Instruction::new(Opcode::BgtS, Operand::Target(5)),
            Instruction::simple(Opcode::LdcI42),
            Instruction::simple(Opcode::Ret),
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::Ret),
        ];
        let blocks = control_flow::discover_blocks(&code, &[], &|_| false);
        assert_eq!(blocks, vec![(0, 3), (3, 5), (5, 7)]);
        let preds = control_flow::predecessors(&code, &blocks);
        assert!(preds[0].is_empty());
        assert_eq!(preds[1], vec![0]);
        assert_eq!(preds[2], vec![0]);
    }

    #[test]
    fn lowers_a_switch() {
        let body = MethodBodyImage {
            max_stack: 1,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::simple(Opcode::Ldarg0),
                Instruction::new(Opcode::Switch, Operand::Switch(vec![4, 6, 8].into())),
                Instruction::new(Opcode::LdcI4S, Operand::Int8(40)),
                Instruction::simple(Opcode::Ret),
                Instruction::new(Opcode::LdcI4S, Operand::Int8(10)),
                Instruction::simple(Opcode::Ret),
                Instruction::new(Opcode::LdcI4S, Operand::Int8(20)),
                Instruction::simple(Opcode::Ret),
                Instruction::new(Opcode::LdcI4S, Operand::Int8(30)),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let func = lower_method(&body).unwrap();
        assert!(lamella_ir::verify(&func).is_ok());
        let eq_tests = func
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter(|(_, i)| matches!(i, Inst::Compare { op: CmpOp::Eq, .. }))
            .count();
        assert_eq!(eq_tests, 3);
        assert_eq!(func.blocks.len(), 7);
        #[cfg(feature = "arm32")]
        assert!(crate::arm32::lower(&func).is_ok());
    }

    #[test]
    fn switch_splits_a_merge_case_edge() {
        let body = MethodBodyImage {
            max_stack: 1,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::simple(Opcode::Ldarg0),
                Instruction::new(Opcode::Switch, Operand::Switch(vec![3].into())),
                Instruction::new(Opcode::BrS, Operand::Target(3)),
                Instruction::new(Opcode::LdcI4S, Operand::Int8(42)),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let func = lower_method(&body).unwrap();
        assert!(lamella_ir::verify(&func).is_ok());
        assert_eq!(func.blocks.len(), 4);
        #[cfg(feature = "arm32")]
        assert!(crate::arm32::lower(&func).is_ok());
    }

    #[test]
    fn discovers_a_switch_cfg() {
        let code = [
            Instruction::simple(Opcode::Ldarg0),
            Instruction::new(Opcode::Switch, Operand::Switch(vec![4, 6, 8].into())),
            Instruction::new(Opcode::LdcI4S, Operand::Int8(40)),
            Instruction::simple(Opcode::Ret),
            Instruction::new(Opcode::LdcI4S, Operand::Int8(10)),
            Instruction::simple(Opcode::Ret),
            Instruction::new(Opcode::LdcI4S, Operand::Int8(20)),
            Instruction::simple(Opcode::Ret),
            Instruction::new(Opcode::LdcI4S, Operand::Int8(30)),
            Instruction::simple(Opcode::Ret),
        ];
        let blocks = control_flow::discover_blocks(&code, &[], &|_| false);
        assert_eq!(blocks, vec![(0, 2), (2, 4), (4, 6), (6, 8), (8, 10)]);
        let preds = control_flow::predecessors(&code, &blocks);
        assert!(preds[0].is_empty());
        assert_eq!(preds[1], vec![0]);
        assert_eq!(preds[2], vec![0]);
        assert_eq!(preds[3], vec![0]);
        assert_eq!(preds[4], vec![0]);
    }

    #[test]
    fn lowers_reference_array_element_access() {
        use lamella_token::Token;
        struct Arrays;
        impl CallResolver for Arrays {
            fn resolve(&self, _: &Operand) -> Option<CallInfo> {
                None
            }
            fn array_element(&self, _: &Operand) -> Option<ArrayElement> {
                Some(ArrayElement {
                    handle: lamella_ir::TypeHandle(9),
                    element_size: 4,
                })
            }
        }
        let body = MethodBodyImage {
            max_stack: 4,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::simple(Opcode::LdcI41),
                Instruction::new(Opcode::Newarr, Operand::Token(Token::new(0x01, 2))),
                Instruction::simple(Opcode::Dup),
                Instruction::simple(Opcode::LdcI40),
                Instruction::simple(Opcode::Ldnull),
                Instruction::simple(Opcode::StelemRef),
                Instruction::simple(Opcode::LdcI40),
                Instruction::simple(Opcode::LdelemRef),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let (func, _) = lower_method_debug_with(&body, &Arrays).unwrap();
        assert!(lamella_ir::verify(&func).is_ok());
        let insts: Vec<_> = func.blocks.iter().flat_map(|b| &b.insts).collect();
        assert!(insts.iter().any(|(_, i)| matches!(
            i,
            Inst::ArrayStore {
                element_size: 4,
                ..
            }
        )));
        assert!(insts.iter().any(|(_, i)| matches!(
            i,
            Inst::ArrayLoad {
                element_size: 4,
                signed: false,
                ..
            }
        )));
        assert_eq!(func.ret, Some(MirType::ObjectRef));
    }

    #[test]
    fn lowers_long_array_element_access() {
        use lamella_token::Token;
        struct LongArrays;
        impl CallResolver for LongArrays {
            fn resolve(&self, _: &Operand) -> Option<CallInfo> {
                None
            }
            fn array_element(&self, _: &Operand) -> Option<ArrayElement> {
                Some(ArrayElement {
                    handle: lamella_ir::TypeHandle(8),
                    element_size: 8,
                })
            }
        }
        let body = MethodBodyImage {
            max_stack: 4,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::simple(Opcode::LdcI41),
                Instruction::new(Opcode::Newarr, Operand::Token(Token::new(0x01, 1))),
                Instruction::simple(Opcode::Dup),
                Instruction::simple(Opcode::LdcI40),
                Instruction::new(Opcode::LdcI8, Operand::Int64(42)),
                Instruction::simple(Opcode::StelemI8),
                Instruction::simple(Opcode::LdcI40),
                Instruction::simple(Opcode::LdelemI8),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let (func, _) = lower_method_debug_with(&body, &LongArrays).unwrap();
        assert!(lamella_ir::verify(&func).is_ok());
        let insts: Vec<_> = func.blocks.iter().flat_map(|b| &b.insts).collect();
        assert!(insts.iter().any(|(_, i)| matches!(
            i,
            Inst::ArrayStore {
                element_size: 8,
                ..
            }
        )));
        assert!(insts.iter().any(|(_, i)| matches!(
            i,
            Inst::ArrayLoad {
                element_size: 8,
                ..
            }
        )));
        assert_eq!(func.ret, Some(MirType::I64));
    }

    #[test]
    fn lowers_box_and_unbox() {
        use lamella_token::Token;
        struct BoxMock;
        impl CallResolver for BoxMock {
            fn resolve(&self, _: &Operand) -> Option<CallInfo> {
                None
            }
            fn boxed_layout(&self, _: &Operand) -> Option<ReferenceLayout> {
                Some(ReferenceLayout {
                    handle: lamella_ir::TypeHandle(7),
                    size: 4,
                    reference_offsets: Vec::new(),
                })
            }
            fn boxed_value_type(&self, _: &Operand) -> Option<MirType> {
                Some(MirType::I32)
            }
            fn builtin_exception_tag(&self, _: &str, _: &str) -> Option<u32> {
                Some(0x8000_0001)
            }
        }
        let int = Operand::Token(Token::new(0x01, 7));
        let body = MethodBodyImage {
            max_stack: 1,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::new(Opcode::LdcI4S, Operand::Int8(40)),
                Instruction::new(Opcode::Box, int.clone()),
                Instruction::new(Opcode::UnboxAny, int),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let (func, _) = lower_method_debug_with(&body, &BoxMock).unwrap();
        assert!(lamella_ir::verify(&func).is_ok());
        let insts: Vec<_> = func.blocks.iter().flat_map(|b| &b.insts).collect();
        assert!(insts.iter().any(|(_, i)| matches!(
            i,
            Inst::Alloc {
                payload_size: 4,
                ..
            }
        )));
        assert!(
            insts
                .iter()
                .any(|(_, i)| matches!(i, Inst::FieldStore { offset: 0, .. }))
        );
        assert!(
            insts
                .iter()
                .any(|(_, i)| matches!(i, Inst::FieldLoad { offset: 0, .. }))
        );
        assert_eq!(func.ret, Some(MirType::I32));
        #[cfg(feature = "arm32")]
        assert!(crate::arm32::lower_module_gc(&[func], 0x09).is_ok());
    }

    #[test]
    fn lowers_float_to_int_conversion() {
        let body = MethodBodyImage {
            max_stack: 1,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::new(Opcode::LdcR4, Operand::Float32(2.5)),
                Instruction::simple(Opcode::ConvI4),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let func = lower_method(&body).unwrap();
        assert!(lamella_ir::verify(&func).is_ok());
        let insts: Vec<_> = func.blocks.iter().flat_map(|b| &b.insts).collect();
        assert!(insts.iter().any(|(_, i)| matches!(
            i,
            Inst::ConstInt {
                ty: MirType::F32,
                ..
            }
        )));
        assert!(insts.iter().any(|(_, i)| matches!(
            i,
            Inst::Convert {
                kind: ConvKind::Float32ToInt,
                ..
            }
        )));
        assert_eq!(func.ret, Some(MirType::I32));
        #[cfg(feature = "arm32")]
        assert!(crate::arm32::lower(&func).is_ok());
    }

    #[test]
    fn lowers_int_to_float_conversion() {
        let body = MethodBodyImage {
            max_stack: 1,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::simple(Opcode::LdcI45),
                Instruction::simple(Opcode::ConvR4),
                Instruction::simple(Opcode::ConvI4),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let func = lower_method(&body).unwrap();
        assert!(lamella_ir::verify(&func).is_ok());
        let insts: Vec<_> = func.blocks.iter().flat_map(|b| &b.insts).collect();
        assert!(insts.iter().any(|(_, i)| matches!(
            i,
            Inst::Convert {
                kind: ConvKind::IntToFloat32,
                ..
            }
        )));
        assert!(insts.iter().any(|(_, i)| matches!(
            i,
            Inst::Convert {
                kind: ConvKind::Float32ToInt,
                ..
            }
        )));
        assert_eq!(func.ret, Some(MirType::I32));
        #[cfg(feature = "arm32")]
        assert!(crate::arm32::lower(&func).is_ok());
    }

    #[test]
    fn rejects_a_body_with_no_return() {
        let body = MethodBodyImage {
            max_stack: 1,
            init_locals: false,
            local_var_sig: None,
            code: vec![Instruction::simple(Opcode::Nop)].into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        assert!(lower_method(&body).is_err());
    }

    #[cfg(feature = "arm32")]
    #[test]
    fn ldc_add_ret_lowers_through_to_arm32() {
        let func = lower_method(&forty_plus_two()).unwrap();
        let bytes = crate::arm32::lower(&func).unwrap();
        assert_eq!(&bytes[bytes.len() - 2..], &[0x70, 0x47]);
    }
}
