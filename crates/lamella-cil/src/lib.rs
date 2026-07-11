#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! The CIL instruction model: opcodes and the instruction-stream codec.

extern crate alloc;

pub mod body;
pub mod codec;
pub mod instruction;
pub mod opcode;

pub use body::{
    BodyError, BodyLayout, EhClause, EhKind, InstructionRange, MethodBodyImage, read_body_layout,
    read_method_body, read_method_body_sized, write_method_body,
};
pub use codec::{
    DecodeError, EncodeError, decode, decode_at, decode_with_offsets, encode, encode_with_offsets,
    instruction_index_of_offset, instruction_offsets,
};
pub use instruction::{Instruction, Operand};
pub use opcode::{Encoding, Opcode, OperandKind};
