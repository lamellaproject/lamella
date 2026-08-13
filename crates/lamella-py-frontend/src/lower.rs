//! The typed `Python -> MIR` lowering: a [`bc::CodeObject`] to a
//! [`lamella_ir::Function`].


use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use lamella_ir::{
    BasicBlock, BinOp as MBinOp, BlockId, CmpOp as MCmpOp, ConvKind, Function, Inst, MirType, PyOp,
    StaticOwner, Terminator, TypeHandle, ValueId,
};
use lamella_py_bytecode as bc;

/// Why a code object could not be lowered to MIR. Most variants mark a construct
/// outside the typed subset rather than a true error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LowerError {
    /// An op needed more operands than the abstract stack held.
    StackUnderflow,
    /// A block left values on the stack at its boundary -- the subset guarantees an
    /// empty stack there, so this signals an unexpected (out-of-subset) shape.
    StackNotEmpty,
    /// A constant-pool index was out of range.
    BadConstIndex(u32),
    /// A local slot index was out of range.
    BadLocalIndex(u32),
    /// An integer literal did not fit a 32-bit machine word (outside `[i32::MIN, u32::MAX]`);
    /// the typed lane has no bignum.
    IntLiteralTooLarge(i64),
    /// A non-integer constant in the typed path (`None`/`True`/`False`/string); not
    /// lowered in the typed integer path.
    UnsupportedConst,
    /// Arithmetic or comparison on a dynamic (non-`I32`) operand; the typed path
    /// handles integer operands only.
    DynamicOperation,
    /// A global name resolved to no user function in this module (e.g. a builtin like
    /// `print`); only intra-module calls are lowered in the typed path.
    UnresolvedGlobal(String),
    /// A name-pool index was out of range.
    BadNameIndex(u32),
    /// An op's `site` did not index the code object's `wide_operands` table. Unreachable for a code
    /// object this crate built or the decoder produced -- both assign the index and the entry
    /// together -- so it means the two were built by different passes.
    BadOperandSite(u32),
    /// A function name was used as a plain operand (functions are not first-class
    /// values in the typed subset).
    CallableAsValue,
    /// `Call` was applied to something that was not a resolved function.
    CallTargetNotCallable,
    /// A call passed the wrong number of arguments for its callee.
    ArityMismatch {
        /// The callee's declared parameter count.
        expected: usize,
        /// The number of arguments the call site passed.
        found: usize,
    },
    /// A keyword argument named a parameter the callee does not have.
    UnexpectedKeyword(String),
    /// An argument was passed both positionally and by keyword, or a keyword was repeated.
    DuplicateArgument(String),
    /// A conditional branch's condition was not an `I32`.
    BadConditionType,
    /// A `return` value's type did not match the function's return type.
    ReturnTypeMismatch,
    /// Control fell off the end of the body (a function body that does not return).
    RunsOffEnd,
    /// A non-parameter local was not an integer, so it has no typed-path default to
    /// initialize it with at entry.
    UnsupportedLocalType(usize),
    /// A non-merge block was reached before its single predecessor was lowered -- an
    /// irreducible control-flow shape the structured subset does not emit.
    UnsupportedControlFlow,
}

impl core::fmt::Display for LowerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            LowerError::StackUnderflow => f.write_str("operand stack underflow"),
            LowerError::StackNotEmpty => f.write_str("operand stack not empty at a block boundary"),
            LowerError::BadConstIndex(i) => write!(f, "constant index {i} out of range"),
            LowerError::BadLocalIndex(i) => write!(f, "local index {i} out of range"),
            LowerError::IntLiteralTooLarge(v) => {
                write!(f, "integer literal {v} does not fit a 32-bit machine word")
            }
            LowerError::UnsupportedConst => {
                f.write_str("non-integer constant is not lowered in the typed integer path")
            }
            LowerError::DynamicOperation => f.write_str(
                "arithmetic/comparison on a dynamic value is not supported in the typed path",
            ),
            LowerError::UnresolvedGlobal(name) => {
                write!(f, "global `{name}` is not a user function in this module")
            }
            LowerError::BadNameIndex(i) => write!(f, "name index {i} out of range"),
            LowerError::BadOperandSite(i) => {
                write!(f, "operand site {i} out of range")
            }
            LowerError::CallableAsValue => {
                f.write_str("a function name was used as a plain value")
            }
            LowerError::CallTargetNotCallable => f.write_str("call target is not a function"),
            LowerError::ArityMismatch { expected, found } => {
                write!(f, "call passed {found} argument(s) but the callee takes {expected}")
            }
            LowerError::UnexpectedKeyword(name) => {
                write!(f, "unexpected keyword argument `{name}`")
            }
            LowerError::DuplicateArgument(name) => {
                write!(f, "argument `{name}` given both positionally and by keyword, or repeated")
            }
            LowerError::BadConditionType => f.write_str("a branch condition was not an i32"),
            LowerError::ReturnTypeMismatch => {
                f.write_str("a return value's type did not match the function's return type")
            }
            LowerError::RunsOffEnd => f.write_str("control runs off the end of the function body"),
            LowerError::UnsupportedLocalType(i) => {
                write!(f, "local slot {i} has no typed-path default initializer")
            }
            LowerError::UnsupportedControlFlow => {
                f.write_str("unsupported (irreducible) control-flow shape")
            }
        }
    }
}

/// The MIR type a Python static type lowers to: an annotated `int` is a machine
/// `I32` (machine-width integers, no bignum); anything dynamic is a tagged `PyValue`.
fn mir_type(ty: bc::StaticType) -> MirType {
    match ty {
        bc::StaticType::Int => MirType::I32,
        bc::StaticType::Float => MirType::F64,
        bc::StaticType::ListInt
        | bc::StaticType::ListFloat
        | bc::StaticType::TupleInt
        | bc::StaticType::TupleFloat
        | bc::StaticType::GrowListInt
        | bc::StaticType::GrowListFloat => MirType::ObjectRef,
        bc::StaticType::Dynamic => MirType::PyValue,
    }
}

/// How a typed sequence's `ObjectRef` reaches its elements -- see [`ArrayInfo`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum SeqKind {
    /// The reference IS the packed array (`[u32 len][elems...]`): a fixed `list`, whose length is the
    /// array's own prefix and whose elements are addressed directly.
    FixedList,
    /// A [`SeqKind::FixedList`] that rejects element stores: a `tuple`.
    FixedTuple,
    /// The reference is a growable list's HEADER (`[i32 len][i32 cap][ObjectRef backing]`); the
    /// elements live in the `backing` array, and the header's `len` -- NOT the backing's prefix, which
    /// is the allocated `cap` -- is the list's length.
    GrowList,
}

impl SeqKind {
    /// Whether `xs[i] = v` is allowed: everything except a `tuple`, which is immutable.
    fn mutable(self) -> bool {
        self != SeqKind::FixedTuple
    }
}

/// What the typed lane knows about a typed sequence's `ObjectRef`: its element kind (`I32`/`F64`, which
/// drives an element op's `element_size` and the loaded value's type) and how the reference reaches
/// those elements. A `list` and a `tuple` share ALL the array read machinery (index / `len` / iterate);
/// a growable list shares it too, one `backing` field-load further in. Keyed by value id in `arrays`.
#[derive(Clone, Copy)]
struct ArrayInfo {
    elem: MirType,
    kind: SeqKind,
}

/// Whether a value the typed lane holds may be passed for a parameter declared `ty` -- the check the
/// argument's MIR type cannot make.
///
/// Every typed sequence is an `ObjectRef`, so MIR type alone cannot tell a packed array from a
/// growable list's header, nor an `int` element from a `float` one. Reading either as the other is a
/// silent miscompile (a header's `len` read as element 0), so the argument's recorded sequence must be
/// exactly what the parameter declares -- and a non-sequence parameter must get a non-sequence value.
fn sequence_matches(held: Option<&ArrayInfo>, ty: bc::StaticType) -> bool {
    match (held, seq_info_of(ty)) {
        (Some(held), Some(declared)) => held.elem == declared.elem && held.kind == declared.kind,
        (None, None) => true,
        _ => false,
    }
}

/// The [`ArrayInfo`] a typed sequence static type carries, or `None` if `ty` is not a typed sequence.
fn seq_info_of(ty: bc::StaticType) -> Option<ArrayInfo> {
    let (elem, kind) = match ty {
        bc::StaticType::ListInt => (MirType::I32, SeqKind::FixedList),
        bc::StaticType::ListFloat => (MirType::F64, SeqKind::FixedList),
        bc::StaticType::TupleInt => (MirType::I32, SeqKind::FixedTuple),
        bc::StaticType::TupleFloat => (MirType::F64, SeqKind::FixedTuple),
        bc::StaticType::GrowListInt => (MirType::I32, SeqKind::GrowList),
        bc::StaticType::GrowListFloat => (MirType::F64, SeqKind::GrowList),
        _ => return None,
    };
    Some(ArrayInfo { elem, kind })
}

/// Emit the fixed-array materialization for a homogeneous numeric sequence literal (a `list` or a
/// `tuple`): `AllocArray(len)` then one `ArrayStore` per element, index `0..n`, returning the array
/// `ObjectRef` and recording its [`ArrayInfo`] in `arrays`. Every element must be a value of the one
/// numeric kind `elem` (`I32`/`F64`); anything else is out of the typed lane (`DynamicOperation`).
/// Shared by `BuildList` (mutable) and the tuple-store path (immutable).
fn materialize_array(
    values: &mut Values,
    insts: &mut Vec<(ValueId, Inst)>,
    arrays: &mut BTreeMap<ValueId, ArrayInfo>,
    elems: &[(ValueId, MirType)],
    elem: MirType,
    kind: SeqKind,
) -> Result<ValueId, LowerError> {
    if !matches!(elem, MirType::I32 | MirType::F64) || elems.iter().any(|&(_, t)| t != elem) {
        return Err(LowerError::DynamicOperation);
    }
    let element_size = elem_size(elem);
    let length = emit(
        values,
        insts,
        Inst::ConstInt { ty: MirType::I32, value: elems.len() as i64 },
        MirType::I32,
    );
    let obj = emit(
        values,
        insts,
        Inst::AllocArray {
            handle: list_type_handle(list_element_kind(elem)),
            element: None,
            length,
            element_size,
            element_kind: list_element_kind(elem),
        },
        MirType::ObjectRef,
    );
    for (k, &(value, _)) in elems.iter().enumerate() {
        let index = emit(
            values,
            insts,
            Inst::ConstInt { ty: MirType::I32, value: k as i64 },
            MirType::I32,
        );
        emit(
            values,
            insts,
            Inst::ArrayStore { array: obj, index, value, element_size },
            MirType::I32,
        );
    }
    arrays.insert(obj, ArrayInfo { elem, kind });
    Ok(obj)
}

/// Emit a growable list's HEADER over an already-built backing array, initializing its three fields
/// and recording it in `arrays`. Returns the header -- the reference the local holds and every later
/// element op starts from.
///
/// The store order is load-bearing: `lamella_gc_alloc` does not zero the payload, so from the `Alloc`
/// until the `backing` store the header's traced word holds garbage a collector would follow. The
/// caller must therefore have allocated `backing` ALREADY (it stays live across the header's
/// allocation safepoint, being stored here), and none of the three stores allocates, so no safepoint
/// falls inside that window.
fn alloc_growlist_header(
    values: &mut Values,
    insts: &mut Vec<(ValueId, Inst)>,
    arrays: &mut BTreeMap<ValueId, ArrayInfo>,
    backing: ValueId,
    len: ValueId,
    cap: ValueId,
    elem: MirType,
) -> ValueId {
    let header = emit(
        values,
        insts,
        Inst::Alloc {
            handle: GROWLIST_HEADER_TYPE_HANDLE,
            payload_size: GROWLIST_PAYLOAD_SIZE,
            ref_offsets: alloc::vec![GROWLIST_BACKING_OFFSET].into_boxed_slice(),
        },
        MirType::ObjectRef,
    );
    for (offset, value) in [
        (GROWLIST_BACKING_OFFSET, backing),
        (GROWLIST_LEN_OFFSET, len),
        (GROWLIST_CAP_OFFSET, cap),
    ] {
        emit(values, insts, Inst::FieldStore { base: header, offset, value }, MirType::I32);
    }
    arrays.insert(header, ArrayInfo { elem, kind: SeqKind::GrowList });
    header
}

/// Emit an EMPTY growable list (`xs = []`): a seed-capacity backing and a `len = 0` header. The
/// element kind comes from the local's static type, since `[]` carries no element to infer it from.
fn materialize_empty_growlist(
    values: &mut Values,
    insts: &mut Vec<(ValueId, Inst)>,
    arrays: &mut BTreeMap<ValueId, ArrayInfo>,
    elem: MirType,
) -> Result<ValueId, LowerError> {
    if !matches!(elem, MirType::I32 | MirType::F64) {
        return Err(LowerError::DynamicOperation);
    }
    let cap = emit(values, insts, Inst::ConstInt { ty: MirType::I32, value: GROWLIST_SEED_CAP }, MirType::I32);
    let backing = emit(
        values,
        insts,
        Inst::AllocArray {
            handle: list_type_handle(list_element_kind(elem)),
            element: None,
            length: cap,
            element_size: elem_size(elem),
            element_kind: list_element_kind(elem),
        },
        MirType::ObjectRef,
    );
    let len = emit(values, insts, Inst::ConstInt { ty: MirType::I32, value: 0 }, MirType::I32);
    Ok(alloc_growlist_header(values, insts, arrays, backing, len, cap, elem))
}

/// Adopt a just-built fixed array as a growable list's first backing (`xs = [1, 2]` where `xs` is
/// appended to): wrap it in a header with `len = cap =` the array's length. `cap` is the array's true
/// extent, so the list starts out full and the first `append` grows it.
fn wrap_array_as_growlist(
    values: &mut Values,
    insts: &mut Vec<(ValueId, Inst)>,
    arrays: &mut BTreeMap<ValueId, ArrayInfo>,
    backing: ValueId,
    elem: MirType,
) -> ValueId {
    let len = emit(values, insts, Inst::FieldLoad { base: backing, offset: 0 }, MirType::I32);
    alloc_growlist_header(values, insts, arrays, backing, len, len, elem)
}

/// Narrow a growable list's element index from its backing's bounds to the LIST's bounds, so that an
/// out-of-range access traps exactly as a fixed array's does.
///
/// A growable list's backing is allocated to `cap`, so the bounds check the backend already emits on
/// every `ArrayLoad`/`ArrayStore` -- `index <u <the array's length prefix>`, else trap -- would let an
/// index in `len..cap` through to read uninitialized slack. This forces such an index to `0xFFFFFFFF`,
/// which no capacity can satisfy, so that same check traps on it: `mask = 0 - (index >=u len)` is 0 in
/// range and all-ones out of it, and `index | mask` is either the index or `0xFFFFFFFF`. Branchless,
/// like the negative-index wrap it follows, so an element op stays a straight line with no block split
/// -- which also keeps a check off the (already bounds-checked) fixed-array path.
///
/// A `try`-guarded access never reaches this: its block-split bounds check has already diverted an
/// out-of-range index to a catchable `IndexError`, so here the mask is always 0.
fn narrow_index_to_len(
    values: &mut Values,
    insts: &mut Vec<(ValueId, Inst)>,
    header: ValueId,
    index: ValueId,
) -> ValueId {
    let len = emit(values, insts, Inst::FieldLoad { base: header, offset: GROWLIST_LEN_OFFSET }, MirType::I32);
    let oob = emit(
        values,
        insts,
        Inst::Compare { op: MCmpOp::UnsignedGe, lhs: index, rhs: len },
        MirType::I32,
    );
    let zero = emit(values, insts, Inst::ConstInt { ty: MirType::I32, value: 0 }, MirType::I32);
    let mask = emit(values, insts, Inst::Binary { op: MBinOp::Sub, lhs: zero, rhs: oob }, MirType::I32);
    emit(values, insts, Inst::Binary { op: MBinOp::Or, lhs: index, rhs: mask }, MirType::I32)
}

/// The array a typed sequence's elements live in, given the reference the typed lane holds: a fixed
/// list/tuple IS its array, while a growable list reaches its elements through the header's `backing`.
fn backing_array(
    values: &mut Values,
    insts: &mut Vec<(ValueId, Inst)>,
    container: ValueId,
    info: ArrayInfo,
) -> ValueId {
    match info.kind {
        SeqKind::FixedList | SeqKind::FixedTuple => container,
        SeqKind::GrowList => emit(
            values,
            insts,
            Inst::FieldLoad { base: container, offset: GROWLIST_BACKING_OFFSET },
            MirType::ObjectRef,
        ),
    }
}

/// The byte size of one array element of the given (numeric) element type: 4 for an `i32`, 8 for an
/// `f64`. This is the `element_size` the array MIR ops carry (`array + 4 + index*element_size`).
fn elem_size(elem: MirType) -> u32 {
    match elem {
        MirType::F64 => 8,
        _ => 4,
    }
}

/// The `TypeHandle` stamped on a typed list's `AllocArray`, derived from what the list HOLDS.
///
/// One shared handle was enough while the backend emitted an all-zero descriptor for every
/// primitive-element array. It stopped being enough when an array descriptor started carrying its
/// element kind: descriptors are deduplicated by handle, so `list[int]` (4-byte `I4`) and
/// `list[float]` (8-byte `F8`) sharing one handle would share one descriptor, and whichever the
/// backend emitted first would describe both -- a collector would then stride an 8-byte list by 4.
/// Keying the handle by element kind makes that unrepresentable.
///
/// `list[<obj>]` (PyValue elements) still needs its own answer: a tagged word is neither a frozen
/// primitive nor a bare reference, so it wants a kind of its own rather than either of these.
fn list_type_handle(element_kind: u32) -> TypeHandle {
    lamella_ir::synthetic_array_handle(element_kind)
}

/// The array element kind for a typed list's elements -- the frozen primitive codes `I4 = 5` and
/// `F8 = 8`, mirrored from the backend's element-kind space. Only `I32`/`F64` lists lower today
/// (both callers reject anything else before reaching here), so an unexpected type takes the
/// "cannot be described" code rather than a wrong one.
fn list_element_kind(elem: MirType) -> u32 {
    match elem {
        MirType::I32 => 5,
        MirType::F64 => 8,
        _ => 0xFF,
    }
}

/// The `TypeHandle` stamped on a growable list's HEADER `Alloc`. It MUST differ from
/// [`LIST_TYPE_HANDLE`]: the backend emits one canonical TypeDesc per handle, so sharing a handle with
/// the packed backing array would give the header the array's ref-less descriptor and the collector
/// would never trace `backing`. One handle serves every element kind -- the header's layout does not
/// mention the element type.
const GROWLIST_HEADER_TYPE_HANDLE: TypeHandle = TypeHandle(1);

/// A growable list's header field offsets. The header is a small heap object, SEPARATE from the packed
/// `backing` array it points at, so that the list's identity survives a grow: `b = a; a.append(1)`
/// leaves `b` pointing at the same header and observing the new element, and one header shape serves
/// every element kind.
///
/// `len` is the list's LENGTH; `cap` is how many elements `backing` has room for. The backing's own
/// `[u32 len]` prefix holds `cap` (its true allocated extent -- what a heap walk must see), NOT `len`,
/// so an element op's bounds must be checked against THIS `len` before the access.
const GROWLIST_LEN_OFFSET: u32 = 0;
const GROWLIST_CAP_OFFSET: u32 = 4;
const GROWLIST_BACKING_OFFSET: u32 = 8;
/// The header's payload size: `len` + `cap` + `backing`, three words.
const GROWLIST_PAYLOAD_SIZE: u32 = 12;
/// The element capacity a growable list's backing is seeded with when the list starts out empty.
const GROWLIST_SEED_CAP: i64 = 4;

/// The runtime-support entry `append` calls to ensure a growable list's backing has room:
/// `py_list_grow(header, needed_cap, element_size)`. It returns immediately when `cap` already
/// suffices, so `append` can call it unconditionally instead of branching on `len == cap`; when it
/// does grow, it allocates a larger backing, copies the live elements, and updates the header's
/// `backing` and `cap`. The growth POLICY lives there rather than here, and the element size is a
/// compile-time constant this end passes because the backing does not record it.
const PY_LIST_GROW_SYMBOL: &str = "py_list_grow";

/// Emit one instruction of type `ty`, returning its result value.
fn emit(values: &mut Values, insts: &mut Vec<(ValueId, Inst)>, inst: Inst, ty: MirType) -> ValueId {
    let id = values.fresh(ty);
    insts.push((id, inst));
    id
}

/// The static-region byte offset of `g_exception_tag`, the no-GC exception model's in-flight tag
/// word: a `raise` stores the thrown type's tag here, a `catch`/`except` dispatch loads and compares
/// it, and zero means no exception is propagating. This is the SAME reserved word (offset 0) the C#
/// lowering uses, so a mixed image shares one convention; the typed Python lane emits no other static
/// field, so nothing aliases it.
const EXCEPTION_TAG_OFFSET: u32 = 0;

/// Promote a value to `f64`: an F64 passes through; an I32 is widened with `IntToFloat64` (Python
/// promotes an int operand of a mixed or true-division expression to float). Any other type is out
/// of the typed float lane.
fn promote_to_f64(
    values: &mut Values,
    insts: &mut Vec<(ValueId, Inst)>,
    value: ValueId,
    ty: MirType,
) -> Result<ValueId, LowerError> {
    match ty {
        MirType::F64 => Ok(value),
        MirType::I32 => Ok(emit(
            values,
            insts,
            Inst::Convert {
                value,
                kind: ConvKind::IntToFloat64,
            },
            MirType::F64,
        )),
        _ => Err(LowerError::DynamicOperation),
    }
}

/// The 0/1 correction that turns truncating division into Python's floor division:
/// 1 when there is a remainder AND the operands have different signs (then floor and
/// truncation disagree by one). Grounded in the 3.14.6 binary-arithmetic semantics
/// (`//` floors toward negative infinity; `%` takes the divisor's sign;
/// `x == (x//y)*y + (x%y)`).
fn floor_adjust(
    values: &mut Values,
    insts: &mut Vec<(ValueId, Inst)>,
    rem: ValueId,
    lhs: ValueId,
    rhs: ValueId,
) -> ValueId {
    let zero = emit(values, insts, Inst::ConstInt {
        ty: MirType::I32,
        value: 0,
    }, MirType::I32);
    let rem_nonzero = emit(values, insts, Inst::Compare {
        op: MCmpOp::Ne,
        lhs: rem,
        rhs: zero,
    }, MirType::I32);
    let xor = emit(values, insts, Inst::Binary {
        op: MBinOp::Xor,
        lhs,
        rhs,
    }, MirType::I32);
    let signs_differ = emit(values, insts, Inst::Compare {
        op: MCmpOp::SignedLt,
        lhs: xor,
        rhs: zero,
    }, MirType::I32);
    emit(values, insts, Inst::Binary {
        op: MBinOp::And,
        lhs: rem_nonzero,
        rhs: signs_differ,
    }, MirType::I32)
}

/// Python `a // b` for typed integers: the truncating quotient minus the floor
/// correction. A zero divisor traps in hardware; `ZeroDivisionError` requires
/// the exception machinery.
fn emit_floor_div(
    values: &mut Values,
    insts: &mut Vec<(ValueId, Inst)>,
    lhs: ValueId,
    rhs: ValueId,
) -> ValueId {
    let q = emit(values, insts, Inst::Binary {
        op: MBinOp::DivSigned,
        lhs,
        rhs,
    }, MirType::I32);
    let r = emit(values, insts, Inst::Binary {
        op: MBinOp::RemSigned,
        lhs,
        rhs,
    }, MirType::I32);
    let adjust = floor_adjust(values, insts, r, lhs, rhs);
    emit(values, insts, Inst::Binary {
        op: MBinOp::Sub,
        lhs: q,
        rhs: adjust,
    }, MirType::I32)
}

/// Python `a % b` for typed integers: the truncating remainder plus the floor
/// correction times the divisor, so the result takes the divisor's sign.
fn emit_floor_mod(
    values: &mut Values,
    insts: &mut Vec<(ValueId, Inst)>,
    lhs: ValueId,
    rhs: ValueId,
) -> ValueId {
    let r = emit(values, insts, Inst::Binary {
        op: MBinOp::RemSigned,
        lhs,
        rhs,
    }, MirType::I32);
    let adjust = floor_adjust(values, insts, r, lhs, rhs);
    let adjust_b = emit(values, insts, Inst::Binary {
        op: MBinOp::Mul,
        lhs: adjust,
        rhs,
    }, MirType::I32);
    emit(values, insts, Inst::Binary {
        op: MBinOp::Add,
        lhs: r,
        rhs: adjust_b,
    }, MirType::I32)
}

fn map_cmpop(op: bc::CmpOp) -> MCmpOp {
    match op {
        bc::CmpOp::Eq => MCmpOp::Eq,
        bc::CmpOp::Ne => MCmpOp::Ne,
        bc::CmpOp::Lt => MCmpOp::SignedLt,
        bc::CmpOp::Le => MCmpOp::SignedLe,
        bc::CmpOp::Gt => MCmpOp::SignedGt,
        bc::CmpOp::Ge => MCmpOp::SignedGe,
        bc::CmpOp::Is | bc::CmpOp::IsNot => {
            unreachable!("identity comparison is not in the typed lane")
        }
    }
}

/// Handle a built-in over typed integer arguments, returning its result value AND that
/// result's type. `abs`/`min`/`max` are branchless arithmetic (`abs(x)` = `(x ^ (x>>31)) -
/// (x>>31)`; `min`/`max` select via the `(a ^ b) & -(a < b)` mask), each an `i32`. The MMIO
/// builtins lower to a volatile `Load`/`Store`: a read yields the loaded `i32`; a write is a
/// side effect whose Python `None` result is modelled as an ignored `PyValue` placeholder (so
/// using a write's result in typed arithmetic fails loud rather than reading a bogus int). The
/// interpreter provides the same surface for the dynamic path; here the typed path needs no
/// runtime call.
fn inline_builtin(
    builtin: Builtin,
    values: &mut Values,
    insts: &mut Vec<(ValueId, Inst)>,
    args: &[ValueId],
) -> Result<(ValueId, MirType), LowerError> {
    if args.len() != builtin.arity() {
        return Err(LowerError::ArityMismatch {
            expected: builtin.arity(),
            found: args.len(),
        });
    }
    Ok(match builtin {
        Builtin::Abs => {
            let x = args[0];
            let shift = emit(values, insts, Inst::ConstInt {
                ty: MirType::I32,
                value: 31,
            }, MirType::I32);
            let mask = emit(values, insts, Inst::Binary {
                op: MBinOp::ShrSigned,
                lhs: x,
                rhs: shift,
            }, MirType::I32);
            let flipped = emit(values, insts, Inst::Binary {
                op: MBinOp::Xor,
                lhs: x,
                rhs: mask,
            }, MirType::I32);
            let result = emit(values, insts, Inst::Binary {
                op: MBinOp::Sub,
                lhs: flipped,
                rhs: mask,
            }, MirType::I32);
            (result, MirType::I32)
        }
        Builtin::Min => (
            emit_select_extreme(values, insts, args[0], args[1], args[1]),
            MirType::I32,
        ),
        Builtin::Max => (
            emit_select_extreme(values, insts, args[0], args[1], args[0]),
            MirType::I32,
        ),
        Builtin::MmioRead { width } => {
            let value = emit(values, insts, Inst::Load {
                address: args[0],
                width,
                signed: false,
            }, MirType::I32);
            (value, MirType::I32)
        }
        Builtin::MmioWrite { width } => {
            let placeholder = emit(values, insts, Inst::Store {
                address: args[0],
                value: args[1],
                width,
            }, MirType::PyValue);
            (placeholder, MirType::PyValue)
        }
        Builtin::Print => {
            let placeholder =
                emit(values, insts, Inst::WriteInt { value: args[0] }, MirType::PyValue);
            (placeholder, MirType::PyValue)
        }
        Builtin::IntCast
        | Builtin::FloatCast
        | Builtin::Divmod
        | Builtin::Round
        | Builtin::BoolCast
        | Builtin::Len => return Err(LowerError::DynamicOperation),
    })
}

fn emit_select_extreme(
    values: &mut Values,
    insts: &mut Vec<(ValueId, Inst)>,
    a: ValueId,
    b: ValueId,
    pick: ValueId,
) -> ValueId {
    let lt = emit(values, insts, Inst::Compare {
        op: MCmpOp::SignedLt,
        lhs: a,
        rhs: b,
    }, MirType::I32);
    let zero = emit(values, insts, Inst::ConstInt {
        ty: MirType::I32,
        value: 0,
    }, MirType::I32);
    let mask = emit(values, insts, Inst::Binary {
        op: MBinOp::Sub,
        lhs: zero,
        rhs: lt,
    }, MirType::I32);
    let axb = emit(values, insts, Inst::Binary {
        op: MBinOp::Xor,
        lhs: a,
        rhs: b,
    }, MirType::I32);
    let masked = emit(values, insts, Inst::Binary {
        op: MBinOp::And,
        lhs: axb,
        rhs: mask,
    }, MirType::I32);
    emit(values, insts, Inst::Binary {
        op: MBinOp::Xor,
        lhs: pick,
        rhs: masked,
    }, MirType::I32)
}

/// Normalize a Python subscript index to a non-negative array offset, BRANCHLESSLY (the abstract-
/// interp lowering cannot emit a mid-expression branch, so a `select` is not available): compute
/// `i < 0 ? i + len : i`. With `len` read from the array header at offset 0 and `sign = i >>signed 31`
/// (`0` when `i >= 0`, all-ones `-1` when `i < 0`), the result is `i + (len & sign)` -- `i` unchanged
/// when non-negative, `i + len` when negative, exactly CPython's wrap. It is arithmetically identical
/// to `i` for a provably-non-negative index (the extra ops fold to a no-op at the value level), so the
/// normalize is always emitted. An index still out of range after this (`xs[5]` on a length-3 list, or
/// a very negative one whose wrap stays negative) is caught by the `ArrayLoad`/`ArrayStore` bounds
/// check (a trap when unguarded -- the documented divergence, the same class as an unguarded `//0`).
fn normalize_index(
    values: &mut Values,
    insts: &mut Vec<(ValueId, Inst)>,
    container: ValueId,
    index: ValueId,
) -> ValueId {
    let len = emit(values, insts, Inst::FieldLoad { base: container, offset: 0 }, MirType::I32);
    let bits = emit(values, insts, Inst::ConstInt { ty: MirType::I32, value: 31 }, MirType::I32);
    let sign = emit(values, insts, Inst::Binary { op: MBinOp::ShrSigned, lhs: index, rhs: bits }, MirType::I32);
    let wrap = emit(values, insts, Inst::Binary { op: MBinOp::And, lhs: len, rhs: sign }, MirType::I32);
    emit(values, insts, Inst::Binary { op: MBinOp::Add, lhs: index, rhs: wrap }, MirType::I32)
}

/// Allocates dense, single-assignment value ids and records each one's type.
struct Values {
    types: Vec<MirType>,
}

impl Values {
    fn new() -> Self {
        Values { types: Vec::new() }
    }

    fn fresh(&mut self, ty: MirType) -> ValueId {
        let id = ValueId(self.types.len() as u32);
        self.types.push(ty);
        id
    }
}

/// Per-block live-in sets over the bytecode CFG (a backward dataflow): which local
/// slots are read before being reassigned on some path out of the block. This drives
/// the minimal block-parameter set -- a merge carries a parameter only for a live-in
/// local, and the entry zero-initializes only a local that is live at the very start.
fn liveness(metas: &[BlockMeta], ops: &[bc::Op], n_locals: usize) -> Vec<Vec<bool>> {
    let n = metas.len();
    let mut use_set = vec![vec![false; n_locals]; n];
    let mut def_set = vec![vec![false; n_locals]; n];
    for (b, meta) in metas.iter().enumerate() {
        let body_end = if meta.ends_in_terminator {
            meta.end - 1
        } else {
            meta.end
        };
        for op in &ops[meta.start..body_end] {
            match op {
                bc::Op::LoadFast(i) => {
                    let s = *i as usize;
                    if s < n_locals && !def_set[b][s] {
                        use_set[b][s] = true;
                    }
                }
                bc::Op::StoreFast(i) => {
                    let s = *i as usize;
                    if s < n_locals {
                        def_set[b][s] = true;
                    }
                }
                _ => {}
            }
        }
    }
    let mut live_in = vec![vec![false; n_locals]; n];
    let mut changed = true;
    while changed {
        changed = false;
        for b in (0..n).rev() {
            for s in 0..n_locals {
                let live_out = metas[b].succs.iter().any(|succ| live_in[succ.index()][s]);
                if (use_set[b][s] || (live_out && !def_set[b][s])) && !live_in[b][s] {
                    live_in[b][s] = true;
                    changed = true;
                }
            }
        }
    }
    live_in
}

/// Lower one code object (a function or the `<module>` body) to a verified
/// [`Function`]. The caller hands the verified functions to the backend's
/// `lower_module_py`.
fn lower_function(
    co: &bc::CodeObject,
    funcs: &BTreeMap<String, FuncSig>,
    constants: &BTreeMap<String, i32>,
) -> Result<Function, LowerError> {
    let n_params = co.params.len();
    let n_locals = co.n_locals;
    let local_ty: Vec<MirType> = co.local_types.iter().map(|t| mir_type(*t)).collect();
    let ret_ty = mir_type(co.ret_ty);
    let mut values = Values::new();

    let metas = block_layout(&co.ops, &co.exc_table)?;
    if metas.is_empty() {
        return Err(LowerError::RunsOffEnd);
    }
    let n_bc = metas.len();
    let preds = compute_preds(&metas);
    let reachable = reachable_blocks(&metas);
    let live_in = liveness(&metas, &co.ops, n_locals);

    let fn_uses_exc = !co.exc_table.is_empty();
    let block_of_leader: BTreeMap<usize, BlockId> = metas
        .iter()
        .enumerate()
        .map(|(i, m)| (m.start, BlockId(i as u32)))
        .collect();
    let handler_of = |raise_idx: usize| -> Option<BlockId> {
        enclosing_handler(&co.exc_table, raise_idx).and_then(|h| block_of_leader.get(&h).copied())
    };
    let is_handler_target: Vec<bool> = {
        let mut v = vec![false; n_bc];
        for e in &co.exc_table {
            if let Some(b) = block_of_leader.get(&(e.target as usize)) {
                v[b.index()] = true;
            }
        }
        v
    };

    let is_merge: Vec<bool> = (0..n_bc)
        .map(|i| reachable[i] && preds[i].len() + usize::from(i == 0) >= 2)
        .collect();

    let func_params: Vec<ValueId> = (0..n_params).map(|i| values.fresh(local_ty[i])).collect();

    let mut synth_insts: Vec<(ValueId, Inst)> = Vec::new();
    let mut synth_locals: Vec<ValueId> = func_params.clone();
    for (i, &ty) in local_ty.iter().enumerate().skip(n_params) {
        if !live_in[0][i] {
            synth_locals.push(ValueId(0));
            continue;
        }
        if ty != MirType::I32 && ty != MirType::F64 {
            return Err(LowerError::UnsupportedLocalType(i));
        }
        let zero = values.fresh(ty);
        synth_insts.push((zero, Inst::ConstInt { ty, value: 0 }));
        synth_locals.push(zero);
    }
    debug_assert_eq!(synth_locals.len(), n_locals);

    let synth_id = BlockId(n_bc as u32);
    let tramp_base = n_bc + 1;
    let mut tramps: Vec<BasicBlock> = Vec::new();
    let mut blocks: Vec<BasicBlock> = Vec::with_capacity(n_bc + 1);
    let mut exit_locals: Vec<Option<Vec<ValueId>>> = vec![None; n_bc];
    let mut exit_entries: Vec<Option<Vec<StackEntry>>> = vec![None; n_bc];

    let mut arrays: BTreeMap<ValueId, ArrayInfo> = BTreeMap::new();

    for i in 0..n_bc {
        if !reachable[i] {
            blocks.push(unreachable_block());
            continue;
        }
        let (mut params, mut locals) = if is_merge[i] {
            let mut params = Vec::new();
            let mut locals = vec![ValueId(0); n_locals];
            for (s, slot) in locals.iter_mut().enumerate() {
                if live_in[i][s] {
                    let p = values.fresh(local_ty[s]);
                    params.push(p);
                    *slot = p;
                }
            }
            (params, locals)
        } else if i == 0 {
            (Vec::new(), synth_locals.clone())
        } else {
            let pred = preds[i][0];
            let inherited = exit_locals[pred]
                .clone()
                .ok_or(LowerError::UnsupportedControlFlow)?;
            (Vec::new(), inherited)
        };

        let mut stack: Vec<StackEntry> = if is_handler_target[i] {
            Vec::new()
        } else if is_merge[i] {
            let incoming = match preds[i].iter().find_map(|p| exit_entries[*p].as_ref()) {
                Some(entries) => stack_exit(entries)?,
                None => Vec::new(),
            };
            incoming
                .into_iter()
                .map(|(_, ty)| {
                    let p = values.fresh(ty);
                    params.push(p);
                    StackEntry::Value(p, ty)
                })
                .collect()
        } else if i == 0 {
            Vec::new()
        } else {
            exit_entries[preds[i][0]].clone().unwrap_or_default()
        };

        let meta = &metas[i];
        let body_end = if meta.ends_in_terminator {
            meta.end - 1
        } else {
            meta.end
        };
        let mut insts: Vec<(ValueId, Inst)> = Vec::new();
        let mut grown = meta.grow_done;
        for op in &co.ops[meta.start..body_end] {
            lower_op(co, funcs, constants, &local_ty, &mut values, &mut insts, &mut locals, &mut stack, &mut arrays, &mut grown, op)?;
        }

        let terminator = if meta.call_exc_handler.is_some() {
            let tag =
                emit(&mut values, &mut insts, Inst::StaticLoad { owner: StaticOwner::Own, offset: EXCEPTION_TAG_OFFSET }, MirType::I32);
            let if_false =
                branch_edge(&is_merge, &live_in, meta.succs[0], &locals, &stack, tramp_base, &mut tramps)?;
            let if_true =
                branch_edge(&is_merge, &live_in, meta.succs[1], &locals, &[], tramp_base, &mut tramps)?;
            Terminator::Branch {
                cond: tag,
                if_true,
                true_args: Vec::new(),
                if_false,
                false_args: Vec::new(),
            }
        } else if meta.div_check_handler.is_some() {
            let (divisor, dty) = match stack.last() {
                Some(&StackEntry::Value(v, t)) if t == MirType::I32 || t == MirType::F64 => (v, t),
                _ => return Err(LowerError::DynamicOperation),
            };
            let zero = emit(&mut values, &mut insts, Inst::ConstInt { ty: dty, value: 0 }, dty);
            let is_zero = emit(
                &mut values,
                &mut insts,
                Inst::Compare { op: MCmpOp::Eq, lhs: divisor, rhs: zero },
                MirType::I32,
            );
            let if_false =
                branch_edge(&is_merge, &live_in, meta.succs[0], &locals, &stack, tramp_base, &mut tramps)?;
            let handler = meta.succs[1];
            let raise_id = BlockId((tramp_base + tramps.len()) as u32);
            let mut raise_insts: Vec<(ValueId, Inst)> = Vec::new();
            let tagv = emit(
                &mut values,
                &mut raise_insts,
                Inst::ConstInt { ty: MirType::I32, value: i64::from(bc::exception_tag("ZeroDivisionError")) },
                MirType::I32,
            );
            let _stored = emit(
                &mut values,
                &mut raise_insts,
                Inst::StaticStore { owner: StaticOwner::Own, offset: EXCEPTION_TAG_OFFSET, value: tagv },
                MirType::I32,
            );
            tramps.push(BasicBlock {
                params: Vec::new(),
                insts: raise_insts,
                terminator: Some(Terminator::Jump {
                    target: handler,
                    args: merge_args(&is_merge, &live_in, handler, &locals, &[])?,
                }),
            });
            Terminator::Branch {
                cond: is_zero,
                if_true: raise_id,
                true_args: Vec::new(),
                if_false,
                false_args: Vec::new(),
            }
        } else if meta.subscript_check.is_some() {
            let n = stack.len();
            let index = match stack.get(n.wrapping_sub(1)) {
                Some(&StackEntry::Value(v, MirType::I32)) => Some(v),
                _ => None,
            };
            let obj = match stack.get(n.wrapping_sub(2)) {
                Some(&StackEntry::Value(v, _)) if arrays.contains_key(&v) => Some(v),
                _ => None,
            };
            match (index, obj) {
                (Some(index), Some(obj)) => {
                    let len =
                        emit(&mut values, &mut insts, Inst::FieldLoad { base: obj, offset: 0 }, MirType::I32);
                    let bits =
                        emit(&mut values, &mut insts, Inst::ConstInt { ty: MirType::I32, value: 31 }, MirType::I32);
                    let sign = emit(
                        &mut values,
                        &mut insts,
                        Inst::Binary { op: MBinOp::ShrSigned, lhs: index, rhs: bits },
                        MirType::I32,
                    );
                    let wrap = emit(
                        &mut values,
                        &mut insts,
                        Inst::Binary { op: MBinOp::And, lhs: len, rhs: sign },
                        MirType::I32,
                    );
                    let normalized = emit(
                        &mut values,
                        &mut insts,
                        Inst::Binary { op: MBinOp::Add, lhs: index, rhs: wrap },
                        MirType::I32,
                    );
                    let oob = emit(
                        &mut values,
                        &mut insts,
                        Inst::Compare { op: MCmpOp::UnsignedGe, lhs: normalized, rhs: len },
                        MirType::I32,
                    );
                    let if_false =
                        branch_edge(&is_merge, &live_in, meta.succs[0], &locals, &stack, tramp_base, &mut tramps)?;
                    let handler = meta.succs[1];
                    let raise_id = BlockId((tramp_base + tramps.len()) as u32);
                    let mut raise_insts: Vec<(ValueId, Inst)> = Vec::new();
                    let tagv = emit(
                        &mut values,
                        &mut raise_insts,
                        Inst::ConstInt { ty: MirType::I32, value: i64::from(bc::exception_tag("IndexError")) },
                        MirType::I32,
                    );
                    let _stored = emit(
                        &mut values,
                        &mut raise_insts,
                        Inst::StaticStore { owner: StaticOwner::Own, offset: EXCEPTION_TAG_OFFSET, value: tagv },
                        MirType::I32,
                    );
                    tramps.push(BasicBlock {
                        params: Vec::new(),
                        insts: raise_insts,
                        terminator: Some(Terminator::Jump {
                            target: handler,
                            args: merge_args(&is_merge, &live_in, handler, &locals, &[])?,
                        }),
                    });
                    Terminator::Branch {
                        cond: oob,
                        if_true: raise_id,
                        true_args: Vec::new(),
                        if_false,
                        false_args: Vec::new(),
                    }
                }
                _ => {
                            jump_to(&is_merge, &live_in, meta.succs[0], &locals, &stack)?
                }
            }
        } else if let Some(slot) = meta.grow_check {
            let header = locals[slot as usize];
            match arrays.get(&header).copied() {
                Some(info) => {
                    let element_size = elem_size(info.elem);
                    let len = emit(&mut values, &mut insts, Inst::FieldLoad { base: header, offset: GROWLIST_LEN_OFFSET }, MirType::I32);
                    let one = emit(&mut values, &mut insts, Inst::ConstInt { ty: MirType::I32, value: 1 }, MirType::I32);
                    let needed = emit(&mut values, &mut insts, Inst::Binary { op: MBinOp::Add, lhs: len, rhs: one }, MirType::I32);
                    let width = emit(&mut values, &mut insts, Inst::ConstInt { ty: MirType::I32, value: i64::from(element_size) }, MirType::I32);
                    let grew = emit(
                        &mut values,
                        &mut insts,
                        Inst::PInvoke { import: PY_LIST_GROW_SYMBOL.into(), args: vec![header, needed, width] },
                        MirType::I32,
                    );
                    let if_true =
                        branch_edge(&is_merge, &live_in, meta.succs[0], &locals, &stack, tramp_base, &mut tramps)?;
                    let raise_id = BlockId((tramp_base + tramps.len()) as u32);
                    let mut raise_insts: Vec<(ValueId, Inst)> = Vec::new();
                    let tagv = emit(
                        &mut values,
                        &mut raise_insts,
                        Inst::ConstInt { ty: MirType::I32, value: i64::from(bc::exception_tag("MemoryError")) },
                        MirType::I32,
                    );
                    let _stored = emit(
                        &mut values,
                        &mut raise_insts,
                        Inst::StaticStore { owner: StaticOwner::Own, offset: EXCEPTION_TAG_OFFSET, value: tagv },
                        MirType::I32,
                    );
                    let raise_term = raise_route(
                        handler_of(meta.end - 1),
                        &is_merge,
                        &live_in,
                        ret_ty,
                        &locals,
                        &mut values,
                        &mut raise_insts,
                    )?;
                    tramps.push(BasicBlock {
                        params: Vec::new(),
                        insts: raise_insts,
                        terminator: Some(raise_term),
                    });
                    Terminator::Branch {
                        cond: grew,
                        if_true,
                        true_args: Vec::new(),
                        if_false: raise_id,
                        false_args: Vec::new(),
                    }
                }
                None => jump_to(&is_merge, &live_in, meta.succs[0], &locals, &stack)?,
            }
        } else if !meta.ends_in_terminator {
            jump_to(&is_merge, &live_in, meta.succs[0], &locals, &stack)?
        } else {
            match &co.ops[meta.end - 1] {
                bc::Op::Jump(_) => {
                            jump_to(&is_merge, &live_in, meta.succs[0], &locals, &stack)?
                }
                bc::Op::PopJumpIfFalse(_) => {
                    let (cond, ct) = pop_value(&mut stack)?;
                    if ct != MirType::I32 {
                        return Err(LowerError::BadConditionType);
                    }
                            let if_false =
                        branch_edge(&is_merge, &live_in, meta.succs[0], &locals, &stack, tramp_base, &mut tramps)?;
                    let if_true =
                        branch_edge(&is_merge, &live_in, meta.succs[1], &locals, &stack, tramp_base, &mut tramps)?;
                    Terminator::Branch {
                        cond,
                        if_true,
                        true_args: Vec::new(),
                        if_false,
                        false_args: Vec::new(),
                    }
                }
                bc::Op::Return => {
                    let (value, ty) = pop_value(&mut stack)?;
                    if ty != ret_ty {
                        return Err(LowerError::ReturnTypeMismatch);
                    }
                    if fn_uses_exc {
                        let zero =
                            emit(&mut values, &mut insts, Inst::ConstInt { ty: MirType::I32, value: 0 }, MirType::I32);
                        let _cleared = emit(
                            &mut values,
                            &mut insts,
                            Inst::StaticStore { owner: StaticOwner::Own, offset: EXCEPTION_TAG_OFFSET, value: zero },
                            MirType::I32,
                        );
                    }
                    Terminator::Return(Some(value))
                }
                bc::Op::Raise(1) => {
                    let name = match pop(&mut stack)? {
                        StackEntry::ExcType(n) => n,
                        _ => return Err(LowerError::DynamicOperation),
                    };
                    let tag = bc::exception_tag(&name);
                    let tagv =
                        emit(&mut values, &mut insts, Inst::ConstInt { ty: MirType::I32, value: i64::from(tag) }, MirType::I32);
                    let _stored = emit(
                        &mut values,
                        &mut insts,
                        Inst::StaticStore { owner: StaticOwner::Own, offset: EXCEPTION_TAG_OFFSET, value: tagv },
                        MirType::I32,
                    );
                    raise_route(handler_of(meta.end - 1), &is_merge, &live_in, ret_ty, &locals, &mut values, &mut insts)?
                }
                bc::Op::Raise(0) | bc::Op::Reraise => {
                    raise_route(handler_of(meta.end - 1), &is_merge, &live_in, ret_ty, &locals, &mut values, &mut insts)?
                }
                bc::Op::Raise(_) => return Err(LowerError::DynamicOperation),
                _ => return Err(LowerError::RunsOffEnd),
            }
        };
        exit_entries[i] = Some(stack);
        exit_locals[i] = Some(locals);
        blocks.push(BasicBlock {
            params,
            insts,
            terminator: Some(terminator),
        });
    }

    let synth_term = jump_to(&is_merge, &live_in, BlockId(0), &synth_locals, &[])?;
    blocks.push(BasicBlock {
        params: func_params,
        insts: synth_insts,
        terminator: Some(synth_term),
    });
    blocks.extend(tramps);

    Ok(Function {
        params: (0..n_params).map(|i| local_ty[i]).collect(),
        ret: Some(ret_ty),
        blocks,
        entry: synth_id,
        value_types: values.types,
    })
}

fn unreachable_block() -> BasicBlock {
    BasicBlock {
        params: Vec::new(),
        insts: Vec::new(),
        terminator: Some(Terminator::Unreachable),
    }
}

/// The arguments a branch into `target` carries: when `target` is a merge, its live-in
/// locals (in slot order) followed by the threaded operand-stack values (bottom to
/// top) -- matching the order the merge declares those parameters. A non-merge target
/// has no parameters, so no arguments: it reuses the predecessor's values directly.
///
/// The stack arrives as ENTRIES and is converted to values here, only for a merge -- which is what
/// decides whether an entry has to be a value at all. A non-merge successor inherits its predecessor's
/// entries whole, so a callee mid-expression (`print(xs[i])` around a block split) crosses that edge
/// fine; only a merge, which must declare a parameter per slot, cannot take one.
fn merge_args(
    is_merge: &[bool],
    live_in: &[Vec<bool>],
    target: BlockId,
    locals: &[ValueId],
    stack: &[StackEntry],
) -> Result<Vec<ValueId>, LowerError> {
    if !is_merge.get(target.index()).copied().unwrap_or(false) {
        return Ok(Vec::new());
    }
    let live = &live_in[target.index()];
    let mut args: Vec<ValueId> = (0..live.len()).filter(|&s| live[s]).map(|s| locals[s]).collect();
    args.extend(stack_exit(stack)?.iter().map(|(v, _)| *v));
    Ok(args)
}

/// The operand stack as `(ValueId, type)` pairs, for threading to a MERGE successor. A
/// callable left on the stack at a merge (a function used across a
/// mid-expression branch) is not supported in the typed subset.
fn stack_exit(stack: &[StackEntry]) -> Result<Vec<(ValueId, MirType)>, LowerError> {
    stack
        .iter()
        .map(|e| match e {
            StackEntry::Value(v, t) => Ok((*v, *t)),
            StackEntry::Callable(_) | StackEntry::Builtin(_) => Err(LowerError::CallableAsValue),
            StackEntry::Tuple(_)
            | StackEntry::EmptyList
            | StackEntry::ListAppend(_)
            | StackEntry::ListPop(_) => Err(LowerError::DynamicOperation),
            StackEntry::ExcType(_) | StackEntry::ExcTypeTuple(_) | StackEntry::ConstStr => {
                Err(LowerError::DynamicOperation)
            }
        })
        .collect()
}

/// A `Jump` to `target`, passing its live-in locals plus the threaded stack when
/// `target` is a merge; a non-merge target takes none.
fn jump_to(
    is_merge: &[bool],
    live_in: &[Vec<bool>],
    target: BlockId,
    locals: &[ValueId],
    stack: &[StackEntry],
) -> Result<Terminator, LowerError> {
    Ok(Terminator::Jump {
        target,
        args: merge_args(is_merge, live_in, target, locals, stack)?,
    })
}

/// Resolve one edge of a `Branch`. A `Branch` may carry no arguments, so an edge into
/// a merge block (which expects its live-in locals plus the threaded stack) is routed
/// through a parameter-less trampoline that jumps there with them; an edge into a
/// non-merge block is direct.
fn branch_edge(
    is_merge: &[bool],
    live_in: &[Vec<bool>],
    target: BlockId,
    locals: &[ValueId],
    stack: &[StackEntry],
    tramp_base: usize,
    tramps: &mut Vec<BasicBlock>,
) -> Result<BlockId, LowerError> {
    if is_merge.get(target.index()).copied().unwrap_or(false) {
        let id = BlockId((tramp_base + tramps.len()) as u32);
        let args = merge_args(is_merge, live_in, target, locals, stack)?;
        tramps.push(BasicBlock {
            params: Vec::new(),
            insts: Vec::new(),
            terminator: Some(Terminator::Jump { target, args }),
        });
        Ok(id)
    } else {
        Ok(target)
    }
}

/// The terminator routing an in-flight exception (its tag already stored) to `handler`, or -- when
/// no `try` guards the raise -- out of the function: a `Return` of a typed-zero placeholder, leaving
/// `g_exception_tag` set so the caller/entry observes it. The handler is entered at value-stack depth
/// 0 (the exception table truncates the stack), so only locals cross.
fn raise_route(
    handler: Option<BlockId>,
    is_merge: &[bool],
    live_in: &[Vec<bool>],
    ret_ty: MirType,
    locals: &[ValueId],
    values: &mut Values,
    insts: &mut Vec<(ValueId, Inst)>,
) -> Result<Terminator, LowerError> {
    match handler {
        Some(h) => jump_to(is_merge, live_in, h, locals, &[]),
        None => {
            let zero = emit(values, insts, Inst::ConstInt { ty: ret_ty, value: 0 }, ret_ty);
            Ok(Terminator::Return(Some(zero)))
        }
    }
}

/// One basic block's op range and how it leaves.
struct BlockMeta {
    /// The first op index (a leader).
    start: usize,
    /// One past the last op index.
    end: usize,
    /// The successor block ids (for predecessor and reachability analysis).
    succs: Vec<BlockId>,
    /// Whether the block's last op is a control-flow op (else it falls through).
    ends_in_terminator: bool,
    /// Set when the block's last op is a `Call`/`CallKw` inside a `try`: the handler op index the
    /// callee's in-flight exception routes to. The block then ends in an after-call tag check
    /// (a `Branch` on `g_exception_tag`), not a plain fall-through. `succs` are `[continue, handler]`.
    call_exc_handler: Option<usize>,
    /// Set when the NEXT block begins with a `try`-guarded division (`//`/`%`/`/`): the handler op
    /// index a `ZeroDivisionError` routes to. The block ends in a divisor check (a `Branch` on
    /// `divisor == 0`) BEFORE the division runs. `succs` are `[divide, handler]`.
    div_check_handler: Option<usize>,
    /// Set when the NEXT block begins with a `try`-guarded subscript load/store (`xs[i]` / `xs[i]=v`):
    /// the handler op index an `IndexError` routes to. When the container is a typed list, the block
    /// ends in a bounds check (a `Branch` on `index >=u len`) BEFORE the element op, and `succs` are
    /// `[access, handler]`; when it is a dynamic container the check is skipped (a plain fall-through --
    /// that access stays the interpreter's), the handler edge unused.
    subscript_check: Option<usize>,
    /// Set when the block's LAST op is [`bc::Op::ListGrow`]: the local slot of the list about to be
    /// appended to. The block ends in a grow-and-check (a `Branch` on whether the runtime found room)
    /// BEFORE the store that follows, because a store past the capacity a failed grow left behind is
    /// a memory-safety defect rather than a wrong answer. `succs` are `[store]`, plus the handler when
    /// a `try` guards the append -- and unlike the division and subscript checks this one is emitted
    /// GUARDED OR NOT, since an unguarded heap exhaustion must still raise rather than fault.
    grow_check: Option<u32>,
    /// Set when the PREVIOUS block ended in [`bc::Op::ListGrow`], so this block's first append has
    /// already had its capacity ensured and must NOT ask again. Without it the grow would be called
    /// twice per append -- harmless, since the second call returns at once, and a wasted call on the
    /// hot path all the same.
    grow_done: bool,
}

/// Whether `op` is a division that raises `ZeroDivisionError` on a zero divisor: floor-division
/// (`//`), modulo (`%`), or true-division (`/`). When one is guarded by a `try`, the lowering emits a
/// divisor check ahead of it (a synthesized raise, the twin of the C# bounds/divide-by-zero check).
fn is_zero_div_op(op: &bc::Op) -> bool {
    matches!(
        op,
        bc::Op::Binary(bc::BinOp::FloorDiv | bc::BinOp::Mod | bc::BinOp::TrueDiv)
    )
}

/// Whether `op` is a subscript element access -- a load (`xs[i]`) or a store (`xs[i] = v`) -- that
/// raises `IndexError` out of range on a typed list. When one is guarded by a `try`, the lowering
/// emits a bounds check ahead of it (the `IndexError` twin of the `ZeroDivisionError` divisor check).
fn is_subscript_op(op: &bc::Op) -> bool {
    matches!(op, bc::Op::Subscript { .. } | bc::Op::Setitem)
}

/// The handler op index a raise at `idx` transfers to: the target of the INNERMOST exception-table
/// entry whose protected range covers `idx`, or `None` when no `try` guards `idx` (an uncaught raise
/// that propagates out of the function). The table lists entries innermost-first, so the first match
/// is the tightest.
fn enclosing_handler(exc_table: &[bc::ExcEntry], idx: usize) -> Option<usize> {
    exc_table
        .iter()
        .find(|e| (e.start as usize) <= idx && idx < (e.end as usize))
        .map(|e| e.target as usize)
}

/// Split the op stream into basic blocks at leaders and record each block's
/// successors. A leader is op 0, any jump target, the op after any jump / return /
/// raise, and any exception-table handler target (entered only via the table). A block
/// ending in `raise`/re-raise leaves to its enclosing handler (or exits the function,
/// leaving the in-flight tag set, when none guards it) -- the exception edge in the CFG.
fn block_layout(ops: &[bc::Op], exc_table: &[bc::ExcEntry]) -> Result<Vec<BlockMeta>, LowerError> {
    if ops.is_empty() {
        return Ok(Vec::new());
    }
    let mut leaders: Vec<usize> = vec![0];
    for (i, op) in ops.iter().enumerate() {
        match op {
            bc::Op::Jump(t) | bc::Op::PopJumpIfFalse(t) => {
                leaders.push(*t as usize);
                if i + 1 < ops.len() {
                    leaders.push(i + 1);
                }
            }
            bc::Op::Return | bc::Op::Raise(_) | bc::Op::Reraise if i + 1 < ops.len() => {
                leaders.push(i + 1);
            }
            bc::Op::Call(_) | bc::Op::CallKw { .. }
                if i + 1 < ops.len() && enclosing_handler(exc_table, i).is_some() =>
            {
                leaders.push(i + 1);
            }
            _ if is_zero_div_op(op) && enclosing_handler(exc_table, i).is_some() => {
                leaders.push(i);
            }
            _ if is_subscript_op(op) && enclosing_handler(exc_table, i).is_some() => {
                leaders.push(i);
            }
            bc::Op::ListGrow { .. } if i + 1 < ops.len() => leaders.push(i + 1),
            _ => {}
        }
    }
    for e in exc_table {
        leaders.push(e.target as usize);
    }
    leaders.sort_unstable();
    leaders.dedup();

    let block_of: BTreeMap<usize, BlockId> = leaders
        .iter()
        .enumerate()
        .map(|(i, &op)| (op, BlockId(i as u32)))
        .collect();
    let block_id = |op: usize| -> Result<BlockId, LowerError> {
        block_of.get(&op).copied().ok_or(LowerError::RunsOffEnd)
    };

    let mut metas = Vec::with_capacity(leaders.len());
    for (i, &start) in leaders.iter().enumerate() {
        let end = leaders.get(i + 1).copied().unwrap_or(ops.len());
        let last = &ops[end - 1];
        let call_exc_handler = match last {
            bc::Op::Call(_) | bc::Op::CallKw { .. } => enclosing_handler(exc_table, end - 1),
            _ => None,
        };
        let div_check_handler = match ops.get(end) {
            Some(op) if call_exc_handler.is_none() && is_zero_div_op(op) => {
                enclosing_handler(exc_table, end)
            }
            _ => None,
        };
        let subscript_check = match ops.get(end) {
            Some(op)
                if call_exc_handler.is_none()
                    && div_check_handler.is_none()
                    && is_subscript_op(op) =>
            {
                enclosing_handler(exc_table, end)
            }
            _ => None,
        };
        let grow_done = start
            .checked_sub(1)
            .and_then(|i| ops.get(i))
            .is_some_and(|op| matches!(op, bc::Op::ListGrow { .. }));
        let grow_check = match last {
            bc::Op::ListGrow { list } if end < ops.len() => Some(*list),
            _ => None,
        };
        let (succs, ends_in_terminator) = match last {
            bc::Op::Jump(t) => (vec![block_id(*t as usize)?], true),
            bc::Op::PopJumpIfFalse(t) => (vec![block_id(*t as usize)?, block_id(end)?], true),
            bc::Op::Return => (Vec::new(), true),
            bc::Op::Raise(_) | bc::Op::Reraise => {
                let succs = match enclosing_handler(exc_table, end - 1) {
                    Some(handler) => vec![block_id(handler)?],
                    None => Vec::new(),
                };
                (succs, true)
            }
            _ if call_exc_handler.is_some() => {
                (vec![block_id(end)?, block_id(call_exc_handler.unwrap())?], false)
            }
            _ if div_check_handler.is_some() => {
                (vec![block_id(end)?, block_id(div_check_handler.unwrap())?], false)
            }
            _ if subscript_check.is_some() => {
                (vec![block_id(end)?, block_id(subscript_check.unwrap())?], false)
            }
            _ if grow_check.is_some() => {
                let mut succs = vec![block_id(end)?];
                if let Some(handler) = enclosing_handler(exc_table, end - 1) {
                    succs.push(block_id(handler)?);
                }
                (succs, false)
            }
            _ => (vec![block_id(end)?], false),
        };
        metas.push(BlockMeta {
            start,
            end,
            succs,
            ends_in_terminator,
            call_exc_handler,
            div_check_handler,
            subscript_check,
            grow_check,
            grow_done,
        });
    }
    Ok(metas)
}

/// The predecessor block indices of each block, inverted from the successor lists.
fn compute_preds(metas: &[BlockMeta]) -> Vec<Vec<usize>> {
    let mut preds = vec![Vec::new(); metas.len()];
    for (j, meta) in metas.iter().enumerate() {
        for succ in &meta.succs {
            preds[succ.index()].push(j);
        }
    }
    preds
}

/// Mark blocks reachable from the entry (block 0) by following successors.
fn reachable_blocks(metas: &[BlockMeta]) -> Vec<bool> {
    let mut reachable = vec![false; metas.len()];
    if metas.is_empty() {
        return reachable;
    }
    let mut work = vec![BlockId(0)];
    reachable[0] = true;
    while let Some(b) = work.pop() {
        for &s in &metas[b.index()].succs {
            if !reachable[s.index()] {
                reachable[s.index()] = true;
                work.push(s);
            }
        }
    }
    reachable
}

/// A callee's call signature, for resolving `LoadGlobal`/`Call`: its index in the
/// module (the `Inst::Call` callee), its MIR return type, its parameter count (for the
/// arity check), and its parameter NAMES (to static-bind a keyword call to positional).
/// The index is module-relative; a driver that prepends functions (e.g. an AOT entry
/// shim) offsets it.
#[derive(Clone)]
struct FuncSig {
    index: u32,
    ret: MirType,
    arity: usize,
    param_names: Vec<String>,
    /// The parameters' declared PYTHON types, not just their MIR types: an argument's MIR type alone
    /// cannot say whether it may be passed. Every typed sequence is an `ObjectRef`, so a growable
    /// list's header would satisfy a `list[int]` parameter on MIR type alone -- and the callee, reading
    /// it as a packed array, would take the header's `len` for element 0.
    param_types: Vec<bc::StaticType>,
}

/// A built-in the typed path handles with no runtime call: `abs`/`min`/`max` over integers
/// (inlined to branchless arithmetic), the MMIO primitives (lowered to a volatile `Store`/`Load`),
/// and `print` of one int (lowered to a semihosting `WriteInt`). Other built-ins (`len`,
/// container/string operations) dispatch to the runtime and arrive with the dynamic surface.
///
/// The MMIO builtins are the AOT half of the language-neutral MMIO contract: `mmio_read{8,16,32}
/// (addr) -> int` and `mmio_write{8,16,32}(addr, value)`, lowering to the SAME volatile ldr/str
/// a C / Rust / C#-AOT BSP reaches (the "type gradient" -- no `py_*` runtime call). Names are
/// provisional pending the cross-lane contract (python-runtime wires the matching interp builtin).
#[derive(Clone, Copy)]
enum Builtin {
    Abs,
    Min,
    Max,
    /// `mmio_read{8,16,32}(addr) -> int` -- a volatile load of `width` bytes (zero-extended to i32).
    MmioRead { width: u32 },
    /// `mmio_write{8,16,32}(addr, value)` -- a volatile store of `width` bytes; returns None.
    MmioWrite { width: u32 },
    /// `print(x)` for one int argument -- a semihosting `WriteInt` (signed decimal + newline);
    /// returns None. Only the single-positional-int form; `print()`, multi-arg, and keyworded
    /// `print` fall to the dynamic path (they need spacing/`sep`/`end` the primitive does not do).
    Print,
    /// `int(x)` on a numeric argument -- `int(int)` is identity, `int(float)` a `Float64ToInt`
    /// convert (truncates toward zero == Python `int()`). A non-numeric arg (`int("5")`, base) is
    /// dynamic. Handled in the `Call` op (it needs the argument's type), not `inline_builtin`.
    IntCast,
    /// `float(x)` on a numeric argument -- `float(float)` is identity, `float(int)` an `IntToFloat64`
    /// convert. A non-numeric arg (`float("3.14")`) is dynamic. Handled in the `Call` op.
    FloatCast,
    /// `divmod(a, b)` on two ints -- returns the tuple `(a // b, a % b)`. Lowered in the `Call` op to
    /// a threaded `StackEntry::Tuple` of the floor-quotient and floor-remainder (the same ops `//`
    /// and `%` emit), so `q, r = divmod(a, b)` elides the heap tuple. Float args are dynamic.
    Divmod,
    /// `round(x)` (one arg) -- returns an int. `round(int)` is identity; `round(float)` is
    /// round-half-to-even (`lamella_rint`) then `Float64ToInt`. `round(x, ndigits)` (two args, a float
    /// result) is dynamic. Handled in the `Call` op (it needs the argument's type).
    Round,
    /// `bool(x)` on a numeric argument -- the truthiness `x != 0` (int) / `x != 0.0` (float), an i32
    /// 0/1. A non-numeric arg (container/string truthiness) is dynamic. Handled in the `Call` op.
    BoolCast,
    /// `len(xs)` on a typed list -- reads the `u32` length prefix at the array header (offset 0), an
    /// i32, with NO runtime call. A non-array argument (`len(str)`, a dynamic container) is dynamic.
    /// Handled in the `Call` op (it needs the argument's type -- an array `ObjectRef`), not inline.
    Len,
}

impl Builtin {
    fn from_name(name: &str) -> Option<Builtin> {
        match name {
            "abs" => Some(Builtin::Abs),
            "min" => Some(Builtin::Min),
            "max" => Some(Builtin::Max),
            "mmio_read8" => Some(Builtin::MmioRead { width: 1 }),
            "mmio_read16" => Some(Builtin::MmioRead { width: 2 }),
            "mmio_read32" => Some(Builtin::MmioRead { width: 4 }),
            "mmio_write8" => Some(Builtin::MmioWrite { width: 1 }),
            "mmio_write16" => Some(Builtin::MmioWrite { width: 2 }),
            "mmio_write32" => Some(Builtin::MmioWrite { width: 4 }),
            "print" => Some(Builtin::Print),
            "int" => Some(Builtin::IntCast),
            "float" => Some(Builtin::FloatCast),
            "divmod" => Some(Builtin::Divmod),
            "round" => Some(Builtin::Round),
            "bool" => Some(Builtin::BoolCast),
            "len" => Some(Builtin::Len),
            _ => None,
        }
    }

    fn arity(self) -> usize {
        match self {
            Builtin::Abs
            | Builtin::MmioRead { .. }
            | Builtin::Print
            | Builtin::IntCast
            | Builtin::FloatCast
            | Builtin::Round
            | Builtin::BoolCast
            | Builtin::Len => 1,
            Builtin::Min | Builtin::Max | Builtin::MmioWrite { .. } | Builtin::Divmod => 2,
        }
    }
}

/// One operand-stack slot during abstract interpretation: a typed value; a reference to a
/// callee -- a user function or a built-in -- pushed by `LoadGlobal` and consumed by `Call`
/// (keeping callees on the stack lets nested calls `f(g(x))` resolve); or a threaded `Tuple`
/// of typed values, pushed by `BuildTuple` and consumed by `UnpackSequence`, which lets the
/// `a, b = <exprs>` idiom elide its heap tuple (any other consumer pops it as a plain value
/// via `pop_value` and falls to the dynamic path -- a real tuple object).
///
/// `Clone` so that a single-successor block boundary can carry the whole stack across: those edges
/// pass no parameters, so an entry crosses as ITSELF rather than as a threaded value.
#[derive(Clone)]
enum StackEntry {
    Value(ValueId, MirType),
    Callable(FuncSig),
    Builtin(Builtin),
    Tuple(Vec<(ValueId, MirType)>),
    /// An empty list literal `[]`, threaded UNMATERIALIZED because it carries no element to take its
    /// element kind from: only a `StoreFast` into a growable-list local -- whose static type names that
    /// kind -- can build it. Every other consumer pops it as dynamic, which is what `[]` already was.
    EmptyList,
    /// A growable list's bound `append` method, carrying the HEADER it was read off. Only the `Call`
    /// that immediately consumes it lowers it; anywhere else (a stored or passed bound method) it is
    /// dynamic.
    ListAppend(ValueId),
    /// A growable list's bound `pop` method, carrying the HEADER it was read off. Like
    /// [`Self::ListAppend`], only the `Call` that immediately consumes it lowers it.
    ListPop(ValueId),
    /// A built-in exception TYPE pushed by `LoadGlobal` (`raise IndexError`, `except IndexError:`).
    /// It is not a plain value: only `Raise` (which stores its tag) and `MatchExc` (which tests the
    /// in-flight tag against its subtype closure) consume it; anywhere else falls to the dynamic path.
    ExcType(String),
    /// A tuple of built-in exception types pushed by `BuildTuple` when every element is an `ExcType`
    /// (`except (A, B):`). Only `MatchExc` consumes it (testing the in-flight tag against the UNION of
    /// the members' subtype closures); anywhere else it is a real tuple value -> the dynamic path.
    ExcTypeTuple(Vec<String>),
    /// A STRING constant, carried inert. The typed lane has no string values, so this is not one:
    /// it exists only so `raise E("...")` can reach its `Call`, which discards it (see the
    /// `ExcType` arm of `Op::Call`). Any other consumer falls to the dynamic path, exactly as a
    /// string literal did before it was representable at all.
    ConstStr,
}

fn pop(stack: &mut Vec<StackEntry>) -> Result<StackEntry, LowerError> {
    stack.pop().ok_or(LowerError::StackUnderflow)
}

/// Pop a typed value; a callee here means a function or built-in name used as a plain
/// value, which the typed subset does not support. A `Tuple` entry that was not consumed by
/// an `UnpackSequence` is a real (heap) tuple value -- dynamic in the typed lane.
fn pop_value(stack: &mut Vec<StackEntry>) -> Result<(ValueId, MirType), LowerError> {
    match pop(stack)? {
        StackEntry::Value(v, t) => Ok((v, t)),
        StackEntry::Callable(_) | StackEntry::Builtin(_) => Err(LowerError::CallableAsValue),
        StackEntry::Tuple(_)
        | StackEntry::EmptyList
        | StackEntry::ListAppend(_)
        | StackEntry::ListPop(_) => Err(LowerError::DynamicOperation),
        StackEntry::ExcType(_) | StackEntry::ExcTypeTuple(_) | StackEntry::ConstStr => {
            Err(LowerError::DynamicOperation)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn lower_op(
    co: &bc::CodeObject,
    funcs: &BTreeMap<String, FuncSig>,
    constants: &BTreeMap<String, i32>,
    local_ty: &[MirType],
    values: &mut Values,
    insts: &mut Vec<(ValueId, Inst)>,
    locals: &mut [ValueId],
    stack: &mut Vec<StackEntry>,
    arrays: &mut BTreeMap<ValueId, ArrayInfo>,
    grown: &mut bool,
    op: &bc::Op,
) -> Result<(), LowerError> {
    match op {
        bc::Op::LoadConst(k) => {
            let c = co
                .consts
                .get(*k as usize)
                .ok_or(LowerError::BadConstIndex(*k))?;
            if matches!(c, bc::Const::Str(_)) {
                stack.push(StackEntry::ConstStr);
                return Ok(());
            }
            if let bc::Const::Float(bits) = c {
                let id = values.fresh(MirType::F64);
                insts.push((id, Inst::ConstInt {
                    ty: MirType::F64,
                    value: *bits as i64,
                }));
                stack.push(StackEntry::Value(id, MirType::F64));
                return Ok(());
            }
            let value: i64 = match c {
                bc::Const::Int(v) => {
                    if let Ok(n) = i32::try_from(*v) {
                        i64::from(n)
                    } else if let Ok(u) = u32::try_from(*v) {
                        i64::from(u as i32)
                    } else {
                        return Err(LowerError::IntLiteralTooLarge(*v));
                    }
                }
                bc::Const::Bool(b) => i64::from(*b),
                bc::Const::None
                | bc::Const::Str(_)
                | bc::Const::KwNames(_)
                | bc::Const::ArgKinds(_)
                | bc::Const::Imaginary(_)
                | bc::Const::BigInt(_)
                | bc::Const::Bytes(_) => {
                    return Err(LowerError::UnsupportedConst);
                }
                bc::Const::Float(_) => unreachable!("a float constant is materialized above"),
            };
            let id = values.fresh(MirType::I32);
            insts.push((id, Inst::ConstInt {
                ty: MirType::I32,
                value,
            }));
            stack.push(StackEntry::Value(id, MirType::I32));
        }
        bc::Op::LoadFast(i) => {
            let slot = *i as usize;
            let value = *locals.get(slot).ok_or(LowerError::BadLocalIndex(*i))?;
            if let Some(info) = seq_info_of(co.local_types[slot]) {
                arrays.insert(value, info);
            }
            stack.push(StackEntry::Value(value, local_ty[slot]));
        }
        bc::Op::StoreFast(i) => {
            let slot = *i as usize;
            if slot >= locals.len() {
                return Err(LowerError::BadLocalIndex(*i));
            }
            if let Some(info) = seq_info_of(co.local_types[slot]) {
                if info.kind == SeqKind::FixedTuple && matches!(stack.last(), Some(StackEntry::Tuple(_))) {
                    let StackEntry::Tuple(elems) = pop(stack)? else { unreachable!() };
                    let obj = materialize_array(values, insts, arrays, &elems, info.elem, info.kind)?;
                    locals[slot] = obj;
                    return Ok(());
                }
                if info.kind == SeqKind::FixedList
                    && matches!(stack.last(), Some(StackEntry::EmptyList))
                {
                    pop(stack)?;
                    let obj = materialize_array(values, insts, arrays, &[], info.elem, info.kind)?;
                    locals[slot] = obj;
                    return Ok(());
                }
                if info.kind == SeqKind::GrowList {
                    let header = if matches!(stack.last(), Some(StackEntry::EmptyList)) {
                        pop(stack)?;
                        materialize_empty_growlist(values, insts, arrays, info.elem)?
                    } else {
                        let (value, _ty) = pop_value(stack)?;
                        match arrays.get(&value).map(|held| held.kind) {
                            Some(SeqKind::GrowList) => value,
                            Some(SeqKind::FixedList) => {
                                wrap_array_as_growlist(values, insts, arrays, value, info.elem)
                            }
                            _ => return Err(LowerError::DynamicOperation),
                        }
                    };
                    locals[slot] = header;
                    return Ok(());
                }
            }
            let (value, _ty) = pop_value(stack)?;
            locals[slot] = value;
        }
        bc::Op::Binary(b) | bc::Op::InplaceBinOp(b) => {
            let (rhs, rt) = pop_value(stack)?;
            let (lhs, lt) = pop_value(stack)?;
            if lt == MirType::F64 || rt == MirType::F64 || matches!(b, bc::BinOp::TrueDiv) {
                let lhs = promote_to_f64(values, insts, lhs, lt)?;
                let rhs = promote_to_f64(values, insts, rhs, rt)?;
                let op = match b {
                    bc::BinOp::Add => MBinOp::Add,
                    bc::BinOp::Sub => MBinOp::Sub,
                    bc::BinOp::Mul => MBinOp::Mul,
                    bc::BinOp::TrueDiv => MBinOp::DivSigned,
                    _ => return Err(LowerError::DynamicOperation),
                };
                let id = emit(values, insts, Inst::Binary { op, lhs, rhs }, MirType::F64);
                stack.push(StackEntry::Value(id, MirType::F64));
                return Ok(());
            }
            if lt != MirType::I32 || rt != MirType::I32 {
                return Err(LowerError::DynamicOperation);
            }
            let id = match b {
                bc::BinOp::Add => emit(values, insts, Inst::Binary {
                    op: MBinOp::Add,
                    lhs,
                    rhs,
                }, MirType::I32),
                bc::BinOp::Sub => emit(values, insts, Inst::Binary {
                    op: MBinOp::Sub,
                    lhs,
                    rhs,
                }, MirType::I32),
                bc::BinOp::Mul => emit(values, insts, Inst::Binary {
                    op: MBinOp::Mul,
                    lhs,
                    rhs,
                }, MirType::I32),
                bc::BinOp::FloorDiv => emit_floor_div(values, insts, lhs, rhs),
                bc::BinOp::Mod => emit_floor_mod(values, insts, lhs, rhs),
                bc::BinOp::BitAnd => emit(values, insts, Inst::Binary {
                    op: MBinOp::And,
                    lhs,
                    rhs,
                }, MirType::I32),
                bc::BinOp::BitOr => emit(values, insts, Inst::Binary {
                    op: MBinOp::Or,
                    lhs,
                    rhs,
                }, MirType::I32),
                bc::BinOp::BitXor => emit(values, insts, Inst::Binary {
                    op: MBinOp::Xor,
                    lhs,
                    rhs,
                }, MirType::I32),
                bc::BinOp::LShift => emit(values, insts, Inst::Binary {
                    op: MBinOp::Shl,
                    lhs,
                    rhs,
                }, MirType::I32),
                bc::BinOp::RShift => emit(values, insts, Inst::Binary {
                    op: MBinOp::ShrSigned,
                    lhs,
                    rhs,
                }, MirType::I32),
                bc::BinOp::TrueDiv => return Err(LowerError::DynamicOperation),
                bc::BinOp::Pow => return Err(LowerError::DynamicOperation),
                bc::BinOp::MatMul => return Err(LowerError::DynamicOperation),
            };
            stack.push(StackEntry::Value(id, MirType::I32));
        }
        bc::Op::Compare(c) => {
            if matches!(c, bc::CmpOp::Is | bc::CmpOp::IsNot) {
                return Err(LowerError::DynamicOperation);
            }
            let (rhs, rt) = pop_value(stack)?;
            let (lhs, lt) = pop_value(stack)?;
            let (lhs, rhs) = if lt == MirType::F64 || rt == MirType::F64 {
                (
                    promote_to_f64(values, insts, lhs, lt)?,
                    promote_to_f64(values, insts, rhs, rt)?,
                )
            } else if lt == MirType::I32 && rt == MirType::I32 {
                (lhs, rhs)
            } else {
                return Err(LowerError::DynamicOperation);
            };
            let id = values.fresh(MirType::I32);
            insts.push((id, Inst::Compare {
                op: map_cmpop(*c),
                lhs,
                rhs,
            }));
            stack.push(StackEntry::Value(id, MirType::I32));
        }
        bc::Op::Unary(u) => {
            let (operand, ty) = pop_value(stack)?;
            if ty == MirType::F64 {
                let id = match u {
                    bc::UnaryOp::Pos => operand,
                    bc::UnaryOp::Neg => {
                        let zero = emit(values, insts, Inst::ConstInt {
                            ty: MirType::F64,
                            value: 0,
                        }, MirType::F64);
                        emit(values, insts, Inst::Binary {
                            op: MBinOp::Sub,
                            lhs: zero,
                            rhs: operand,
                        }, MirType::F64)
                    }
                    bc::UnaryOp::Invert => return Err(LowerError::DynamicOperation),
                };
                stack.push(StackEntry::Value(id, MirType::F64));
                return Ok(());
            }
            if ty != MirType::I32 {
                return Err(LowerError::DynamicOperation);
            }
            let id = match u {
                bc::UnaryOp::Pos => operand,
                bc::UnaryOp::Neg => {
                    let zero = emit(values, insts, Inst::ConstInt {
                        ty: MirType::I32,
                        value: 0,
                    }, MirType::I32);
                    emit(values, insts, Inst::Binary {
                        op: MBinOp::Sub,
                        lhs: zero,
                        rhs: operand,
                    }, MirType::I32)
                }
                bc::UnaryOp::Invert => {
                    let ones = emit(values, insts, Inst::ConstInt {
                        ty: MirType::I32,
                        value: -1,
                    }, MirType::I32);
                    emit(values, insts, Inst::Binary {
                        op: MBinOp::Xor,
                        lhs: operand,
                        rhs: ones,
                    }, MirType::I32)
                }
            };
            stack.push(StackEntry::Value(id, MirType::I32));
        }
        bc::Op::LoadAttr { site } => {
            let [name, cache] = co
                .wide_operands
                .get(*site as usize)
                .ok_or(LowerError::BadOperandSite(*site))?;
            let (obj, _ot) = pop_value(stack)?;
            if matches!(arrays.get(&obj).map(|info| info.kind), Some(SeqKind::GrowList))
                && co.names.get(*name as usize).is_some_and(|n| n == "append")
            {
                stack.push(StackEntry::ListAppend(obj));
                return Ok(());
            }
            if matches!(arrays.get(&obj).map(|info| info.kind), Some(SeqKind::GrowList))
                && co.names.get(*name as usize).is_some_and(|n| n == "pop")
            {
                if !co.exc_table.is_empty() {
                    return Err(LowerError::DynamicOperation);
                }
                stack.push(StackEntry::ListPop(obj));
                return Ok(());
            }
            let id = values.fresh(MirType::PyValue);
            insts.push((id, Inst::PyIntrinsic {
                op: PyOp::Getattr { name: *name },
                args: vec![obj],
                cache: *cache,
            }));
            stack.push(StackEntry::Value(id, MirType::PyValue));
        }
        bc::Op::Subscript { cache } => {
            let (index, it) = pop_value(stack)?;
            let (container, _ct) = pop_value(stack)?;
            if let Some(&info) = arrays.get(&container) {
                if it != MirType::I32 {
                    return Err(LowerError::DynamicOperation);
                }
                let index = normalize_index(values, insts, container, index);
                let index = match info.kind {
                    SeqKind::GrowList => narrow_index_to_len(values, insts, container, index),
                    _ => index,
                };
                let array = backing_array(values, insts, container, info);
                let id = emit(
                    values,
                    insts,
                    Inst::ArrayLoad { array, index, element_size: elem_size(info.elem), signed: false },
                    info.elem,
                );
                stack.push(StackEntry::Value(id, info.elem));
            } else {
                let id = values.fresh(MirType::PyValue);
                insts.push((id, Inst::PyIntrinsic {
                    op: PyOp::Getitem,
                    args: vec![container, index],
                    cache: *cache,
                }));
                stack.push(StackEntry::Value(id, MirType::PyValue));
            }
        }
        bc::Op::BuildSlice => {
            return Err(LowerError::DynamicOperation);
        }
        bc::Op::BuildList(n) => {
            let n = *n as usize;
            if n == 0 {
                stack.push(StackEntry::EmptyList);
                return Ok(());
            }
            let mut elems: Vec<(ValueId, MirType)> = Vec::with_capacity(n);
            for _ in 0..n {
                elems.push(pop_value(stack)?);
            }
            elems.reverse();
            let obj = materialize_array(values, insts, arrays, &elems, elems[0].1, SeqKind::FixedList)?;
            stack.push(StackEntry::Value(obj, MirType::ObjectRef));
        }
        bc::Op::BuildDict(_) => {
            return Err(LowerError::DynamicOperation);
        }
        bc::Op::BuildTuple(n) => {
            let n = *n as usize;
            let base = stack.len().checked_sub(n).ok_or(LowerError::StackUnderflow)?;
            if n > 0 && stack[base..].iter().all(|e| matches!(e, StackEntry::ExcType(_))) {
                let names = stack
                    .split_off(base)
                    .into_iter()
                    .filter_map(|e| match e {
                        StackEntry::ExcType(name) => Some(name),
                        _ => None,
                    })
                    .collect();
                stack.push(StackEntry::ExcTypeTuple(names));
            } else {
                let mut elems = Vec::with_capacity(n);
                for _ in 0..n {
                    elems.push(pop_value(stack)?);
                }
                elems.reverse();
                stack.push(StackEntry::Tuple(elems));
            }
        }
        bc::Op::UnpackSequence(n) => {
            let n = *n as usize;
            match pop(stack)? {
                StackEntry::Tuple(elems) if elems.len() == n => {
                    for (v, t) in elems.into_iter().rev() {
                        stack.push(StackEntry::Value(v, t));
                    }
                }
                _ => return Err(LowerError::DynamicOperation),
            }
        }
        bc::Op::GetIter | bc::Op::ForIter(_) => {
            return Err(LowerError::DynamicOperation);
        }
        bc::Op::Setitem => {
            let (index, it) = pop_value(stack)?;
            let (container, _ct) = pop_value(stack)?;
            if let Some(&info) = arrays.get(&container) {
                if !info.kind.mutable() {
                    return Err(LowerError::DynamicOperation);
                }
                let (value, vt) = pop_value(stack)?;
                if it != MirType::I32 || vt != info.elem {
                    return Err(LowerError::DynamicOperation);
                }
                let index = normalize_index(values, insts, container, index);
                let index = match info.kind {
                    SeqKind::GrowList => narrow_index_to_len(values, insts, container, index),
                    _ => index,
                };
                let array = backing_array(values, insts, container, info);
                emit(
                    values,
                    insts,
                    Inst::ArrayStore { array, index, value, element_size: elem_size(info.elem) },
                    MirType::I32,
                );
            } else {
                return Err(LowerError::DynamicOperation);
            }
        }
        bc::Op::Contains { .. } => {
            return Err(LowerError::DynamicOperation);
        }
        bc::Op::MatchExc => {
            let tags: Vec<u32> = match pop(stack)? {
                StackEntry::ExcType(name) => crate::exc::subtype_tags(&name),
                StackEntry::ExcTypeTuple(names) => {
                    let mut union: Vec<u32> = Vec::new();
                    for name in &names {
                        for tag in crate::exc::subtype_tags(name) {
                            if !union.contains(&tag) {
                                union.push(tag);
                            }
                        }
                    }
                    union
                }
                _ => return Err(LowerError::DynamicOperation),
            };
            let loaded = emit(
                values,
                insts,
                Inst::StaticLoad {
                    owner: StaticOwner::Own,
                    offset: EXCEPTION_TAG_OFFSET,
                },
                MirType::I32,
            );
            let mut cond: Option<ValueId> = None;
            for tag in tags {
                let expected = emit(
                    values,
                    insts,
                    Inst::ConstInt {
                        ty: MirType::I32,
                        value: i64::from(tag),
                    },
                    MirType::I32,
                );
                let matched = emit(
                    values,
                    insts,
                    Inst::Compare {
                        op: MCmpOp::Eq,
                        lhs: loaded,
                        rhs: expected,
                    },
                    MirType::I32,
                );
                cond = Some(match cond {
                    None => matched,
                    Some(prev) => emit(
                        values,
                        insts,
                        Inst::Binary {
                            op: MBinOp::Or,
                            lhs: prev,
                            rhs: matched,
                        },
                        MirType::I32,
                    ),
                });
            }
            stack.push(StackEntry::Value(cond.unwrap_or(loaded), MirType::I32));
        }
        bc::Op::PopExcept => {
            let zero = emit(
                values,
                insts,
                Inst::ConstInt {
                    ty: MirType::I32,
                    value: 0,
                },
                MirType::I32,
            );
            let _cleared = emit(
                values,
                insts,
                Inst::StaticStore {
                    owner: StaticOwner::Own,
                    offset: EXCEPTION_TAG_OFFSET,
                    value: zero,
                },
                MirType::I32,
            );
        }
        bc::Op::Raise(_) | bc::Op::Reraise => {
            return Err(LowerError::UnsupportedControlFlow);
        }
        bc::Op::LoadExc
        | bc::Op::DeleteItem
        | bc::Op::DeleteAttr { .. }
        | bc::Op::DeleteFast(_) => {
            return Err(LowerError::DynamicOperation);
        }
        bc::Op::ListGrow { .. } => {}
        bc::Op::MakeFunction { .. }
        | bc::Op::Yield
        | bc::Op::YieldFrom
        | bc::Op::Await
        | bc::Op::CallEx { .. }
        | bc::Op::BuildClass
        | bc::Op::BuildClassKw { .. }
        | bc::Op::SetAttr { .. }
        | bc::Op::ListAppend
        | bc::Op::SetAdd
        | bc::Op::DictInsert
        | bc::Op::LoadSuper(_)
        | bc::Op::BuildSet(_)
        | bc::Op::UnpackEx { .. }
        | bc::Op::LoadDeref(_)
        | bc::Op::StoreDeref(_)
        | bc::Op::LoadClosure(_)
        | bc::Op::SetupClassNamespace
        | bc::Op::StoreName(_)
        | bc::Op::LoadName(_)
        | bc::Op::ImportName(_)
        | bc::Op::ImportFrom(_)
        | bc::Op::ImportStar
        | bc::Op::StoreGlobal(_) => {
            return Err(LowerError::DynamicOperation);
        }
        bc::Op::PopTop => {
            pop(stack)?;
        }
        bc::Op::LoadGlobal(name_idx) => {
            let name = co
                .names
                .get(*name_idx as usize)
                .ok_or(LowerError::BadNameIndex(*name_idx))?;
            if let Some(sig) = funcs.get(name).cloned() {
                stack.push(StackEntry::Callable(sig));
            } else if let Some(builtin) = Builtin::from_name(name) {
                stack.push(StackEntry::Builtin(builtin));
            } else if let Some(&value) = constants.get(name) {
                let id = emit(
                    values,
                    insts,
                    Inst::ConstInt {
                        ty: MirType::I32,
                        value: i64::from(value),
                    },
                    MirType::I32,
                );
                stack.push(StackEntry::Value(id, MirType::I32));
            } else if crate::exc::is_builtin_exception(name) {
                stack.push(StackEntry::ExcType(String::from(name)));
            } else {
                return Err(LowerError::UnresolvedGlobal(String::from(name)));
            }
        }
        bc::Op::Call(argc) => {
            let argc = *argc as usize;
            if stack.len() > argc {
                let callee_at = stack.len() - argc - 1;
                if matches!(stack[callee_at], StackEntry::ExcType(_))
                    && stack[callee_at + 1..]
                        .iter()
                        .all(|e| matches!(e, StackEntry::ConstStr))
                {
                    stack.truncate(callee_at + 1);
                    return Ok(());
                }
            }
            let mut typed_args = Vec::with_capacity(argc);
            for _ in 0..argc {
                typed_args.push(pop_value(stack)?);
            }
            typed_args.reverse();
            match pop(stack)? {
                StackEntry::Callable(sig) => {
                    if argc != sig.arity {
                        return Err(LowerError::ArityMismatch {
                            expected: sig.arity,
                            found: argc,
                        });
                    }
                    let mut args = Vec::with_capacity(argc);
                    for ((value, ty), &pty) in typed_args.into_iter().zip(&sig.param_types) {
                        if ty != mir_type(pty) || !sequence_matches(arrays.get(&value), pty) {
                            return Err(LowerError::DynamicOperation);
                        }
                        args.push(value);
                    }
                    let id = values.fresh(sig.ret);
                    insts.push((id, Inst::Call {
                        callee: sig.index,
                        args,
                    }));
                    stack.push(StackEntry::Value(id, sig.ret));
                }
                StackEntry::Builtin(Builtin::Divmod) => {
                    if argc != 2 {
                        return Err(LowerError::ArityMismatch {
                            expected: 2,
                            found: argc,
                        });
                    }
                    let (lhs, lt) = typed_args[0];
                    let (rhs, rt) = typed_args[1];
                    if lt != MirType::I32 || rt != MirType::I32 {
                        return Err(LowerError::DynamicOperation);
                    }
                    let trunc_q = emit(values, insts, Inst::Binary {
                        op: MBinOp::DivSigned,
                        lhs,
                        rhs,
                    }, MirType::I32);
                    let trunc_r = emit(values, insts, Inst::Binary {
                        op: MBinOp::RemSigned,
                        lhs,
                        rhs,
                    }, MirType::I32);
                    let adjust = floor_adjust(values, insts, trunc_r, lhs, rhs);
                    let q = emit(values, insts, Inst::Binary {
                        op: MBinOp::Sub,
                        lhs: trunc_q,
                        rhs: adjust,
                    }, MirType::I32);
                    let adjust_b = emit(values, insts, Inst::Binary {
                        op: MBinOp::Mul,
                        lhs: adjust,
                        rhs,
                    }, MirType::I32);
                    let r = emit(values, insts, Inst::Binary {
                        op: MBinOp::Add,
                        lhs: trunc_r,
                        rhs: adjust_b,
                    }, MirType::I32);
                    stack.push(StackEntry::Tuple(vec![(q, MirType::I32), (r, MirType::I32)]));
                }
                StackEntry::Builtin(Builtin::Abs) => {
                    if argc != 1 {
                        return Err(LowerError::ArityMismatch {
                            expected: 1,
                            found: argc,
                        });
                    }
                    let (value, ty) = typed_args[0];
                    match ty {
                        MirType::I32 => {
                            let (id, rty) = inline_builtin(Builtin::Abs, values, insts, &[value])?;
                            stack.push(StackEntry::Value(id, rty));
                        }
                        MirType::F64 => {
                            let id = emit(values, insts, Inst::PInvoke {
                                import: "lamella_fabs".into(),
                                args: vec![value],
                            }, MirType::F64);
                            stack.push(StackEntry::Value(id, MirType::F64));
                        }
                        _ => return Err(LowerError::DynamicOperation),
                    }
                }
                StackEntry::Builtin(Builtin::Len) => {
                    if argc != 1 {
                        return Err(LowerError::ArityMismatch {
                            expected: 1,
                            found: argc,
                        });
                    }
                    let (obj, _ty) = typed_args[0];
                    if !arrays.contains_key(&obj) {
                        return Err(LowerError::DynamicOperation);
                    }
                    let id = emit(values, insts, Inst::FieldLoad { base: obj, offset: 0 }, MirType::I32);
                    stack.push(StackEntry::Value(id, MirType::I32));
                }
                StackEntry::Builtin(Builtin::Round) => {
                    if argc != 1 {
                        return Err(LowerError::DynamicOperation);
                    }
                    let (value, ty) = typed_args[0];
                    let id = match ty {
                        MirType::I32 => value,
                        MirType::F64 => {
                            let rounded = emit(values, insts, Inst::PInvoke {
                                import: "lamella_rint".into(),
                                args: vec![value],
                            }, MirType::F64);
                            emit(values, insts, Inst::Convert {
                                value: rounded,
                                kind: ConvKind::Float64ToInt,
                            }, MirType::I32)
                        }
                        _ => return Err(LowerError::DynamicOperation),
                    };
                    stack.push(StackEntry::Value(id, MirType::I32));
                }
                StackEntry::Builtin(mm @ (Builtin::Min | Builtin::Max)) => {
                    if argc != 2 {
                        return Err(LowerError::ArityMismatch {
                            expected: 2,
                            found: argc,
                        });
                    }
                    let (a, at) = typed_args[0];
                    let (b, bt) = typed_args[1];
                    match (at, bt) {
                        (MirType::I32, MirType::I32) => {
                            let (id, rty) = inline_builtin(mm, values, insts, &[a, b])?;
                            stack.push(StackEntry::Value(id, rty));
                        }
                        (MirType::F64, MirType::F64) => {
                            let import = if matches!(mm, Builtin::Min) {
                                "lamella_fmin"
                            } else {
                                "lamella_fmax"
                            };
                            let id = emit(values, insts, Inst::PInvoke {
                                import: import.into(),
                                args: vec![a, b],
                            }, MirType::F64);
                            stack.push(StackEntry::Value(id, MirType::F64));
                        }
                        _ => return Err(LowerError::DynamicOperation),
                    }
                }
                StackEntry::Builtin(cast @ (Builtin::IntCast | Builtin::FloatCast)) => {
                    if argc != 1 {
                        return Err(LowerError::ArityMismatch {
                            expected: 1,
                            found: argc,
                        });
                    }
                    let (value, ty) = typed_args[0];
                    let (id, rty) = match (cast, ty) {
                        (Builtin::IntCast, MirType::I32) | (Builtin::FloatCast, MirType::F64) => {
                            (value, ty)
                        }
                        (Builtin::IntCast, MirType::F64) => (
                            emit(values, insts, Inst::Convert {
                                value,
                                kind: ConvKind::Float64ToInt,
                            }, MirType::I32),
                            MirType::I32,
                        ),
                        (Builtin::FloatCast, MirType::I32) => (
                            emit(values, insts, Inst::Convert {
                                value,
                                kind: ConvKind::IntToFloat64,
                            }, MirType::F64),
                            MirType::F64,
                        ),
                        _ => return Err(LowerError::DynamicOperation),
                    };
                    stack.push(StackEntry::Value(id, rty));
                }
                StackEntry::Builtin(Builtin::BoolCast) => {
                    if argc != 1 {
                        return Err(LowerError::ArityMismatch {
                            expected: 1,
                            found: argc,
                        });
                    }
                    let (value, ty) = typed_args[0];
                    let zero_ty = match ty {
                        MirType::I32 | MirType::F64 => ty,
                        _ => return Err(LowerError::DynamicOperation),
                    };
                    let zero = emit(values, insts, Inst::ConstInt {
                        ty: zero_ty,
                        value: 0,
                    }, zero_ty);
                    let id = emit(values, insts, Inst::Compare {
                        op: MCmpOp::Ne,
                        lhs: value,
                        rhs: zero,
                    }, MirType::I32);
                    stack.push(StackEntry::Value(id, MirType::I32));
                }
                StackEntry::Builtin(builtin) => {
                    let mut args = Vec::with_capacity(argc);
                    for (value, ty) in typed_args {
                        if ty != MirType::I32 {
                            return Err(LowerError::DynamicOperation);
                        }
                        args.push(value);
                    }
                    let (id, ty) = inline_builtin(builtin, values, insts, &args)?;
                    stack.push(StackEntry::Value(id, ty));
                }
                StackEntry::ListPop(header) => {
                    if argc != 0 {
                        return Err(LowerError::DynamicOperation);
                    }
                    let Some(&info) = arrays.get(&header) else {
                        return Err(LowerError::DynamicOperation);
                    };
                    let element_size = elem_size(info.elem);
                    let len = emit(values, insts, Inst::FieldLoad { base: header, offset: GROWLIST_LEN_OFFSET }, MirType::I32);
                    let one = emit(values, insts, Inst::ConstInt { ty: MirType::I32, value: 1 }, MirType::I32);
                    let idx = emit(values, insts, Inst::Binary { op: MBinOp::Sub, lhs: len, rhs: one }, MirType::I32);
                    let empty = emit(values, insts, Inst::Compare { op: MCmpOp::UnsignedGe, lhs: idx, rhs: len }, MirType::I32);
                    let zero = emit(values, insts, Inst::ConstInt { ty: MirType::I32, value: 0 }, MirType::I32);
                    let mask = emit(values, insts, Inst::Binary { op: MBinOp::Sub, lhs: zero, rhs: empty }, MirType::I32);
                    let narrowed = emit(values, insts, Inst::Binary { op: MBinOp::Or, lhs: idx, rhs: mask }, MirType::I32);
                    let backing = emit(values, insts, Inst::FieldLoad { base: header, offset: GROWLIST_BACKING_OFFSET }, MirType::ObjectRef);
                    let value = emit(
                        values,
                        insts,
                        Inst::ArrayLoad { array: backing, index: narrowed, element_size, signed: false },
                        info.elem,
                    );
                    emit(
                        values,
                        insts,
                        Inst::FieldStore { base: header, offset: GROWLIST_LEN_OFFSET, value: idx },
                        MirType::I32,
                    );
                    stack.push(StackEntry::Value(value, info.elem));
                }
                StackEntry::ListAppend(header) => {
                    if argc != 1 {
                        return Err(LowerError::ArityMismatch { expected: 1, found: argc });
                    }
                    let Some(&info) = arrays.get(&header) else {
                        return Err(LowerError::DynamicOperation);
                    };
                    let (value, vt) = typed_args[0];
                    if vt != info.elem {
                        return Err(LowerError::DynamicOperation);
                    }
                    let element_size = elem_size(info.elem);
                    let len = emit(values, insts, Inst::FieldLoad { base: header, offset: GROWLIST_LEN_OFFSET }, MirType::I32);
                    let one = emit(values, insts, Inst::ConstInt { ty: MirType::I32, value: 1 }, MirType::I32);
                    let needed = emit(values, insts, Inst::Binary { op: MBinOp::Add, lhs: len, rhs: one }, MirType::I32);
                    if !*grown {
                        let width = emit(values, insts, Inst::ConstInt { ty: MirType::I32, value: i64::from(element_size) }, MirType::I32);
                        emit(
                            values,
                            insts,
                            Inst::PInvoke { import: PY_LIST_GROW_SYMBOL.into(), args: vec![header, needed, width] },
                            MirType::I32,
                        );
                    }
                    *grown = false;
                    let backing = emit(values, insts, Inst::FieldLoad { base: header, offset: GROWLIST_BACKING_OFFSET }, MirType::ObjectRef);
                    emit(values, insts, Inst::ArrayStore { array: backing, index: len, value, element_size }, MirType::I32);
                    emit(
                        values,
                        insts,
                        Inst::FieldStore { base: header, offset: GROWLIST_LEN_OFFSET, value: needed },
                        MirType::I32,
                    );
                    let none = emit(values, insts, Inst::ConstInt { ty: MirType::I32, value: 0 }, MirType::I32);
                    stack.push(StackEntry::Value(none, MirType::I32));
                }
                StackEntry::Value(..) | StackEntry::Tuple(_) | StackEntry::EmptyList => {
                    return Err(LowerError::CallTargetNotCallable);
                }
                StackEntry::ExcType(_) | StackEntry::ExcTypeTuple(_) | StackEntry::ConstStr => {
                    return Err(LowerError::DynamicOperation);
                }
            }
        }
        bc::Op::CallKw { site } => {
            let [argc, kwnames] = co
                .wide_operands
                .get(*site as usize)
                .ok_or(LowerError::BadOperandSite(*site))?;
            let argc = *argc as usize;
            let names: &[String] = match co.consts.get(*kwnames as usize) {
                Some(bc::Const::KwNames(names)) => names,
                _ => return Err(LowerError::BadConstIndex(*kwnames)),
            };
            let k = names.len();
            let mut kw_values = Vec::with_capacity(k);
            for _ in 0..k {
                let (value, ty) = pop_value(stack)?;
                if ty != MirType::I32 {
                    return Err(LowerError::DynamicOperation);
                }
                kw_values.push(value);
            }
            kw_values.reverse();
            let mut pos_values = Vec::with_capacity(argc);
            for _ in 0..argc {
                let (value, ty) = pop_value(stack)?;
                if ty != MirType::I32 {
                    return Err(LowerError::DynamicOperation);
                }
                pos_values.push(value);
            }
            pos_values.reverse();
            let sig = match pop(stack)? {
                StackEntry::Callable(sig) => sig,
                StackEntry::Builtin(_)
                | StackEntry::Value(..)
                | StackEntry::Tuple(_)
                | StackEntry::EmptyList
                | StackEntry::ListAppend(_)
                | StackEntry::ListPop(_) => {
                    return Err(LowerError::CallTargetNotCallable);
                }
                StackEntry::ExcType(_) | StackEntry::ExcTypeTuple(_) | StackEntry::ConstStr => {
                    return Err(LowerError::DynamicOperation);
                }
            };
            if argc + k != sig.arity {
                return Err(LowerError::ArityMismatch {
                    expected: sig.arity,
                    found: argc + k,
                });
            }
            let mut bound: Vec<Option<ValueId>> = vec![None; sig.arity];
            for (i, value) in pos_values.into_iter().enumerate() {
                bound[i] = Some(value);
            }
            for (name, value) in names.iter().zip(kw_values) {
                let slot = sig
                    .param_names
                    .iter()
                    .position(|p| p == name)
                    .ok_or_else(|| LowerError::UnexpectedKeyword(name.clone()))?;
                if bound[slot].is_some() {
                    return Err(LowerError::DuplicateArgument(name.clone()));
                }
                bound[slot] = Some(value);
            }
            let args: Vec<ValueId> =
                bound.into_iter().map(|b| b.expect("every param bound")).collect();
            let id = values.fresh(sig.ret);
            insts.push((id, Inst::Call {
                callee: sig.index,
                args,
            }));
            stack.push(StackEntry::Value(id, sig.ret));
        }
        bc::Op::Jump(_) | bc::Op::PopJumpIfFalse(_) | bc::Op::Return => {
            return Err(LowerError::StackNotEmpty);
        }
    }
    Ok(())
}

/// The module-level integer constants a typed function may reference by name: a `NAME = <int
/// literal>` in the module body's LEADING straight-line prefix (so it is unconditionally executed
/// -- always bound, matching the interpreter), assigned exactly once (never reassigned), and
/// fitting a 32-bit machine word. Anything else (computed, reassigned, conditional, or out of
/// range) stays an unresolved global in the typed lane -- conservative, never a wrong or divergent
/// value. Lets a typed AOT driver name a register address (`GPIO = 0x5000_0000`).
fn module_constants(body: &bc::CodeObject) -> BTreeMap<String, i32> {
    let mut store_count = vec![0u32; body.n_locals];
    for op in &body.ops {
        if let bc::Op::StoreFast(s) = op {
            if let Some(c) = store_count.get_mut(*s as usize) {
                *c += 1;
            }
        }
    }
    let mut constants = BTreeMap::new();
    for (i, pair) in body.ops.windows(2).enumerate() {
        if matches!(
            body.ops[i],
            bc::Op::Jump(_) | bc::Op::PopJumpIfFalse(_) | bc::Op::ForIter(_) | bc::Op::Return
        ) {
            break;
        }
        if let [bc::Op::LoadConst(k), bc::Op::StoreFast(s)] = pair {
            let slot = *s as usize;
            if store_count.get(slot).copied() != Some(1) {
                continue;
            }
            let Some(bc::Const::Int(v)) = body.consts.get(*k as usize) else {
                continue;
            };
            let bits = match i32::try_from(*v) {
                Ok(n) => n,
                Err(_) => match u32::try_from(*v) {
                    Ok(u) => u as i32,
                    Err(_) => continue,
                },
            };
            if let Some(name) = body.local_names.get(slot) {
                constants.insert(String::from(name), bits);
            }
        }
    }
    constants
}

/// Lower every function of a compiled module, returning each `(name, Function)`.
/// The `<module>` body is not lowered in the typed path: the parity harness drives
/// the call boundary.
///
/// **`Ok` IS NOT "THIS PROGRAM REACHES THE TYPED LANE".** A module with no `def` at all -- an
/// ordinary dynamic script -- has no function to lower and returns `Ok(vec![])`, which is a truthful
/// answer to the question this asks and a badly misleading one to the question a caller usually
/// means. Measured on the 443-row corpus: **51 rows lower "successfully" with ZERO functions**, and
/// a first pass at counting AOT coverage read every one of them as covered.
///
/// **The AOT lane's real entry condition is a lowered function named `main`**, which is what
/// `--example diff-run` requires and what `--example lower-probe` reports separately from the count.
/// A caller asking "can this be AOT-compiled" must check what came back, not merely that something
/// did.
pub fn lower_module(module: &bc::Module) -> Result<Vec<(String, Function)>, LowerError> {
    let funcs: BTreeMap<String, FuncSig> = module
        .functions
        .iter_bodies()
        .enumerate()
        .map(|(i, co)| {
            (co.name.clone(), FuncSig {
                index: i as u32,
                ret: mir_type(co.ret_ty),
                arity: co.params.len(),
                param_names: co.params.iter().map(|p| p.name.clone()).collect(),
                param_types: co.params.iter().map(|p| p.ty).collect(),
            })
        })
        .collect();
    let constants = module_constants(&module.body);
    module
        .functions
        .iter_bodies()
        .map(|co| Ok((co.name.clone(), lower_function(co, &funcs, &constants)?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile_str;
    use alloc::format;

    fn lower_named(source: &str, name: &str) -> Function {
        let module = compile_str("test", source).expect("compiles");
        let lowered = lower_module(&module).expect("lowers");
        let func = lowered
            .into_iter()
            .find(|(n, _)| n == name)
            .expect("function present")
            .1;
        assert_eq!(
            lamella_ir::verify(&func),
            Ok(()),
            "lowered function must verify: {}",
            describe(&func)
        );
        func
    }

    /// Lower a whole module, returning every `(name, Function)` for multi-function and
    /// call-resolution tests.
    fn lower_all(source: &str) -> Vec<(String, Function)> {
        let module = compile_str("test", source).expect("compiles");
        let lowered = lower_module(&module).expect("lowers");
        for (name, func) in &lowered {
            assert_eq!(
                lamella_ir::verify(func),
                Ok(()),
                "function `{name}` must verify: {}",
                describe(func)
            );
        }
        lowered
    }

    fn describe(func: &Function) -> String {
        format!(
            "{} blocks, {} values",
            func.blocks.len(),
            func.value_types.len()
        )
    }

    fn count_insts(func: &Function, pred: impl Fn(&Inst) -> bool) -> usize {
        func.blocks
            .iter()
            .flat_map(|b| b.insts.iter())
            .filter(|(_, inst)| pred(inst))
            .count()
    }

    const FIB: &str = "\
def fib(n: int) -> int:
    a: int = 0
    b: int = 1
    i: int = 0
    while i < n:
        t: int = a + b
        a = b
        b = t
        i = i + 1
    return a
";

    #[test]
    fn typed_fib_lowers_and_verifies() {
        let func = lower_named(FIB, "fib");
        assert_eq!(func.params, vec![MirType::I32]);
        assert_eq!(func.ret, Some(MirType::I32));
        let has_branch = func
            .blocks
            .iter()
            .any(|b| matches!(b.terminator, Some(Terminator::Branch { .. })));
        let has_jump = func
            .blocks
            .iter()
            .any(|b| matches!(b.terminator, Some(Terminator::Jump { .. })));
        assert!(has_branch && has_jump);
        assert!(count_insts(&func, |i| matches!(i, Inst::Binary { .. })) >= 2);
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::Compare { .. })), 1);
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    #[test]
    fn typed_float_arithmetic_lowers_and_verifies() {
        let func = lower_named(
            "\
def poly(x: float, y: float) -> float:
    return x * x + 2.0 * y - 1.0
",
            "poly",
        );
        assert_eq!(func.params, vec![MirType::F64, MirType::F64]);
        assert_eq!(func.ret, Some(MirType::F64));
        assert!(count_insts(&func, |i| matches!(i, Inst::Binary { .. })) >= 3);
        assert!(
            count_insts(&func, |i| matches!(i, Inst::ConstInt {
                ty: MirType::F64,
                ..
            })) >= 2
        );
    }

    #[test]
    fn true_division_promotes_int_operands_to_f64() {
        let func = lower_named(
            "\
def half(n: int) -> float:
    return n / 2
",
            "half",
        );
        assert_eq!(func.ret, Some(MirType::F64));
        assert_eq!(
            count_insts(&func, |i| matches!(i, Inst::Convert {
                kind: ConvKind::IntToFloat64,
                ..
            })),
            2
        );
    }

    #[test]
    fn float_unary_negation_lowers() {
        let func = lower_named(
            "\
def flip(x: float) -> float:
    return -x
",
            "flip",
        );
        assert_eq!(func.ret, Some(MirType::F64));
        assert!(count_insts(&func, |i| matches!(i, Inst::Binary {
            op: MBinOp::Sub,
            ..
        })) >= 1);
    }

    #[test]
    fn floor_division_and_modulo_lower_with_a_sign_correction() {
        let div = lower_named("def f(a: int, b: int) -> int:\n    return a // b\n", "f");
        assert_eq!(
            count_insts(&div, |i| matches!(i, Inst::Binary {
                op: MBinOp::DivSigned,
                ..
            })),
            1
        );
        assert!(count_insts(&div, |i| matches!(i, Inst::Binary {
            op: MBinOp::RemSigned,
            ..
        })) >= 1);
        assert_eq!(count_insts(&div, |i| matches!(i, Inst::Compare { .. })), 2);

        let modulo = lower_named("def g(a: int, b: int) -> int:\n    return a % b\n", "g");
        assert!(count_insts(&modulo, |i| matches!(i, Inst::Binary {
            op: MBinOp::RemSigned,
            ..
        })) >= 1);
        assert_eq!(count_insts(&modulo, |i| matches!(i, Inst::Compare { .. })), 2);
    }

    #[test]
    fn minimal_ssa_keeps_the_value_count_small() {
        let func = lower_named(FIB, "fib");
        assert!(
            func.value_types.len() < 15,
            "fib lowered to {} values, expected a minimal-SSA + liveness reduction",
            func.value_types.len()
        );
        let param_blocks = func.blocks.iter().filter(|b| !b.params.is_empty()).count();
        assert_eq!(param_blocks, 2);
        let max_params = func.blocks.iter().map(|b| b.params.len()).max().unwrap();
        assert_eq!(max_params, 4);
    }

    #[test]
    fn dynamic_getattr_lowers_to_a_py_intrinsic() {
        let func = lower_named("def get_x(obj):\n    return obj.x\n", "get_x");
        assert_eq!(func.params, vec![MirType::PyValue]);
        assert_eq!(func.ret, Some(MirType::PyValue));
        let getattrs = count_insts(&func, |i| {
            matches!(i, Inst::PyIntrinsic { op: PyOp::Getattr { name: _ }, .. })
        });
        assert_eq!(getattrs, 1);
    }

    #[test]
    fn straight_line_typed_function() {
        let func = lower_named("def inc(n: int) -> int:\n    return n + 1\n", "inc");
        assert!(func
            .blocks
            .iter()
            .any(|b| matches!(b.terminator, Some(Terminator::Return(Some(_))))));
        assert!(func
            .blocks
            .iter()
            .any(|b| matches!(b.terminator, Some(Terminator::Unreachable))));
    }

    #[test]
    fn if_else_lowers_and_verifies() {
        let src = "\
def sign(n: int) -> int:
    if n < 0:
        return 0
    else:
        return 1
";
        let func = lower_named(src, "sign");
        assert!(func
            .blocks
            .iter()
            .any(|b| matches!(b.terminator, Some(Terminator::Branch { .. }))));
    }

    #[test]
    fn loop_first_with_a_body_local_verifies() {
        let func = lower_named(
            "def f(n: int) -> int:\n    while n > 0:\n        x: int = n\n        n = n - x\n    return n\n",
            "f",
        );
        assert!(func
            .blocks
            .iter()
            .any(|b| matches!(b.terminator, Some(Terminator::Branch { .. }))));
    }

    #[test]
    fn a_call_to_a_helper_lowers_to_inst_call() {
        let funcs = lower_all(
            "def inc(x: int) -> int:\n    return x + 1\n\
             def main() -> int:\n    return inc(41)\n",
        );
        let main = funcs.iter().find(|(n, _)| n == "main").unwrap().1.clone();
        let inc_index = funcs.iter().position(|(n, _)| n == "inc").unwrap() as u32;
        let calls: Vec<(u32, usize)> = main
            .blocks
            .iter()
            .flat_map(|b| b.insts.iter())
            .filter_map(|(_, i)| match i {
                Inst::Call { callee, args } => Some((*callee, args.len())),
                _ => None,
            })
            .collect();
        assert_eq!(calls, vec![(inc_index, 1)]);
    }

    #[test]
    fn direct_recursion_resolves_to_the_callee_itself() {
        let func = lower_named(
            "def fib(n: int) -> int:\n    if n < 2:\n        return n\n    return fib(n - 1) + fib(n - 2)\n",
            "fib",
        );
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::Call { .. })), 2);
    }

    #[test]
    fn nested_calls_resolve_inside_out() {
        let func = lower_named(
            "def g(x: int) -> int:\n    return x * 2\n\
             def f(x: int) -> int:\n    return x + 1\n\
             def main(n: int) -> int:\n    return f(g(n))\n",
            "main",
        );
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::Call { .. })), 2);
    }

    #[test]
    fn a_multi_argument_call_carries_every_argument() {
        let func = lower_named(
            "def add3(a: int, b: int, c: int) -> int:\n    return a + b + c\n\
             def main() -> int:\n    return add3(1, 2, 3)\n",
            "main",
        );
        let argc = func
            .blocks
            .iter()
            .flat_map(|b| b.insts.iter())
            .find_map(|(_, i)| match i {
                Inst::Call { args, .. } => Some(args.len()),
                _ => None,
            });
        assert_eq!(argc, Some(3));
    }

    #[test]
    fn an_arity_mismatch_is_rejected() {
        let module = compile_str(
            "test",
            "def one(x: int) -> int:\n    return x\n\
             def main() -> int:\n    return one(1, 2)\n",
        )
        .expect("compiles");
        assert_eq!(
            lower_module(&module),
            Err(LowerError::ArityMismatch {
                expected: 1,
                found: 2,
            })
        );
    }

    #[test]
    fn an_unknown_global_is_rejected() {
        let module = compile_str("test", "def main() -> int:\n    return nope(1)\n")
            .expect("compiles");
        assert!(matches!(
            lower_module(&module),
            Err(LowerError::UnresolvedGlobal(_))
        ));
    }

    #[test]
    fn a_function_used_as_a_value_is_rejected() {
        let module = compile_str(
            "test",
            "def inc(x: int) -> int:\n    return x\n\
             def main() -> int:\n    return inc + 1\n",
        )
        .expect("compiles");
        assert_eq!(lower_module(&module), Err(LowerError::CallableAsValue));
    }

    #[test]
    fn bitwise_operators_lower_to_their_mir_ops() {
        for (src_op, mir_op) in [
            ("&", MBinOp::And),
            ("|", MBinOp::Or),
            ("^", MBinOp::Xor),
            ("<<", MBinOp::Shl),
            (">>", MBinOp::ShrSigned),
        ] {
            let src = format!("def f(a: int, b: int) -> int:\n    return a {src_op} b\n");
            let func = lower_named(&src, "f");
            assert_eq!(
                count_insts(&func, |i| matches!(i, Inst::Binary { op, .. } if *op == mir_op)),
                1,
                "operator `{src_op}` should lower to one {mir_op:?}"
            );
        }
    }

    #[test]
    fn bitwise_precedence_follows_python() {
        let module =
            compile_str("test", "def f() -> int:\n    return 1 | 2 & 3\n").expect("compiles");
        let binops: Vec<bc::BinOp> = module.functions[0]
            .ops
            .iter()
            .filter_map(|o| match o {
                bc::Op::Binary(b) => Some(*b),
                _ => None,
            })
            .collect();
        assert_eq!(binops, vec![bc::BinOp::BitAnd, bc::BinOp::BitOr]);
    }

    #[test]
    fn unary_operators_lower_for_typed_ints() {
        let neg = lower_named("def f(x: int) -> int:\n    return -x\n", "f");
        assert_eq!(
            count_insts(&neg, |i| matches!(i, Inst::Binary {
                op: MBinOp::Sub,
                ..
            })),
            1
        );
        let inv = lower_named("def f(x: int) -> int:\n    return ~x\n", "f");
        assert_eq!(
            count_insts(&inv, |i| matches!(i, Inst::Binary {
                op: MBinOp::Xor,
                ..
            })),
            1
        );
        let pos = lower_named("def f(x: int) -> int:\n    return +x\n", "f");
        assert_eq!(count_insts(&pos, |i| matches!(i, Inst::Binary { .. })), 0);
    }

    #[test]
    fn unary_on_a_literal_folds_but_on_a_variable_emits_an_op() {
        let var = compile_str("test", "def f(x: int) -> int:\n    return ~x\n").expect("compiles");
        assert!(var.functions[0]
            .ops
            .iter()
            .any(|o| matches!(o, bc::Op::Unary(bc::UnaryOp::Invert))));
        let lit = compile_str("test", "def g() -> int:\n    return ~3\n").expect("compiles");
        assert!(lit.functions[0].consts.contains(&bc::Const::Int(!3)));
        assert!(!lit.functions[0]
            .ops
            .iter()
            .any(|o| matches!(o, bc::Op::Unary(_))));
    }

    #[test]
    fn a_nested_boolean_threads_the_stack_and_verifies() {
        let func = lower_named(
            "def f(a: int, b: int) -> int:\n    return 10 + (a and b)\n",
            "f",
        );
        assert!(func
            .blocks
            .iter()
            .any(|b| matches!(b.terminator, Some(Terminator::Branch { .. }))));
    }

    #[test]
    fn builtins_inline_without_a_call() {
        let abs = lower_named("def f(x: int) -> int:\n    return abs(x)\n", "f");
        assert_eq!(count_insts(&abs, |i| matches!(i, Inst::Call { .. })), 0);
        assert!(count_insts(&abs, |i| matches!(i, Inst::Binary {
            op: MBinOp::ShrSigned,
            ..
        })) >= 1);
        let mx = lower_named("def f(a: int, b: int) -> int:\n    return max(a, b)\n", "f");
        assert_eq!(count_insts(&mx, |i| matches!(i, Inst::Call { .. })), 0);
    }

    #[test]
    fn subscript_lowers_to_a_getitem_intrinsic() {
        let func = lower_named("def f(s, i):\n    return s[i]\n", "f");
        assert_eq!(
            count_insts(&func, |i| matches!(
                i,
                Inst::PyIntrinsic {
                    op: PyOp::Getitem,
                    ..
                }
            )),
            1
        );
    }

    #[test]
    fn a_builtin_arity_mismatch_is_rejected() {
        let module = compile_str("test", "def f(x: int) -> int:\n    return abs(x, x)\n")
            .expect("compiles");
        assert!(matches!(
            lower_module(&module),
            Err(LowerError::ArityMismatch { .. })
        ));
    }

    #[test]
    fn mmio_write_and_read_lower_to_volatile_store_and_load() {
        let func = lower_named(
            "def poke(addr: int, val: int) -> int:\n    mmio_write32(addr, val)\n    return mmio_read32(addr)\n",
            "poke",
        );
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::Store { width: 4, .. })), 1);
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::Load { width: 4, .. })), 1);
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::Call { .. })), 0);
    }

    #[test]
    fn mmio_widths_map_to_store_and_load_byte_widths() {
        for (bytes, read, write) in [
            (1u32, "mmio_read8", "mmio_write8"),
            (2, "mmio_read16", "mmio_write16"),
            (4, "mmio_read32", "mmio_write32"),
        ] {
            let src =
                format!("def f(a: int, v: int) -> int:\n    {write}(a, v)\n    return {read}(a)\n");
            let func = lower_named(&src, "f");
            assert_eq!(
                count_insts(&func, |i| matches!(i, Inst::Store { width, .. } if *width == bytes)),
                1,
                "{write} should store {bytes} byte(s)"
            );
            assert_eq!(
                count_insts(&func, |i| matches!(i, Inst::Load { width, .. } if *width == bytes)),
                1,
                "{read} should load {bytes} byte(s)"
            );
        }
    }

    #[test]
    fn an_mmio_read_result_is_a_usable_typed_int() {
        let func = lower_named(
            "def f(a: int) -> int:\n    x = mmio_read32(a)\n    return x + 1\n",
            "f",
        );
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::Load { width: 4, .. })), 1);
        assert_eq!(
            count_insts(&func, |i| matches!(i, Inst::Binary {
                op: MBinOp::Add,
                ..
            })),
            1
        );
    }

    #[test]
    fn an_mmio_write_result_cannot_be_used_as_an_int() {
        let module = compile_str(
            "test",
            "def f(a: int) -> int:\n    return mmio_write32(a, 1) + 1\n",
        )
        .expect("compiles");
        assert_eq!(lower_module(&module), Err(LowerError::DynamicOperation));
    }

    #[test]
    fn a_u32_peripheral_address_lowers_as_a_32_bit_word() {
        let direct = lower_named(
            "def read_systick() -> int:\n    return mmio_read32(0xE000E010)\n",
            "read_systick",
        );
        assert_eq!(count_insts(&direct, |i| matches!(i, Inst::Load { width: 4, .. })), 1);
        let want = 0xE000_E010_u32 as i32 as i64;
        assert_eq!(
            count_insts(&direct, |i| matches!(i, Inst::ConstInt { value, .. } if *value == want)),
            1
        );

        let via_local = lower_named(
            "def enable() -> int:\n    ctrl = 0xE000E010\n    mmio_write32(ctrl, 0xDEADBEEF)\n    return 0\n",
            "enable",
        );
        assert_eq!(count_insts(&via_local, |i| matches!(i, Inst::Store { width: 4, .. })), 1);
    }

    #[test]
    fn an_int_literal_beyond_32_bits_is_still_rejected() {
        let module =
            compile_str("test", "def f() -> int:\n    return 4294967296\n").expect("compiles");
        assert_eq!(
            lower_module(&module),
            Err(LowerError::IntLiteralTooLarge(4294967296))
        );
    }

    #[test]
    fn a_keyword_call_static_binds_to_parameter_order() {
        let funcs = lower_all(
            "def sub(a: int, b: int) -> int:\n    return a - b\n\
             def main() -> int:\n    return sub(b=3, a=10)\n",
        );
        let main = funcs.iter().find(|(n, _)| n == "main").unwrap().1.clone();
        let sub_index = funcs.iter().position(|(n, _)| n == "sub").unwrap() as u32;
        let const_of = |id: ValueId| -> Option<i64> {
            main.blocks
                .iter()
                .flat_map(|b| &b.insts)
                .find_map(|(vid, inst)| match inst {
                    Inst::ConstInt { value, .. } if *vid == id => Some(*value),
                    _ => None,
                })
        };
        let args = main
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .find_map(|(_, i)| match i {
                Inst::Call { callee, args } if *callee == sub_index => Some(args.clone()),
                _ => None,
            })
            .expect("a Call to sub");
        assert_eq!(args.len(), 2);
        assert_eq!(const_of(args[0]), Some(10));
        assert_eq!(const_of(args[1]), Some(3));
    }

    #[test]
    fn a_keyword_call_rejects_unknown_and_duplicate() {
        let unknown = compile_str(
            "test",
            "def f(a: int) -> int:\n    return a\n\
             def main() -> int:\n    return f(z=1)\n",
        )
        .expect("compiles");
        assert_eq!(
            lower_module(&unknown),
            Err(LowerError::UnexpectedKeyword(String::from("z")))
        );

        let dup = compile_str(
            "test",
            "def g(a: int, b: int) -> int:\n    return a\n\
             def main() -> int:\n    return g(1, a=2)\n",
        )
        .expect("compiles");
        assert_eq!(
            lower_module(&dup),
            Err(LowerError::DuplicateArgument(String::from("a")))
        );
    }

    #[test]
    fn a_module_level_int_constant_is_usable_in_a_typed_function() {
        let funcs = lower_all(
            "SYSTICK = 0xE000E010\n\
             def read() -> int:\n    return mmio_read32(SYSTICK)\n",
        );
        let read = funcs.iter().find(|(n, _)| n == "read").unwrap().1.clone();
        let want = 0xE000_E010_u32 as i32 as i64;
        assert_eq!(
            count_insts(&read, |i| matches!(i, Inst::ConstInt { value, .. } if *value == want)),
            1
        );
        assert_eq!(count_insts(&read, |i| matches!(i, Inst::Load { width: 4, .. })), 1);
    }

    #[test]
    fn a_reassigned_module_name_is_not_a_constant() {
        let module =
            compile_str("test", "x = 5\nx = 6\ndef f() -> int:\n    return x\n").expect("compiles");
        assert!(matches!(
            lower_module(&module),
            Err(LowerError::UnresolvedGlobal(_))
        ));
    }

    #[test]
    fn a_parallel_assignment_lowers_fully_typed_with_no_heap_tuple() {
        let func = lower_named(
            "def f() -> int:\n    a, b = 1, 2\n    a, b = b, a\n    return a * 10 + b\n",
            "f",
        );
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::Call { .. })), 0);
    }

    #[test]
    fn a_swap_is_pure_value_threading() {
        let func = lower_named(
            "def f(a: int, b: int) -> int:\n    a, b = b, a\n    return a - b\n",
            "f",
        );
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::Binary { .. })), 1);
    }

    #[test]
    fn fib_via_parallel_assignment_lowers_and_verifies() {
        let func = lower_named(
            "def fib(n: int) -> int:\n    a, b = 0, 1\n    i: int = 0\n    while i < n:\n        a, b = b, a + b\n        i = i + 1\n    return a\n",
            "fib",
        );
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
        assert!(func
            .blocks
            .iter()
            .any(|b| matches!(b.terminator, Some(Terminator::Branch { .. }))));
    }

    #[test]
    fn an_inline_indexed_tuple_literal_falls_to_the_dynamic_path() {
        let module = compile_str("test", "def f() -> int:\n    return (1, 2)[0]\n")
            .expect("compiles");
        assert_eq!(lower_module(&module), Err(LowerError::DynamicOperation));
    }

    #[test]
    fn a_tuple_arity_mismatch_falls_to_the_dynamic_path() {
        let module = compile_str("test", "def f() -> int:\n    a, b, c = 1, 2\n    return a\n")
            .expect("compiles");
        assert_eq!(lower_module(&module), Err(LowerError::DynamicOperation));
    }

    #[test]
    fn print_of_an_int_lowers_to_a_semihosting_write() {
        let func = lower_named(
            "def f(x: int) -> int:\n    print(x)\n    print(x + 1)\n    return 0\n",
            "f",
        );
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::WriteInt { .. })), 2);
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::Call { .. })), 0);
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    #[test]
    fn print_of_the_wrong_arity_or_a_keyword_falls_to_the_dynamic_path() {
        let two =
            compile_str("test", "def f(a: int, b: int) -> int:\n    print(a, b)\n    return 0\n")
                .expect("compiles");
        assert!(lower_module(&two).is_err());
        let kw = compile_str("test", "def f(a: int) -> int:\n    print(a, end=\"\")\n    return 0\n")
            .expect("compiles");
        assert!(lower_module(&kw).is_err());
    }

    #[test]
    fn int_and_float_conversions_lower_to_converts() {
        let to_int = lower_named("def f(x: float) -> int:\n    return int(x)\n", "f");
        assert_eq!(
            count_insts(&to_int, |i| matches!(i, Inst::Convert {
                kind: ConvKind::Float64ToInt,
                ..
            })),
            1
        );
        let to_float = lower_named("def f(n: int) -> float:\n    return float(n)\n", "f");
        assert_eq!(
            count_insts(&to_float, |i| matches!(i, Inst::Convert {
                kind: ConvKind::IntToFloat64,
                ..
            })),
            1
        );
    }

    #[test]
    fn a_same_type_conversion_is_identity() {
        let ii = lower_named("def f(n: int) -> int:\n    return int(n)\n", "f");
        assert_eq!(count_insts(&ii, |i| matches!(i, Inst::Convert { .. })), 0);
        let ff = lower_named("def f(x: float) -> float:\n    return float(x)\n", "f");
        assert_eq!(count_insts(&ff, |i| matches!(i, Inst::Convert { .. })), 0);
    }

    #[test]
    fn a_conversion_of_a_non_numeric_falls_to_the_dynamic_path() {
        let module =
            compile_str("test", "def f() -> int:\n    return int(\"5\")\n").expect("compiles");
        assert!(lower_module(&module).is_err());
    }

    #[test]
    fn divmod_lowers_to_a_threaded_tuple_with_no_heap_tuple_or_call() {
        let func = lower_named(
            "def f(a: int, b: int) -> int:\n    q, r = divmod(a, b)\n    return q * 100 + r\n",
            "f",
        );
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::Call { .. })), 0);
        assert_eq!(count_insts(&func, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
        assert_eq!(
            count_insts(&func, |i| matches!(i, Inst::Binary {
                op: MBinOp::DivSigned,
                ..
            })),
            1
        );
        assert_eq!(
            count_insts(&func, |i| matches!(i, Inst::Binary {
                op: MBinOp::RemSigned,
                ..
            })),
            1
        );
    }

    #[test]
    fn divmod_on_floats_falls_to_the_dynamic_path() {
        let module = compile_str("test", "def f(a: float, b: float) -> int:\n    q, r = divmod(a, b)\n    return int(q)\n")
            .expect("compiles");
        assert!(lower_module(&module).is_err());
    }

    #[test]
    fn a_divmod_result_not_unpacked_falls_to_the_dynamic_path() {
        let module = compile_str("test", "def f(a: int, b: int) -> int:\n    t = divmod(a, b)\n    return t[0]\n")
            .expect("compiles");
        assert!(lower_module(&module).is_err());
    }

    #[test]
    fn abs_of_a_float_lowers_to_a_fabs_pinvoke() {
        let ff = lower_named("def f(x: float) -> float:\n    return abs(x)\n", "f");
        assert_eq!(
            count_insts(&ff, |i| matches!(i, Inst::PInvoke { import, .. } if &**import == "lamella_fabs")),
            1
        );
        assert_eq!(count_insts(&ff, |i| matches!(i, Inst::Call { .. })), 0);
        let fi = lower_named("def f(x: int) -> int:\n    return abs(x)\n", "f");
        assert_eq!(count_insts(&fi, |i| matches!(i, Inst::PInvoke { .. })), 0);
        assert!(count_insts(&fi, |i| matches!(i, Inst::Binary {
            op: MBinOp::ShrSigned,
            ..
        })) >= 1);
    }

    #[test]
    fn round_of_a_float_lowers_to_rint_then_to_int() {
        let rf = lower_named("def f(x: float) -> int:\n    return round(x)\n", "f");
        assert_eq!(
            count_insts(&rf, |i| matches!(i, Inst::PInvoke { import, .. } if &**import == "lamella_rint")),
            1
        );
        assert_eq!(
            count_insts(&rf, |i| matches!(i, Inst::Convert {
                kind: ConvKind::Float64ToInt,
                ..
            })),
            1
        );
        let ri = lower_named("def f(x: int) -> int:\n    return round(x)\n", "f");
        assert_eq!(count_insts(&ri, |i| matches!(i, Inst::PInvoke { .. })), 0);
        assert_eq!(count_insts(&ri, |i| matches!(i, Inst::Convert { .. })), 0);
    }

    #[test]
    fn min_max_of_floats_lower_to_fmin_fmax_pinvokes() {
        let mn = lower_named("def f(a: float, b: float) -> float:\n    return min(a, b)\n", "f");
        assert_eq!(
            count_insts(&mn, |i| matches!(i, Inst::PInvoke { import, .. } if &**import == "lamella_fmin")),
            1
        );
        let mx = lower_named("def f(a: float, b: float) -> float:\n    return max(a, b)\n", "f");
        assert_eq!(
            count_insts(&mx, |i| matches!(i, Inst::PInvoke { import, .. } if &**import == "lamella_fmax")),
            1
        );
        let mi = lower_named("def f(a: int, b: int) -> int:\n    return min(a, b)\n", "f");
        assert_eq!(count_insts(&mi, |i| matches!(i, Inst::PInvoke { .. })), 0);
    }

    #[test]
    fn min_of_mixed_int_and_float_is_dynamic() {
        let module =
            compile_str("test", "def f(a: int, b: float) -> int:\n    return int(min(a, b))\n")
                .expect("compiles");
        assert!(lower_module(&module).is_err());
    }

    #[test]
    fn bool_of_a_numeric_lowers_to_a_not_equal_zero_compare() {
        let bi = lower_named("def f(x: int) -> int:\n    return bool(x)\n", "f");
        assert_eq!(
            count_insts(&bi, |i| matches!(i, Inst::Compare { op: MCmpOp::Ne, .. })),
            1
        );
        assert_eq!(
            count_insts(&bi, |i| matches!(i, Inst::Call { .. } | Inst::PInvoke { .. })),
            0
        );
        let bf = lower_named("def f(x: float) -> int:\n    return bool(x)\n", "f");
        assert_eq!(
            count_insts(&bf, |i| matches!(i, Inst::Compare { op: MCmpOp::Ne, .. })),
            1
        );
    }

    fn tag_stores(func: &Function) -> usize {
        count_insts(func, |i| matches!(i, Inst::StaticStore { offset: EXCEPTION_TAG_OFFSET, .. }))
    }

    fn tag_loads(func: &Function) -> usize {
        count_insts(func, |i| matches!(i, Inst::StaticLoad { offset: EXCEPTION_TAG_OFFSET, .. }))
    }

    #[test]
    fn typed_raise_catch_lowers_and_verifies() {
        let f = lower_named(
            "def main() -> int:\n    try:\n        raise IndexError\n    except IndexError:\n        return 42\n    return 0\n",
            "main",
        );
        assert_eq!(f.ret, Some(MirType::I32));
        assert_eq!(tag_stores(&f), 2);
        assert_eq!(tag_loads(&f), 1);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::Compare { op: MCmpOp::Eq, .. })), 1);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    #[test]
    fn an_annotated_empty_list_is_a_zero_length_array() {
        let f = lower_named(
            "def main() -> int:\n    xs: list[int] = []\n    return len(xs)\n",
            "main",
        );
        assert_eq!(
            count_insts(&f, |i| matches!(i, Inst::AllocArray { element_size: 4, .. })),
            1
        );
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::ArrayStore { .. })), 0);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    #[test]
    fn an_unannotated_empty_list_is_still_not_a_typed_array() {
        let module = compile_str("test", "def main() -> int:\n    xs = []\n    return len(xs)\n")
            .expect("compiles");
        assert!(lower_module(&module).is_err());
    }

    #[test]
    fn raising_a_constructed_exception_lowers_to_the_same_tag() {
        let constructed = lower_named(
            "def main() -> int:\n    try:\n        raise ValueError(\"empty\")\n    except ValueError:\n        return 42\n    return 0\n",
            "main",
        );
        let bare = lower_named(
            "def main() -> int:\n    try:\n        raise ValueError\n    except ValueError:\n        return 42\n    return 0\n",
            "main",
        );
        assert_eq!(tag_stores(&constructed), tag_stores(&bare));
        assert_eq!(count_insts(&constructed, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
        assert_eq!(count_insts(&constructed, |i| matches!(i, Inst::Call { .. })), 0);
        assert_eq!(tag_loads(&constructed), tag_loads(&bare) + 1);
    }

    #[test]
    fn a_string_constant_is_not_a_value_anywhere_else() {
        let module = compile_str("test", "def main() -> int:\n    s = \"hello\"\n    return 0\n")
            .expect("compiles");
        assert!(
            lower_module(&module).is_err(),
            "a string local must stay dynamic"
        );
    }

    #[test]
    fn typed_raise_catch_threads_a_caught_local() {
        let f = lower_named(
            "def main() -> int:\n    x = 1\n    try:\n        raise IndexError\n    except IndexError:\n        x = 42\n    return x\n",
            "main",
        );
        assert_eq!(f.ret, Some(MirType::I32));
        assert_eq!(tag_stores(&f), 3);
        assert_eq!(tag_loads(&f), 1);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    #[test]
    fn typed_bare_except_catches_without_a_type_test() {
        let f = lower_named(
            "def main() -> int:\n    try:\n        raise IndexError\n    except:\n        return 42\n    return 0\n",
            "main",
        );
        assert_eq!(f.ret, Some(MirType::I32));
        assert_eq!(tag_loads(&f), 0);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::Compare { .. })), 0);
        assert_eq!(tag_stores(&f), 2);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    fn eq_compares(func: &Function) -> usize {
        count_insts(func, |i| matches!(i, Inst::Compare { op: MCmpOp::Eq, .. }))
    }

    #[test]
    fn typed_except_base_matches_a_derived_raise() {
        let f = lower_named(
            "def main() -> int:\n    try:\n        raise IndexError\n    except LookupError:\n        return 42\n    return 0\n",
            "main",
        );
        let closure = crate::exc::subtype_tags("LookupError").len();
        assert!(closure >= 3, "LookupError closure = itself + its two children, from the hierarchy");
        assert_eq!(tag_loads(&f), 1);
        assert_eq!(eq_compares(&f), closure);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    #[test]
    fn typed_multi_except_dispatches_to_the_matching_clause() {
        let f = lower_named(
            "def main() -> int:\n    try:\n        raise KeyError\n    except IndexError:\n        return 1\n    except KeyError:\n        return 2\n    return 0\n",
            "main",
        );
        assert_eq!(tag_loads(&f), 2);
        assert_eq!(eq_compares(&f), 2);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    #[test]
    fn typed_tuple_except_ors_the_union_of_member_closures() {
        let f = lower_named(
            "def main() -> int:\n    try:\n        raise IndexError\n    except (ValueError, LookupError):\n        return 42\n    return 0\n",
            "main",
        );
        let mut union: Vec<u32> = Vec::new();
        for name in ["ValueError", "LookupError"] {
            for tag in crate::exc::subtype_tags(name) {
                if !union.contains(&tag) {
                    union.push(tag);
                }
            }
        }
        assert!(union.len() >= 4, "ValueError + LookupError + IndexError + KeyError");
        assert_eq!(tag_loads(&f), 1);
        assert_eq!(eq_compares(&f), union.len());
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    #[test]
    fn typed_try_except_else_lowers_and_verifies() {
        let f = lower_named(
            "def main() -> int:\n    x = 0\n    try:\n        x = 1\n    except IndexError:\n        return -1\n    else:\n        x = 3\n    return x\n",
            "main",
        );
        assert_eq!(f.ret, Some(MirType::I32));
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    #[test]
    fn typed_nested_try_finally_propagates_and_lowers() {
        let f = lower_named(
            "def main() -> int:\n    x = 0\n    try:\n        try:\n            raise IndexError\n        finally:\n            x = 1\n    except IndexError:\n        x = x + 40\n    return x\n",
            "main",
        );
        assert_eq!(f.ret, Some(MirType::I32));
        assert!(tag_stores(&f) >= 1);
        assert_eq!(tag_loads(&f), 1);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    const CROSSFN_SRC: &str = "\
def boom() -> int:
    raise IndexError
    return 0


def main() -> int:
    try:
        return boom()
    except IndexError:
        return 42
    return 0
";

    #[test]
    fn typed_cross_function_raise_routes_to_the_caller_handler() {
        let f = lower_named(CROSSFN_SRC, "main");
        assert_eq!(f.ret, Some(MirType::I32));
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::Call { .. })), 1);
        assert_eq!(tag_loads(&f), 2);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
        let boom = lower_named(CROSSFN_SRC, "boom");
        assert!(tag_stores(&boom) >= 1);
    }

    #[test]
    fn typed_cross_function_call_result_crosses_the_no_exception_edge() {
        let f = lower_named(
            "def calc(n: int) -> int:\n    return n * 2\n\n\ndef main() -> int:\n    try:\n        return calc(21)\n    except IndexError:\n        return -1\n    return 0\n",
            "main",
        );
        assert_eq!(f.ret, Some(MirType::I32));
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::Call { .. })), 1);
        assert!(tag_loads(&f) >= 1);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    fn has_const_int(func: &Function, value: i64) -> bool {
        func.blocks
            .iter()
            .flat_map(|b| b.insts.iter())
            .any(|(_, i)| matches!(i, Inst::ConstInt { value: v, .. } if *v == value))
    }

    #[test]
    fn typed_protected_divide_by_zero_synthesizes_zerodivisionerror() {
        let f = lower_named(
            "def main() -> int:\n    a = 10\n    b = 0\n    try:\n        return a // b\n    except ZeroDivisionError:\n        return 42\n    return 0\n",
            "main",
        );
        assert_eq!(f.ret, Some(MirType::I32));
        assert!(has_const_int(&f, i64::from(bc::exception_tag("ZeroDivisionError"))));
        assert!(tag_stores(&f) >= 1);
        assert_eq!(tag_loads(&f), 1);
        assert_eq!(eq_compares(&f), 2);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    #[test]
    fn typed_unprotected_divide_has_no_check() {
        let f = lower_named(
            "def main() -> int:\n    a = 84\n    b = 2\n    return a // b\n",
            "main",
        );
        assert_eq!(f.ret, Some(MirType::I32));
        assert_eq!(tag_stores(&f), 0);
        assert_eq!(tag_loads(&f), 0);
        assert!(!has_const_int(&f, i64::from(bc::exception_tag("ZeroDivisionError"))));
    }

    fn count_array_stores(f: &Function) -> usize {
        count_insts(f, |i| matches!(i, Inst::ArrayStore { .. }))
    }

    #[test]
    fn a_numeric_list_literal_lowers_to_an_allocarray_and_element_stores() {
        let f = lower_named(
            "def main() -> int:\n    xs = [10, 20, 30]\n    return xs[0]\n",
            "main",
        );
        assert_eq!(
            count_insts(&f, |i| matches!(i, Inst::AllocArray { element_size: 4, .. })),
            1
        );
        assert_eq!(count_array_stores(&f), 3);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    #[test]
    fn a_list_index_read_lowers_to_an_arrayload_not_a_getitem() {
        let f = lower_named(
            "def main() -> int:\n    xs = [10, 20, 30]\n    i = 1\n    return xs[i]\n",
            "main",
        );
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::ArrayLoad { element_size: 4, .. })), 1);
        assert_eq!(
            count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { op: PyOp::Getitem, .. })),
            0
        );
    }

    #[test]
    fn a_float_list_uses_element_size_eight() {
        let f = lower_named(
            "def main() -> int:\n    xs = [1.5, 2.5]\n    return int(xs[0])\n",
            "main",
        );
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::AllocArray { element_size: 8, .. })), 1);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::ArrayLoad { element_size: 8, .. })), 1);
    }

    #[test]
    fn len_of_a_typed_list_reads_the_header_word_with_no_call() {
        let f = lower_named(
            "def main() -> int:\n    xs = [1, 2, 3, 4]\n    return len(xs)\n",
            "main",
        );
        assert!(count_insts(&f, |i| matches!(i, Inst::FieldLoad { offset: 0, .. })) >= 1);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::Call { .. })), 0);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    #[test]
    fn a_subscript_normalizes_a_negative_index_branchlessly() {
        let f = lower_named(
            "def main() -> int:\n    xs = [10, 20, 30]\n    return xs[-1]\n",
            "main",
        );
        assert!(count_insts(&f, |i| matches!(i, Inst::Binary { op: MBinOp::ShrSigned, .. })) >= 1);
        assert!(count_insts(&f, |i| matches!(i, Inst::Binary { op: MBinOp::And, .. })) >= 1);
        assert!(count_insts(&f, |i| matches!(i, Inst::FieldLoad { offset: 0, .. })) >= 1);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::ArrayLoad { .. })), 1);
    }

    #[test]
    fn a_mixed_numeric_list_is_not_a_typed_array() {
        let module = compile_str(
            "test",
            "def main() -> int:\n    xs = [1, 2.0]\n    return xs[0]\n",
        )
        .expect("compiles");
        assert_eq!(lower_module(&module), Err(LowerError::DynamicOperation));
    }

    #[test]
    fn an_appended_to_empty_list_is_a_growable_list() {
        let f = lower_named(
            "def main() -> int:\n    xs = []\n    xs.append(1)\n    xs.append(2)\n    return xs[1]\n",
            "main",
        );
        assert_eq!(f.ret, Some(MirType::I32));
        assert_eq!(
            count_insts(&f, |i| matches!(
                i,
                Inst::Alloc { handle, payload_size: GROWLIST_PAYLOAD_SIZE, ref_offsets }
                    if *handle == GROWLIST_HEADER_TYPE_HANDLE
                        && ref_offsets[..] == [GROWLIST_BACKING_OFFSET]
            )),
            1
        );
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::AllocArray { .. })), 1);
        assert_eq!(
            count_insts(&f, |i| matches!(i, Inst::PInvoke { import, .. } if &**import == PY_LIST_GROW_SYMBOL)),
            2
        );
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    #[test]
    fn a_growable_list_literal_seed_adopts_its_array_as_the_backing() {
        let f = lower_named(
            "def main() -> int:\n    xs = [1, 2]\n    xs.append(3)\n    return len(xs)\n",
            "main",
        );
        assert_eq!(f.ret, Some(MirType::I32));
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::AllocArray { .. })), 1);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::Alloc { .. })), 1);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    #[test]
    fn appending_the_other_numeric_kind_stays_dynamic() {
        let module = compile_str(
            "test",
            "def main() -> int:\n    xs = [1]\n    xs.append(2.0)\n    return len(xs)\n",
        )
        .expect("compiles");
        assert!(lower_module(&module).is_err());
    }

    #[test]
    fn an_unprotected_growable_subscript_narrows_its_index_to_len() {
        let f = lower_named(
            "def main() -> int:\n    xs = []\n    xs.append(7)\n    return xs[0]\n",
            "main",
        );
        assert_eq!(f.ret, Some(MirType::I32));
        assert!(!has_const_int(&f, i64::from(bc::exception_tag("IndexError"))));
        let narrow = f
            .blocks
            .iter()
            .flat_map(|b| b.insts.iter())
            .find(|(_, i)| matches!(i, Inst::Compare { op: MCmpOp::UnsignedGe, .. }))
            .expect("the index is narrowed against len")
            .0;
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::Binary { op: MBinOp::Or, .. })), 1);
        assert!(!f.blocks.iter().any(|b| matches!(
            b.terminator,
            Some(Terminator::Branch { cond, .. }) if cond == narrow
        )));
    }

    #[test]
    fn a_local_takes_a_called_function_s_declared_return_type() {
        let f = lower_named(
            "def helper(n: int) -> int:\n    return n * 2\ndef scale(x: float) -> float:\n    return x * 1.5\ndef main() -> int:\n    a = helper(5)\n    b = a + 1\n    f = scale(2.0)\n    return b + int(f)\n",
            "main",
        );
        assert_eq!(f.ret, Some(MirType::I32));
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::Call { .. })), 2);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    #[test]
    fn a_local_of_the_name_shadows_the_called_function() {
        let f = lower_named(
            "def helper() -> int:\n    return 7\ndef main(helper: int) -> int:\n    a = helper\n    return a + 1\n",
            "main",
        );
        assert_eq!(f.ret, Some(MirType::I32));
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::Call { .. })), 0);
    }

    #[test]
    fn a_list_parameter_takes_a_fixed_list_by_reference() {
        let f = lower_named(
            "def total(xs: list[int]) -> int:\n    return xs[0] + xs[1]\ndef main() -> int:\n    ys = [3, 4]\n    return total(ys)\n",
            "main",
        );
        assert_eq!(f.ret, Some(MirType::I32));
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::Call { .. })), 1);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
        let callee = lower_named(
            "def total(xs: list[int]) -> int:\n    return xs[0] + xs[1]\ndef main() -> int:\n    ys = [3, 4]\n    return total(ys)\n",
            "total",
        );
        assert_eq!(callee.params, vec![MirType::ObjectRef]);
        assert_eq!(count_insts(&callee, |i| matches!(i, Inst::ArrayLoad { .. })), 2);
    }

    #[test]
    fn a_growable_list_is_refused_for_a_fixed_list_parameter() {
        let module = compile_str(
            "test",
            "def first(xs: list[int]) -> int:\n    return xs[0]\ndef main() -> int:\n    g = []\n    g.append(42)\n    return first(g)\n",
        )
        .expect("compiles");
        assert!(lower_module(&module).is_err());
    }

    #[test]
    fn a_list_parameter_refuses_the_other_element_kind() {
        let module = compile_str(
            "test",
            "def total(xs: list[float]) -> int:\n    return int(xs[0])\ndef main() -> int:\n    ys = [3, 4]\n    return total(ys)\n",
        )
        .expect("compiles");
        assert!(lower_module(&module).is_err());
    }

    #[test]
    fn a_fixed_list_subscript_is_not_narrowed() {
        let f = lower_named(
            "def main() -> int:\n    xs = [10, 20, 30]\n    return xs[1]\n",
            "main",
        );
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::Compare { .. })), 0);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::Binary { op: MBinOp::Or, .. })), 0);
    }

    #[test]
    fn a_dynamic_subscript_still_lowers_to_getitem() {
        let f = lower_named("def f(s, i):\n    return s[i]\n", "f");
        assert_eq!(
            count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { op: PyOp::Getitem, .. })),
            1
        );
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::ArrayLoad { .. })), 0);
    }

    #[test]
    fn a_list_index_store_lowers_to_an_arraystore() {
        let f = lower_named(
            "def main() -> int:\n    xs = [10, 20, 30]\n    xs[1] = 99\n    xs[-1] = 7\n    return xs[0]\n",
            "main",
        );
        assert_eq!(
            count_insts(&f, |i| matches!(i, Inst::ArrayStore { element_size: 4, .. })),
            5
        );
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    #[test]
    fn a_protected_subscript_synthesizes_an_indexerror_bounds_check() {
        let f = lower_named(
            "def main() -> int:\n    xs = [10, 20]\n    try:\n        return xs[5]\n    except IndexError:\n        return 42\n    return 0\n",
            "main",
        );
        assert_eq!(f.ret, Some(MirType::I32));
        assert!(has_const_int(&f, i64::from(bc::exception_tag("IndexError"))));
        assert!(tag_stores(&f) >= 1);
        assert_eq!(tag_loads(&f), 1);
        assert_eq!(
            count_insts(&f, |i| matches!(i, Inst::Compare { op: MCmpOp::UnsignedGe, .. })),
            1
        );
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::ArrayLoad { .. })), 1);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    #[test]
    fn a_guarded_subscript_lowers_inside_a_call_argument() {
        let f = lower_named(
            "def main() -> int:\n    xs = [10, 20]\n    try:\n        print(xs[5])\n    except IndexError:\n        print(9)\n    return 0\n",
            "main",
        );
        assert!(has_const_int(&f, i64::from(bc::exception_tag("IndexError"))));
        assert_eq!(
            count_insts(&f, |i| matches!(i, Inst::Compare { op: MCmpOp::UnsignedGe, .. })),
            1
        );
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
        assert!(count_insts(&f, |i| matches!(i, Inst::WriteInt { .. })) >= 2);
    }

    #[test]
    fn a_callee_may_not_cross_a_merge() {
        let module = compile_str(
            "test",
            "def main() -> int:\n    c = 1\n    print(10 if c else 20)\n    return 0\n",
        )
        .expect("compiles");
        assert!(lower_module(&module).is_err());
    }

    #[test]
    fn an_unprotected_subscript_has_no_bounds_check() {
        let f = lower_named(
            "def main() -> int:\n    xs = [10, 20, 30]\n    return xs[1]\n",
            "main",
        );
        assert_eq!(f.ret, Some(MirType::I32));
        assert!(!has_const_int(&f, i64::from(bc::exception_tag("IndexError"))));
        assert_eq!(tag_stores(&f), 0);
        assert_eq!(
            count_insts(&f, |i| matches!(i, Inst::Compare { op: MCmpOp::UnsignedGe, .. })),
            0
        );
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::ArrayLoad { .. })), 1);
    }

    #[test]
    fn a_for_loop_over_a_typed_list_lowers_to_counted_array_loads() {
        let f = lower_named(
            "def main() -> int:\n    xs = [10, 20, 30]\n    total = 0\n    for x in xs:\n        total = total + x\n    return total\n",
            "main",
        );
        assert!(count_insts(&f, |i| matches!(i, Inst::ArrayLoad { .. })) >= 1);
        assert!(count_insts(&f, |i| matches!(i, Inst::FieldLoad { offset: 0, .. })) >= 1);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    #[test]
    fn an_aliased_list_store_targets_the_same_object() {
        let f = lower_named(
            "def main() -> int:\n    a = [1, 2, 3]\n    b = a\n    a[0] = 99\n    return b[0]\n",
            "main",
        );
        assert!(count_insts(&f, |i| matches!(i, Inst::ArrayStore { .. })) >= 4);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::ArrayLoad { .. })), 1);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    #[test]
    fn a_numeric_tuple_lowers_to_the_same_allocarray_as_a_list() {
        let f = lower_named(
            "def main() -> int:\n    t = (10, 20, 30)\n    return t[0] + t[2]\n",
            "main",
        );
        assert_eq!(
            count_insts(&f, |i| matches!(i, Inst::AllocArray { element_size: 4, .. })),
            1
        );
        assert_eq!(count_array_stores(&f), 3);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::ArrayLoad { element_size: 4, .. })), 2);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    #[test]
    fn a_tuple_rejects_item_assignment() {
        let module = compile_str(
            "test",
            "def main() -> int:\n    t = (1, 2, 3)\n    t[0] = 9\n    return t[0]\n",
        )
        .expect("compiles");
        assert_eq!(lower_module(&module), Err(LowerError::DynamicOperation));
    }

    #[test]
    fn a_float_tuple_uses_element_size_eight() {
        let f = lower_named(
            "def main() -> int:\n    t = (1.5, 2.5)\n    return int(t[0])\n",
            "main",
        );
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::AllocArray { element_size: 8, .. })), 1);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::ArrayLoad { element_size: 8, .. })), 1);
    }

    #[test]
    fn a_for_loop_over_a_typed_tuple_lowers_to_counted_array_loads() {
        let f = lower_named(
            "def main() -> int:\n    total = 0\n    for x in (10, 20, 30):\n        total = total + x\n    return total\n",
            "main",
        );
        assert!(count_insts(&f, |i| matches!(i, Inst::ArrayLoad { .. })) >= 1);
        assert!(count_insts(&f, |i| matches!(i, Inst::FieldLoad { offset: 0, .. })) >= 1);
        assert_eq!(count_insts(&f, |i| matches!(i, Inst::PyIntrinsic { .. })), 0);
    }

    #[test]
    fn a_mixed_numeric_tuple_is_not_a_typed_array() {
        let module = compile_str(
            "test",
            "def main() -> int:\n    t = (1, 2.0)\n    return int(t[1])\n",
        )
        .expect("compiles");
        assert_eq!(lower_module(&module), Err(LowerError::DynamicOperation));
    }
}




