//! The frame a core is stopped in, placed from the instructions around it, for a function an index
//! table describes.
//!
//! An index entry describes a function's frame as it stands at a call. A debugger stops anywhere:
//! before the function has pushed anything, partway through its prologue, or partway through an
//! exit sequence, and at each of those the entry reads the wrong stack slots. So the innermost frame
//! is placed from the code first, and the entry is trusted for it only where the instructions show
//! the frame is the one the entry describes. Where neither decides, the walk ends -- a truncated
//! stack is visible, and a caller read from the wrong slot is not.

use lamella_elf::ehabi::{self, Description, Registers};

use crate::thumb::{decode, is_32_bit, Effect};

/// Where the frame a core is stopped in stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Placement {
    /// The instructions decide the caller. These are the caller's registers: r13 its stack pointer,
    /// r15 the return address, and `None` for any register whose value in the caller is not known.
    Caller(Registers),
    /// The function's frame is built and not being taken down, so its index entry applies.
    Body,
    /// Neither the instructions nor the index entry decide.
    Undecided,
}

/// The registers a called function need not preserve: r0-r3 and r12, and r9, whose role each
/// platform decides. A caller's value for one of these is known only where a frame restores it.
///
/// AAPCS32 2025Q4, "Core registers": a subroutine must preserve r4-r8, r10, r11 and the stack
/// pointer, and r9 only in the variants that designate it a variable register.
pub(crate) const NOT_PRESERVED: [usize; 6] = [0, 1, 2, 3, 9, 12];

/// How many instructions an exit sequence is followed for before the scan stops looking.
const EXIT_SEQUENCE_LIMIT: usize = 8;

/// Places the frame of a core stopped with `registers` in the function that starts at `function`,
/// which `description` describes. `xpsr` is the core's program status register; `halfword` and
/// `word` read the target.
pub(crate) fn place(
    function: u32,
    registers: &Registers,
    xpsr: u32,
    description: &Description,
    halfword: impl Fn(u32) -> Option<u16>,
    word: impl Fn(u32) -> Option<u32>,
) -> Placement {
    let (Some(pc), Some(sp)) = (registers[15], registers[13]) else {
        return Placement::Undecided;
    };
    let instruction = |address: u32| -> Option<(Effect, u32)> {
        let first = halfword(address)?;
        let second = if is_32_bit(first) { halfword(address.checked_add(2)?)? } else { 0 };
        Some(decode(first, second))
    };

    let in_it_block = (xpsr >> 25) & 0b11 != 0 || (xpsr >> 10) & 0x3F != 0;

    match exit_sequence(pc, sp, registers, in_it_block, &instruction, &word) {
        Exit::Returns(caller) => return Placement::Caller(caller),
        Exit::Undecided => return Placement::Undecided,
        Exit::Stays => {}
    }

    if pc == function {
        return finish(*registers, sp, registers[14]).map_or(Placement::Undecided, Placement::Caller);
    }

    let Some(prologue) = Prologue::run(function, pc, &instruction) else {
        return Placement::Undecided;
    };
    if prologue.end == pc {
        return prologue
            .caller(registers, sp, &word)
            .map_or(Placement::Undecided, Placement::Caller);
    }
    if prologue.end > pc {
        return Placement::Undecided;
    }
    if prologue.agrees_with(description) {
        Placement::Body
    } else {
        Placement::Undecided
    }
}

/// What following the instructions from the program counter showed.
enum Exit {
    /// They return, and these are the registers of the caller they return to.
    Returns(Registers),
    /// They would return, and cannot be followed.
    Undecided,
    /// They are not taking the frame down.
    Stays,
}

/// Follows the instructions from `pc` for as long as they take the frame down, and answers the caller
/// they return to.
///
/// **THIS IS EXACT WHEREVER IN THE SEQUENCE THE CORE STOPPED**, which the index entry is not: it
/// simulates what the code is about to do, and the stack as it stands is the input. The first
/// instruction that is not part of taking a frame down ends the scan, and the frame is judged as a
/// body instead.
fn exit_sequence(
    pc: u32,
    sp: u32,
    registers: &Registers,
    in_it_block: bool,
    instruction: &impl Fn(u32) -> Option<(Effect, u32)>,
    word: &impl Fn(u32) -> Option<u32>,
) -> Exit {
    let mut caller = *registers;
    let mut stack_pointer = sp;
    let mut restored_link_register = false;
    let mut at = pc;
    for _ in 0..EXIT_SEQUENCE_LIMIT {
        let Some((effect, width)) = instruction(at) else {
            return Exit::Undecided;
        };
        let leaves = matches!(
            effect,
            Effect::Release(_)
                | Effect::StackPointerFrom(_)
                | Effect::Pop { .. }
                | Effect::ReturnToLinkRegister
        ) || (effect == Effect::Branch && restored_link_register);
        if leaves && in_it_block {
            return Exit::Undecided;
        }
        match effect {
            Effect::Release(bytes) => {
                let Some(raised) = stack_pointer.checked_add(bytes) else {
                    return Exit::Undecided;
                };
                stack_pointer = raised;
            }
            Effect::StackPointerFrom(register) => {
                let Some(value) = caller[usize::from(register)] else {
                    return Exit::Undecided;
                };
                stack_pointer = value;
            }
            Effect::Pop { registers: mask, bytes } => {
                let mut address = stack_pointer;
                for (register, value) in caller.iter_mut().enumerate() {
                    if mask & (1 << register) == 0 {
                        continue;
                    }
                    let Some(loaded) = word(address) else {
                        return Exit::Undecided;
                    };
                    *value = Some(loaded);
                    address = address.wrapping_add(4);
                }
                let Some(raised) = stack_pointer.checked_add(bytes) else {
                    return Exit::Undecided;
                };
                stack_pointer = raised;
                if mask & (1 << 14) != 0 {
                    restored_link_register = true;
                }
                if mask & (1 << 15) != 0 {
                    return finish(caller, stack_pointer, caller[15])
                        .map_or(Exit::Undecided, Exit::Returns);
                }
            }
            Effect::ReturnToLinkRegister => {
                return finish(caller, stack_pointer, caller[14]).map_or(Exit::Undecided, Exit::Returns);
            }
            Effect::Branch if restored_link_register => {
                return finish(caller, stack_pointer, caller[14]).map_or(Exit::Undecided, Exit::Returns);
            }
            _ => return Exit::Stays,
        }
        let Some(next) = at.checked_add(width) else {
            return Exit::Undecided;
        };
        at = next;
    }
    Exit::Stays
}

/// A caller's registers from `registers`, with its stack pointer and the return address filled in and
/// the registers a callee need not preserve unknown.
fn finish(mut registers: Registers, stack_pointer: u32, return_address: Option<u32>) -> Option<Registers> {
    let return_address = return_address?;
    for register in NOT_PRESERVED {
        registers[register] = None;
    }
    registers[13] = Some(stack_pointer);
    registers[14] = Some(return_address);
    registers[15] = Some(return_address);
    Some(registers)
}

/// What the instructions a function opens with have done, counted from its entry.
struct Prologue {
    /// One past the last prologue instruction that was followed.
    end: u32,
    /// How far below the entry stack pointer the stack pointer has moved.
    depth: u32,
    /// How far below the entry stack pointer the link register was saved, where it was.
    link_register: Option<u32>,
    /// For each register, how far below the entry stack pointer its value was saved, where it was.
    saved: [Option<u32>; 16],
    /// For each register set from the stack pointer, how far below the entry stack pointer it
    /// points. Negative is above.
    frame_pointers: [Option<i64>; 16],
    /// Registers set from the stack pointer before any push saved them, whose caller values are gone.
    overwritten: [bool; 16],
}

impl Prologue {
    /// Follows the pushes, reservations and frame-pointer setups from `function` up to `pc`, stopping
    /// at the first other instruction. `None` where the code cannot be read.
    fn run(function: u32, pc: u32, instruction: &impl Fn(u32) -> Option<(Effect, u32)>) -> Option<Self> {
        let mut prologue = Prologue {
            end: function,
            depth: 0,
            link_register: None,
            saved: [None; 16],
            frame_pointers: [None; 16],
            overwritten: [false; 16],
        };
        while prologue.end < pc {
            let (effect, width) = instruction(prologue.end)?;
            match effect {
                Effect::Push { registers, bytes } => {
                    let depth = prologue.depth.checked_add(bytes)?;
                    let mut slot = depth;
                    for (register, saved) in prologue.saved.iter_mut().enumerate() {
                        if registers & (1 << register) == 0 {
                            continue;
                        }
                        if saved.is_none() {
                            *saved = Some(slot);
                        }
                        if register == 14 {
                            prologue.link_register = Some(slot);
                        }
                        slot = slot.checked_sub(4)?;
                    }
                    prologue.depth = depth;
                }
                Effect::Reserve(bytes) => prologue.depth = prologue.depth.checked_add(bytes)?,
                Effect::FramePointer { register, offset } => {
                    let register = usize::from(register);
                    if prologue.saved[register].is_none() {
                        prologue.overwritten[register] = true;
                    }
                    prologue.frame_pointers[register] = Some(i64::from(prologue.depth) - i64::from(offset));
                }
                _ => break,
            }
            prologue.end = prologue.end.checked_add(width)?;
        }
        Some(prologue)
    }

    /// The caller, for a core stopped at [`Self::end`] with stack pointer `sp`: every instruction up
    /// to here has run, and nothing after it has.
    fn caller(&self, registers: &Registers, sp: u32, word: &impl Fn(u32) -> Option<u32>) -> Option<Registers> {
        let caller_stack_pointer = sp.checked_add(self.depth)?;
        let below = |slot: u32| caller_stack_pointer.checked_sub(slot).and_then(word);
        let return_address = match self.link_register {
            Some(slot) => below(slot)?,
            None => registers[14]?,
        };
        let mut caller = *registers;
        for (register, value) in caller.iter_mut().enumerate().take(13) {
            if self.frame_pointers[register].is_some() {
                *value = if self.overwritten[register] {
                    None
                } else {
                    self.saved[register].and_then(below)
                };
            }
        }
        finish(caller, caller_stack_pointer, Some(return_address))
    }

    /// Whether `description` describes the frame this prologue built.
    ///
    /// # THE ENTRY IS RUN OVER A STACK WHERE EVERY WORD HOLDS ITS OWN ADDRESS
    ///
    /// Then the return address it reads back IS the slot it read it from, and the stack pointer it
    /// finishes with is where it thinks the caller's is. The prologue says where both are. Where the
    /// two agree the entry describes this frame and is trusted for any stop in the body; where they
    /// do not -- a prologue with an instruction this decoder does not follow, or an entry for some
    /// other frame -- the answer is not the entry's to give.
    fn agrees_with(&self, description: &Description) -> bool {
        const BASE: u32 = 0x4000_0000;
        const LIVE_LINK_REGISTER: u32 = 1;

        let Some(entry_stack_pointer) = BASE.checked_add(self.depth) else {
            return false;
        };
        let mut registers: Registers = [None; 16];
        registers[13] = Some(BASE);
        registers[14] = Some(LIVE_LINK_REGISTER);
        for (register, depth) in self.frame_pointers.iter().enumerate() {
            if let Some(depth) = depth {
                registers[register] = u32::try_from(i64::from(entry_stack_pointer) - depth).ok();
            }
        }
        if ehabi::unwind(description, &mut registers, Some).is_err() {
            return false;
        }
        let return_address = match self.link_register {
            Some(slot) => entry_stack_pointer.checked_sub(slot),
            None => Some(LIVE_LINK_REGISTER),
        };
        registers[13] == Some(entry_stack_pointer) && registers[15] == return_address
    }
}

#[cfg(test)]
mod tests;
