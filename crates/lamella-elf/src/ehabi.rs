//! The Arm exception-handling ABI's unwinding tables, read: the index table a linked image carries
//! in `.ARM.exidx`, and the table entries it points into, usually in `.ARM.extab`.
//!
//! The tables exist so that an exception can propagate through a frame, and they are often the only
//! description of a function's frame an image carries: a toolchain that writes them need not write
//! DWARF call-frame information as well. So a stack walk reads them to recover a caller wherever
//! `.debug_frame` has no row.
//!
//! The pieces follow the specification's own. An [`IndexTable`] finds the entry for an address,
//! [`describe`] reads what that entry says, [`decode`] reads one frame-unwinding instruction, and
//! [`unwind`] runs a description's instructions over a frame's registers. [`tables`] finds the
//! tables in an ELF.
//!
//! Sources: *Exception Handling ABI for the Arm Architecture* (EHABI32, release 2025Q4) and *ELF for
//! the Arm Architecture* (AAELF32, release 2025Q4). Their source form numbers no clauses, so a
//! citation names a section heading.
//!
//! # A frame at a call site, and nowhere else
//!
//! The specification describes a function's frame as it stands where unwinding can start. For an
//! exception that is a call site: a function's entry and exit sequences are taken to be unable to
//! throw, so neither is described. A debugger stops anywhere. Every frame above the innermost is
//! stopped at a call, so a description is exact there; the innermost is not described while its
//! function is still building its frame or already taking it down, and a caller must settle those
//! cases some other way before it runs a description over that frame.
//!
//! # A description is data from elsewhere, and can be wrong
//!
//! [`unwind`] refuses a description that reads a saved register from below the frame's stack
//! pointer. Nothing below the stack pointer survives an interrupt, so no frame keeps a register
//! there, and a return address read from that memory belongs to no caller the program ever had.

use alloc::vec::Vec;

/// `SHT_ARM_EXIDX`, the section type of an index table (AAELF32 2025Q4, "Section Types").
///
/// A processor-specific number, so it means an index table only in a file whose machine is Arm.
pub const SHT_ARM_EXIDX: u32 = 0x7000_0001;

/// The second word of an index entry whose function's frames cannot be unwound (EHABI32 2025Q4,
/// "Index table entries").
pub const EXIDX_CANTUNWIND: u32 = 0x1;

/// `EM_ARM`, the machine number of a 32-bit Arm file (AAELF32 2025Q4, "ELF Header").
pub(crate) const EM_ARM: u16 = 40;

/// Bit 31 of a word: set in a compact table entry's first word, and clear in a prel31 offset.
const BIT_31: u32 = 0x8000_0000;

/// Bytes a linked image places at an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region<'a> {
    /// Where the first byte is loaded.
    pub address: u32,
    /// The bytes, in address order.
    pub bytes: &'a [u8],
}

impl Region<'_> {
    /// The little-endian word loaded at `address`, where this region holds all four of its bytes.
    #[must_use]
    pub fn word(&self, address: u32) -> Option<u32> {
        let offset = usize::try_from(address.checked_sub(self.address)?).ok()?;
        let bytes = self.bytes.get(offset..offset.checked_add(4)?)?;
        Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

/// Why a table could not be read, or a frame could not be unwound through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// An index table is not a whole number of two-word entries, or it runs past the end of the
    /// address space.
    IndexLength {
        /// The table's length in bytes.
        bytes: usize,
    },
    /// An index entry names a function that starts below the one before it. The table is searched by
    /// halving, which answers the wrong function on a table that is out of order.
    IndexOutOfOrder {
        /// The position of the first entry that is out of order.
        entry: usize,
    },
    /// An index entry's first word has bit 31 set, where the format has it clear.
    FunctionWordBit31 {
        /// The position of the entry.
        entry: usize,
    },
    /// A table entry, or one of its words, lies in no region the image loads.
    NotLoaded {
        /// The address that could not be read.
        address: u32,
    },
    /// An entry carried inline in the index counts additional words, which an inline entry has no
    /// room for.
    InlineLongEntry {
        /// How many additional words the entry counts.
        words: u8,
    },
    /// The index says the function's frames cannot be unwound.
    CantUnwind,
    /// The entry follows the generic model, in which how to unwind the frame is private to the
    /// entry's personality routine.
    GenericPersonality {
        /// The personality routine's address.
        routine: u32,
    },
    /// A compact entry names a personality routine index the ABI reserves.
    ReservedPersonality {
        /// The index, from 3 to 15.
        index: u8,
    },
    /// A compact entry's first word has one of bits 30 to 28 set, where the format has them clear.
    MalformedHeader {
        /// The word.
        word: u32,
    },
    /// The instruction that starts at offset `at` runs past the end of the instructions.
    Truncated {
        /// The offset of the instruction's first byte.
        at: usize,
    },
    /// The instruction at offset `at` is one the ABI reserves or leaves spare, and a personality
    /// routine does not unwind through it.
    Reserved {
        /// The offset of the instruction's first byte.
        at: usize,
        /// The instruction's first byte.
        opcode: u8,
    },
    /// The description refuses to unwind the frame.
    RefuseToUnwind,
    /// The description reads a saved register from below the frame's stack pointer, where no frame
    /// can have kept one.
    BelowStackPointer {
        /// The address the description would read.
        address: u32,
        /// The frame's stack pointer.
        stack_pointer: u32,
    },
    /// The description needs a register whose value in the frame is not known.
    UnknownRegister {
        /// The register's number.
        register: u8,
    },
    /// A saved register could not be read.
    Unreadable {
        /// The address that could not be read.
        address: u32,
    },
    /// An adjustment would carry the stack pointer outside the 32-bit address space.
    Overflow,
}

/// What an index entry's second word holds (EHABI32 2025Q4, "Index table entries").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Content {
    /// [`EXIDX_CANTUNWIND`]: the function's frames cannot be unwound.
    CantUnwind,
    /// The table entry itself, carried in the index because it fits in one word: the word, bit 31
    /// set.
    Inline(u32),
    /// The address of the table entry, resolved from the word's prel31 offset.
    Table(u32),
}

/// One two-word entry of an index table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexEntry {
    /// The address of the entry's first word.
    pub place: u32,
    /// The address of the function the entry describes, without the Thumb bit.
    pub function: u32,
    /// What the entry's second word holds.
    pub content: Content,
}

/// The address a prel31 offset in the word at `place` refers to: `place` plus bits 30 to 0 of `word`
/// taken as a signed number.
///
/// EHABI32 2025Q4, "Relocations"; AAELF32 2025Q4 gives `R_ARM_PREL31`'s addend as
/// `sign_extend(P[30:0])`. Bit 31 takes no part.
fn prel31(word: u32, place: u32) -> u32 {
    let offset = ((word << 1) as i32) >> 1;
    place.wrapping_add(offset as u32)
}

/// An index table: two-word entries in ascending order of the functions they describe.
///
/// EHABI32 2025Q4, "The binary searched index table" and "Index table entries". An entry covers its
/// function from its first address up to the next entry's; the table does not record where the last
/// function ends, so a caller that needs that bound takes it from somewhere else.
#[derive(Debug, Clone, Copy)]
pub struct IndexTable<'a> {
    region: Region<'a>,
}

impl<'a> IndexTable<'a> {
    /// Reads the index table loaded where `region` says.
    ///
    /// # Errors
    /// A table that is not whole entries, an entry whose first word has bit 31 set, and a table out of
    /// order are each refused here, once, so that no lookup ever answers from one.
    pub fn new(region: Region<'a>) -> Result<Self, Error> {
        let length = region.bytes.len();
        let fits = u32::try_from(length)
            .ok()
            .and_then(|length| region.address.checked_add(length))
            .is_some();
        if !length.is_multiple_of(8) || !fits {
            return Err(Error::IndexLength { bytes: length });
        }
        let table = IndexTable { region };
        let mut previous: Option<u32> = None;
        for position in 0..table.len() {
            let (entry, first) = table
                .read(position)
                .ok_or(Error::IndexLength { bytes: length })?;
            if first & BIT_31 != 0 {
                return Err(Error::FunctionWordBit31 { entry: position });
            }
            if previous.is_some_and(|previous| entry.function < previous) {
                return Err(Error::IndexOutOfOrder { entry: position });
            }
            previous = Some(entry.function);
        }
        Ok(table)
    }

    /// How many entries the table holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.region.bytes.len() / 8
    }

    /// Whether the table holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The entry at `position`.
    #[must_use]
    pub fn entry(&self, position: usize) -> Option<IndexEntry> {
        self.read(position).map(|(entry, _)| entry)
    }

    /// Every entry, in order.
    pub fn entries(&self) -> impl Iterator<Item = IndexEntry> + 'a {
        let table = *self;
        (0..table.len()).filter_map(move |position| table.entry(position))
    }

    /// The entry for the function containing `address`: the last entry whose function starts at or
    /// below it.
    ///
    /// `None` below the first function. Above the last function's start the last entry answers,
    /// because the table does not say where that function ends.
    #[must_use]
    pub fn lookup(&self, address: u32) -> Option<IndexEntry> {
        let (mut low, mut high) = (0, self.len());
        while low < high {
            let middle = low + (high - low) / 2;
            if self.entry(middle)?.function <= address {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        self.entry(low.checked_sub(1)?)
    }

    /// The entry at `position`, and its first word as the table holds it.
    fn read(&self, position: usize) -> Option<(IndexEntry, u32)> {
        let offset = u32::try_from(position.checked_mul(8)?).ok()?;
        let place = self.region.address.checked_add(offset)?;
        let first = self.region.word(place)?;
        let second = self.region.word(place.checked_add(4)?)?;
        let content = if second == EXIDX_CANTUNWIND {
            Content::CantUnwind
        } else if second & BIT_31 != 0 {
            Content::Inline(second)
        } else {
            Content::Table(prel31(second, place.wrapping_add(4)))
        };
        let function = prel31(first, place) & !1;
        Some((IndexEntry { place, function, content }, first))
    }
}

/// How a function's frames are unwound, as its table entry says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Description {
    /// The frames cannot be unwound.
    CantUnwind,
    /// Arm's compact model (EHABI32 2025Q4, "The Arm-defined compact model" and "Personality routine
    /// exception-handling table entries").
    Compact {
        /// The personality routine index: 0 for the short format, 1 or 2 for the long.
        personality: u8,
        /// Where the entry was read, or `None` for an entry carried inline in the index.
        table: Option<u32>,
        /// The frame-unwinding instructions: each word's bytes most significant first, words in
        /// ascending address order, and any padding after the last instruction included.
        instructions: Vec<u8>,
    },
    /// The generic model (EHABI32 2025Q4, "The generic model"): the entry begins with a prel31 offset
    /// to its personality routine, and everything after that is private to the routine.
    Generic {
        /// Where the entry was read.
        table: u32,
        /// The personality routine's address as the offset resolves; a Thumb routine keeps bit 0.
        routine: u32,
    },
}

/// What the table entry for `entry` says about unwinding its function's frames.
///
/// A table entry is read from whichever region in `loaded` holds it: the specification names the
/// section a producer writes entries into, and a link is free to gather them into any read-only
/// output section.
///
/// # Errors
/// An entry that cannot be read, that names a reserved personality index, whose header has reserved
/// bits set, or that is carried inline while counting additional words.
pub fn describe(entry: &IndexEntry, loaded: &[Region<'_>]) -> Result<Description, Error> {
    match entry.content {
        Content::CantUnwind => Ok(Description::CantUnwind),
        Content::Inline(word) => compact(word, None, |_| None),
        Content::Table(address) => {
            let read = |at: u32| loaded.iter().find_map(|region| region.word(at));
            let first = read(address).ok_or(Error::NotLoaded { address })?;
            if first & BIT_31 == 0 {
                return Ok(Description::Generic {
                    table: address,
                    routine: prel31(first, address),
                });
            }
            compact(first, Some(address), read)
        }
    }
}

/// A compact-model entry whose first word is `word`, read at `table` or inline, with `more` reading
/// the words after it.
///
/// EHABI32 2025Q4, "Personality routine exception-handling table entries": bits 27 to 24 select the
/// personality routine, and the figure of a compact entry has bit 31 set and bits 30 to 28 clear.
/// Index 0 carries three instruction bytes in bits 23 to 0. Indexes 1 and 2 count additional words
/// in bits 23 to 16 and carry two instruction bytes in bits 15 to 0, followed by all four bytes of
/// each additional word.
fn compact(
    word: u32,
    table: Option<u32>,
    more: impl Fn(u32) -> Option<u32>,
) -> Result<Description, Error> {
    if word & 0x7000_0000 != 0 {
        return Err(Error::MalformedHeader { word });
    }
    let [header, high, middle, low] = word.to_be_bytes();
    let personality = header & 0x0F;
    let instructions = match personality {
        0 => alloc::vec![high, middle, low],
        1 | 2 => {
            let words = high;
            let mut instructions = alloc::vec![middle, low];
            match table {
                None if words != 0 => return Err(Error::InlineLongEntry { words }),
                None => {}
                Some(address) => {
                    for count in 1..=u32::from(words) {
                        let at = address
                            .checked_add(4 * count)
                            .ok_or(Error::NotLoaded { address })?;
                        let next = more(at).ok_or(Error::NotLoaded { address: at })?;
                        instructions.extend_from_slice(&next.to_be_bytes());
                    }
                }
            }
            instructions
        }
        index => return Err(Error::ReservedPersonality { index }),
    };
    Ok(Description::Compact {
        personality,
        table,
        instructions,
    })
}

/// One frame-unwinding instruction (EHABI32 2025Q4, "Frame unwinding instructions").
///
/// Each moves a virtual stack pointer, `vsp`, which starts as the stack pointer of the frame being
/// unwound. A register mask has bit `n` set for register `rn`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Instruction {
    /// `00xxxxxx`, and `10110010` followed by a LEB128 value: `vsp = vsp + bytes`.
    AddToStackPointer(u32),
    /// `01xxxxxx`: `vsp = vsp - bytes`.
    SubtractFromStackPointer(u32),
    /// `10000000 00000000`: refuse to unwind.
    RefuseToUnwind,
    /// `1000iiii iiiiiiii`, `10100nnn`, `10101nnn` and `10110001 0000iiii`: pop the core registers in
    /// the mask, the lowest-numbered from the lowest address.
    PopCore(u16),
    /// `1001nnnn`: `vsp = rn`.
    StackPointerFromRegister(u8),
    /// `10110000`: finish.
    Finish,
    /// A pop of consecutive double-precision floating-point registers, and the stack bytes they
    /// occupy: 8 per register, plus 4 for those saved as if by `FSTMFDX` (the table's remark d).
    PopVfp {
        /// The first register's number.
        first: u8,
        /// How many registers.
        count: u8,
        /// The stack bytes the registers occupy.
        bytes: u32,
    },
    /// `10110100`: pop the return address authentication code pseudo-register, one word.
    PopAuthenticationCode,
    /// `10110101`: use the current `vsp` as the return address authentication modifier.
    AuthenticationModifier,
    /// A pop of consecutive Intel Wireless MMX data registers, 8 bytes each.
    PopWmmxData {
        /// The first register's number.
        first: u8,
        /// How many registers.
        count: u8,
    },
    /// `11000111 0000iiii`: pop the Intel Wireless MMX control registers in the mask, 4 bytes each.
    PopWmmxControl(u8),
    /// An instruction the ABI reserves or leaves spare, or a register range no Arm architecture has:
    /// the instruction's first byte.
    Reserved(u8),
}

/// Decodes the frame-unwinding instruction at offset `at` of `instructions`: the instruction, and how
/// many bytes it occupies.
///
/// EHABI32 2025Q4, "Frame unwinding instructions" and its table of Arm-defined instructions. A
/// register range past the last register the architecture has decodes as [`Instruction::Reserved`],
/// as that section's remark (e) says.
///
/// # Errors
/// [`Error::Truncated`] where the instruction runs past the end of `instructions`, and
/// [`Error::Overflow`] where a LEB128 increment does not fit the address space.
pub fn decode(instructions: &[u8], at: usize) -> Result<(Instruction, usize), Error> {
    use Instruction::*;

    let truncated = Error::Truncated { at };
    let byte = |offset: usize| at.checked_add(offset).and_then(|index| instructions.get(index).copied());
    let opcode = byte(0).ok_or(truncated)?;
    let operand = || byte(1).ok_or(truncated);
    let range = |operand: u8| (operand >> 4, (operand & 0x0F) + 1);

    Ok(match opcode {
        0x00..=0x3F => (AddToStackPointer((u32::from(opcode & 0x3F) << 2) + 4), 1),
        0x40..=0x7F => (SubtractFromStackPointer((u32::from(opcode & 0x3F) << 2) + 4), 1),
        0x80..=0x8F => {
            let mask = (u16::from(opcode & 0x0F) << 12) | (u16::from(operand()?) << 4);
            (if mask == 0 { RefuseToUnwind } else { PopCore(mask) }, 2)
        }
        0x9D | 0x9F => (Reserved(opcode), 1),
        0x90..=0x9F => (StackPointerFromRegister(opcode & 0x0F), 1),
        0xA0..=0xAF => {
            let last = opcode & 0x07;
            let mut mask = ((1u16 << (last + 1)) - 1) << 4;
            if opcode & 0x08 != 0 {
                mask |= 1 << 14;
            }
            (PopCore(mask), 1)
        }
        0xB0 => (Finish, 1),
        0xB1 => {
            let operand = operand()?;
            let spare = operand == 0 || operand & 0xF0 != 0;
            (if spare { Reserved(opcode) } else { PopCore(u16::from(operand)) }, 2)
        }
        0xB2 => {
            let (value, length) = uleb128(instructions, at)?;
            let bytes = value
                .checked_mul(4)
                .and_then(|scaled| scaled.checked_add(0x204))
                .ok_or(Error::Overflow)?;
            (AddToStackPointer(bytes), 1 + length)
        }
        0xB3 => {
            let (first, count) = range(operand()?);
            let instruction = if first + count > 16 {
                Reserved(opcode)
            } else {
                PopVfp { first, count, bytes: 8 * u32::from(count) + 4 }
            };
            (instruction, 2)
        }
        0xB4 => (PopAuthenticationCode, 1),
        0xB5 => (AuthenticationModifier, 1),
        0xB6 | 0xB7 => (Reserved(opcode), 1),
        0xB8..=0xBF => {
            let count = (opcode & 0x07) + 1;
            (PopVfp { first: 8, count, bytes: 8 * u32::from(count) + 4 }, 1)
        }
        0xC0..=0xC5 => (PopWmmxData { first: 10, count: (opcode & 0x07) + 1 }, 1),
        0xC6 => {
            let (first, count) = range(operand()?);
            let instruction =
                if first + count > 16 { Reserved(opcode) } else { PopWmmxData { first, count } };
            (instruction, 2)
        }
        0xC7 => {
            let operand = operand()?;
            let spare = operand == 0 || operand & 0xF0 != 0;
            (if spare { Reserved(opcode) } else { PopWmmxControl(operand) }, 2)
        }
        0xC8 => {
            let (first, count) = range(operand()?);
            let instruction = if first + count > 16 {
                Reserved(opcode)
            } else {
                PopVfp { first: 16 + first, count, bytes: 8 * u32::from(count) }
            };
            (instruction, 2)
        }
        0xC9 => {
            let (first, count) = range(operand()?);
            (PopVfp { first, count, bytes: 8 * u32::from(count) }, 2)
        }
        0xCA..=0xCF => (Reserved(opcode), 1),
        0xD0..=0xD7 => {
            let count = (opcode & 0x07) + 1;
            (PopVfp { first: 8, count, bytes: 8 * u32::from(count) }, 1)
        }
        0xD8..=0xFF => (Reserved(opcode), 1),
    })
}

/// The unsigned LEB128 value that follows the opcode at offset `at`, and how many bytes it occupies.
///
/// A value wider than 32 bits is an [`Error::Overflow`], and one cut off by the end of the
/// instructions is [`Error::Truncated`] at the opcode.
fn uleb128(instructions: &[u8], at: usize) -> Result<(u32, usize), Error> {
    let mut value = 0u32;
    for length in 0..5 {
        let index = at.checked_add(1 + length).ok_or(Error::Truncated { at })?;
        let byte = *instructions.get(index).ok_or(Error::Truncated { at })?;
        let low = u32::from(byte & 0x7F);
        let shift = 7 * length as u32;
        if shift == 28 && low > 0x0F {
            return Err(Error::Overflow);
        }
        value |= low << shift;
        if byte & 0x80 == 0 {
            return Ok((value, length + 1));
        }
    }
    Err(Error::Overflow)
}

/// A frame's core registers, r0 to r15, each `None` where its value in that frame is not known.
pub type Registers = [Option<u32>; 16];

/// Unwinds one frame: runs `description`'s instructions over `registers`, reading saved registers
/// through `read`, and leaves the caller's registers in their place.
///
/// This is the specification's virtual register set (EHABI32 2025Q4, "Virtual register set
/// manipulation"), restricted to the core registers. `registers[13]` is the frame's stack pointer and
/// must be known. On success r13 holds the caller's stack pointer, r15 the return address (with the
/// Thumb bit as it was saved), and every register the frame restored its saved value; a register the
/// frame did not restore keeps the value it came in with, which for a callee-saved register is the
/// caller's. The result is the mask of registers read back from the stack.
///
/// Finish, written or implied after the last instruction, copies r14 into r15 unless the frame popped
/// r15 itself (the table's remark c). A popped r13 takes effect once its whole instruction has run
/// (remark b). The floating-point, authentication and Intel Wireless MMX pops are not kept: they move
/// the stack pointer past the space those registers occupy.
///
/// # Errors
/// On any error `registers` is left as it was. The errors that stop a description are the index's
/// [`Error::CantUnwind`], a generic entry's [`Error::GenericPersonality`], a refusal or a reserved
/// instruction, a register the description needs and the frame does not know, a slot `read` cannot
/// answer, and [`Error::BelowStackPointer`] for a slot below the frame's stack pointer: nothing there
/// survives an interrupt, so no frame keeps a register in it.
pub fn unwind(
    description: &Description,
    registers: &mut Registers,
    mut read: impl FnMut(u32) -> Option<u32>,
) -> Result<u16, Error> {
    let instructions = match description {
        Description::CantUnwind => return Err(Error::CantUnwind),
        Description::Generic { routine, .. } => {
            return Err(Error::GenericPersonality { routine: *routine })
        }
        Description::Compact { instructions, .. } => instructions,
    };
    let stack_pointer = registers[13].ok_or(Error::UnknownRegister { register: 13 })?;
    let mut next = *registers;
    let mut vsp = stack_pointer;
    let mut restored = 0u16;

    let mut at = 0;
    while at < instructions.len() {
        let (instruction, length) = decode(instructions, at)?;
        match instruction {
            Instruction::AddToStackPointer(bytes) => {
                vsp = vsp.checked_add(bytes).ok_or(Error::Overflow)?;
            }
            Instruction::SubtractFromStackPointer(bytes) => {
                vsp = vsp.checked_sub(bytes).ok_or(Error::Overflow)?;
            }
            Instruction::RefuseToUnwind => return Err(Error::RefuseToUnwind),
            Instruction::PopCore(mask) => {
                let mut address = vsp;
                let mut loaded_stack_pointer = None;
                for register in 0..16u8 {
                    if mask & (1 << register) == 0 {
                        continue;
                    }
                    if address < stack_pointer {
                        return Err(Error::BelowStackPointer { address, stack_pointer });
                    }
                    let value = read(address).ok_or(Error::Unreadable { address })?;
                    if register == 13 {
                        loaded_stack_pointer = Some(value);
                    } else {
                        next[usize::from(register)] = Some(value);
                    }
                    address = address.checked_add(4).ok_or(Error::Overflow)?;
                }
                vsp = loaded_stack_pointer.unwrap_or(address);
                restored |= mask;
            }
            Instruction::StackPointerFromRegister(register) => {
                vsp = next[usize::from(register)].ok_or(Error::UnknownRegister { register })?;
            }
            Instruction::Finish => break,
            Instruction::PopVfp { bytes, .. } => vsp = pass_over(vsp, stack_pointer, bytes)?,
            Instruction::PopAuthenticationCode => vsp = pass_over(vsp, stack_pointer, 4)?,
            Instruction::AuthenticationModifier => {}
            Instruction::PopWmmxData { count, .. } => {
                vsp = pass_over(vsp, stack_pointer, 8 * u32::from(count))?;
            }
            Instruction::PopWmmxControl(mask) => {
                vsp = pass_over(vsp, stack_pointer, 4 * mask.count_ones())?;
            }
            Instruction::Reserved(opcode) => return Err(Error::Reserved { at, opcode }),
        }
        at += length;
    }

    if restored & (1 << 15) == 0 {
        next[15] = Some(next[14].ok_or(Error::UnknownRegister { register: 14 })?);
    }
    next[13] = Some(vsp);
    *registers = next;
    Ok(restored)
}

/// The stack pointer after popping `bytes` of registers this reader does not keep, from a frame whose
/// stack pointer was `stack_pointer`.
fn pass_over(vsp: u32, stack_pointer: u32, bytes: u32) -> Result<u32, Error> {
    if vsp < stack_pointer {
        return Err(Error::BelowStackPointer { address: vsp, stack_pointer });
    }
    vsp.checked_add(bytes).ok_or(Error::Overflow)
}

/// The unwinding tables of a linked ELF.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tables<'a> {
    /// Every index table: each section of type [`SHT_ARM_EXIDX`].
    pub indexes: Vec<Region<'a>>,
    /// Every section the image loads, which is where an index entry's table entry is looked for.
    pub loaded: Vec<Region<'a>>,
}

/// Finds the unwinding tables of a linked little-endian 32-bit Arm ELF.
///
/// An index table is found by its section type rather than its name: EHABI32 2025Q4, "Sections",
/// requires the type and lets the name carry a suffix. A section loads when it has `SHF_ALLOC` and
/// occupies bytes in the file; a `SHT_NOBITS` section's offset addresses whatever the file put
/// there instead.
///
/// Any other file yields no tables, because [`SHT_ARM_EXIDX`] is a processor-specific number that
/// means an index table only when the machine is Arm.
#[must_use]
pub fn tables(elf: &[u8]) -> Tables<'_> {
    let mut found = Tables::default();
    let arm = elf.len() >= 20
        && elf[..4] == [0x7F, b'E', b'L', b'F']
        && elf[4] == 1
        && elf[5] == 1
        && u16::from_le_bytes([elf[18], elf[19]]) == EM_ARM;
    if !arm {
        return found;
    }
    for section in crate::section_headers(elf) {
        if section.kind == crate::SHT_NOBITS {
            continue;
        }
        let (Some(bytes), Ok(address)) = (section.data, u32::try_from(section.address)) else {
            continue;
        };
        if section.kind == SHT_ARM_EXIDX {
            found.indexes.push(Region { address, bytes });
        }
        if section.flags & u64::from(crate::SHF_ALLOC) != 0 {
            found.loaded.push(Region { address, bytes });
        }
    }
    found
}

#[cfg(test)]
mod tests;
