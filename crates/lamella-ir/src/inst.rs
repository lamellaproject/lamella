//! MIR instructions and the operators they use.

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
    /// A direct call to another function of the program (named by index), passing
    /// `args` and producing the callee's return value.
    Call {
        /// The index of the called function within the program.
        callee: u32,
        /// The argument values, in order (placed in the ABI's argument registers).
        args: Vec<ValueId>,
    },
}
