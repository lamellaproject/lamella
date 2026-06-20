//! MIR instructions and the operators they use.

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::function::ValueId;
use crate::types::MirType;

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
}
