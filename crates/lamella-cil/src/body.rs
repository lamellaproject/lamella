//! Method bodies: the II.25.4 header, the instruction stream, and the
//! exception-handling clauses, decoded together.

use crate::codec::{self, DecodeError, EncodeError};
use crate::instruction::Instruction;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt;
use lamella_token::Token;

const FORMAT_MASK: u8 = 0x3;
const TINY_FORMAT: u8 = 0x2;
const FAT_FORMAT: u8 = 0x3;
const FAT_HEADER_LEN: usize = 12;
const FLAG_MORE_SECTS: u16 = 0x8;
const FLAG_INIT_LOCALS: u16 = 0x10;

const SECT_EH_TABLE: u8 = 0x1;
const SECT_FAT_FORMAT: u8 = 0x40;
const SECT_MORE_SECTS: u8 = 0x80;

const CLAUSE_FILTER: u32 = 0x1;
const CLAUSE_FINALLY: u32 = 0x2;
const CLAUSE_FAULT: u32 = 0x4;

/// A decoded method body: the header values, the instruction list, and the
/// exception-handling clauses (ECMA-335 1st ed, II.24.4).
///
/// The local-variable signature is kept as its StandAloneSig [`Token`]
/// ([`MethodBodyImage::local_var_sig`]); resolving it to a signature is
/// `lamella-metadata`'s job.
#[derive(Debug, Clone, PartialEq)]
pub struct MethodBodyImage {
    /// The maximum depth of the evaluation stack (II.24.4.3); 8 for a tiny body.
    pub max_stack: u16,
    /// Whether the runtime must zero the locals on entry (`CorILMethod_InitLocals`).
    pub init_locals: bool,
    /// The StandAloneSig token describing the local variables, or `None` when the
    /// method declares no locals (a token of 0).
    pub local_var_sig: Option<Token>,
    /// The decoded instruction stream.
    pub code: Box<[Instruction]>,
    /// The exception-handling clauses, in file order.
    pub handlers: Box<[EhClause]>,
}

/// One exception-handling clause: a protected (try) region, a handler region, and
/// what the handler is (II.24.4.6). Regions are half-open instruction-index
/// ranges, resolved from the clause's byte offsets at decode time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EhClause {
    /// The protected region the handler guards.
    pub try_range: InstructionRange,
    /// The handler's own region.
    pub handler_range: InstructionRange,
    /// What kind of handler this is, and any type or filter it carries.
    pub kind: EhKind,
}

/// What an [`EhClause`] does (II.24.4.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EhKind {
    /// A typed `catch`, carrying the caught type's metadata token.
    Catch(Token),
    /// A filtered handler; the filter code begins at this instruction index.
    Filter {
        /// The instruction index where the filter expression starts.
        filter_start: u32,
    },
    /// A `finally` handler, run on both normal exit and exception.
    Finally,
    /// A `fault` handler, run only when an exception leaves the try region.
    Fault,
}

/// A half-open `[start, end)` range of instruction indices. `end` may equal the
/// instruction count, denoting a region that runs to the end of the method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstructionRange {
    /// The first instruction in the range.
    pub start: u32,
    /// One past the last instruction in the range.
    pub end: u32,
}

/// Why a method body could not be read or written. Reported instead of panicking.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BodyError {
    /// The bytes ended before a complete header, code, or section was available.
    UnexpectedEnd,
    /// The header's format bits were neither tiny nor fat (II.24.4.1).
    BadHeaderFormat {
        /// The first header byte whose low bits named no format.
        first_byte: u8,
    },
    /// A data section was not a supported exception-handling table (II.24.4.5).
    UnsupportedSection {
        /// The section's kind byte.
        kind: u8,
    },
    /// A section's declared size was not a whole number of clauses.
    BadSectionSize {
        /// The declared data size, including the 4-byte section header.
        data_size: u32,
    },
    /// A clause's flags named no known handler kind (II.24.4.6).
    BadClauseKind {
        /// The unrecognised clause flags.
        flags: u32,
    },
    /// A try, handler, or filter region did not align to an instruction boundary.
    RegionNotAtBoundary {
        /// The byte offset that did not begin (or end) an instruction.
        offset: u32,
    },
    /// A region named an instruction index outside the method (on the way out).
    RegionIndexOutOfRange {
        /// The offending instruction index.
        index: u32,
    },
    /// The encoded body or an exception section would exceed its size field.
    TooLarge,
    /// Decoding the instruction stream failed.
    Code(DecodeError),
    /// Encoding the instruction stream failed.
    Encode(EncodeError),
}

impl From<DecodeError> for BodyError {
    fn from(error: DecodeError) -> BodyError {
        BodyError::Code(error)
    }
}

impl From<EncodeError> for BodyError {
    fn from(error: EncodeError) -> BodyError {
        BodyError::Encode(error)
    }
}

impl fmt::Display for BodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BodyError::UnexpectedEnd => f.write_str("method body ended unexpectedly"),
            BodyError::BadHeaderFormat { first_byte } => {
                write!(
                    f,
                    "method header byte 0x{first_byte:02X} is neither tiny nor fat"
                )
            }
            BodyError::UnsupportedSection { kind } => {
                write!(f, "unsupported method data section kind 0x{kind:02X}")
            }
            BodyError::BadSectionSize { data_size } => {
                write!(
                    f,
                    "exception section size {data_size} is not a whole number of clauses"
                )
            }
            BodyError::BadClauseKind { flags } => {
                write!(
                    f,
                    "exception clause flags 0x{flags:X} name no known handler kind"
                )
            }
            BodyError::RegionNotAtBoundary { offset } => {
                write!(
                    f,
                    "exception region offset {offset} is not an instruction boundary"
                )
            }
            BodyError::RegionIndexOutOfRange { index } => {
                write!(
                    f,
                    "exception region instruction index {index} is out of range"
                )
            }
            BodyError::TooLarge => f.write_str("method body exceeds its size field"),
            BodyError::Code(error) => write!(f, "method code: {error}"),
            BodyError::Encode(error) => write!(f, "method code: {error}"),
        }
    }
}

/// Reads a complete method body (header, CIL, and any exception section) into a
/// [`MethodBodyImage`].
///
/// # Errors
/// Returns a [`BodyError`] for a truncated or malformed header, code, or
/// exception section.
pub fn read_method_body(bytes: &[u8]) -> Result<MethodBodyImage, BodyError> {
    let first = *bytes.first().ok_or(BodyError::UnexpectedEnd)?;
    let (header_len, max_stack, init_locals, local_var_sig, code_size, more_sections) =
        match first & FORMAT_MASK {
            TINY_FORMAT => (1usize, 8u16, false, None, (first >> 2) as usize, false),
            FAT_FORMAT => read_fat_header(bytes)?,
            _ => return Err(BodyError::BadHeaderFormat { first_byte: first }),
        };

    let code_bytes = bytes
        .get(header_len..header_len.saturating_add(code_size))
        .ok_or(BodyError::UnexpectedEnd)?;
    let (code, offsets) = codec::decode_with_offsets(code_bytes)?;

    let handlers = if more_sections {
        let section_start = round_up_to_4(header_len.saturating_add(code_size));
        read_sections(bytes, section_start, &offsets, code_size as u32)?
    } else {
        Vec::new()
    };

    Ok(MethodBodyImage {
        max_stack,
        init_locals,
        local_var_sig,
        code: code.into_boxed_slice(),
        handlers: handlers.into_boxed_slice(),
    })
}

type FatHeader = (usize, u16, bool, Option<Token>, usize, bool);

fn read_fat_header(bytes: &[u8]) -> Result<FatHeader, BodyError> {
    let header = bytes
        .get(..FAT_HEADER_LEN)
        .ok_or(BodyError::UnexpectedEnd)?;
    let flags_and_size = u16::from_le_bytes([header[0], header[1]]);
    let flags = flags_and_size & 0x0FFF;
    let max_stack = u16::from_le_bytes([header[2], header[3]]);
    let code_size = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    let sig = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
    Ok((
        FAT_HEADER_LEN,
        max_stack,
        flags & FLAG_INIT_LOCALS != 0,
        (sig != 0).then_some(Token(sig)),
        code_size,
        flags & FLAG_MORE_SECTS != 0,
    ))
}

fn read_sections(
    bytes: &[u8],
    mut pos: usize,
    offsets: &[u32],
    code_size: u32,
) -> Result<Vec<EhClause>, BodyError> {
    let mut handlers = Vec::new();
    loop {
        let kind = *bytes.get(pos).ok_or(BodyError::UnexpectedEnd)?;
        if kind & SECT_EH_TABLE == 0 {
            return Err(BodyError::UnsupportedSection { kind });
        }
        let fat = kind & SECT_FAT_FORMAT != 0;
        let (data_size, clause_len) = if fat {
            let head = bytes.get(pos..pos + 4).ok_or(BodyError::UnexpectedEnd)?;
            (
                u32::from_le_bytes([head[1], head[2], head[3], 0]) as usize,
                24usize,
            )
        } else {
            (
                *bytes.get(pos + 1).ok_or(BodyError::UnexpectedEnd)? as usize,
                12usize,
            )
        };
        if data_size < 4 || (data_size - 4) % clause_len != 0 {
            return Err(BodyError::BadSectionSize {
                data_size: data_size as u32,
            });
        }
        let count = (data_size - 4) / clause_len;
        let clauses = bytes
            .get(pos + 4..pos + 4 + count * clause_len)
            .ok_or(BodyError::UnexpectedEnd)?;
        for clause in clauses.chunks_exact(clause_len) {
            handlers.push(read_clause(clause, fat, offsets, code_size)?);
        }
        if kind & SECT_MORE_SECTS == 0 {
            break;
        }
        pos = round_up_to_4(pos + data_size);
    }
    Ok(handlers)
}

fn read_clause(
    clause: &[u8],
    fat: bool,
    offsets: &[u32],
    code_size: u32,
) -> Result<EhClause, BodyError> {
    let (flags, try_offset, try_len, handler_offset, handler_len, last) = if fat {
        (
            le32(clause, 0),
            le32(clause, 4),
            le32(clause, 8),
            le32(clause, 12),
            le32(clause, 16),
            le32(clause, 20),
        )
    } else {
        (
            le16(clause, 0),
            le16(clause, 2),
            clause[4] as u32,
            le16(clause, 5),
            clause[7] as u32,
            le32(clause, 8),
        )
    };
    let try_range = resolve_region(offsets, code_size, try_offset, try_len)?;
    let handler_range = resolve_region(offsets, code_size, handler_offset, handler_len)?;
    let kind = if flags & CLAUSE_FILTER != 0 {
        EhKind::Filter {
            filter_start: resolve_offset(offsets, code_size, last)?,
        }
    } else if flags & CLAUSE_FINALLY != 0 {
        EhKind::Finally
    } else if flags & CLAUSE_FAULT != 0 {
        EhKind::Fault
    } else if flags == 0 {
        EhKind::Catch(Token(last))
    } else {
        return Err(BodyError::BadClauseKind { flags });
    };
    Ok(EhClause {
        try_range,
        handler_range,
        kind,
    })
}

/// Writes a method body back to bytes: a tiny header when the body is eligible,
/// otherwise a fat header, and a fat exception section when there are handlers.
///
/// # Errors
/// Returns a [`BodyError`] if the instruction stream cannot be encoded or a
/// region names an instruction index that does not exist.
pub fn write_method_body(body: &MethodBodyImage) -> Result<Vec<u8>, BodyError> {
    let (code, offsets) = codec::encode_with_offsets(&body.code)?;
    let mut out = Vec::new();

    let tiny = body.handlers.is_empty()
        && body.local_var_sig.is_none()
        && !body.init_locals
        && body.max_stack <= 8
        && code.len() < 64;

    if tiny {
        out.push(((code.len() as u8) << 2) | TINY_FORMAT);
        out.extend_from_slice(&code);
        return Ok(out);
    }

    let mut flags = FAT_FORMAT as u16;
    if body.init_locals {
        flags |= FLAG_INIT_LOCALS;
    }
    if !body.handlers.is_empty() {
        flags |= FLAG_MORE_SECTS;
    }
    out.extend_from_slice(&(flags | ((FAT_HEADER_LEN as u16 / 4) << 12)).to_le_bytes());
    out.extend_from_slice(&body.max_stack.to_le_bytes());
    out.extend_from_slice(&(code.len() as u32).to_le_bytes());
    out.extend_from_slice(&body.local_var_sig.map_or(0, |token| token.0).to_le_bytes());
    out.extend_from_slice(&code);

    if !body.handlers.is_empty() {
        write_eh_section(&mut out, &body.handlers, &offsets)?;
    }
    Ok(out)
}

fn write_eh_section(
    out: &mut Vec<u8>,
    handlers: &[EhClause],
    offsets: &[u32],
) -> Result<(), BodyError> {
    while out.len() % 4 != 0 {
        out.push(0);
    }
    let data_size = handlers
        .len()
        .checked_mul(24)
        .and_then(|clauses| clauses.checked_add(4))
        .filter(|size| *size <= 0x00FF_FFFF)
        .ok_or(BodyError::TooLarge)? as u32;
    out.push(SECT_EH_TABLE | SECT_FAT_FORMAT);
    out.extend_from_slice(&data_size.to_le_bytes()[..3]);
    for clause in handlers {
        let (flags, last) = match clause.kind {
            EhKind::Catch(token) => (0u32, token.0),
            EhKind::Filter { filter_start } => (CLAUSE_FILTER, offset_of(offsets, filter_start)?),
            EhKind::Finally => (CLAUSE_FINALLY, 0),
            EhKind::Fault => (CLAUSE_FAULT, 0),
        };
        let (try_offset, try_len) = region_bytes(offsets, clause.try_range)?;
        let (handler_offset, handler_len) = region_bytes(offsets, clause.handler_range)?;
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&try_offset.to_le_bytes());
        out.extend_from_slice(&try_len.to_le_bytes());
        out.extend_from_slice(&handler_offset.to_le_bytes());
        out.extend_from_slice(&handler_len.to_le_bytes());
        out.extend_from_slice(&last.to_le_bytes());
    }
    Ok(())
}

/// Maps a clause's `[offset, offset + length)` byte region to an instruction-index
/// range, allowing the end to be the end of the code.
fn resolve_region(
    offsets: &[u32],
    code_size: u32,
    offset: u32,
    length: u32,
) -> Result<InstructionRange, BodyError> {
    let end = offset
        .checked_add(length)
        .ok_or(BodyError::RegionNotAtBoundary { offset })?;
    Ok(InstructionRange {
        start: resolve_offset(offsets, code_size, offset)?,
        end: resolve_offset(offsets, code_size, end)?,
    })
}

/// Maps a byte offset to the index of the instruction that begins there, treating
/// the code size as the index one past the last instruction.
fn resolve_offset(offsets: &[u32], code_size: u32, offset: u32) -> Result<u32, BodyError> {
    if offset == code_size {
        return Ok(offsets.len() as u32);
    }
    offsets
        .binary_search(&offset)
        .map(|index| index as u32)
        .map_err(|_| BodyError::RegionNotAtBoundary { offset })
}

/// The byte offset of the instruction at `index`, where `index` may be the
/// instruction count (the end of the code). `offsets` has one entry per
/// instruction plus a final total, as [`codec::encode_with_offsets`] returns.
fn offset_of(offsets: &[u32], index: u32) -> Result<u32, BodyError> {
    offsets
        .get(index as usize)
        .copied()
        .ok_or(BodyError::RegionIndexOutOfRange { index })
}

fn region_bytes(offsets: &[u32], range: InstructionRange) -> Result<(u32, u32), BodyError> {
    let start = offset_of(offsets, range.start)?;
    let end = offset_of(offsets, range.end)?;
    Ok((start, end.saturating_sub(start)))
}

fn round_up_to_4(value: usize) -> usize {
    value.wrapping_add(3) & !3
}

fn le16(bytes: &[u8], at: usize) -> u32 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]]) as u32
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instruction::Instruction;
    use crate::opcode::Opcode;
    use alloc::vec;

    fn body(code: Vec<Instruction>, handlers: Vec<EhClause>) -> MethodBodyImage {
        MethodBodyImage {
            max_stack: 8,
            init_locals: false,
            local_var_sig: None,
            code: code.into_boxed_slice(),
            handlers: handlers.into_boxed_slice(),
        }
    }

    fn round_trip(image: &MethodBodyImage) {
        let bytes = write_method_body(image).expect("write");
        let read = read_method_body(&bytes).expect("read");
        assert_eq!(&read, image, "structure round-trip");
        assert_eq!(
            write_method_body(&read).expect("rewrite"),
            bytes,
            "byte round-trip"
        );
    }

    #[test]
    fn tiny_header_for_two_plus_two() {
        let image = body(
            vec![
                Instruction::simple(Opcode::LdcI42),
                Instruction::simple(Opcode::LdcI42),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Ret),
            ],
            vec![],
        );
        let bytes = write_method_body(&image).unwrap();
        assert_eq!(bytes[0], (4 << 2) | 0x2);
        assert_eq!(bytes.len(), 5);
        round_trip(&image);
    }

    #[test]
    fn fat_header_when_locals_are_present() {
        let image = MethodBodyImage {
            max_stack: 3,
            init_locals: true,
            local_var_sig: Some(Token::new(0x11, 1)),
            code: vec![Instruction::simple(Opcode::Ret)].into_boxed_slice(),
            handlers: Box::new([]),
        };
        let bytes = write_method_body(&image).unwrap();
        assert_eq!(bytes[0] & FORMAT_MASK, FAT_FORMAT);
        round_trip(&image);
    }

    #[test]
    fn fat_header_when_the_code_is_large() {
        let mut code: Vec<Instruction> =
            (0..80).map(|_| Instruction::simple(Opcode::Nop)).collect();
        code.push(Instruction::simple(Opcode::Ret));
        let image = body(code, vec![]);
        let bytes = write_method_body(&image).unwrap();
        assert_eq!(bytes[0] & FORMAT_MASK, FAT_FORMAT);
        round_trip(&image);
    }

    #[test]
    fn round_trips_a_typed_catch() {
        let code = vec![
            Instruction::simple(Opcode::Nop),
            Instruction::simple(Opcode::Nop),
            Instruction::simple(Opcode::Nop),
            Instruction::simple(Opcode::Nop),
            Instruction::simple(Opcode::Ret),
        ];
        let handlers = vec![EhClause {
            try_range: InstructionRange { start: 0, end: 2 },
            handler_range: InstructionRange { start: 2, end: 4 },
            kind: EhKind::Catch(Token::new(0x01, 5)),
        }];
        round_trip(&body(code, handlers));
    }

    #[test]
    fn round_trips_finally_and_filter() {
        let code: Vec<Instruction> = (0..6).map(|_| Instruction::simple(Opcode::Nop)).collect();
        let handlers = vec![
            EhClause {
                try_range: InstructionRange { start: 0, end: 2 },
                handler_range: InstructionRange { start: 2, end: 4 },
                kind: EhKind::Finally,
            },
            EhClause {
                try_range: InstructionRange { start: 0, end: 2 },
                handler_range: InstructionRange { start: 4, end: 6 },
                kind: EhKind::Filter { filter_start: 3 },
            },
        ];
        round_trip(&body(code, handlers));
    }

    #[test]
    fn a_region_to_the_end_of_the_method_round_trips() {
        let code: Vec<Instruction> = (0..4).map(|_| Instruction::simple(Opcode::Nop)).collect();
        let handlers = vec![EhClause {
            try_range: InstructionRange { start: 0, end: 2 },
            handler_range: InstructionRange { start: 2, end: 4 },
            kind: EhKind::Fault,
        }];
        round_trip(&body(code, handlers));
    }

    #[test]
    fn empty_input_is_an_error_not_a_panic() {
        assert_eq!(read_method_body(&[]), Err(BodyError::UnexpectedEnd));
    }

    #[test]
    fn a_truncated_fat_header_is_an_error() {
        assert_eq!(
            read_method_body(&[0x03, 0x30, 0x01]),
            Err(BodyError::UnexpectedEnd)
        );
    }

    #[test]
    fn a_tiny_header_promising_too_much_code_is_an_error() {
        assert_eq!(
            read_method_body(&[(4 << 2) | 0x2]),
            Err(BodyError::UnexpectedEnd)
        );
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        let mut state = 0x0123_4567_89AB_CDEFu64;
        let mut buffer = Vec::new();
        for _ in 0..20_000 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let len = (state >> 56) as usize % 48;
            buffer.clear();
            for _ in 0..len {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                buffer.push((state >> 56) as u8);
            }
            let _ = read_method_body(&buffer);
        }
    }
}
