//! Decoding a CIL byte stream to instructions, and encoding it back.

use crate::instruction::{Instruction, Operand};
use crate::opcode::{EXTENDED_PREFIX, Opcode, OperandKind};
use alloc::vec::Vec;
use core::fmt;
use lamella_token::Token;

/// Why a byte stream could not be decoded into instructions. Reported instead of
/// panicking, so the decoder is safe on arbitrary input.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeError {
    /// The stream ended in the middle of an opcode or its operand.
    UnexpectedEnd {
        /// The byte offset at which a further byte was needed.
        offset: u32,
    },
    /// A byte, or a byte following the `0xFE` prefix, is not a defined opcode.
    UnknownOpcode {
        /// The byte offset of the opcode.
        offset: u32,
        /// The unrecognised byte (the second byte for a two-byte opcode).
        byte: u8,
        /// Whether the byte followed the `0xFE` prefix.
        extended: bool,
    },
    /// A branch or `switch` displacement points outside the method body.
    TargetOutOfRange {
        /// The byte offset of the instruction whose target is out of range.
        offset: u32,
    },
    /// A branch or `switch` displacement points into the middle of an
    /// instruction rather than at one of its boundaries.
    TargetNotAtBoundary {
        /// The byte offset of the instruction whose target is misaligned.
        offset: u32,
        /// The byte offset the target resolved to.
        target: u32,
    },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::UnexpectedEnd { offset } => {
                write!(
                    f,
                    "instruction stream ended unexpectedly at offset {offset}"
                )
            }
            DecodeError::UnknownOpcode {
                offset,
                byte,
                extended,
            } => {
                let prefix = if *extended { "0xFE " } else { "" };
                write!(f, "unknown opcode {prefix}0x{byte:02X} at offset {offset}")
            }
            DecodeError::TargetOutOfRange { offset } => {
                write!(f, "branch target out of range at offset {offset}")
            }
            DecodeError::TargetNotAtBoundary { offset, target } => write!(
                f,
                "branch target {target} is not an instruction boundary (from offset {offset})"
            ),
        }
    }
}

/// Why an instruction list could not be encoded to bytes. Reported instead of
/// panicking.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EncodeError {
    /// An instruction's operand shape does not match its opcode.
    OperandMismatch {
        /// The opcode whose operand was wrong.
        opcode: Opcode,
    },
    /// A branch or `switch` target names an instruction that does not exist.
    TargetIndexOutOfRange {
        /// The offending instruction index.
        index: u32,
    },
    /// A branch displacement does not fit the opcode's form: a signed byte for a
    /// short branch, or a signed 4-byte integer otherwise.
    DisplacementOutOfRange,
    /// A short-form variable slot number does not fit in a single byte.
    VariableOutOfRange {
        /// The slot number that was too large.
        slot: u16,
    },
    /// The encoded body would exceed the addressable 4-byte size.
    CodeTooLarge,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::OperandMismatch { opcode } => {
                write!(f, "operand does not match opcode {}", opcode.mnemonic())
            }
            EncodeError::TargetIndexOutOfRange { index } => {
                write!(f, "branch target index {index} is out of range")
            }
            EncodeError::DisplacementOutOfRange => f.write_str("branch displacement out of range"),
            EncodeError::VariableOutOfRange { slot } => {
                write!(f, "variable slot {slot} does not fit a short-form operand")
            }
            EncodeError::CodeTooLarge => f.write_str("encoded method body is too large"),
        }
    }
}

/// A forward-only reader over the instruction bytes that never indexes out of
/// bounds: every read past the end yields [`DecodeError::UnexpectedEnd`].
struct Reader<'a> {
    code: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(code: &'a [u8]) -> Reader<'a> {
        Reader { code, pos: 0 }
    }

    fn at_end(&self) -> bool {
        self.pos >= self.code.len()
    }

    fn offset(&self) -> u32 {
        self.pos as u32
    }

    fn read_u8(&mut self) -> Result<u8, DecodeError> {
        let byte = *self.code.get(self.pos).ok_or(DecodeError::UnexpectedEnd {
            offset: self.offset(),
        })?;
        self.pos += 1;
        Ok(byte)
    }

    fn read_bytes<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        match self.code.get(self.pos..).and_then(<[u8]>::first_chunk::<N>) {
            Some(chunk) => {
                self.pos += N;
                Ok(*chunk)
            }
            None => Err(DecodeError::UnexpectedEnd {
                offset: self.code.len() as u32,
            }),
        }
    }

    fn read_i8(&mut self) -> Result<i8, DecodeError> {
        Ok(self.read_u8()? as i8)
    }

    fn read_u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.read_bytes()?))
    }

    fn read_i32(&mut self) -> Result<i32, DecodeError> {
        Ok(i32::from_le_bytes(self.read_bytes()?))
    }

    fn read_u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.read_bytes()?))
    }

    fn read_i64(&mut self) -> Result<i64, DecodeError> {
        Ok(i64::from_le_bytes(self.read_bytes()?))
    }

    fn read_f32(&mut self) -> Result<f32, DecodeError> {
        Ok(f32::from_bits(u32::from_le_bytes(self.read_bytes()?)))
    }

    fn read_f64(&mut self) -> Result<f64, DecodeError> {
        Ok(f64::from_bits(u64::from_le_bytes(self.read_bytes()?)))
    }
}

/// Decodes the instruction bytes of a method body into a list of instructions.
///
/// Branch and `switch` operands are resolved from byte displacements to indices
/// into the returned list. An empty input decodes to an empty list.
///
/// # Errors
/// Returns a [`DecodeError`] for a truncated stream, an undefined opcode, or a
/// branch or `switch` target that lands outside the body or off an instruction
/// boundary.
pub fn decode(code: &[u8]) -> Result<Vec<Instruction>, DecodeError> {
    Ok(decode_with_offsets(code)?.0)
}

/// Like [`decode`], but also returns the starting byte offset of each
/// instruction, in the same order as the instruction list.
///
/// The offset table maps an instruction index to where its opcode begins in the
/// stream; the method-body reader uses it to resolve exception-handling regions,
/// and a debugger uses it to map sequence points. `offsets[i]` is the offset of
/// instruction `i`, and the stream length is one past the last instruction.
///
/// # Errors
/// The same conditions as [`decode`].
pub fn decode_with_offsets(code: &[u8]) -> Result<(Vec<Instruction>, Vec<u32>), DecodeError> {
    let mut reader = Reader::new(code);
    let mut instructions = Vec::new();
    let mut offsets = Vec::new();

    while !reader.at_end() {
        let start = reader.offset();
        offsets.push(start);
        let opcode = decode_opcode(&mut reader, start)?;
        let operand = decode_operand(&mut reader, opcode, start)?;
        instructions.push(Instruction { opcode, operand });
    }

    resolve_targets(&mut instructions, &offsets)?;
    Ok((instructions, offsets))
}

fn decode_opcode(reader: &mut Reader<'_>, start: u32) -> Result<Opcode, DecodeError> {
    let first = reader.read_u8()?;
    if first == EXTENDED_PREFIX {
        let second = reader.read_u8()?;
        Opcode::from_extended(second).ok_or(DecodeError::UnknownOpcode {
            offset: start,
            byte: second,
            extended: true,
        })
    } else {
        Opcode::from_single(first).ok_or(DecodeError::UnknownOpcode {
            offset: start,
            byte: first,
            extended: false,
        })
    }
}

fn decode_operand(
    reader: &mut Reader<'_>,
    opcode: Opcode,
    start: u32,
) -> Result<Operand, DecodeError> {
    Ok(match opcode.operand_kind() {
        OperandKind::None => Operand::None,
        OperandKind::Int8 => Operand::Int8(reader.read_i8()?),
        OperandKind::Int32 => Operand::Int32(reader.read_i32()?),
        OperandKind::Int64 => Operand::Int64(reader.read_i64()?),
        OperandKind::Float32 => Operand::Float32(reader.read_f32()?),
        OperandKind::Float64 => Operand::Float64(reader.read_f64()?),
        OperandKind::ShortVariable => Operand::Variable(reader.read_u8()? as u16),
        OperandKind::Variable => Operand::Variable(reader.read_u16()?),
        OperandKind::Alignment => Operand::Alignment(reader.read_u8()?),
        OperandKind::Token => Operand::Token(Token(reader.read_u32()?)),
        OperandKind::ShortTarget => {
            let displacement = reader.read_i8()? as i64;
            Operand::Target(branch_target(reader.offset(), displacement, start)?)
        }
        OperandKind::Target => {
            let displacement = reader.read_i32()? as i64;
            Operand::Target(branch_target(reader.offset(), displacement, start)?)
        }
        OperandKind::Switch => {
            let count = reader.read_u32()?;
            let mut displacements = Vec::new();
            for _ in 0..count {
                displacements.push(reader.read_i32()? as i64);
            }
            let base = reader.offset();
            let mut targets = Vec::new();
            for displacement in displacements {
                targets.push(branch_target(base, displacement, start)?);
            }
            Operand::Switch(targets.into_boxed_slice())
        }
    })
}

/// Resolves an absolute byte target from a displacement relative to `base` (the
/// offset following the current instruction), keeping it within the body.
fn branch_target(base: u32, displacement: i64, start: u32) -> Result<u32, DecodeError> {
    let target = base as i64 + displacement;
    u32::try_from(target).map_err(|_| DecodeError::TargetOutOfRange { offset: start })
}

/// Rewrites the byte targets left in branch and `switch` operands into indices
/// into the instruction list, failing if a target is not an instruction start.
fn resolve_targets(instructions: &mut [Instruction], offsets: &[u32]) -> Result<(), DecodeError> {
    let index_of = |byte: u32, start: u32| {
        offsets
            .binary_search(&byte)
            .map(|index| index as u32)
            .map_err(|_| DecodeError::TargetNotAtBoundary {
                offset: start,
                target: byte,
            })
    };
    for (index, instruction) in instructions.iter_mut().enumerate() {
        let start = offsets[index];
        match &mut instruction.operand {
            Operand::Target(target) => *target = index_of(*target, start)?,
            Operand::Switch(targets) => {
                for target in targets.iter_mut() {
                    *target = index_of(*target, start)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Encodes a list of instructions into the bytes of a method body.
///
/// Branch and `switch` targets, held as instruction indices, are turned back
/// into byte displacements in each opcode's form.
///
/// # Errors
/// Returns an [`EncodeError`] for an instruction whose operand does not match its
/// opcode, a target index that names no instruction, a displacement or slot that
/// does not fit its form, or a body that would exceed four gigabytes.
pub fn encode(instructions: &[Instruction]) -> Result<Vec<u8>, EncodeError> {
    Ok(encode_with_offsets(instructions)?.0)
}

/// Like [`encode`], but also returns the byte offset of each instruction plus a
/// final entry holding the total length, so `offsets[i]` is the offset of
/// instruction `i` and `offsets[len]` is the encoded size. The method-body writer
/// uses it to turn exception-handling instruction indices back into byte regions.
///
/// # Errors
/// The same conditions as [`encode`].
pub fn encode_with_offsets(
    instructions: &[Instruction],
) -> Result<(Vec<u8>, Vec<u32>), EncodeError> {
    let offsets = layout(instructions)?;
    let total = *offsets.last().unwrap_or(&0);
    let mut out = Vec::with_capacity(total as usize);

    for (index, instruction) in instructions.iter().enumerate() {
        match instruction.opcode.encoding() {
            crate::opcode::Encoding::Single(byte) => out.push(byte),
            crate::opcode::Encoding::Extended(byte) => {
                out.push(EXTENDED_PREFIX);
                out.push(byte);
            }
        }
        let next = offsets[index + 1];
        encode_operand(instruction, next, &offsets, &mut out)?;
    }

    Ok((out, offsets))
}

/// Computes the byte offset of each instruction plus a final entry for the total
/// length, so `offsets[i + 1]` is the offset following instruction `i`.
fn layout(instructions: &[Instruction]) -> Result<Vec<u32>, EncodeError> {
    let mut offsets = Vec::with_capacity(instructions.len() + 1);
    let mut total = 0u32;
    for instruction in instructions {
        offsets.push(total);
        total = total
            .checked_add(instruction_size(instruction)?)
            .ok_or(EncodeError::CodeTooLarge)?;
    }
    offsets.push(total);
    Ok(offsets)
}

fn instruction_size(instruction: &Instruction) -> Result<u32, EncodeError> {
    if !instruction.is_consistent() {
        return Err(EncodeError::OperandMismatch {
            opcode: instruction.opcode,
        });
    }
    let opcode_bytes = instruction.opcode.encoding().byte_len() as u32;
    let operand_bytes = match instruction.opcode.operand_kind() {
        OperandKind::Switch => {
            let Operand::Switch(targets) = &instruction.operand else {
                return Err(EncodeError::OperandMismatch {
                    opcode: instruction.opcode,
                });
            };
            let cases = u32::try_from(targets.len()).map_err(|_| EncodeError::CodeTooLarge)?;
            cases
                .checked_mul(4)
                .and_then(|table| table.checked_add(4))
                .ok_or(EncodeError::CodeTooLarge)?
        }
        kind => kind.fixed_operand_len().unwrap_or(0) as u32,
    };
    opcode_bytes
        .checked_add(operand_bytes)
        .ok_or(EncodeError::CodeTooLarge)
}

fn encode_operand(
    instruction: &Instruction,
    next: u32,
    offsets: &[u32],
    out: &mut Vec<u8>,
) -> Result<(), EncodeError> {
    let target_offset = |index: u32| {
        offsets
            .get(index as usize)
            .copied()
            .ok_or(EncodeError::TargetIndexOutOfRange { index })
    };
    match &instruction.operand {
        Operand::None => {}
        Operand::Int8(value) => out.push(*value as u8),
        Operand::Int32(value) => out.extend_from_slice(&value.to_le_bytes()),
        Operand::Int64(value) => out.extend_from_slice(&value.to_le_bytes()),
        Operand::Float32(value) => out.extend_from_slice(&value.to_bits().to_le_bytes()),
        Operand::Float64(value) => out.extend_from_slice(&value.to_bits().to_le_bytes()),
        Operand::Token(token) => out.extend_from_slice(&token.0.to_le_bytes()),
        Operand::Alignment(value) => out.push(*value),
        Operand::Variable(slot) => {
            if instruction.opcode.operand_kind() == OperandKind::ShortVariable {
                let byte = u8::try_from(*slot)
                    .map_err(|_| EncodeError::VariableOutOfRange { slot: *slot })?;
                out.push(byte);
            } else {
                out.extend_from_slice(&slot.to_le_bytes());
            }
        }
        Operand::Target(index) => {
            let displacement = target_offset(*index)? as i64 - next as i64;
            if instruction.opcode.operand_kind() == OperandKind::ShortTarget {
                let byte =
                    i8::try_from(displacement).map_err(|_| EncodeError::DisplacementOutOfRange)?;
                out.push(byte as u8);
            } else {
                let word =
                    i32::try_from(displacement).map_err(|_| EncodeError::DisplacementOutOfRange)?;
                out.extend_from_slice(&word.to_le_bytes());
            }
        }
        Operand::Switch(targets) => {
            let count = u32::try_from(targets.len()).map_err(|_| EncodeError::CodeTooLarge)?;
            out.extend_from_slice(&count.to_le_bytes());
            for index in targets.iter() {
                let displacement = target_offset(*index)? as i64 - next as i64;
                let word =
                    i32::try_from(displacement).map_err(|_| EncodeError::DisplacementOutOfRange)?;
                out.extend_from_slice(&word.to_le_bytes());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instruction::Instruction;
    use alloc::vec;

    fn round_trip(instructions: &[Instruction]) {
        let bytes = encode(instructions).expect("encode");
        let decoded = decode(&bytes).expect("decode");
        assert_eq!(decoded, instructions, "structure round-trip");
        let reencoded = encode(&decoded).expect("re-encode");
        assert_eq!(reencoded, bytes, "byte round-trip");
    }

    #[test]
    fn evaluates_two_plus_two() {
        let program = vec![
            Instruction::simple(Opcode::LdcI42),
            Instruction::simple(Opcode::LdcI42),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::Ret),
        ];
        assert_eq!(encode(&program).unwrap(), [0x18, 0x18, 0x58, 0x2A]);
        round_trip(&program);
    }

    #[test]
    fn encodes_inline_constants_little_endian() {
        let program = vec![
            Instruction::new(Opcode::LdcI4, Operand::Int32(1000)),
            Instruction::new(Opcode::LdcI8, Operand::Int64(-1)),
            Instruction::new(Opcode::LdcI4S, Operand::Int8(-2)),
            Instruction::simple(Opcode::Ret),
        ];
        let bytes = encode(&program).unwrap();
        assert_eq!(&bytes[0..5], [0x20, 0xE8, 0x03, 0x00, 0x00]);
        round_trip(&program);
    }

    #[test]
    fn round_trips_floats_including_their_bits() {
        let program = vec![
            Instruction::new(Opcode::LdcR4, Operand::Float32(1.5)),
            Instruction::new(Opcode::LdcR8, Operand::Float64(-0.0)),
            Instruction::simple(Opcode::Ret),
        ];
        round_trip(&program);
    }

    #[test]
    fn round_trips_a_token_operand() {
        let program = vec![
            Instruction::new(Opcode::Call, Operand::Token(Token(0x0600_0001))),
            Instruction::new(Opcode::Ldstr, Operand::Token(Token(0x7000_0005))),
            Instruction::simple(Opcode::Ret),
        ];
        round_trip(&program);
    }

    #[test]
    fn resolves_a_forward_branch_to_an_index() {
        let program = vec![
            Instruction::simple(Opcode::LdcI40),
            Instruction::new(Opcode::BrfalseS, Operand::Target(3)),
            Instruction::simple(Opcode::Nop),
            Instruction::simple(Opcode::Ret),
        ];
        let bytes = encode(&program).unwrap();
        assert_eq!(bytes, [0x16, 0x2C, 0x01, 0x00, 0x2A]);
        round_trip(&program);
    }

    #[test]
    fn resolves_a_backward_branch() {
        let program = vec![
            Instruction::simple(Opcode::Nop),
            Instruction::new(Opcode::BrS, Operand::Target(0)),
        ];
        let bytes = encode(&program).unwrap();
        assert_eq!(bytes, [0x00, 0x2B, (-3i8) as u8]);
        round_trip(&program);
    }

    #[test]
    fn round_trips_a_switch_table() {
        let program = vec![
            Instruction::simple(Opcode::LdcI40),
            Instruction::new(
                Opcode::Switch,
                Operand::Switch(vec![3, 2].into_boxed_slice()),
            ),
            Instruction::simple(Opcode::Ret),
            Instruction::simple(Opcode::Ret),
        ];
        round_trip(&program);
    }

    #[test]
    fn decodes_two_byte_opcodes() {
        let program = vec![
            Instruction::new(Opcode::Ldarg, Operand::Variable(300)),
            Instruction::simple(Opcode::Ceq),
            Instruction::simple(Opcode::Ret),
        ];
        let bytes = encode(&program).unwrap();
        assert_eq!(&bytes[0..4], [0xFE, 0x09, 0x2C, 0x01]);
        round_trip(&program);
    }

    #[test]
    fn empty_code_decodes_to_no_instructions() {
        assert_eq!(decode(&[]).unwrap(), []);
        assert_eq!(encode(&[]).unwrap(), []);
    }

    #[test]
    fn truncated_operand_is_an_error_not_a_panic() {
        assert_eq!(
            decode(&[0x20, 0x01, 0x02]),
            Err(DecodeError::UnexpectedEnd { offset: 3 })
        );
        assert_eq!(
            decode(&[0xFE]),
            Err(DecodeError::UnexpectedEnd { offset: 1 })
        );
    }

    #[test]
    fn unknown_opcodes_are_reported() {
        assert_eq!(
            decode(&[0x24]),
            Err(DecodeError::UnknownOpcode {
                offset: 0,
                byte: 0x24,
                extended: false,
            })
        );
        assert_eq!(
            decode(&[0xFE, 0x08]),
            Err(DecodeError::UnknownOpcode {
                offset: 0,
                byte: 0x08,
                extended: true,
            })
        );
    }

    #[test]
    fn a_target_off_an_instruction_boundary_is_rejected() {
        let bytes = [0x2B, 0x01, 0x20, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(
            decode(&bytes),
            Err(DecodeError::TargetNotAtBoundary {
                offset: 0,
                target: 3
            })
        );
    }

    #[test]
    fn a_target_past_the_body_is_out_of_range() {
        let bytes = [0x2B, (-10i8) as u8];
        assert_eq!(
            decode(&bytes),
            Err(DecodeError::TargetOutOfRange { offset: 0 })
        );
    }

    #[test]
    fn encoding_an_inconsistent_instruction_fails() {
        let program = vec![Instruction::new(Opcode::Add, Operand::Int32(1))];
        assert_eq!(
            encode(&program),
            Err(EncodeError::OperandMismatch {
                opcode: Opcode::Add
            })
        );
    }

    #[test]
    fn encoding_an_out_of_reach_short_branch_fails() {
        let mut program = vec![Instruction::new(Opcode::BrS, Operand::Target(200))];
        program.extend((0..200).map(|_| Instruction::simple(Opcode::Nop)));
        program.push(Instruction::simple(Opcode::Ret));
        assert_eq!(encode(&program), Err(EncodeError::DisplacementOutOfRange));
    }

    #[test]
    fn encoding_an_overlarge_short_variable_fails() {
        let program = vec![Instruction::new(Opcode::LdargS, Operand::Variable(256))];
        assert_eq!(
            encode(&program),
            Err(EncodeError::VariableOutOfRange { slot: 256 })
        );
    }

    #[test]
    fn encoding_a_dangling_branch_index_fails() {
        let program = vec![Instruction::new(Opcode::Br, Operand::Target(9))];
        assert_eq!(
            encode(&program),
            Err(EncodeError::TargetIndexOutOfRange { index: 9 })
        );
    }

    #[test]
    fn arbitrary_bytes_never_panic_when_decoded() {
        let mut state = 0xDEAD_BEEF_F00D_CAFEu64;
        let mut buffer = Vec::new();
        for _ in 0..20_000 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let len = (state >> 56) as usize % 32;
            buffer.clear();
            for _ in 0..len {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                buffer.push((state >> 56) as u8);
            }
            if let Ok(instructions) = decode(&buffer) {
                let _ = encode(&instructions);
            }
        }
    }
}
