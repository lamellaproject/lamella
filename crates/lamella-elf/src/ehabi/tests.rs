//! The unwinding-table reader, driven over tables these tests lay out byte by byte.

use super::*;
use alloc::vec;
use alloc::vec::Vec;

/// A prel31 offset from `place` to `target` with bit 31 clear, as the index and table formats store one.
fn prel31(target: u32, place: u32) -> u32 {
    target.wrapping_sub(place) & 0x7FFF_FFFF
}

/// What an index entry's second word says, before it is encoded.
enum Second {
    CantUnwind,
    Inline(u32),
    Table(u32),
}

/// An index table at `address`, one two-word entry per `(function, second word)`.
fn index(address: u32, entries: &[(u32, Second)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (position, (function, second)) in entries.iter().enumerate() {
        let place = address + 8 * position as u32;
        out.extend_from_slice(&prel31(*function, place).to_le_bytes());
        let word = match second {
            Second::CantUnwind => EXIDX_CANTUNWIND,
            Second::Inline(word) => *word,
            Second::Table(target) => prel31(*target, place + 4),
        };
        out.extend_from_slice(&word.to_le_bytes());
    }
    out
}

/// Little-endian words, as a table section holds them.
fn words(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|word| word.to_le_bytes()).collect()
}

/// A table that is known to be well formed.
fn table(address: u32, bytes: &[u8]) -> IndexTable<'_> {
    IndexTable::new(Region { address, bytes }).expect("the fixture is well formed")
}

const SP: u32 = 0x2000_1000;
const LR: u32 = 0x0800_0333;

/// The registers of a frame whose stack pointer and link register are known, and nothing else.
fn frame(sp: u32, lr: u32) -> Registers {
    let mut registers = [None; 16];
    registers[13] = Some(sp);
    registers[14] = Some(lr);
    registers
}

/// A compact description carrying `instructions`.
fn compact(instructions: &[u8]) -> Description {
    Description::Compact {
        personality: 1,
        table: None,
        instructions: instructions.to_vec(),
    }
}

/// A stack holding `slots` and nothing else.
fn stack(slots: &[(u32, u32)]) -> impl FnMut(u32) -> Option<u32> + '_ {
    move |address| {
        slots
            .iter()
            .find(|&&(at, _)| at == address)
            .map(|&(_, value)| value)
    }
}


#[test]
fn a_region_answers_a_word_only_when_it_holds_all_four_bytes() {
    let bytes = words(&[0x1122_3344, 0x5566_7788]);
    let region = Region { address: 0x1000, bytes: &bytes };
    assert_eq!(region.word(0x1000), Some(0x1122_3344));
    assert_eq!(region.word(0x1004), Some(0x5566_7788));
    assert_eq!(region.word(0x1006), None, "straddles the end");
    assert_eq!(region.word(0x0FFC), None, "before the start");
    assert_eq!(region.word(0xFFFF_FFFE), None, "an address near the top must not wrap into the region");
}


#[test]
fn a_prel31_offset_reaches_functions_before_and_after_the_table() {
    const TABLE: u32 = 0x0800_1000;
    let bytes = index(
        TABLE,
        &[(0x0800_0100, Second::CantUnwind), (0x0800_1100, Second::CantUnwind)],
    );
    let table = table(TABLE, &bytes);
    assert_eq!(table.len(), 2);
    assert!(!table.is_empty());
    assert_eq!(table.entry(0).map(|e| e.function), Some(0x0800_0100), "an offset back from the table");
    assert_eq!(table.entry(1).map(|e| e.function), Some(0x0800_1100), "an offset forward from the table");
    assert_eq!(table.entry(1).map(|e| e.place), Some(TABLE + 8), "each entry's place is its own address");
    assert_eq!(table.entry(2), None);
    assert_eq!(
        table.entries().collect::<Vec<_>>(),
        vec![table.entry(0).unwrap(), table.entry(1).unwrap()]
    );
}

#[test]
fn an_address_belongs_to_the_last_function_that_starts_at_or_below_it() {
    const TABLE: u32 = 0x0800_8000;
    let bytes = index(
        TABLE,
        &[
            (0x0800_0100, Second::CantUnwind),
            (0x0800_0200, Second::Inline(0x80A8_B0B0)),
            (0x0800_0300, Second::CantUnwind),
        ],
    );
    let table = table(TABLE, &bytes);
    let function = |address| table.lookup(address).map(|entry| entry.function);
    assert_eq!(function(0x0800_00FF), None, "below the first function nothing is described");
    assert_eq!(function(0x0800_0100), Some(0x0800_0100));
    assert_eq!(function(0x0800_01FF), Some(0x0800_0100));
    assert_eq!(function(0x0800_0200), Some(0x0800_0200));
    assert_eq!(function(0x0800_0250), Some(0x0800_0200));
    assert_eq!(
        function(0x0900_0000),
        Some(0x0800_0300),
        "the table records where functions start and not where they end"
    );
}

#[test]
fn the_thumb_bit_is_not_part_of_a_function_address() {
    const TABLE: u32 = 0x0800_8000;
    let bytes = index(
        TABLE,
        &[(0x0800_0101, Second::CantUnwind), (0x0800_0201, Second::CantUnwind)],
    );
    let table = table(TABLE, &bytes);
    assert_eq!(table.entry(0).map(|e| e.function), Some(0x0800_0100));
    assert_eq!(
        table.lookup(0x0800_0100).map(|e| e.function),
        Some(0x0800_0100),
        "a lookup at the function's first byte must find that function, not the one before it"
    );
}

#[test]
fn a_table_that_is_not_a_whole_number_of_entries_is_refused() {
    let bytes = [0u8; 12];
    assert_eq!(
        IndexTable::new(Region { address: 0x1000, bytes: &bytes }).err(),
        Some(Error::IndexLength { bytes: 12 })
    );
}

#[test]
fn a_table_out_of_order_is_refused_because_a_binary_search_would_answer_the_wrong_function() {
    const TABLE: u32 = 0x0800_8000;
    let bytes = index(
        TABLE,
        &[(0x0800_0200, Second::CantUnwind), (0x0800_0100, Second::CantUnwind)],
    );
    assert_eq!(
        IndexTable::new(Region { address: TABLE, bytes: &bytes }).err(),
        Some(Error::IndexOutOfOrder { entry: 1 })
    );
}

#[test]
fn a_function_word_with_bit_31_set_is_refused() {
    const TABLE: u32 = 0x0800_8000;
    let mut bytes = index(
        TABLE,
        &[(0x0800_0100, Second::CantUnwind), (0x0800_0200, Second::CantUnwind)],
    );
    bytes[11] |= 0x80;
    assert_eq!(
        IndexTable::new(Region { address: TABLE, bytes: &bytes }).err(),
        Some(Error::FunctionWordBit31 { entry: 1 })
    );
}

#[test]
fn the_second_word_is_cantunwind_an_inline_entry_or_an_offset_to_a_table_entry() {
    const TABLE: u32 = 0x0800_8000;
    let bytes = index(
        TABLE,
        &[
            (0x0800_0100, Second::CantUnwind),
            (0x0800_0200, Second::Inline(0x80A8_B0B0)),
            (0x0800_0300, Second::Table(0x0800_7000)),
        ],
    );
    let table = table(TABLE, &bytes);
    assert_eq!(table.entry(0).map(|e| e.content), Some(Content::CantUnwind));
    assert_eq!(table.entry(1).map(|e| e.content), Some(Content::Inline(0x80A8_B0B0)));
    assert_eq!(
        table.entry(2).map(|e| e.content),
        Some(Content::Table(0x0800_7000)),
        "the offset counts from the second word's own address"
    );
}


#[test]
fn cantunwind_describes_a_frame_that_cannot_be_unwound() {
    let entry = IndexEntry { place: 0x0800_8000, function: 0x0800_0100, content: Content::CantUnwind };
    assert_eq!(describe(&entry, &[]), Ok(Description::CantUnwind));
    let mut registers = frame(SP, LR);
    assert_eq!(
        unwind(&Description::CantUnwind, &mut registers, stack(&[])),
        Err(Error::CantUnwind)
    );
}

#[test]
fn an_inline_entry_is_personality_zero_with_three_instruction_bytes() {
    let entry = IndexEntry {
        place: 0x0800_8000,
        function: 0x0800_0100,
        content: Content::Inline(0x80A8_B0B0),
    };
    assert_eq!(
        describe(&entry, &[]),
        Ok(Description::Compact { personality: 0, table: None, instructions: vec![0xA8, 0xB0, 0xB0] })
    );
}

#[test]
fn a_table_entry_is_read_from_the_loaded_section_its_offset_points_into() {
    const EXTAB: u32 = 0x0800_7000;
    let extab = words(&[0xDEAD_BEEF, 0x8084_80B0]);
    let loaded = [Region { address: EXTAB, bytes: &extab }];
    let entry = IndexEntry {
        place: 0x0800_8000,
        function: 0x0800_0100,
        content: Content::Table(EXTAB + 4),
    };
    assert_eq!(
        describe(&entry, &loaded),
        Ok(Description::Compact {
            personality: 0,
            table: Some(EXTAB + 4),
            instructions: vec![0x84, 0x80, 0xB0],
        })
    );
}

#[test]
fn a_long_entry_reads_as_many_additional_words_as_its_count_says() {
    const EXTAB: u32 = 0x0800_7000;
    let extab = words(&[0x8101_4584, 0x8001_A600, 0x0000_0000]);
    let loaded = [Region { address: EXTAB, bytes: &extab }];
    let entry = IndexEntry { place: 0x0800_8000, function: 0x0800_0100, content: Content::Table(EXTAB) };
    assert_eq!(
        describe(&entry, &loaded),
        Ok(Description::Compact {
            personality: 1,
            table: Some(EXTAB),
            instructions: vec![0x45, 0x84, 0x80, 0x01, 0xA6, 0x00],
        })
    );
}

#[test]
fn personality_two_reads_its_instructions_as_personality_one_does() {
    const EXTAB: u32 = 0x0800_7000;
    let extab = words(&[0x8202_B0B0, 0x0102_0304, 0x0506_0708, 0]);
    let loaded = [Region { address: EXTAB, bytes: &extab }];
    let entry = IndexEntry { place: 0x0800_8000, function: 0x0800_0100, content: Content::Table(EXTAB) };
    assert_eq!(
        describe(&entry, &loaded),
        Ok(Description::Compact {
            personality: 2,
            table: Some(EXTAB),
            instructions: vec![0xB0, 0xB0, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08],
        })
    );
}

#[test]
fn a_generic_entry_names_its_personality_routine() {
    const EXTAB: u32 = 0x0800_7000;
    const ROUTINE: u32 = 0x0800_0401;
    let extab = words(&[prel31(ROUTINE, EXTAB), 0x1234_5678]);
    let loaded = [Region { address: EXTAB, bytes: &extab }];
    let entry = IndexEntry { place: 0x0800_8000, function: 0x0800_0100, content: Content::Table(EXTAB) };
    assert_eq!(
        describe(&entry, &loaded),
        Ok(Description::Generic { table: EXTAB, routine: ROUTINE })
    );
    let mut registers = frame(SP, LR);
    assert_eq!(
        unwind(&Description::Generic { table: EXTAB, routine: ROUTINE }, &mut registers, stack(&[])),
        Err(Error::GenericPersonality { routine: ROUTINE }),
        "what follows a generic entry's first word is private to its routine"
    );
}

#[test]
fn a_reserved_personality_index_is_refused() {
    for (word, index) in [(0x8300_0000u32, 3u8), (0x8F00_0000, 15)] {
        let entry = IndexEntry { place: 0x0800_8000, function: 0x0800_0100, content: Content::Inline(word) };
        assert_eq!(describe(&entry, &[]), Err(Error::ReservedPersonality { index }));
    }
}

#[test]
fn a_compact_header_with_bits_30_to_28_set_is_refused() {
    let entry = IndexEntry {
        place: 0x0800_8000,
        function: 0x0800_0100,
        content: Content::Inline(0x90A8_B0B0),
    };
    assert_eq!(describe(&entry, &[]), Err(Error::MalformedHeader { word: 0x90A8_B0B0 }));
}

#[test]
fn a_table_entry_that_is_not_loaded_is_refused() {
    const EXTAB: u32 = 0x0800_7000;
    let extab = words(&[0x8102_B0B0, 0x0102_0304]);
    let loaded = [Region { address: EXTAB, bytes: &extab }];

    let elsewhere =
        IndexEntry { place: 0x0800_8000, function: 0x0800_0100, content: Content::Table(0x0800_9000) };
    assert_eq!(describe(&elsewhere, &loaded), Err(Error::NotLoaded { address: 0x0800_9000 }));

    let short = IndexEntry { place: 0x0800_8000, function: 0x0800_0100, content: Content::Table(EXTAB) };
    assert_eq!(describe(&short, &loaded), Err(Error::NotLoaded { address: EXTAB + 8 }));
}

#[test]
fn an_inline_entry_cannot_carry_additional_words() {
    let long = IndexEntry { place: 0x0800_8000, function: 0x0800_0100, content: Content::Inline(0x8101_B0B0) };
    assert_eq!(describe(&long, &[]), Err(Error::InlineLongEntry { words: 1 }));

    let fits = IndexEntry { place: 0x0800_8000, function: 0x0800_0100, content: Content::Inline(0x8100_B0B0) };
    assert_eq!(
        describe(&fits, &[]),
        Ok(Description::Compact { personality: 1, table: None, instructions: vec![0xB0, 0xB0] })
    );
}


#[test]
fn each_instruction_decodes_as_the_frame_unwinding_table_defines_it() {
    use Instruction::*;
    let cases: &[(&[u8], Instruction, usize)] = &[
        (&[0x00], AddToStackPointer(4), 1),
        (&[0x3F], AddToStackPointer(256), 1),
        (&[0x40], SubtractFromStackPointer(4), 1),
        (&[0x7F], SubtractFromStackPointer(256), 1),
        (&[0x80, 0x00], RefuseToUnwind, 2),
        (&[0x80, 0x01], PopCore(1 << 4), 2),
        (&[0x84, 0x80], PopCore((1 << 11) | (1 << 14)), 2),
        (&[0x8F, 0xFF], PopCore(0xFFF0), 2),
        (&[0x97], StackPointerFromRegister(7), 1),
        (&[0x9D], Reserved(0x9D), 1),
        (&[0x9F], Reserved(0x9F), 1),
        (&[0xA0], PopCore(1 << 4), 1),
        (&[0xA7], PopCore(0x0FF0), 1),
        (&[0xA8], PopCore((1 << 4) | (1 << 14)), 1),
        (&[0xAF], PopCore(0x4FF0), 1),
        (&[0xB0], Finish, 1),
        (&[0xB1, 0x00], Reserved(0xB1), 2),
        (&[0xB1, 0x01], PopCore(0x0001), 2),
        (&[0xB1, 0x0F], PopCore(0x000F), 2),
        (&[0xB1, 0x10], Reserved(0xB1), 2),
        (&[0xB2, 0x00], AddToStackPointer(0x204), 2),
        (&[0xB2, 0x7F], AddToStackPointer(0x400), 2),
        (&[0xB2, 0x80, 0x01], AddToStackPointer(0x204 + (0x80 << 2)), 3),
        (&[0xB3, 0x07], PopVfp { first: 0, count: 8, bytes: 8 * 8 + 4 }, 2),
        (&[0xB3, 0xF0], PopVfp { first: 15, count: 1, bytes: 12 }, 2),
        (&[0xB3, 0xF1], Reserved(0xB3), 2),
        (&[0xB4], PopAuthenticationCode, 1),
        (&[0xB5], AuthenticationModifier, 1),
        (&[0xB6], Reserved(0xB6), 1),
        (&[0xB7], Reserved(0xB7), 1),
        (&[0xB8], PopVfp { first: 8, count: 1, bytes: 12 }, 1),
        (&[0xBF], PopVfp { first: 8, count: 8, bytes: 68 }, 1),
        (&[0xC0], PopWmmxData { first: 10, count: 1 }, 1),
        (&[0xC5], PopWmmxData { first: 10, count: 6 }, 1),
        (&[0xC6, 0x23], PopWmmxData { first: 2, count: 4 }, 2),
        (&[0xC6, 0xF1], Reserved(0xC6), 2),
        (&[0xC7, 0x00], Reserved(0xC7), 2),
        (&[0xC7, 0x05], PopWmmxControl(0x05), 2),
        (&[0xC7, 0x10], Reserved(0xC7), 2),
        (&[0xC8, 0x03], PopVfp { first: 16, count: 4, bytes: 32 }, 2),
        (&[0xC8, 0xF0], PopVfp { first: 31, count: 1, bytes: 8 }, 2),
        (&[0xC8, 0xF1], Reserved(0xC8), 2),
        (&[0xC9, 0x87], PopVfp { first: 8, count: 8, bytes: 64 }, 2),
        (&[0xCA], Reserved(0xCA), 1),
        (&[0xCF], Reserved(0xCF), 1),
        (&[0xD0], PopVfp { first: 8, count: 1, bytes: 8 }, 1),
        (&[0xD7], PopVfp { first: 8, count: 8, bytes: 64 }, 1),
        (&[0xD8], Reserved(0xD8), 1),
        (&[0xFF], Reserved(0xFF), 1),
    ];
    for (bytes, expected, length) in cases {
        assert_eq!(decode(bytes, 0), Ok((*expected, *length)), "{bytes:02x?}");
    }
    assert_eq!(
        decode(&[0xB0, 0x97], 1),
        Ok((StackPointerFromRegister(7), 1)),
        "decoding starts at the offset it is given"
    );
}

#[test]
fn an_instruction_cut_off_by_the_end_of_its_bytes_is_an_error_rather_than_a_guess() {
    let cases: &[(&[u8], usize)] = &[
        (&[0x80], 0),
        (&[0xB0, 0x84], 1),
        (&[0xB1], 0),
        (&[0xB2], 0),
        (&[0xB2, 0x80], 0),
        (&[0xB3], 0),
        (&[0xC6], 0),
        (&[0xC7], 0),
        (&[0xC8], 0),
        (&[0xC9], 0),
        (&[0xB0], 1),
    ];
    for &(bytes, at) in cases {
        assert_eq!(decode(bytes, at), Err(Error::Truncated { at }), "{bytes:02x?} at {at}");
    }
}

#[test]
fn an_increment_too_large_for_the_address_space_is_an_overflow() {
    assert_eq!(decode(&[0xB2, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F], 0), Err(Error::Overflow));
    assert_eq!(
        decode(&[0xB2, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01], 0),
        Err(Error::Overflow),
        "a LEB128 longer than any 32-bit value needs"
    );
}


#[test]
fn a_pop_takes_the_lowest_numbered_register_from_the_lowest_address() {
    let mut registers = frame(SP, LR);
    let restored = unwind(
        &compact(&[0x84, 0x80, 0xB0]),
        &mut registers,
        stack(&[(SP, 0x2000_2000), (SP + 4, 0x0800_0555)]),
    );
    assert_eq!(restored, Ok((1 << 11) | (1 << 14)));
    assert_eq!(registers[11], Some(0x2000_2000), "r11 from the lower slot");
    assert_eq!(registers[14], Some(0x0800_0555), "r14 from the higher slot");
    assert_eq!(registers[15], Some(0x0800_0555), "finish copies r14 into r15");
    assert_eq!(registers[13], Some(SP + 8));
}

#[test]
fn finish_leaves_a_program_counter_the_frame_popped_alone() {
    let mut registers = frame(SP, LR);
    let restored = unwind(
        &compact(&[0x88, 0x01, 0xB0]),
        &mut registers,
        stack(&[(SP, 0x44), (SP + 4, 0x0800_0777)]),
    );
    assert_eq!(restored, Ok((1 << 4) | (1 << 15)));
    assert_eq!(registers[15], Some(0x0800_0777), "the popped value, not r14");
    assert_eq!(registers[14], Some(LR));
    assert_eq!(registers[13], Some(SP + 8));
}

#[test]
fn an_implicit_finish_follows_the_last_instruction() {
    let mut registers = frame(SP, LR);
    assert_eq!(unwind(&compact(&[0x01]), &mut registers, stack(&[])), Ok(0));
    assert_eq!(registers[15], Some(LR));
    assert_eq!(registers[13], Some(SP + 8));
}

#[test]
fn a_popped_stack_pointer_is_written_back_after_the_whole_instruction() {
    let mut registers = frame(SP, LR);
    let restored = unwind(
        &compact(&[0x86, 0x00]),
        &mut registers,
        stack(&[(SP, 0x2000_3000), (SP + 4, 0x0800_0999)]),
    );
    assert_eq!(restored, Ok((1 << 13) | (1 << 14)));
    assert_eq!(
        registers[14],
        Some(0x0800_0999),
        "r14 comes from the slot after r13's: the loaded stack pointer does not move the read"
    );
    assert_eq!(registers[13], Some(0x2000_3000), "the loaded value, not the stack pointer after the pop");
    assert_eq!(registers[15], Some(0x0800_0999));
}

#[test]
fn the_stack_pointer_can_be_taken_from_a_register_the_frame_knows() {
    let mut registers = frame(SP, LR);
    registers[7] = Some(SP + 0x20);
    let restored = unwind(&compact(&[0x97, 0x80, 0x08]), &mut registers, stack(&[(SP + 0x20, 0x2000_5000)]));
    assert_eq!(restored, Ok(1 << 7));
    assert_eq!(registers[7], Some(0x2000_5000));
    assert_eq!(registers[13], Some(SP + 0x24));
    assert_eq!(registers[15], Some(LR));
}

#[test]
fn the_stack_pointer_cannot_be_taken_from_a_register_nobody_recovered() {
    let mut registers = frame(SP, LR);
    assert_eq!(
        unwind(&compact(&[0x97]), &mut registers, stack(&[])),
        Err(Error::UnknownRegister { register: 7 })
    );
}

#[test]
fn floating_point_pops_move_the_stack_by_the_space_their_registers_occupy() {
    let cases: &[(&[u8], u32)] = &[(&[0xC9, 0x87], 64), (&[0xB3, 0x07], 68), (&[0xD7], 64), (&[0xBF], 68)];
    for &(bytes, moved) in cases {
        let mut registers = frame(SP, LR);
        assert_eq!(unwind(&compact(bytes), &mut registers, stack(&[])), Ok(0), "{bytes:02x?}");
        assert_eq!(registers[13], Some(SP + moved), "{bytes:02x?}");
    }
}

#[test]
fn a_description_that_reads_below_the_stack_pointer_is_refused() {
    let mut registers = frame(SP, LR);
    assert_eq!(
        unwind(
            &compact(&[0x45, 0x84, 0x80, 0x01, 0xA6, 0x00]),
            &mut registers,
            |_| Some(0x0800_0001),
        ),
        Err(Error::BelowStackPointer { address: SP - 24, stack_pointer: SP }),
        "a word below the stack pointer is not preserved, so no frame's saved register is there"
    );
}

#[test]
fn refuse_to_unwind_and_reserved_instructions_stop_the_unwind() {
    let mut registers = frame(SP, LR);
    assert_eq!(unwind(&compact(&[0x80, 0x00]), &mut registers, stack(&[])), Err(Error::RefuseToUnwind));
    let mut registers = frame(SP, LR);
    assert_eq!(
        unwind(&compact(&[0x01, 0xB6]), &mut registers, stack(&[])),
        Err(Error::Reserved { at: 1, opcode: 0xB6 })
    );
}

#[test]
fn a_slot_that_cannot_be_read_is_an_error_rather_than_a_zero() {
    let mut registers = frame(SP, LR);
    assert_eq!(
        unwind(&compact(&[0xA8]), &mut registers, |_| None),
        Err(Error::Unreadable { address: SP })
    );
}

#[test]
fn a_frame_needs_its_stack_pointer_and_a_return_address() {
    let mut nothing = [None; 16];
    assert_eq!(
        unwind(&compact(&[0x01]), &mut nothing, stack(&[])),
        Err(Error::UnknownRegister { register: 13 })
    );
    let mut no_link = [None; 16];
    no_link[13] = Some(SP);
    assert_eq!(
        unwind(&compact(&[0x01]), &mut no_link, stack(&[])),
        Err(Error::UnknownRegister { register: 14 }),
        "finish copies r14, and an unknown r14 is not a return address"
    );
}


const SHT_PROGBITS: u32 = 1;
const SHT_NOBITS: u32 = 8;
const SHF_ALLOC: u32 = 0x2;
const SHF_EXECINSTR: u32 = 0x4;
const SHF_LINK_ORDER: u32 = 0x80;
const EM_ARM: u16 = 40;

/// A little-endian ELF32 executable holding `(name, type, flags, address, bytes)` sections after
/// the null section, with the section-name table last.
fn elf(machine: u16, sections: &[(&str, u32, u32, u32, &[u8])]) -> Vec<u8> {
    const EHDR: usize = 52;
    const SHDR: u16 = 40;

    let mut names = vec![0u8];
    let mut name_offsets = Vec::new();
    for (name, ..) in sections {
        name_offsets.push(names.len() as u32);
        names.extend_from_slice(name.as_bytes());
        names.push(0);
    }
    let names_name = names.len() as u32;
    names.extend_from_slice(b".shstrtab\0");

    let mut body = Vec::new();
    let mut offsets = Vec::new();
    for (.., bytes) in sections {
        offsets.push((EHDR + body.len()) as u32);
        body.extend_from_slice(bytes);
    }
    let names_offset = (EHDR + body.len()) as u32;
    body.extend_from_slice(&names);
    while body.len() % 4 != 0 {
        body.push(0);
    }
    let shoff = (EHDR + body.len()) as u32;
    let count = sections.len() as u16 + 2;

    let mut out = vec![0x7F, b'E', b'L', b'F', 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&machine.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&shoff.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(EHDR as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&SHDR.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&(count - 1).to_le_bytes());
    assert_eq!(out.len(), EHDR);
    out.extend_from_slice(&body);

    let mut header = |name: u32, kind: u32, flags: u32, address: u32, offset: u32, size: u32| {
        for field in [name, kind, flags, address, offset, size, 0, 0, 1, 0] {
            out.extend_from_slice(&field.to_le_bytes());
        }
    };
    header(0, 0, 0, 0, 0, 0);
    for (position, (_, kind, flags, address, bytes)) in sections.iter().enumerate() {
        header(name_offsets[position], *kind, *flags, *address, offsets[position], bytes.len() as u32);
    }
    header(names_name, 3, 0, 0, names_offset, names.len() as u32);
    out
}

#[test]
fn an_arm_elf_hands_over_its_index_table_and_every_loaded_section() {
    let text = [0u8; 8];
    let extab = words(&[0x8084_80B0]);
    let exidx = index(0x0800_0100, &[(0x0800_0000, Second::Table(0x0800_00F8))]);
    let file = elf(
        EM_ARM,
        &[
            (".text", SHT_PROGBITS, SHF_ALLOC | SHF_EXECINSTR, 0x0800_0000, &text),
            (".ARM.extab", SHT_PROGBITS, SHF_ALLOC, 0x0800_00F8, &extab),
            (".ARM.exidx", SHT_ARM_EXIDX, SHF_ALLOC | SHF_LINK_ORDER, 0x0800_0100, &exidx),
            (".debug_frame", SHT_PROGBITS, 0, 0, &[1, 2, 3, 4]),
        ],
    );
    let found = tables(&file);
    assert_eq!(found.indexes, vec![Region { address: 0x0800_0100, bytes: &exidx[..] }]);
    assert_eq!(
        found.loaded.iter().map(|region| region.address).collect::<Vec<_>>(),
        vec![0x0800_0000, 0x0800_00F8, 0x0800_0100],
        "a debug section occupies no memory, so nothing is loaded from it"
    );

    let entry = table(found.indexes[0].address, found.indexes[0].bytes)
        .lookup(0x0800_0004)
        .expect("the function is described");
    assert_eq!(
        describe(&entry, &found.loaded),
        Ok(Description::Compact {
            personality: 0,
            table: Some(0x0800_00F8),
            instructions: vec![0x84, 0x80, 0xB0],
        })
    );
}

#[test]
fn a_section_type_is_an_index_table_only_on_an_arm_file() {
    let exidx = index(0x1000, &[(0x0800, Second::CantUnwind)]);
    let file = elf(3, &[(".ARM.exidx", SHT_ARM_EXIDX, SHF_ALLOC, 0x1000, &exidx)]);
    assert!(
        tables(&file).indexes.is_empty(),
        "a processor-specific section type means what that machine's ABI says it means"
    );
}

#[test]
fn a_section_that_occupies_no_file_bytes_is_not_loaded() {
    let exidx = index(0x1000, &[(0x0800, Second::CantUnwind)]);
    let file = elf(
        EM_ARM,
        &[
            (".bss", SHT_NOBITS, SHF_ALLOC, 0x2000_0000, &[0xAA; 4]),
            (".ARM.exidx", SHT_ARM_EXIDX, SHF_ALLOC, 0x1000, &exidx),
        ],
    );
    assert_eq!(
        tables(&file).loaded.iter().map(|region| region.address).collect::<Vec<_>>(),
        vec![0x1000],
        "the bytes at a NOBITS section's offset belong to whatever the file put there"
    );
}
