//! The Thumb instructions that move the stack pointer, save or restore the link register, or
//! return: what a stack walk has to account for to place the frame a core is stopped in.
//!
//! Decoded from the Armv7-M Architecture Reference Manual (DDI 0403E.d). A5.1 decides an
//! instruction's width, and A7.7 defines the encodings decoded here. Every
//! other instruction is [`Effect::Other`], with its width still decided, so a caller can step over
//! it.

/// What an instruction does that a stack walk has to account for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Stores the core registers in `registers` (bit `n` for `rn`), the lowest-numbered at the
    /// lowest address, below the stack pointer, and lowers the stack pointer by `bytes`. A
    /// floating-point push stores no core register.
    Push {
        /// The core registers stored.
        registers: u16,
        /// How far the stack pointer moves.
        bytes: u32,
    },
    /// Lowers the stack pointer by this many bytes.
    Reserve(u32),
    /// Raises the stack pointer by this many bytes.
    Release(u32),
    /// Sets `register` to the stack pointer plus `offset`.
    FramePointer {
        /// The register set.
        register: u8,
        /// What is added to the stack pointer.
        offset: u32,
    },
    /// Sets the stack pointer to this register's value.
    StackPointerFrom(u8),
    /// Loads the core registers in `registers` from the stack, the lowest-numbered from the lowest
    /// address, and raises the stack pointer by `bytes`. Loading r15 returns.
    Pop {
        /// The core registers loaded.
        registers: u16,
        /// How far the stack pointer moves.
        bytes: u32,
    },
    /// `BX LR`: a return through the link register.
    ReturnToLinkRegister,
    /// An unconditional branch.
    Branch,
    /// `IT`: up to four following instructions are conditional.
    IfThen,
    /// Anything else.
    Other,
}

/// The link register's bit in a register mask.
const LR: u16 = 1 << 14;
/// The program counter's bit in a register mask.
const PC: u16 = 1 << 15;

/// The effect of the instruction whose first halfword is `first`, given the halfword after it, and
/// the instruction's width in bytes. `second` is not read for a 16-bit instruction.
pub(crate) fn decode(first: u16, second: u16) -> (Effect, u32) {
    if first >> 11 >= 0b11101 {
        (decode_32(first, second), 4)
    } else {
        (decode_16(first), 2)
    }
}

/// Whether `first` begins a 32-bit instruction (A5.1).
pub(crate) fn is_32_bit(first: u16) -> bool {
    first >> 11 >= 0b11101
}

fn decode_16(first: u16) -> Effect {
    match first {
        0xB400..=0xB5FF => {
            let registers = (first & 0x00FF) | if first & 0x0100 != 0 { LR } else { 0 };
            Effect::Push { registers, bytes: 4 * registers.count_ones() }
        }
        0xBC00..=0xBDFF => {
            let registers = (first & 0x00FF) | if first & 0x0100 != 0 { PC } else { 0 };
            Effect::Pop { registers, bytes: 4 * registers.count_ones() }
        }
        0xB080..=0xB0FF => Effect::Reserve(u32::from(first & 0x7F) << 2),
        0xB000..=0xB07F => Effect::Release(u32::from(first & 0x7F) << 2),
        0xA800..=0xAFFF => Effect::FramePointer {
            register: ((first >> 8) & 0x07) as u8,
            offset: u32::from(first & 0xFF) << 2,
        },
        0x4600..=0x46FF => {
            let destination = ((((first >> 7) & 1) << 3) | (first & 0x07)) as u8;
            let source = ((first >> 3) & 0x0F) as u8;
            match (destination, source) {
                (13, 13) | (15, _) => Effect::Other,
                (13, source) => Effect::StackPointerFrom(source),
                (destination, 13) => Effect::FramePointer { register: destination, offset: 0 },
                _ => Effect::Other,
            }
        }
        0x4770 => Effect::ReturnToLinkRegister,
        0xE000..=0xE7FF => Effect::Branch,
        0xBF00..=0xBFFF if first & 0x000F != 0 => Effect::IfThen,
        _ => Effect::Other,
    }
}

fn decode_32(first: u16, second: u16) -> Effect {
    let destination = ((second >> 8) & 0x0F) as u8;
    let imm12 = (u32::from((first >> 10) & 1) << 11)
        | (u32::from((second >> 12) & 0x07) << 8)
        | u32::from(second & 0xFF);
    let data_processing = second & 0x8000 == 0;

    match first {
        0xE92D if second & 0xA000 == 0 => {
            Effect::Push { registers: second, bytes: 4 * second.count_ones() }
        }
        0xE8BD if second & 0x2000 == 0 && second & 0xC000 != 0xC000 => {
            Effect::Pop { registers: second, bytes: 4 * second.count_ones() }
        }
        0xF84D if second & 0x0FFF == 0x0D04 && !matches!(second >> 12, 13 | 15) => {
            Effect::Push { registers: 1 << (second >> 12), bytes: 4 }
        }
        0xF85D if second & 0x0FFF == 0x0B04 && second >> 12 != 13 => {
            Effect::Pop { registers: 1 << (second >> 12), bytes: 4 }
        }
        0xF1AD | 0xF5AD if data_processing && destination == 13 => {
            thumb_expand_imm(imm12).map_or(Effect::Other, Effect::Reserve)
        }
        0xF2AD | 0xF6AD if data_processing && destination == 13 => Effect::Reserve(imm12),
        0xF10D | 0xF50D if data_processing && destination != 15 => match thumb_expand_imm(imm12) {
            Some(bytes) if destination == 13 => Effect::Release(bytes),
            Some(offset) => Effect::FramePointer { register: destination, offset },
            None => Effect::Other,
        },
        0xF20D | 0xF60D if data_processing && destination != 15 => {
            if destination == 13 {
                Effect::Release(imm12)
            } else {
                Effect::FramePointer { register: destination, offset: imm12 }
            }
        }
        0xED2D | 0xED6D if (second >> 9) & 0x07 == 0b101 => {
            Effect::Push { registers: 0, bytes: u32::from(second & 0xFF) << 2 }
        }
        0xECBD | 0xECFD if (second >> 9) & 0x07 == 0b101 => {
            Effect::Pop { registers: 0, bytes: u32::from(second & 0xFF) << 2 }
        }
        _ if first & 0xF800 == 0xF000 && second & 0xD000 == 0x9000 => Effect::Branch,
        _ => Effect::Other,
    }
}

/// A modified immediate constant: A5.3.2's `ThumbExpandImm`, or `None` where the manual makes the
/// encoding UNPREDICTABLE.
pub(crate) fn thumb_expand_imm(imm12: u32) -> Option<u32> {
    let imm8 = imm12 & 0xFF;
    if (imm12 >> 10) & 0b11 == 0 {
        match (imm12 >> 8) & 0b11 {
            0 => Some(imm8),
            _ if imm8 == 0 => None,
            1 => Some((imm8 << 16) | imm8),
            2 => Some((imm8 << 24) | (imm8 << 8)),
            _ => Some(imm8 * 0x0101_0101),
        }
    } else {
        Some((0x80 | (imm12 & 0x7F)).rotate_right(imm12 >> 7))
    }
}

#[cfg(test)]
mod tests;
