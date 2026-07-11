//! MIR instructions and the operators they use.

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::function::ValueId;
use crate::types::{MirType, TypeHandle};

/// A binary arithmetic or bitwise operator. Both operands and the result share
/// one [`MirType`]; where signedness matters it is part of the operator (the two
/// shift-right forms), following the CLI's stack typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BinOp {
    /// Two's-complement addition.
    Add,
    /// Two's-complement subtraction.
    Sub,
    /// Two's-complement multiplication, keeping the low bits.
    Mul,
    /// Bitwise AND.
    And,
    /// Bitwise OR.
    Or,
    /// Bitwise exclusive OR.
    Xor,
    /// Shift left.
    Shl,
    /// Arithmetic (sign-propagating) shift right.
    ShrSigned,
    /// Logical (zero-filling) shift right.
    ShrUnsigned,
    /// Signed truncating division (the CLI's `div`). Division by zero / overflow are the
    /// hardware's (a target without a divide instruction lowers it to a soft routine).
    DivSigned,
    /// Unsigned division (the CLI's `div.un`).
    DivUnsigned,
    /// Signed remainder (the CLI's `rem`), with the sign of the dividend.
    RemSigned,
    /// Unsigned remainder (the CLI's `rem.un`).
    RemUnsigned,
}

/// An integer comparison operator. The result is an `int32` equal to 1 when the
/// comparison holds and 0 otherwise, matching the CLI's comparison instructions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CmpOp {
    /// Equal.
    Eq,
    /// Not equal.
    Ne,
    /// Signed less-than.
    SignedLt,
    /// Signed less-than-or-equal.
    SignedLe,
    /// Signed greater-than.
    SignedGt,
    /// Signed greater-than-or-equal.
    SignedGe,
    /// Unsigned less-than.
    UnsignedLt,
    /// Unsigned less-than-or-equal.
    UnsignedLe,
    /// Unsigned greater-than.
    UnsignedGt,
    /// Unsigned greater-than-or-equal.
    UnsignedGe,
}

/// A width conversion: narrowing an `int32` to a smaller integer and re-extending it to
/// the stack's 32-bit width, signed or unsigned -- the CLI's `conv.i1`/`conv.u1`/
/// `conv.i2`/`conv.u2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConvKind {
    /// Sign-extend the low 8 bits (`conv.i1`).
    SignExtend8,
    /// Zero-extend the low 8 bits (`conv.u1`).
    ZeroExtend8,
    /// Sign-extend the low 16 bits (`conv.i2`).
    SignExtend16,
    /// Zero-extend the low 16 bits (`conv.u2`).
    ZeroExtend16,
    /// Truncate a 32-bit float toward zero to a signed `int32` (`conv.i4` from an `R4`). A
    /// no-FPU target lowers this with a soft routine rather than a hardware convert.
    Float32ToInt,
    /// Convert a signed `int32` to a 32-bit float (`conv.r4` from an integer), exact for
    /// magnitudes below 2^24. The soft form on a no-FPU target.
    IntToFloat32,
    /// Truncate a 64-bit float toward zero to a signed `int32` (`conv.i4` from an `R8`). wasm has a
    /// native truncate; a no-FPU ARM target needs the aeabi soft helper (unsupported).
    Float64ToInt,
    /// Convert a signed `int32` to a 64-bit float (`conv.r8` from an int32 -- e.g. `double d = intVar`).
    /// The no-FPU ARM form is the `__aeabi_i2d` soft helper; wasm has `f64.convert_i32_s`.
    IntToFloat64,
    /// Convert a signed `int64` to a 64-bit float (`conv.r8` from an int64). ARM `__aeabi_l2d`; wasm
    /// `f64.convert_i64_s`.
    LongToFloat64,
    /// Widen a 32-bit float to a 64-bit float (`conv.r8` from an R4 -- `double d = floatVar`). ARM
    /// `__aeabi_f2d`; wasm `f64.promote_f32`.
    Float32ToFloat64,
    /// Narrow a 64-bit float to a 32-bit float (`conv.r4` from an R8 -- `float f = doubleVar`). ARM
    /// `__aeabi_d2f`; wasm `f32.demote_f64`.
    Float64ToFloat32,
    /// Convert a signed `int64` to a 32-bit float (`conv.r4` from an int64). ARM `__aeabi_l2f`; wasm
    /// `f32.convert_i64_s`.
    LongToFloat32,
    /// Convert an UNSIGNED `int32` to a 64-bit float (`conv.r.un` from a uint). ARM `__aeabi_ui2d`;
    /// wasm `f64.convert_i32_u`.
    UIntToFloat64,
    /// Convert an UNSIGNED `int64` to a 64-bit float (`conv.r.un` from a ulong). ARM `__aeabi_ul2d`;
    /// wasm `f64.convert_i64_u`.
    ULongToFloat64,
    /// Reinterpret an `int32` as an `ObjectRef` -- a no-op at the machine level (both are one word), a
    /// pure retype so a computed integer can stand in for a reference. `isinst` uses it to fold a
    /// branchless `matched ? object : null` mask (an `int32`) into the reference result.
    IntToRef,
    /// Reinterpret an `ObjectRef` as an `int32` -- the inverse no-op of [`ConvKind::IntToRef`], so a
    /// reference can enter integer (bitmask) arithmetic. `isinst` masks the object pointer this way.
    RefToInt,
}

impl ConvKind {
    /// The [`MirType`] this conversion produces: `F32` for the int-to-float case, `int32` for the
    /// narrowing/extending and float-to-int cases.
    #[must_use]
    pub fn result_type(self) -> MirType {
        match self {
            ConvKind::IntToFloat32
            | ConvKind::Float64ToFloat32
            | ConvKind::LongToFloat32 => MirType::F32,
            ConvKind::IntToFloat64
            | ConvKind::LongToFloat64
            | ConvKind::Float32ToFloat64
            | ConvKind::UIntToFloat64
            | ConvKind::ULongToFloat64 => MirType::F64,
            ConvKind::IntToRef => MirType::ObjectRef,
            ConvKind::SignExtend8
            | ConvKind::ZeroExtend8
            | ConvKind::SignExtend16
            | ConvKind::ZeroExtend16
            | ConvKind::Float32ToInt
            | ConvKind::Float64ToInt
            | ConvKind::RefToInt => MirType::I32,
        }
    }
}

/// A Python binary operator for [`PyOp::Binop`] -- the abstract `PyNumber_*` operations.
/// The runtime-support entry resolves the operand types, reflected forms (`__radd__`), and
/// in-place variants; the operator selects which protocol slot it consults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PyBinOp {
    /// `+` (`__add__`).
    Add,
    /// `-` (`__sub__`).
    Sub,
    /// `*` (`__mul__`).
    Mul,
    /// `/` (`__truediv__`).
    TrueDiv,
    /// `//` (`__floordiv__`).
    FloorDiv,
    /// `%` (`__mod__`).
    Mod,
    /// `**` (`__pow__`).
    Pow,
    /// `<<` (`__lshift__`).
    LShift,
    /// `>>` (`__rshift__`).
    RShift,
    /// `&` (`__and__`).
    And,
    /// `|` (`__or__`).
    Or,
    /// `^` (`__xor__`).
    Xor,
    /// `@` (`__matmul__`, PEP 465).
    MatMul,
}

/// A Python rich-comparison or identity operator for [`PyOp::Compare`]. The result reduces a
/// Python bool to a branchable `int32` 0 or 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PyCmpOp {
    /// `<` (`__lt__`).
    Lt,
    /// `<=` (`__le__`).
    Le,
    /// `==` (`__eq__`).
    Eq,
    /// `!=` (`__ne__`).
    Ne,
    /// `>` (`__gt__`).
    Gt,
    /// `>=` (`__ge__`).
    Ge,
    /// `is` -- object identity.
    Is,
    /// `is not` -- object non-identity.
    IsNot,
}

/// A dynamic-object operation for [`Inst::PyIntrinsic`] -- one entry of Python's abstract
/// object protocol (CPython's `PyObject_*`/`PyNumber_*`/`tp_*` layer). Each names a single
/// Python runtime-support entry point the backend calls. The op set follows the official Python
/// data-model reference; an un-wired op rejects in codegen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PyOp {
    /// Attribute load `obj.name`. `args = [receiver]`; `name` is the attribute's index into the
    /// module's name table (the per-module name-table-index ABI), and the inst's `cache` is the
    /// inline-cache slot -- so this lowers to `py_getattr(receiver, name_id, cache_slot)`.
    Getattr {
        /// The attribute name, as an index into the module's name table.
        name: u32,
    },
    /// Attribute store `obj.name = value` -- `args = [obj, name, value]`. Side-effecting.
    Setattr,
    /// A binary operator `a <op> b` -- `args = [a, b]`, the operator in the payload.
    Binop(PyBinOp),
    /// A call `f(*args)` -- `args[0]` is the callable, `args[1..]` the positional arguments.
    Call,
    /// Subscript load `obj[key]` -- `args = [obj, key]`.
    Getitem,
    /// Subscript store `obj[key] = value` -- `args = [obj, key, value]`. Side-effecting.
    Setitem,
    /// `iter(obj)` -- get an iterator. `args = [obj]`.
    Iter,
    /// `next(it)` -- advance an iterator. `args = [it]`.
    Next,
    /// `len(obj)` -- the result is a `PyValue` (the Python `int` length, per the locked
    /// `py_len(x) -> PyValue` ABI; the frontend unboxes it where a raw `int32` is wanted). `args =
    /// [obj]`.
    Len,
    /// Truthiness `bool(obj)` for a condition -- the result is an `int32` 0 or 1. `args = [obj]`.
    Truthy,
    /// A rich comparison `a <cmp> b` -- the result is an `int32` 0 or 1. `args = [a, b]`.
    Compare(PyCmpOp),
    /// Membership `x in obj` -- the result is an `int32` 0 or 1. `args = [x, obj]`.
    Contains,
    /// `__enter__` of a `with` context manager -- `args = [obj]`.
    Enter,
    /// `__exit__` of a `with` context manager -- `args = [obj, exc_type, exc_value, traceback]`.
    /// Side-effecting (its bool result governs exception suppression in the runtime, not the slot).
    Exit,
    /// Module import `import name` -- `args = [name]`, the name an interned-string `PyValue`.
    Import,
}

impl PyOp {
    /// The [`MirType`] this operation's result must have, or `None` when the operation is
    /// side-effecting and its result is an ignored placeholder (like [`Inst::Store`]). The
    /// C-bool-result ops (`Truthy`/`Compare`/`Contains`, consumed by a branch) yield `int32`; the
    /// rest yield a `PyValue` (`Len` returns the Python `int` length object, not a raw `int32`).
    #[must_use]
    pub fn result_type(self) -> Option<MirType> {
        match self {
            PyOp::Truthy | PyOp::Compare(_) | PyOp::Contains => Some(MirType::I32),
            PyOp::Setattr | PyOp::Setitem | PyOp::Exit => None,
            PyOp::Getattr { .. }
            | PyOp::Binop(_)
            | PyOp::Call
            | PyOp::Getitem
            | PyOp::Iter
            | PyOp::Next
            | PyOp::Len
            | PyOp::Enter
            | PyOp::Import => Some(MirType::PyValue),
        }
    }
}

/// One MIR instruction: an operation defining a single typed result value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inst {
    /// An integer constant of an integer [`MirType`] (`i32`, `i64`, or native).
    ConstInt {
        /// The integer type of the constant.
        ty: MirType,
        /// The value, interpreted at `ty`'s width.
        value: i64,
    },
    /// `lhs op rhs`, where the operands and the result all share one type.
    Binary {
        /// The operator.
        op: BinOp,
        /// The left operand.
        lhs: ValueId,
        /// The right operand.
        rhs: ValueId,
    },
    /// `lhs cmp rhs`, producing an `int32` of 0 or 1.
    Compare {
        /// The comparison operator.
        op: CmpOp,
        /// The left operand.
        lhs: ValueId,
        /// The right operand.
        rhs: ValueId,
    },
    /// Converts `value` to a narrower integer width and back to `int32`, signed or
    /// unsigned per `kind` -- the CLI's sub-word `conv.*`. The result is `int32`.
    Convert {
        /// The value to convert.
        value: ValueId,
        /// The width and signedness of the conversion.
        kind: ConvKind,
    },
    /// Widens a 32-bit integer to `int64`, sign- or zero-extended per `signed` (the CLI's
    /// `conv.i8`/`conv.u8` from an `int32`). The result is `int64`.
    Widen {
        /// The 32-bit value to widen.
        value: ValueId,
        /// Sign-extend (`conv.i8`) when true, zero-extend (`conv.u8`) when false.
        signed: bool,
    },
    /// Truncates an `int64` to its low 32 bits (the CLI's `conv.i4`/`conv.u4` from an
    /// `int64`). The result is `int32`.
    Truncate {
        /// The 64-bit value to truncate.
        value: ValueId,
    },
    /// A direct call to another function of the program (named by index), passing
    /// `args` and producing the callee's return value.
    Call {
        /// The index of the called function within the program.
        callee: u32,
        /// The argument values, in order (placed in the ABI's argument registers).
        args: Vec<ValueId>,
    },
    /// A virtual call dispatched through the receiver's vtable. `args[0]` is the receiver, whose
    /// `obj-4` TypeDesc anchors the vtable; the target is `[TypeDesc - 4 - slot*4]` (laid out by the
    /// backend's vtable emission). Produces the callee's return value, like [`Inst::Call`], and is a
    /// safepoint (the call may collect).
    CallVirtual {
        /// The called method's vtable slot.
        slot: u32,
        /// The argument values, in order; `args[0]` is the receiver (`this`).
        args: Vec<ValueId>,
        /// Whether the callee returns a value (false for a `void` method). A signature-checked
        /// target (wasm `call_indirect`) needs the exact result arity; an untyped `blx` ignores it.
        returns_value: bool,
    },
    /// A `callvirt` on an INTERFACE method, dispatched through the receiver's interface table. The
    /// implementation is found by matching `tag` (the interface-method identity) in the receiver
    /// type's itable, which the backend lays after the TypeDesc.
    CallInterface {
        /// The called interface method's identity tag (`resolver::interface_method_tag`).
        tag: u32,
        /// The argument values, in order; `args[0]` is the receiver (`this`).
        args: Vec<ValueId>,
        /// Whether the callee returns a value (false for a `void` method); see [`Inst::CallVirtual`].
        returns_value: bool,
    },
    /// The ADDRESS of a function of the program (a code pointer), named by index -- the CIL `ldftn`,
    /// the substrate for delegates and callbacks. The result is an `i32` raw address (the target's
    /// entry, with the Thumb bit on ARM so it can be `blx`ed). In an object the backend emits it as an
    /// absolute relocation to the function's symbol; the linker fills the real address.
    FuncAddr {
        /// The index of the referenced function within the program.
        func: u32,
    },
    /// The virtual function pointer for a `ldvirtftn` -- like [`Inst::FuncAddr`], but the target is
    /// resolved through the object's vtable at run time (the slot's entry off its `obj-4` TypeDesc, as
    /// [`Inst::CallVirtual`] dispatches), so a delegate created from a virtual method binds the override
    /// for the object's RUNTIME type. The result is the same `i32` code pointer `FuncAddr` yields (the
    /// Thumb bit set on ARM), consumed by a delegate's method field or an [`Inst::CallIndirect`].
    VirtualFuncAddr {
        /// The object whose runtime-type vtable resolves the pointer (`ldvirtftn` pops it).
        object: ValueId,
        /// The method's vtable slot.
        slot: u32,
    },
    /// An INDIRECT call through a code pointer in `target` (the CIL `calli`, and the engine of a
    /// delegate's `Invoke`): passes `args` in the ABI's argument registers and calls `target`.
    /// Produces the callee's return value, like [`Inst::Call`], and is a safepoint.
    CallIndirect {
        /// The code pointer to call (e.g. from [`Inst::FuncAddr`] or a delegate's method field).
        target: ValueId,
        /// The argument values, in order (placed in the ABI's argument registers).
        args: Vec<ValueId>,
        /// Whether the callee returns a value (false for a `void` target); see [`Inst::CallVirtual`].
        returns_value: bool,
    },
    /// A call to an EXTERNAL native symbol named by `symbol` -- a `u32` index into the module's
    /// extern-symbol table (`__aeabi_fadd`, a `[DllImport]` entry point, a `py_*` helper). The
    /// substrate for soft-float helpers, P/Invoke, and the GC/Python support entries. `args` pass in
    /// the platform C ABI (ARM AAPCS, so an `f64` rides `r0:r1`); the result is the call's return
    /// value (typed by the producing op). The backend lowers it to a call plus an `R_<arch>_CALL_PLT`
    /// relocation the linker resolves (e.g. against `libgcc.a`). A safepoint. Marshaling (by-ref
    /// structs, varargs, callbacks) layers on top later.
    CallNative {
        /// The index of the external symbol in the module's extern-symbol table.
        symbol: u32,
        /// The argument values, in order (placed in the C ABI's argument registers).
        args: Vec<ValueId>,
    },
    /// A P/Invoke call to an unmanaged `[DllImport]` function -- the CIL `call` of an external method.
    /// Carries the import symbol name; a pre-pass interns it into the module's extern table and rewrites
    /// this to a [`Inst::CallNative`] (the same C-ABI boundary the own-linker resolves). Blittable
    /// arguments pass directly; non-blittable marshaling (strings, by-ref, structs) layers on later.
    PInvoke {
        /// The unmanaged import symbol (the `DllImport` entry point name).
        import: Box<str>,
        /// The argument values, in order (the C ABI's argument registers).
        args: Vec<ValueId>,
    },
    /// Invokes a single-cast delegate -- the CIL `Delegate::Invoke`. Loads the delegate's `_methodPtr`
    /// (offset 4) and calls it indirectly, passing the delegate's `_target` (offset 0) as the implicit
    /// first argument WHEN it is non-null (an instance-method delegate) and omitting it when null (a
    /// static-method delegate); the dispatch branches on `_target` at run time. A safepoint; the result
    /// is the call's return value. (Multicast -- iterating an invocation list -- layers on top later.)
    InvokeDelegate {
        /// The delegate object (`_target` at offset 0, `_methodPtr` at offset 4).
        delegate: ValueId,
        /// The explicit arguments (Invoke's signature params), in order.
        args: Vec<ValueId>,
        /// Whether `Invoke` returns a value (false for an `Action`-style void delegate); see
        /// [`Inst::CallVirtual`].
        returns_value: bool,
    },
    /// A `castclass` base-pointer chain scan: walks `args[0]` (the object's TypeDesc address) up the
    /// base_ptr@12 chain looking for `args[1]` (the target type's TypeDesc address). The result is 1 if
    /// the target is found (the cast holds) or 0 if the chain ends first; the front end feeds it into the
    /// cast's trap leader (a 0 raises InvalidCastException). Exact -- no FNV-collision risk a tag compare
    /// would carry, and O(depth) rather than O(subtype count) at the call site.
    CastClassScan {
        /// `[object_typedesc, target_typedesc]` -- the scan's start descriptor and the sought ancestor.
        args: Vec<ValueId>,
    },
    /// A dynamic-object operation from Python's abstract object protocol -- the `op`-selected
    /// operation over `args` (receiver-first). Lowers to a call into the Python runtime-support
    /// library at a backend-threaded entry point (the [`Inst::Alloc`]/`lamella_gc_alloc` pattern),
    /// so it is a SAFEPOINT: the support entry may run arbitrary Python that allocates and collects.
    /// `cache` is the inline-cache SLOT index the frontend assigned (the runtime sizes the table by
    /// the code object's cache_count, so slot 0 is valid -- not "no cache"); the backend passes it to
    /// the entry, which reads/writes that RAM cache slot. The result type is `op.result_type()` (a
    /// `PyValue`, a scalar `int32`, or an ignored placeholder for a side-effecting op). Emitted by the
    /// Python frontend; the C# lowering never emits one.
    PyIntrinsic {
        /// Which abstract-object operation this performs.
        op: PyOp,
        /// The operand values, receiver-first, with op-specific arity.
        args: Vec<ValueId>,
        /// The inline-cache slot index (sized by cache_count; slot 0 is valid, not "no cache").
        cache: u32,
    },
    /// Stores `value` to the memory address held in `address` -- the memory-mapped-I/O write
    /// primitive. The write is a side effect; the instruction's own result value is a placeholder
    /// that callers ignore.
    Store {
        /// The value holding the destination address.
        address: ValueId,
        /// The value to write there.
        value: ValueId,
        /// The width in bytes of the store: 1 (`stind.i1` -> `strb`), 2 (`stind.i2` -> `strh`), or
        /// 4 (`stind.i4` -> `str`). The low bits of `value` are written; the rest are ignored.
        width: u32,
    },
    /// Loads the value at the memory address held in `address` -- the memory-mapped-I/O read
    /// primitive. The instruction's result is the loaded value, a sub-word load sign- or
    /// zero-extended to 32 bits.
    Load {
        /// The value holding the source address.
        address: ValueId,
        /// The width in bytes of the load: 1 (`ldind.i1`/`u1`), 2 (`ldind.i2`/`u2`), or 4
        /// (`ldind.i4`/`u4`).
        width: u32,
        /// Whether a sub-word load is sign-extended (`ldind.i1`/`i2` -> `ldrsb`/`ldrsh`) or
        /// zero-extended (`ldind.u1`/`u2` -> `ldrb`/`ldrh`). Ignored at word width.
        signed: bool,
    },
    /// Copies `size` bytes from `src` to `dst` -- the CLI's `cpblk` (a raw block copy / `memcpy`). A
    /// side effect; the instruction's result is a placeholder callers ignore.
    CopyBlock {
        /// The destination address.
        dst: ValueId,
        /// The source address.
        src: ValueId,
        /// The number of bytes to copy.
        size: ValueId,
    },
    /// Fills `size` bytes at `dst` with the low byte of `value` -- the CLI's `initblk` (a raw block
    /// fill / `memset`). A side effect; the instruction's result is a placeholder callers ignore.
    FillBlock {
        /// The destination address.
        dst: ValueId,
        /// The fill byte (the low 8 bits of this value).
        value: ValueId,
        /// The number of bytes to fill.
        size: ValueId,
    },
    /// Zero-initializes the value-type instance this defines -- the CLI's `initobj`. The
    /// result is the zeroed value type; its size comes from the result's [`MirType`].
    InitStruct,
    /// Loads the scalar field at byte `offset` of the value-type `base` -- the CLI's
    /// `ldfld` on a local struct. The result is the field's value.
    FieldLoad {
        /// The value-type instance being read.
        base: ValueId,
        /// The field's byte offset within the value type.
        offset: u32,
    },
    /// Stores `value` into the value-type `base` at byte `offset` -- the CLI's `stfld`.
    /// A side effect; the instruction's result is a placeholder callers ignore.
    FieldStore {
        /// The value-type instance being written.
        base: ValueId,
        /// The field's byte offset within the value type.
        offset: u32,
        /// The scalar value to store (its width comes from its type).
        value: ValueId,
    },
    /// The address of the value-type `base`'s field at byte `offset` -- the CLI's `ldflda`
    /// once the address escapes (e.g. as an instance method's `this`). The result is a
    /// managed pointer; the lowering materializes `&base + offset`.
    FieldAddr {
        /// The value-type instance whose field address is taken.
        base: ValueId,
        /// The field's byte offset within the value type.
        offset: u32,
    },
    /// Copies the value-type `src` to the instance this defines -- the CLI's `ldobj`/`stobj`
    /// value copy (struct assignment, pass-by-value). The result is the copy; its size comes
    /// from the result's [`MirType`].
    CopyStruct {
        /// The value-type instance to copy from.
        src: ValueId,
    },
    /// Writes a NUL-terminated string to the host via an ARM semihosting `SYS_WRITE0`
    /// request -- the `Debug.WriteLine` / console-output primitive. A side effect; the
    /// instruction's result is a placeholder that callers ignore.
    SemihostWrite {
        /// The NUL-terminated bytes to emit.
        text: Box<[u8]>,
    },
    /// Formats the `int32` `value` as signed decimal and writes it with a trailing newline via
    /// semihosting -- the `Console.WriteLine(int)` primitive. A side effect; the instruction's
    /// result is a placeholder callers ignore.
    WriteInt {
        /// The integer to format and write.
        value: ValueId,
    },
    /// A static string literal: the result is an `ObjectRef` to a read-only UTF-16 blob
    /// `[u32 length_in_utf16_units][UTF-16LE units]` the target lowering emits. (The build's
    /// string-storage encoding -- UTF-16 by default; the consumer reads the unit count at
    /// offset 0 for `String.Length`.)
    StringLiteral {
        /// The string's UTF-16 code units.
        utf16: Box<[u16]>,
    },
    /// Compares two strings (`ObjectRef`s to UTF-16 blobs, or null) for ordinal equality -- the
    /// CLI's `System.String::op_Equality`. The result is an `int32` 0 or 1: two nulls are equal,
    /// null and non-null are not, otherwise compared length-then-content.
    StringEquals {
        /// The left string.
        lhs: ValueId,
        /// The right string.
        rhs: ValueId,
    },
    /// Concatenates two strings -- the CLI's `System.String::Concat(string, string)` (what `a + b`
    /// emits). Allocates a new `[u32 unit_count][UTF-16LE]` blob of `lhs.length + rhs.length` units
    /// and copies both in; the result is an `ObjectRef` to it. A backend may lower it inline or
    /// rewrite it to a generated helper.
    StringConcat {
        /// The left string.
        lhs: ValueId,
        /// The right string.
        rhs: ValueId,
    },
    /// Formats a signed 32-bit integer as its decimal string -- the CLI's `System.Int32::ToString()`.
    /// Allocates a `[u32 unit_count][UTF-16LE]` blob of the decimal digits (a leading `-` for a
    /// negative value); the result is an `ObjectRef` to it. A backend may lower it inline or rewrite
    /// it to a generated helper.
    IntToString {
        /// The integer value to format.
        value: ValueId,
    },
    /// Allocates a garbage-collected object of a reference type on the managed heap -- the
    /// reference-type `newobj` (and, later, `box`). The result is an `ObjectRef` to the
    /// zero-initialized payload. The target lowers it to a `lamella_gc_alloc(payload_size,
    /// &TypeDesc) -> payload*` runtime call (an allocation safepoint) and emits the type's
    /// `TypeDesc` -- the GC trace map -- from the fields below. The front end carries the map
    /// here because the backend has no metadata access.
    Alloc {
        /// The reference type's identity, so allocations of one type share a single TypeDesc.
        handle: TypeHandle,
        /// The payload size in bytes (the object's fields, from the type's layout).
        payload_size: u32,
        /// The byte offsets within the payload of the fields that hold an `ObjectRef`/`&` --
        /// the roots the emitted TypeDesc lists for the collector to trace and relocate.
        ref_offsets: Box<[u32]>,
    },
    /// Allocates a garbage-collected object of `payload_size` bytes whose runtime type -- its `obj-4`
    /// TypeDesc -- is CLONED from `proto`'s header, rather than baked from a static [`TypeHandle`] like
    /// [`Inst::Alloc`]. The target reads `proto`'s descriptor (`[proto - 4]`), allocates via
    /// `lamella_gc_alloc(payload_size, that_desc)` (an allocation safepoint), writes that descriptor at the
    /// new object's `obj-4`, and yields the `ObjectRef`. Because the descriptor is shared with `proto`, the
    /// GC traces the new object with PROTO's trace map -- so the two MUST be the same shape (a same-type
    /// clone). The engine of an immutable delegate `Combine`: the concrete delegate type is only known at
    /// run time (from the `a`/`b` operands), so the new `MulticastDelegate` clones an operand's descriptor
    /// and a following `castclass` to the concrete delegate type still passes. The payload is NOT zeroed
    /// (the bump allocator does not zero); the producer must initialize the ref fields before any safepoint.
    AllocLike {
        /// The prototype object whose runtime TypeDesc (`obj-4`) the new object clones.
        proto: ValueId,
        /// The payload size in bytes of the new object (a delegate is 12; matches `proto`'s size).
        payload_size: u32,
    },
    /// Loads the TypeDesc pointer of a heap object -- the word the allocator wrote in the header
    /// just before the payload (`object - 4` per the GC ABI). The runtime type identity of a boxed
    /// value / reference, compared against [`Inst::TypeDescAddr`] for an `unbox.any`/`castclass`
    /// type check. Result is the descriptor address (an `i32` on a 32-bit target).
    LoadTypeDesc {
        /// The heap object (an `ObjectRef`).
        object: ValueId,
    },
    /// The address of the TypeDesc the backend emits for `handle` -- the same per-type descriptor an
    /// `Alloc` of that type points at. Compared against [`Inst::LoadTypeDesc`]: equal addresses mean
    /// the same runtime type (descriptors are deduplicated per type). Result is that address.
    TypeDescAddr {
        /// The type whose TypeDesc address this is.
        handle: TypeHandle,
    },
    /// The address of a type's canonical TypeDesc on the object/linked path, identified by `handle`. The
    /// backend emits ONE named descriptor symbol per type (`__lamella_typedesc_<handle>`), laid once per
    /// object, and references it here by an absolute relocation -- so every `new` of a type shares one
    /// descriptor (box / type-identity compare by address), and a program can reference a descriptor that
    /// another assembly defines. The `words` are the GC ABI header + trace map: `[payload_size, nrefs,
    /// type_tag, base_ptr, ref_offsets...]` (`base_ptr` is a `0` placeholder until the base chain is
    /// threaded in). The `vtable` is laid as TypeDesc-relative slots BEFORE the descriptor, so `callvirt`
    /// dispatches through it on the linked device path. Result is the descriptor's address (an `i32`).
    TypeDescLiteral {
        /// The type handle -- the descriptor's identity, deduplicated to one symbol per type per object.
        handle: u32,
        /// The descriptor words, in GC-ABI order.
        words: Box<[u32]>,
        /// The type's vtable as this-module function indices in slot order. The backend lays them as
        /// `R_LAMELLA_REL_DESC` slots before the descriptor (slot `k` at `desc - 4 - k*4`, holding
        /// `method_entry - desc`), so dispatch reads `desc + slot + 1`. Empty when the type has no
        /// virtual methods.
        vtable: Box<[u32]>,
    },
    /// Allocates a garbage-collected array of `length` elements of `element_size` bytes -- the
    /// CLI's `newarr`. The payload is `[u32 length][elements...]`; the result is an `ObjectRef`
    /// to it. Lowers to `lamella_gc_alloc(4 + length*element_size, &TypeDesc)` (a safepoint) and
    /// stores the length at offset 0. `ldlen` reads that length word (a `FieldLoad` at offset 0).
    AllocArray {
        /// The array type's identity, for the emitted TypeDesc.
        handle: TypeHandle,
        /// The number of elements.
        length: ValueId,
        /// The size in bytes of one element.
        element_size: u32,
    },
    /// Loads element `index` of `array` -- the CLI's `ldelem`. The result is the element at
    /// `array + 4 + index*element_size` (the 4-byte length prefix is skipped). A sub-word element
    /// is sign- or zero-extended to the 32-bit result per `signed` (`ldelem.i1` vs `ldelem.u1`).
    ArrayLoad {
        /// The array `ObjectRef`.
        array: ValueId,
        /// The element index.
        index: ValueId,
        /// The size in bytes of one element.
        element_size: u32,
        /// Whether a sub-word element is sign-extended (signed) or zero-extended (unsigned).
        signed: bool,
    },
    /// Stores `value` into element `index` of `array` -- the CLI's `stelem`. A side effect; the
    /// instruction's result is a placeholder callers ignore.
    ArrayStore {
        /// The array `ObjectRef`.
        array: ValueId,
        /// The element index.
        index: ValueId,
        /// The value to store (its width comes from its type).
        value: ValueId,
        /// The size in bytes of one element.
        element_size: u32,
    },
    /// The ADDRESS of element `index` of `array` -- the CLI's `ldelema`. The result is a `ManagedPtr`
    /// to `array + 4 + index*element_size` (the same element location [`Inst::ArrayLoad`] reads, past
    /// the length prefix), bounds-checked like a load. It backs `ref`/`out` array elements and calling
    /// a method on a value-type array element (`arr[i].M()`), and is the address a value-type
    /// `ldelem`/`stelem` loads or stores the struct through.
    ArrayElemAddr {
        /// The array `ObjectRef`.
        array: ValueId,
        /// The element index.
        index: ValueId,
        /// The size in bytes of one element.
        element_size: u32,
    },
    /// Allocates a 2-D rectangular array of `dim0 * dim1` elements -- the CLI's `newobj int[,]::.ctor`
    /// (rectangular arrays go through `System.Array` calls, not the `szarray` opcodes). The payload is
    /// `[u32 dim0][u32 dim1][elements...]` in row-major order; the result is an `ObjectRef` to it.
    /// Lowers to `lamella_gc_alloc(8 + dim0*dim1*element_size, &TypeDesc)` (a safepoint), storing the
    /// two dimensions at offsets 0 and 4.
    AllocArray2D {
        /// The array type's identity, for the emitted TypeDesc.
        handle: TypeHandle,
        /// The number of rows (the first dimension's length).
        dim0: ValueId,
        /// The number of columns (the second dimension's length).
        dim1: ValueId,
        /// The size in bytes of one element.
        element_size: u32,
    },
    /// Loads element `(index0, index1)` of a 2-D `array` -- the CLI's `int[,]::Get`. The element sits
    /// at `array + 8 + (index0*dim1 + index1)*element_size` (row-major; `dim1` is read from
    /// `[array+4]`), with a per-dimension bounds check (`index0 < dim0`, `index1 < dim1`). A sub-word
    /// element is sign- or zero-extended to the 32-bit result per `signed`.
    Array2DLoad {
        /// The array `ObjectRef`.
        array: ValueId,
        /// The first (row) index.
        index0: ValueId,
        /// The second (column) index.
        index1: ValueId,
        /// The size in bytes of one element.
        element_size: u32,
        /// Whether a sub-word element is sign-extended (signed) or zero-extended (unsigned).
        signed: bool,
    },
    /// Stores `value` into element `(index0, index1)` of a 2-D `array` -- the CLI's `int[,]::Set`. A
    /// side effect; the instruction's result is a placeholder callers ignore.
    Array2DStore {
        /// The array `ObjectRef`.
        array: ValueId,
        /// The first (row) index.
        index0: ValueId,
        /// The second (column) index.
        index1: ValueId,
        /// The value to store (its width comes from its type).
        value: ValueId,
        /// The size in bytes of one element.
        element_size: u32,
    },
    /// Allocates a rank-N (N >= 3) rectangular array -- the CLI's `int[,,...]::.ctor`. The layout is
    /// `[dim0][dim1]...[dim(N-1)][elements]`: N u32 dimension words, then the elements row-major, so the
    /// size is `4*N + (dim0*dim1*...*dim(N-1))*element_size`. `dims` holds the N dimension lengths in
    /// order. The result is the array `ObjectRef` (the header base). Rank 1 is [`AllocArray`], rank 2 is
    /// [`AllocArray2D`] (this generalizes it; a rank-N Horner index folds to their forms at N=1/2).
    AllocArrayMD {
        /// The array type's identity, for the emitted TypeDesc.
        handle: TypeHandle,
        /// The N dimension lengths, in order (`dims[0]` is the outermost / slowest-varying).
        dims: alloc::boxed::Box<[ValueId]>,
        /// The size in bytes of one element.
        element_size: u32,
    },
    /// Loads element `(indices[0], ..., indices[N-1])` of a rank-N `array` -- the CLI's `int[,,...]::Get`.
    /// Row-major: the element sits at `array + 4*N + flat*element_size` where the flat index is the Horner
    /// fold `((...(i0*dim1 + i1)*dim2 + i2)...)*dim(N-1) + i(N-1)` (each `dim_k` read from `[array+4*k]`),
    /// with a per-dimension bounds check (`indices[k] < dim_k`). A sub-word element is sign-/zero-extended
    /// per `signed`.
    ArrayMDLoad {
        /// The array `ObjectRef`.
        array: ValueId,
        /// The N indices, in order.
        indices: alloc::boxed::Box<[ValueId]>,
        /// The size in bytes of one element.
        element_size: u32,
        /// Whether a sub-word element is sign-extended (signed) or zero-extended (unsigned).
        signed: bool,
    },
    /// Stores `value` into element `(indices[0], ..., indices[N-1])` of a rank-N `array` -- the CLI's
    /// `int[,,...]::Set`. A side effect; the instruction's result is a placeholder callers ignore.
    ArrayMDStore {
        /// The array `ObjectRef`.
        array: ValueId,
        /// The N indices, in order.
        indices: alloc::boxed::Box<[ValueId]>,
        /// The value to store (its width comes from its type).
        value: ValueId,
        /// The size in bytes of one element.
        element_size: u32,
    },
    /// Loads a static field -- the CLI's `ldsfld`. `offset` is the field's byte offset within the
    /// module's static storage region (the target adds its static base). Static fields holding an
    /// `ObjectRef` are GC roots the collector must scan; only scalar statics are lowered so far.
    StaticLoad {
        /// The field's byte offset within the static region.
        offset: u32,
    },
    /// Stores `value` into a static field -- the CLI's `stsfld`. A side effect; the result is a
    /// placeholder callers ignore.
    StaticStore {
        /// The field's byte offset within the static region.
        offset: u32,
        /// The value to store.
        value: ValueId,
    },
}
