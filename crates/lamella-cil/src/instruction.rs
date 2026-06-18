//! Decoded CIL instructions: an opcode paired with its operand value.

use crate::opcode::{Opcode, OperandKind};
use alloc::boxed::Box;
use lamella_token::Token;

/// The decoded operand carried by an [`Instruction`].
///
/// There is one variant per shape of inline operand. Integer and float constants
/// are held by value; a [`Operand::Variable`] is a local-variable or argument
/// slot number whose encoded width the opcode fixes; a [`Operand::Target`] and
/// the cases of a [`Operand::Switch`] are indices into the instruction list, not
/// byte offsets; a [`Operand::Token`] is an unresolved metadata token, which the
/// runtime resolves on its own side.
#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    /// No operand ([`OperandKind::None`]).
    None,
    /// A signed 1-byte integer constant (`ldc.i4.s`).
    Int8(i8),
    /// A signed 4-byte integer constant (`ldc.i4`).
    Int32(i32),
    /// A signed 8-byte integer constant (`ldc.i8`).
    Int64(i64),
    /// A 4-byte IEEE-754 float constant (`ldc.r4`).
    Float32(f32),
    /// An 8-byte IEEE-754 float constant (`ldc.r8`).
    Float64(f64),
    /// A local-variable or argument slot number. The opcode fixes the encoded
    /// width: one byte for the `.s` forms, two for the `0xFE`-prefixed forms.
    Variable(u16),
    /// A branch target, as an index into the instruction list (`br`, `br.s`, the
    /// conditional branches, and `leave`/`leave.s`).
    Target(u32),
    /// A `switch` jump table: one instruction-list index per case, in order.
    Switch(Box<[u32]>),
    /// A 4-byte metadata token (`call`, `ldfld`, `ldstr`, `ldtoken`, ...), kept
    /// unresolved.
    Token(Token),
    /// The 1-byte alignment of an `unaligned.` prefix (1, 2, or 4).
    Alignment(u8),
}

impl Operand {
    /// Whether this operand has the shape that an opcode of `kind` requires.
    ///
    /// The short and long variable forms both accept [`Operand::Variable`], and
    /// the short and long branch forms both accept [`Operand::Target`]; every
    /// other kind maps to exactly one variant.
    #[must_use]
    pub fn is_compatible_with(&self, kind: OperandKind) -> bool {
        matches!(
            (self, kind),
            (Operand::None, OperandKind::None)
                | (Operand::Int8(_), OperandKind::Int8)
                | (Operand::Int32(_), OperandKind::Int32)
                | (Operand::Int64(_), OperandKind::Int64)
                | (Operand::Float32(_), OperandKind::Float32)
                | (Operand::Float64(_), OperandKind::Float64)
                | (
                    Operand::Variable(_),
                    OperandKind::ShortVariable | OperandKind::Variable
                )
                | (
                    Operand::Target(_),
                    OperandKind::ShortTarget | OperandKind::Target
                )
                | (Operand::Switch(_), OperandKind::Switch)
                | (Operand::Token(_), OperandKind::Token)
                | (Operand::Alignment(_), OperandKind::Alignment)
        )
    }
}

/// One decoded CIL instruction: an [`Opcode`] and its [`Operand`].
#[derive(Debug, Clone, PartialEq)]
pub struct Instruction {
    /// The operation.
    pub opcode: Opcode,
    /// The decoded operand; [`Operand::None`] when the opcode takes none.
    pub operand: Operand,
}

impl Instruction {
    /// Creates an instruction from an opcode and operand.
    #[must_use]
    pub fn new(opcode: Opcode, operand: Operand) -> Instruction {
        Instruction { opcode, operand }
    }

    /// A no-operand instruction such as `add` or `ret`.
    #[must_use]
    pub fn simple(opcode: Opcode) -> Instruction {
        Instruction {
            opcode,
            operand: Operand::None,
        }
    }

    /// Whether the operand matches the shape the opcode requires. The codec
    /// refuses to encode an inconsistent instruction.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        self.operand.is_compatible_with(self.opcode.operand_kind())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consistent_instructions_pair_opcode_and_operand() {
        assert!(Instruction::simple(Opcode::Add).is_consistent());
        assert!(Instruction::new(Opcode::LdcI4, Operand::Int32(7)).is_consistent());
        assert!(Instruction::new(Opcode::LdargS, Operand::Variable(3)).is_consistent());
        assert!(Instruction::new(Opcode::Ldarg, Operand::Variable(300)).is_consistent());
        assert!(Instruction::new(Opcode::BrS, Operand::Target(0)).is_consistent());
        assert!(Instruction::new(Opcode::Br, Operand::Target(0)).is_consistent());
        assert!(Instruction::new(Opcode::Unaligned, Operand::Alignment(4)).is_consistent());
    }

    #[test]
    fn mismatched_operands_are_rejected() {
        assert!(!Instruction::new(Opcode::Add, Operand::Int32(1)).is_consistent());
        assert!(!Instruction::new(Opcode::LdcI4, Operand::None).is_consistent());
        assert!(!Instruction::new(Opcode::LdcI4, Operand::Int8(1)).is_consistent());
        assert!(!Instruction::new(Opcode::BrS, Operand::Variable(0)).is_consistent());
    }
}
