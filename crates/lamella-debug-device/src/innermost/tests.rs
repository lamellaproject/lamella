//! Placing the frame a core is stopped in, over code and a stack laid out byte by byte.

use super::*;
use lamella_elf::ehabi::Description;
use std::collections::BTreeMap;

const FUNCTION: u32 = 0x0800_0100;
const SP: u32 = 0x2000_1000;
const LR: u32 = 0x0800_0321;

const PUSH_R4_LR: u16 = 0xB510;
const PUSH_R7: u16 = 0xB480;
const PUSH_R7_LR: u16 = 0xB580;
const SUB_SP_8: u16 = 0xB082;
const SUB_SP_68: u16 = 0xB091;
const ADD_SP_8: u16 = 0xB002;
const ADD_SP_4: u16 = 0xB001;
const POP_R4_PC: u16 = 0xBD10;
const POP_R7: u16 = 0xBC80;
const MOV_R7_SP: u16 = 0x466F;
const MOV_R5_SP: u16 = 0x466D;
const MOV_SP_R7: u16 = 0x46BD;
const MOV_R4_R0: u16 = 0x4604;
const MOV_R0_R4: u16 = 0x4620;
const BX_LR: u16 = 0x4770;

/// Code and stack as one byte map, little-endian.
#[derive(Default)]
struct Target {
    bytes: BTreeMap<u32, u8>,
}

impl Target {
    fn code(mut self, address: u32, halfwords: &[u16]) -> Self {
        for (index, halfword) in halfwords.iter().enumerate() {
            let at = address + 2 * index as u32;
            for (offset, byte) in halfword.to_le_bytes().into_iter().enumerate() {
                self.bytes.insert(at + offset as u32, byte);
            }
        }
        self
    }

    fn word(mut self, address: u32, value: u32) -> Self {
        for (offset, byte) in value.to_le_bytes().into_iter().enumerate() {
            self.bytes.insert(address + offset as u32, byte);
        }
        self
    }

    fn halfword(&self, address: u32) -> Option<u16> {
        Some(u16::from_le_bytes([
            *self.bytes.get(&address)?,
            *self.bytes.get(&(address + 1))?,
        ]))
    }

    fn read(&self, address: u32) -> Option<u32> {
        let mut bytes = [0u8; 4];
        for (offset, byte) in bytes.iter_mut().enumerate() {
            *byte = *self.bytes.get(&(address + offset as u32))?;
        }
        Some(u32::from_le_bytes(bytes))
    }
}

/// Every register known, as it is in the frame a core is stopped in.
fn registers(pc: u32, sp: u32, lr: u32) -> Registers {
    let mut registers = [None; 16];
    for (number, register) in registers.iter_mut().enumerate().take(13) {
        *register = Some(0x1000_0000 + number as u32);
    }
    registers[13] = Some(sp);
    registers[14] = Some(lr);
    registers[15] = Some(pc);
    registers
}

fn compact(instructions: &[u8]) -> Description {
    Description::Compact {
        personality: 0,
        table: None,
        instructions: instructions.to_vec(),
    }
}

fn run_with(target: &Target, registers: &Registers, xpsr: u32, description: &[u8]) -> Placement {
    place(
        FUNCTION,
        registers,
        xpsr,
        &compact(description),
        |address| target.halfword(address),
        |address| target.read(address),
    )
}

fn run(target: &Target, pc: u32, description: &[u8]) -> Placement {
    run_with(target, &registers(pc, SP, LR), 0, description)
}

fn caller(placement: Placement) -> Registers {
    match placement {
        Placement::Caller(registers) => registers,
        other => panic!("the instructions should decide the caller, and the placement was {other:?}"),
    }
}

#[test]
fn a_stop_on_the_first_instruction_returns_to_the_link_register() {
    let target = Target::default()
        .code(FUNCTION, &[PUSH_R4_LR, MOV_R4_R0])
        .word(SP + 4, 0x0800_0999);
    let placed = caller(run(&target, FUNCTION, &[0xA8, 0xB0, 0xB0]));
    assert_eq!(placed[15], Some(LR), "nothing has been pushed, so the caller is still in the link register");
    assert_eq!(placed[13], Some(SP));
}

#[test]
fn a_stop_inside_a_prologue_counts_only_the_instructions_that_ran() {
    let target = Target::default()
        .code(FUNCTION, &[PUSH_R4_LR, SUB_SP_8, MOV_R4_R0])
        .word(SP, 0x4444)
        .word(SP + 4, 0x0800_0555)
        .word(SP + 12, 0x0800_0999);
    let placed = caller(run(&target, FUNCTION + 2, &[0x01, 0xA8, 0xB0]));
    assert_eq!(placed[15], Some(0x0800_0555), "the slot the push saved the link register in");
    assert_eq!(placed[13], Some(SP + 8));
}

#[test]
fn a_prologue_that_has_not_saved_the_link_register_yet_returns_to_it() {
    let target = Target::default().code(FUNCTION, &[SUB_SP_8, PUSH_R4_LR, MOV_R4_R0]);
    let placed = caller(run(&target, FUNCTION + 2, &[0xA8, 0x01, 0xB0]));
    assert_eq!(placed[15], Some(LR));
    assert_eq!(placed[13], Some(SP + 8), "the reservation has run");
}

#[test]
fn a_stop_in_an_exit_sequence_follows_the_instructions_still_to_run() {
    let target = Target::default()
        .code(FUNCTION, &[PUSH_R4_LR, SUB_SP_8, MOV_R4_R0])
        .code(FUNCTION + 0x20, &[ADD_SP_8, POP_R4_PC])
        .word(SP, 0x4444)
        .word(SP + 4, 0x0800_0555)
        .word(SP + 12, 0x0800_0999);
    let placed = caller(run(&target, FUNCTION + 0x22, &[0x01, 0xA8, 0xB0]));
    assert_eq!(placed[15], Some(0x0800_0555), "the pop's program-counter slot");
    assert_eq!(placed[13], Some(SP + 8));
    assert_eq!(placed[4], Some(0x4444), "the pop restores r4");
    assert_eq!(placed[0], None, "a scratch register does not hold the caller's value");
}

#[test]
fn a_stop_at_a_return_through_the_link_register_leaves_the_stack_as_it_stands() {
    let target = Target::default()
        .code(FUNCTION, &[PUSH_R4_LR, MOV_R4_R0])
        .code(FUNCTION + 0x20, &[0xE8BD, 0x4010, ADD_SP_4, BX_LR])
        .word(SP + 4, 0x0800_0999);
    let placed = caller(run(&target, FUNCTION + 0x26, &[0xA8, 0xB0, 0xB0]));
    assert_eq!(placed[15], Some(LR));
    assert_eq!(placed[13], Some(SP));
}

#[test]
fn a_branch_once_the_link_register_is_restored_is_a_tail_call() {
    let target = Target::default()
        .code(FUNCTION, &[PUSH_R4_LR, MOV_R4_R0])
        .code(FUNCTION + 0x20, &[0xE8BD, 0x4010, 0xF000, 0xB800])
        .word(SP, 0x4444)
        .word(SP + 4, 0x0800_0555);
    let placed = caller(run(&target, FUNCTION + 0x20, &[0xA8, 0xB0, 0xB0]));
    assert_eq!(placed[15], Some(0x0800_0555));
    assert_eq!(placed[13], Some(SP + 8));
    assert_eq!(placed[4], Some(0x4444));
}

#[test]
fn a_stack_pointer_restored_from_a_frame_pointer_is_followed() {
    let target = Target::default()
        .code(FUNCTION, &[PUSH_R7, MOV_R7_SP, MOV_R4_R0])
        .code(FUNCTION + 0x20, &[MOV_SP_R7, POP_R7, BX_LR])
        .word(SP + 0x40, 0x2000_2000);
    let mut live = registers(FUNCTION + 0x20, SP, LR);
    live[7] = Some(SP + 0x40);
    let placed = caller(run_with(&target, &live, 0, &[0x97, 0x80, 0x08]));
    assert_eq!(placed[15], Some(LR));
    assert_eq!(placed[13], Some(SP + 0x44));
    assert_eq!(placed[7], Some(0x2000_2000), "the pop restores r7");
}

#[test]
fn a_body_stop_is_left_to_the_description_when_the_prologue_built_the_frame_it_describes() {
    let target = Target::default().code(
        FUNCTION,
        &[0xE92D, 0x4FF0, SUB_SP_68, 0xF10D, 0x0B60, MOV_R4_R0, MOV_R4_R0, MOV_R4_R0, MOV_R4_R0],
    );
    assert_eq!(run(&target, FUNCTION + 0x0E, &[0x10, 0xAF, 0xB0]), Placement::Body);
}

#[test]
fn a_body_stop_is_undecided_when_the_prologue_and_the_description_disagree() {
    let target = Target::default().code(FUNCTION, &[PUSH_R4_LR, MOV_R4_R0, MOV_R4_R0, MOV_R4_R0]);
    assert_eq!(
        run(&target, FUNCTION + 4, &[0x01, 0xA8, 0xB0]),
        Placement::Undecided,
        "the description's frame is eight bytes deeper than anything the prologue built"
    );
}

#[test]
fn a_frame_pointer_description_is_checked_against_the_instruction_that_set_the_pointer() {
    let target = Target::default().code(
        FUNCTION,
        &[PUSH_R7, MOV_R7_SP, MOV_R5_SP, 0xF36F, 0x050B, MOV_R4_R0, MOV_R4_R0, MOV_R4_R0, MOV_R4_R0],
    );
    assert_eq!(run(&target, FUNCTION + 0x10, &[0x97, 0x80, 0x08]), Placement::Body);
}

#[test]
fn a_release_that_does_not_lead_to_a_return_is_part_of_the_body() {
    let target = Target::default()
        .code(FUNCTION, &[PUSH_R7_LR, MOV_R7_SP, MOV_R4_R0])
        .code(FUNCTION + 0x20, &[ADD_SP_8, MOV_R0_R4]);
    assert_eq!(run(&target, FUNCTION + 0x20, &[0x97, 0x84, 0x08]), Placement::Body);
}

#[test]
fn inside_an_if_then_block_an_exit_instruction_does_not_decide() {
    let target = Target::default()
        .code(FUNCTION, &[PUSH_R4_LR, MOV_R4_R0])
        .code(FUNCTION + 0x20, &[POP_R4_PC]);
    let live = registers(FUNCTION + 0x20, SP, LR);
    assert_eq!(run_with(&target, &live, 0x0000_0C00, &[0xA8, 0xB0, 0xB0]), Placement::Undecided);
}

#[test]
fn a_description_that_reads_below_the_stack_pointer_does_not_decide() {
    let target = Target::default().code(FUNCTION, &[PUSH_R4_LR, MOV_R4_R0, MOV_R4_R0]);
    assert_eq!(
        run(&target, FUNCTION + 4, &[0x45, 0x84, 0x80, 0x01, 0xA6, 0x00]),
        Placement::Undecided
    );
}

#[test]
fn a_program_counter_inside_an_instruction_does_not_decide() {
    let target = Target::default().code(FUNCTION, &[0xE92D, 0x4FF0, MOV_R4_R0]);
    assert_eq!(run(&target, FUNCTION + 2, &[0xAF, 0xB0, 0xB0]), Placement::Undecided);
}

#[test]
fn code_that_cannot_be_read_does_not_decide() {
    let target = Target::default().code(FUNCTION, &[PUSH_R4_LR]);
    assert_eq!(run(&target, FUNCTION + 0x40, &[0xA8, 0xB0, 0xB0]), Placement::Undecided);
}
