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
}

impl ConvKind {
    /// The [`MirType`] this conversion produces: `F32` for the int-to-float case, `int32` for the
    /// narrowing/extending and float-to-int cases.
    #[must_use]
    pub fn result_type(self) -> MirType {
        match self {
            ConvKind::IntToFloat32 => MirType::F32,
            ConvKind::SignExtend8
            | ConvKind::ZeroExtend8
            | ConvKind::SignExtend16
            | ConvKind::ZeroExtend16
            | ConvKind::Float32ToInt => MirType::I32,
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
    },
    /// Stores `value` to the 32-bit memory address held in `address` -- the
    /// memory-mapped-I/O write primitive. The write is a side effect; the
    /// instruction's own result value is a placeholder that callers ignore.
    Store {
        /// The value holding the destination address.
        address: ValueId,
        /// The value to write there.
        value: ValueId,
    },
    /// Loads the 32-bit value at the memory address held in `address` -- the
    /// memory-mapped-I/O read primitive. The instruction's result is the loaded value.
    Load {
        /// The value holding the source address.
        address: ValueId,
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
