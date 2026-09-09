//! Unit tests over hand-built sections.

use alloc::vec;
use alloc::vec::Vec;

use crate::cursor::{Cursor, Format};
use crate::line::{self, Row};
use crate::{DebugInfo, DeviceRow, DwarfError, Options, Sections};

/// Builds a DWARF 5 line-program unit around a caller-supplied program body.
///
/// One directory and one file, both inline strings, which is the shape the AOT backend emits.
fn unit_v5(program: &[u8]) -> Vec<u8> {
    let mut header = Vec::new();
    header.push(1);
    header.push(1);
    header.push(1);
    header.push(0xfbu8);
    header.push(14);
    header.push(13);
    header.extend_from_slice(&[0, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1]);
    header.push(1);
    header.extend_from_slice(&[0x01, 0x08]);
    header.push(1);
    header.extend_from_slice(b"dir\0");
    header.push(2);
    header.extend_from_slice(&[0x01, 0x08, 0x02, 0x0f]);
    header.push(3);
    header.extend_from_slice(b"main.c\0");
    header.push(0);
    header.extend_from_slice(b"other.c\0");
    header.push(0);
    header.extend_from_slice(b"/abs/root.c\0");
    header.push(0);

    let mut body = Vec::new();
    body.extend_from_slice(&5u16.to_le_bytes());
    body.push(4);
    body.push(0);
    body.extend_from_slice(&(header.len() as u32).to_le_bytes());
    body.extend_from_slice(&header);
    body.extend_from_slice(program);

    let mut out = Vec::new();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// Builds a version 2 line-program unit -- the other header shape, with NUL-terminated lists and
/// no address size of its own.
fn unit_v2(program: &[u8]) -> Vec<u8> {
    let mut header = Vec::new();
    header.push(1);
    header.push(1);
    header.push(0xfbu8);
    header.push(14);
    header.push(13);
    header.extend_from_slice(&[0, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1]);
    header.extend_from_slice(b"dir\0");
    header.push(0);
    header.extend_from_slice(b"main.c\0");
    header.push(1);
    header.push(0);
    header.push(0);
    header.push(0);

    let mut body = Vec::new();
    body.extend_from_slice(&2u16.to_le_bytes());
    body.extend_from_slice(&(header.len() as u32).to_le_bytes());
    body.extend_from_slice(&header);
    body.extend_from_slice(program);

    let mut out = Vec::new();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// `DW_LNE_set_address` with a 4-byte operand.
fn set_address(address: u32) -> Vec<u8> {
    let mut out = vec![0x00, 0x05, 0x02];
    out.extend_from_slice(&address.to_le_bytes());
    out
}

const END_SEQUENCE: [u8; 3] = [0x00, 0x01, 0x01];

fn rows_of(section: &[u8]) -> Vec<Row> {
    let sections = Sections {
        debug_line: section,
        ..Sections::default()
    };
    line::programs(&sections)
        .expect("the unit parses")
        .remove(0)
        .rows
}

#[test]
fn a_version_5_program_decodes_its_rows() {
    let mut program = set_address(0x1000);
    program.push(0x01);
    program.push(0x02);
    program.push(0x04);
    program.push(0x03);
    program.push(0x09);
    program.push(0x01);
    program.extend_from_slice(&[0x02, 0x04]);
    program.extend_from_slice(&END_SEQUENCE);
    let rows = rows_of(&unit_v5(&program));
    assert_eq!(rows.len(), 3);
    assert_eq!((rows[0].address, rows[0].line), (0x1000, 1));
    assert_eq!((rows[1].address, rows[1].line), (0x1004, 10));
    assert!(rows[2].end_sequence);
    assert_eq!(rows[2].address, 0x1008);
}

#[test]
fn a_version_2_header_is_a_different_shape_at_the_same_offset() {
    let mut program = set_address(0x2000);
    program.push(0x01);
    program.extend_from_slice(&END_SEQUENCE);
    let rows = rows_of(&unit_v2(&program));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].address, 0x2000);
    assert_eq!(rows[0].line, 1);
    assert_eq!(rows[0].file, 1, "versions before 5 number files from one");
}

#[test]
fn a_version_2_set_address_takes_its_width_from_the_instruction_length() {
    let mut program = vec![0x00, 0x09, 0x02];
    program.extend_from_slice(&0x1_0000_2000u64.to_le_bytes());
    program.push(0x01);
    program.extend_from_slice(&END_SEQUENCE);
    let rows = rows_of(&unit_v2(&program));
    assert_eq!(rows[0].address, 0x1_0000_2000);
}

#[test]
fn a_special_opcode_advances_both_registers() {
    let mut program = set_address(0x100);
    program.push(0x03);
    program.push(0x0a);
    program.push(0x20);
    program.extend_from_slice(&END_SEQUENCE);
    let rows = rows_of(&unit_v5(&program));
    assert_eq!((rows[0].address, rows[0].line), (0x101, 11));
}

#[test]
fn fixed_advance_pc_is_a_uhalf_and_is_not_scaled() {
    let mut program = set_address(0x100);
    program.push(0x09);
    program.extend_from_slice(&0x0123u16.to_le_bytes());
    program.push(0x01);
    program.extend_from_slice(&END_SEQUENCE);
    let rows = rows_of(&unit_v5(&program));
    assert_eq!(rows[0].address, 0x100 + 0x123);
}

#[test]
fn the_flag_opcodes_reach_the_row_and_then_reset() {
    let mut program = set_address(0x100);
    program.push(0x06);
    program.push(0x07);
    program.push(0x0a);
    program.push(0x0b);
    program.push(0x01);
    program.push(0x01);
    program.extend_from_slice(&END_SEQUENCE);
    let rows = rows_of(&unit_v5(&program));
    assert!(!rows[0].is_stmt);
    assert!(rows[0].basic_block && rows[0].prologue_end && rows[0].epilogue_begin);
    assert!(
        !rows[1].basic_block && !rows[1].prologue_end && !rows[1].epilogue_begin,
        "a row-appending opcode resets these four"
    );
    assert!(!rows[1].is_stmt, "is_stmt is NOT one of the four");
}

#[test]
fn an_unknown_standard_opcode_is_skipped_by_its_declared_operand_count() {
    let mut header = Vec::new();
    header.extend_from_slice(&[1, 1, 1, 0xfb, 14, 15]);
    header.extend_from_slice(&[0, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1, 2, 1]);
    header.push(1);
    header.extend_from_slice(&[0x01, 0x08]);
    header.push(1);
    header.extend_from_slice(b"dir\0");
    header.push(2);
    header.extend_from_slice(&[0x01, 0x08, 0x02, 0x0f]);
    header.push(1);
    header.extend_from_slice(b"main.c\0");
    header.push(0);

    let mut program = set_address(0x100);
    program.push(13);
    program.push(0x7f);
    program.push(0x7f);
    program.push(14);
    program.push(0x7f);
    program.push(0x01);
    program.extend_from_slice(&END_SEQUENCE);

    let mut body = Vec::new();
    body.extend_from_slice(&5u16.to_le_bytes());
    body.push(4);
    body.push(0);
    body.extend_from_slice(&(header.len() as u32).to_le_bytes());
    body.extend_from_slice(&header);
    body.extend_from_slice(&program);
    let mut section = Vec::new();
    section.extend_from_slice(&(body.len() as u32).to_le_bytes());
    section.extend_from_slice(&body);

    let rows = rows_of(&section);
    assert_eq!(rows.len(), 2, "the two unknown opcodes were stepped over");
    assert_eq!(rows[0].address, 0x100);
}

#[test]
fn an_unknown_extended_opcode_is_skipped_by_its_length() {
    let mut program = set_address(0x100);
    program.extend_from_slice(&[0x00, 0x04, 0x80, 0xde, 0xad, 0xbe]);
    program.push(0x01);
    program.extend_from_slice(&END_SEQUENCE);
    let rows = rows_of(&unit_v5(&program));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].address, 0x100);
}

#[test]
fn set_discriminator_is_read_and_reset_by_a_row() {
    let mut program = set_address(0x100);
    program.extend_from_slice(&[0x00, 0x02, 0x04, 0x07]);
    program.push(0x01);
    program.push(0x01);
    program.extend_from_slice(&END_SEQUENCE);
    let rows = rows_of(&unit_v5(&program));
    assert_eq!(rows[0].discriminator, 7);
    assert_eq!(rows[1].discriminator, 0);
}

#[test]
fn a_header_field_that_would_divide_by_zero_is_refused_rather_than_reached() {
    for (index, value) in [(4usize, 0u8), (5, 0)] {
        let mut section = unit_v5(&[]);
        section[4 + 8 + index] = value;
        let sections = Sections {
            debug_line: &section,
            ..Sections::default()
        };
        let error = line::programs(&sections).expect_err("a zero divisor is refused");
        assert!(
            matches!(
                error,
                DwarfError::ZeroLineRange | DwarfError::ZeroOpcodeBase
            ),
            "unexpected error {error:?}"
        );
    }
}

#[test]
fn a_truncated_unit_reports_truncation_rather_than_panicking() {
    let mut program = set_address(0x100);
    program.push(0x02);
    let section = unit_v5(&program);
    let sections = Sections {
        debug_line: &section,
        ..Sections::default()
    };
    assert_eq!(
        line::programs(&sections).err(),
        Some(DwarfError::Truncated)
    );
}

#[test]
fn a_reserved_initial_length_is_not_read_as_a_unit() {
    let mut cursor = Cursor::new(&[0xf5, 0xff, 0xff, 0xff]);
    assert!(matches!(
        cursor.initial_length(),
        Err(DwarfError::ReservedInitialLength(_))
    ));
}

#[test]
fn a_64_bit_initial_length_selects_8_byte_offsets() {
    let mut bytes = vec![0xff, 0xff, 0xff, 0xff];
    bytes.extend_from_slice(&0x20u64.to_le_bytes());
    let mut cursor = Cursor::new(&bytes);
    assert_eq!(cursor.initial_length(), Ok((0x20, Format::Dwarf64)));
    assert_eq!(Format::Dwarf64.offset_size(), 8);
}

#[test]
fn leb128_decodes_both_signs_and_refuses_a_run_that_cannot_be_one() {
    let mut cursor = Cursor::new(&[0xe5, 0x8e, 0x26]);
    assert_eq!(cursor.uleb128(), Ok(624_485));
    let mut cursor = Cursor::new(&[0x9b, 0xf1, 0x59]);
    assert_eq!(cursor.sleb128(), Ok(-624_485));
    let mut cursor = Cursor::new(&[0x80; 12]);
    assert_eq!(cursor.uleb128(), Err(DwarfError::MalformedLeb128));
}

#[test]
fn an_address_is_resolved_to_the_row_that_covers_it_and_not_past_the_sequence() {
    let mut program = set_address(0x1000);
    program.push(0x03);
    program.push(0x09);
    program.push(0x01);
    program.extend_from_slice(&[0x02, 0x10]);
    program.extend_from_slice(&END_SEQUENCE);
    let section = unit_v5(&program);
    let sections = Sections {
        debug_line: &section,
        ..Sections::default()
    };
    let info = DebugInfo::parse(&sections).expect("parses");
    assert_eq!(info.location_for_address(0x1000).map(|l| l.line), Some(10));
    assert_eq!(info.location_for_address(0x100f).map(|l| l.line), Some(10));
    assert!(
        info.location_for_address(0x1010).is_none(),
        "the closing row bounds the sequence and describes nothing itself"
    );
    assert!(info.location_for_address(0x0fff).is_none());
}

#[test]
fn a_line_0_row_reports_no_line_and_the_attributed_one_separately() {
    let mut program = set_address(0x1000);
    program.push(0x03);
    program.push(0x09);
    program.push(0x01);
    program.extend_from_slice(&[0x02, 0x04]);
    program.push(0x03);
    program.push(0x76);
    program.push(0x01);
    program.extend_from_slice(&[0x02, 0x04]);
    program.extend_from_slice(&END_SEQUENCE);
    let section = unit_v5(&program);
    let sections = Sections {
        debug_line: &section,
        ..Sections::default()
    };
    let info = DebugInfo::parse(&sections).expect("parses");
    assert_eq!(info.location_for_address(0x1004).map(|l| l.line), Some(0));
    assert_eq!(
        info.nearest_line_for_address(0x1004).map(|l| l.line),
        Some(10),
        "the attributed line is the last one before it that named a position"
    );
}

#[test]
fn the_thumb_bit_is_cleared_only_when_the_caller_says_the_target_carries_one() {
    let mut program = set_address(0x1001);
    program.push(0x01);
    program.extend_from_slice(&[0x02, 0x10]);
    program.extend_from_slice(&END_SEQUENCE);
    let section = unit_v5(&program);
    let sections = Sections {
        debug_line: &section,
        ..Sections::default()
    };
    let faithful = DebugInfo::parse(&sections).expect("parses");
    assert_eq!(faithful.programs()[0].rows[0].address, 0x1001);

    let masked = DebugInfo::parse_with(
        &sections,
        &Options {
            arm_thumb_addresses: true,
        },
    )
    .expect("parses");
    assert_eq!(masked.programs()[0].rows[0].address, 0x1000);
    assert_eq!(masked.location_for_address(0x1000).map(|l| l.line), Some(1));
}

#[test]
fn a_statement_row_wins_over_a_non_statement_row_at_the_same_address() {
    let mut program = set_address(0x1000);
    program.push(0x03);
    program.push(0x09);
    program.push(0x01);
    program.push(0x03);
    program.push(0x05);
    program.push(0x06);
    program.push(0x01);
    program.extend_from_slice(&[0x02, 0x08]);
    program.extend_from_slice(&END_SEQUENCE);
    let section = unit_v5(&program);
    let sections = Sections {
        debug_line: &section,
        ..Sections::default()
    };
    let info = DebugInfo::parse(&sections).expect("parses");
    assert_eq!(info.location_for_address(0x1000).map(|l| l.line), Some(10));
    assert_eq!(
        info.locations_for_address(0x1000).len(),
        1,
        "rows sharing an address are one description, not an overlap"
    );
}

#[test]
fn a_file_entry_resolves_through_its_directory_and_an_absolute_path_ignores_it() {
    let mut program = set_address(0x1000);
    program.push(0x04);
    program.push(0x01);
    program.push(0x01);
    program.extend_from_slice(&[0x02, 0x08]);
    program.push(0x04);
    program.push(0x02);
    program.push(0x01);
    program.extend_from_slice(&[0x02, 0x08]);
    program.extend_from_slice(&END_SEQUENCE);
    let section = unit_v5(&program);
    let sections = Sections {
        debug_line: &section,
        ..Sections::default()
    };
    let info = DebugInfo::parse(&sections).expect("parses");
    assert_eq!(
        info.location_for_address(0x1000).expect("a row").path(),
        "dir/other.c"
    );
    assert_eq!(
        info.location_for_address(0x1008).expect("a row").path(),
        "/abs/root.c",
        "a stored path that already names a root does not get a directory joined onto it"
    );
}

#[test]
fn a_line_query_matches_on_whole_path_components() {
    let mut program = set_address(0x1000);
    program.push(0x04);
    program.push(0x00);
    program.push(0x03);
    program.push(0x09);
    program.push(0x01);
    program.extend_from_slice(&[0x02, 0x08]);
    program.extend_from_slice(&END_SEQUENCE);
    let section = unit_v5(&program);
    let sections = Sections {
        debug_line: &section,
        ..Sections::default()
    };
    let info = DebugInfo::parse(&sections).expect("parses");
    assert_eq!(info.addresses_for_line("main.c", 10), vec![0x1000]);
    assert_eq!(info.addresses_for_line("dir/main.c", 10), vec![0x1000]);
    assert!(
        info.addresses_for_line("ain.c", 10).is_empty(),
        "a suffix that is not a whole component does not match"
    );
    assert!(info.addresses_for_line("main.c", 11).is_empty());
}

#[test]
fn a_unit_with_no_rows_is_not_an_error() {
    let section = unit_v5(&[]);
    let sections = Sections {
        debug_line: &section,
        ..Sections::default()
    };
    let programs = line::programs(&sections).expect("parses");
    assert_eq!(programs.len(), 1);
    assert!(programs[0].rows.is_empty());
    assert_eq!(programs[0].files.len(), 3);
}

#[test]
fn trailing_zero_padding_after_the_last_unit_does_not_invent_one() {
    let mut section = unit_v5(&[]);
    section.extend_from_slice(&[0, 0, 0, 0]);
    let sections = Sections {
        debug_line: &section,
        ..Sections::default()
    };
    assert_eq!(line::programs(&sections).expect("parses").len(), 1);
}

#[test]
fn a_device_table_carries_the_file_of_every_row_and_not_of_the_program() {
    let mut program = set_address(0x1000);
    program.push(0x04);
    program.push(0x00);
    program.push(0x03);
    program.push(0x09);
    program.push(0x01);
    program.extend_from_slice(&[0x02, 0x04]);
    program.push(0x04);
    program.push(0x01);
    program.push(0x03);
    program.push(0x05);
    program.push(0x01);
    program.extend_from_slice(&[0x02, 0x04]);
    program.push(0x04);
    program.push(0x00);
    program.push(0x01);
    program.extend_from_slice(&[0x02, 0x04]);
    program.extend_from_slice(&END_SEQUENCE);
    let section = unit_v5(&program);
    let sections = Sections {
        debug_line: &section,
        ..Sections::default()
    };
    let info = DebugInfo::parse(&sections).expect("parses");

    let table = info.device_lines(0x1000);
    assert_eq!(table.files, vec!["dir/main.c", "dir/other.c"]);
    assert_eq!(
        table.rows,
        vec![
            DeviceRow { offset: 0, line: 10, file: 0 },
            DeviceRow { offset: 4, line: 15, file: 1 },
            DeviceRow { offset: 8, line: 15, file: 0 },
        ]
    );
    assert_eq!(table.file_of(&table.rows[1]), Some("dir/other.c"));
    assert_eq!(
        table.single_file(),
        None,
        "a program built from two files has no single file, and saying it has one is the defect"
    );
    assert_eq!(table.below_base, None);
    assert!(
        !table.rows.iter().any(|row| row.offset == 12),
        "the closing row of a sequence names no code and is not a row of the table"
    );
}

#[test]
fn a_single_file_program_answers_with_its_one_file() {
    let mut program = set_address(0x1000);
    program.push(0x04);
    program.push(0x00);
    program.push(0x01);
    program.extend_from_slice(&[0x02, 0x04]);
    program.extend_from_slice(&END_SEQUENCE);
    let section = unit_v5(&program);
    let sections = Sections {
        debug_line: &section,
        ..Sections::default()
    };
    let info = DebugInfo::parse(&sections).expect("parses");
    assert_eq!(
        info.device_lines(0x1000).single_file(),
        Some("dir/main.c"),
        "the shape an AOT program of one source file has, which is the case that hid the defect"
    );
}

#[test]
fn a_row_below_the_image_base_is_reported_rather_than_offset_or_dropped_silently() {
    let mut program = set_address(0x40);
    program.push(0x01);
    program.extend_from_slice(&[0x02, 0x04]);
    program.extend_from_slice(&END_SEQUENCE);
    let mut second = set_address(0x1000);
    second.push(0x03);
    second.push(0x09);
    second.push(0x01);
    second.extend_from_slice(&[0x02, 0x04]);
    second.extend_from_slice(&END_SEQUENCE);
    let mut section = unit_v5(&program);
    section.extend_from_slice(&unit_v5(&second));
    let sections = Sections {
        debug_line: &section,
        ..Sections::default()
    };
    let info = DebugInfo::parse(&sections).expect("parses");

    let table = info.device_lines(0x1000);
    assert_eq!(table.below_base, Some(0x40));
    assert_eq!(
        table.rows,
        vec![DeviceRow { offset: 0, line: 10, file: 0 }],
        "a row below the base is left out of the table rather than wrapped into one"
    );
}

#[test]
fn a_device_table_deduplicates_a_row_two_units_describe_identically() {
    let mut program = set_address(0x1000);
    program.push(0x03);
    program.push(0x09);
    program.push(0x01);
    program.extend_from_slice(&[0x02, 0x04]);
    program.extend_from_slice(&END_SEQUENCE);
    let mut section = unit_v5(&program);
    section.extend_from_slice(&unit_v5(&program));
    let sections = Sections {
        debug_line: &section,
        ..Sections::default()
    };
    let info = DebugInfo::parse(&sections).expect("parses");
    let table = info.device_lines(0x1000);
    assert_eq!(table.files.len(), 1, "one path is one file, whichever unit named it");
    assert_eq!(table.rows, vec![DeviceRow { offset: 0, line: 10, file: 0 }]);

    let mut elsewhere = set_address(0x1000);
    elsewhere.push(0x04);
    elsewhere.push(0x00);
    elsewhere.push(0x03);
    elsewhere.push(0x09);
    elsewhere.push(0x01);
    elsewhere.extend_from_slice(&[0x02, 0x04]);
    elsewhere.extend_from_slice(&END_SEQUENCE);
    let mut section = unit_v5(&program);
    section.extend_from_slice(&unit_v5(&elsewhere));
    let sections = Sections {
        debug_line: &section,
        ..Sections::default()
    };
    let info = DebugInfo::parse(&sections).expect("parses");
    let table = info.device_lines(0x1000);
    assert_eq!(table.files, vec!["dir/other.c", "dir/main.c"]);
    assert_eq!(
        table.rows,
        vec![
            DeviceRow { offset: 0, line: 10, file: 0 },
            DeviceRow { offset: 0, line: 10, file: 1 },
        ],
        "one offset described by two files is two facts, and the table keeps both"
    );
}

#[test]
fn the_placeholder_file_of_a_pre_version_5_table_is_not_a_file() {
    let mut program = set_address(0x2000);
    program.push(0x04);
    program.push(0x00);
    program.push(0x01);
    program.extend_from_slice(&[0x02, 0x04]);
    program.extend_from_slice(&END_SEQUENCE);
    let section = unit_v2(&program);
    let sections = Sections {
        debug_line: &section,
        ..Sections::default()
    };
    let info = DebugInfo::parse(&sections).expect("parses");
    let table = info.device_lines(0x2000);
    assert!(table.files.is_empty());
    assert!(table.rows.is_empty());
}


fn uleb(value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut value = value;
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
    out
}

fn sleb(value: i64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut value = value;
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        let sign = byte & 0x40 != 0;
        if (value == 0 && !sign) || (value == -1 && sign) {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
    out
}

/// A `.debug_frame` CIE in the 32-bit format.
fn cie(version: u8, code_alignment: u64, data_alignment: i64, ra: u64, program: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&u32::MAX.to_le_bytes());
    body.push(version);
    body.push(0);
    if version >= 4 {
        body.push(4);
        body.push(0);
    }
    body.extend_from_slice(&uleb(code_alignment));
    body.extend_from_slice(&sleb(data_alignment));
    if version == 1 {
        body.push(ra as u8);
    } else {
        body.extend_from_slice(&uleb(ra));
    }
    body.extend_from_slice(program);
    let mut out = Vec::new();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

fn fde(cie_offset: u32, start: u32, length: u32, program: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&cie_offset.to_le_bytes());
    body.extend_from_slice(&start.to_le_bytes());
    body.extend_from_slice(&length.to_le_bytes());
    body.extend_from_slice(program);
    let mut out = Vec::new();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

fn frames(section: &[u8]) -> crate::frame::FrameTable<'_> {
    let sections = Sections {
        debug_frame: section,
        ..Sections::default()
    };
    crate::frame::FrameTable::parse(&sections).expect("the section parses")
}

fn def_cfa(register: u64, offset: u64) -> Vec<u8> {
    let mut out = vec![0x0c];
    out.extend_from_slice(&uleb(register));
    out.extend_from_slice(&uleb(offset));
    out
}

fn cfa_offset_at(table: &crate::frame::FrameTable<'_>, address: u64) -> i64 {
    match table.unwind(address).expect("no error").expect("a row").cfa {
        crate::CfaRule::RegisterOffset { offset, .. } => offset,
        other => panic!("expected a register rule, got {other:?}"),
    }
}

#[test]
fn a_version_1_cie_reads_its_return_address_register_as_a_byte() {
    let mut program = def_cfa(13, 0);
    program.push(0x8e);
    program.extend_from_slice(&uleb(1));
    let mut section = cie(1, 1, -4, 0x81, &program);
    section.extend_from_slice(&fde(0, 0x1000, 0x10, &[]));
    let table = frames(&section);
    assert_eq!(table.cies()[0].version, 1);
    assert_eq!(table.cies()[0].return_address_register, 0x81);
    let row = table.unwind(0x1000).expect("no error").expect("a row");
    assert_eq!(
        row.cfa,
        crate::CfaRule::RegisterOffset {
            register: 13,
            offset: 0
        },
        "the instructions after the register field still decode"
    );
    assert_eq!(row.register(14), crate::RegisterRule::Offset(-4));
}

#[test]
fn a_version_3_cie_has_no_address_size_field_and_a_leb_return_register() {
    let mut section = cie(3, 2, -4, 14, &def_cfa(13, 8));
    section.extend_from_slice(&fde(0, 0x2000, 0x10, &[]));
    let table = frames(&section);
    assert_eq!(table.cies()[0].version, 3);
    assert_eq!(table.cies()[0].code_alignment, 2);
    assert_eq!(table.cies()[0].return_address_register, 14);
    assert_eq!(cfa_offset_at(&table, 0x2000), 8);
}

#[test]
fn every_advance_form_scales_by_the_code_alignment() {
    let mut program = def_cfa(13, 0);
    program.push(0x41);
    program.push(0x0e);
    program.extend_from_slice(&uleb(4));
    program.extend_from_slice(&[0x02, 3]);
    program.push(0x0e);
    program.extend_from_slice(&uleb(8));
    program.extend_from_slice(&[0x03, 0x02, 0x01]);
    program.push(0x0e);
    program.extend_from_slice(&uleb(12));
    program.extend_from_slice(&[0x04, 0x03, 0x00, 0x02, 0x00]);
    program.push(0x0e);
    program.extend_from_slice(&uleb(16));
    let mut section = cie(4, 2, -4, 14, &[]);
    section.extend_from_slice(&fde(0, 0x1000, 0x50000, &program));
    let table = frames(&section);
    for (address, offset) in [
        (0x1000, 0),
        (0x1001, 0),
        (0x1002, 4),
        (0x1007, 4),
        (0x1008, 8),
        (0x120b, 8),
        (0x120c, 12),
        (0x41211, 12),
        (0x41212, 16),
    ] {
        assert_eq!(
            cfa_offset_at(&table, address),
            offset,
            "at {address:#x}, with a code alignment of 2"
        );
    }
}
#[test]
fn set_loc_moves_the_row_outright_rather_than_by_a_delta() {
    let mut program = def_cfa(13, 0);
    program.push(0x01);
    program.extend_from_slice(&0x1020u32.to_le_bytes());
    program.push(0x0e);
    program.extend_from_slice(&uleb(24));
    let mut section = cie(4, 1, -4, 14, &[]);
    section.extend_from_slice(&fde(0, 0x1000, 0x40, &program));
    let table = frames(&section);
    assert_eq!(cfa_offset_at(&table, 0x101f), 0);
    assert_eq!(
        cfa_offset_at(&table, 0x1020),
        24,
        "a set_loc takes effect AT the address it names"
    );
}

#[test]
fn remember_and_restore_state_nest() {
    let mut program = def_cfa(13, 0);
    program.push(0x0a);
    program.push(0x0e);
    program.extend_from_slice(&uleb(8));
    program.push(0x41);
    program.push(0x0a);
    program.push(0x0e);
    program.extend_from_slice(&uleb(16));
    program.push(0x41);
    program.push(0x0b);
    program.push(0x41);
    program.push(0x0b);
    let mut section = cie(4, 1, -4, 14, &[]);
    section.extend_from_slice(&fde(0, 0x1000, 0x40, &program));
    let table = frames(&section);
    assert_eq!(cfa_offset_at(&table, 0x1000), 8);
    assert_eq!(cfa_offset_at(&table, 0x1001), 16);
    assert_eq!(
        cfa_offset_at(&table, 0x1002),
        8,
        "the inner restore pops the inner remember"
    );
    assert_eq!(
        cfa_offset_at(&table, 0x1003),
        0,
        "the outer restore pops the outer one"
    );
}

#[test]
fn restore_returns_a_register_to_the_row_the_cie_established() {
    let mut initial = def_cfa(13, 0);
    initial.push(0x87);
    initial.extend_from_slice(&uleb(2));
    let mut program = Vec::new();
    program.push(0x87);
    program.extend_from_slice(&uleb(4));
    program.push(0x41);
    program.push(0xc7);
    program.push(0x41);
    program.push(0x06);
    program.extend_from_slice(&uleb(7));
    let mut section = cie(4, 1, -4, 14, &initial);
    section.extend_from_slice(&fde(0, 0x1000, 0x40, &program));
    let table = frames(&section);
    let rule = |address: u64| {
        table
            .unwind(address)
            .expect("no error")
            .expect("a row")
            .register(7)
    };
    assert_eq!(rule(0x1000), crate::RegisterRule::Offset(-16));
    assert_eq!(rule(0x1001), crate::RegisterRule::Offset(-8));
    assert_eq!(rule(0x1002), crate::RegisterRule::Offset(-8));
}

#[test]
fn the_signed_forms_carry_an_offset_the_unsigned_ones_cannot() {
    let mut program = Vec::new();
    program.push(0x12);
    program.extend_from_slice(&uleb(13));
    program.extend_from_slice(&sleb(-2));
    program.push(0x11);
    program.extend_from_slice(&uleb(4));
    program.extend_from_slice(&sleb(-3));
    program.push(0x41);
    program.push(0x13);
    program.extend_from_slice(&sleb(-4));
    program.push(0x15);
    program.extend_from_slice(&uleb(5));
    program.extend_from_slice(&sleb(1));
    let mut section = cie(4, 1, -4, 14, &[]);
    section.extend_from_slice(&fde(0, 0x1000, 0x40, &program));
    let table = frames(&section);
    let row = table.unwind(0x1000).expect("no error").expect("a row");
    assert_eq!(
        row.cfa,
        crate::CfaRule::RegisterOffset {
            register: 13,
            offset: 8
        }
    );
    assert_eq!(row.register(4), crate::RegisterRule::Offset(12));

    let row = table.unwind(0x1001).expect("no error").expect("a row");
    assert_eq!(
        row.cfa,
        crate::CfaRule::RegisterOffset {
            register: 13,
            offset: 16
        }
    );
    assert_eq!(
        row.register(5),
        crate::RegisterRule::ValOffset(-4),
        "a val_offset is the ADDRESS, not the memory at it"
    );
}

#[test]
fn same_value_undefined_and_register_are_carried_as_they_are() {
    let mut program = Vec::new();
    program.push(0x08);
    program.extend_from_slice(&uleb(4));
    program.push(0x07);
    program.extend_from_slice(&uleb(5));
    program.push(0x09);
    program.extend_from_slice(&uleb(6));
    program.extend_from_slice(&uleb(9));
    program.push(0x14);
    program.extend_from_slice(&uleb(8));
    program.extend_from_slice(&uleb(2));
    let mut section = cie(4, 1, -4, 14, &def_cfa(13, 0));
    section.extend_from_slice(&fde(0, 0x1000, 0x40, &program));
    let table = frames(&section);
    let row = table.unwind(0x1000).expect("no error").expect("a row");
    assert_eq!(row.register(4), crate::RegisterRule::SameValue);
    assert_eq!(row.register(5), crate::RegisterRule::Undefined);
    assert_eq!(row.register(6), crate::RegisterRule::Register(9));
    assert_eq!(row.register(8), crate::RegisterRule::ValOffset(-8));
    assert_eq!(
        row.register(11),
        crate::RegisterRule::Undefined,
        "a register the table never mentions is undefined rather than absent"
    );
}

#[test]
fn an_expression_is_carried_as_bytes_and_not_evaluated() {
    let mut program = Vec::new();
    program.push(0x0f);
    program.extend_from_slice(&uleb(3));
    program.extend_from_slice(&[0x91, 0x7f, 0x06]);
    program.push(0x10);
    program.extend_from_slice(&uleb(4));
    program.extend_from_slice(&uleb(2));
    program.extend_from_slice(&[0x77, 0x08]);
    program.push(0x16);
    program.extend_from_slice(&uleb(5));
    program.extend_from_slice(&uleb(1));
    program.push(0x30);
    let mut section = cie(4, 1, -4, 14, &[]);
    section.extend_from_slice(&fde(0, 0x1000, 0x40, &program));
    let table = frames(&section);
    let row = table.unwind(0x1000).expect("no error").expect("a row");
    assert_eq!(row.cfa, crate::CfaRule::Expression(&[0x91, 0x7f, 0x06]));
    assert_eq!(
        row.register(4),
        crate::RegisterRule::Expression(&[0x77, 0x08])
    );
    assert_eq!(row.register(5), crate::RegisterRule::ValExpression(&[0x30]));
}

#[test]
fn an_unknown_opcode_ends_the_row_with_what_was_established() {
    let mut program = def_cfa(13, 16);
    program.push(0x87);
    program.extend_from_slice(&uleb(2));
    program.push(0x1c);
    program.push(0x0e);
    program.extend_from_slice(&uleb(99));
    let mut section = cie(4, 1, -4, 14, &[]);
    section.extend_from_slice(&fde(0, 0x1000, 0x40, &program));
    let table = frames(&section);
    let row = table.unwind(0x1000).expect("no error").expect("a row");
    assert_eq!(
        row.cfa,
        crate::CfaRule::RegisterOffset {
            register: 13,
            offset: 16
        },
        "the rules before the unknown opcode stand"
    );
    assert_eq!(row.register(7), crate::RegisterRule::Offset(-8));
    assert_eq!(
        row.truncated_at,
        Some(0x1c),
        "the row names the opcode that stopped it"
    );
}

#[test]
fn a_row_whose_instructions_all_decoded_is_not_reported_as_truncated() {
    let mut program = def_cfa(13, 16);
    program.push(0x87);
    program.extend_from_slice(&uleb(2));
    let mut section = cie(4, 1, -4, 14, &[]);
    section.extend_from_slice(&fde(0, 0x1000, 0x40, &program));
    let table = frames(&section);
    let row = table.unwind(0x1000).expect("no error").expect("a row");
    assert_eq!(row.register(7), crate::RegisterRule::Offset(-8));
    assert_eq!(row.truncated_at, None);
}

#[test]
fn a_non_empty_augmentation_is_refused_rather_than_parsed_past() {
    let mut body = Vec::new();
    body.extend_from_slice(&u32::MAX.to_le_bytes());
    body.push(4);
    body.extend_from_slice(b"zR\x00");
    body.push(4);
    body.push(0);
    body.extend_from_slice(&uleb(1));
    body.extend_from_slice(&sleb(-4));
    body.extend_from_slice(&uleb(14));
    let mut section = Vec::new();
    section.extend_from_slice(&(body.len() as u32).to_le_bytes());
    section.extend_from_slice(&body);
    let sections = Sections {
        debug_frame: &section,
        ..Sections::default()
    };
    assert!(matches!(
        crate::frame::FrameTable::parse(&sections),
        Err(DwarfError::UnknownForm(_))
    ));
}

#[test]
fn an_fde_naming_a_cie_that_is_not_there_is_refused() {
    let mut section = cie(4, 1, -4, 14, &def_cfa(13, 0));
    section.extend_from_slice(&fde(0x999, 0x1000, 0x10, &[]));
    let sections = Sections {
        debug_frame: &section,
        ..Sections::default()
    };
    assert!(crate::frame::FrameTable::parse(&sections).is_err());
}

#[test]
fn an_address_in_no_frame_description_is_not_an_error() {
    let mut section = cie(4, 1, -4, 14, &def_cfa(13, 0));
    section.extend_from_slice(&fde(0, 0x1000, 0x10, &[]));
    let table = frames(&section);
    assert!(table.unwind(0x0fff).expect("no error").is_none());
    assert!(table.unwind(0x1010).expect("no error").is_none());
    assert!(table.unwind(0x100f).expect("no error").is_some());
    assert_eq!(table.len(), 1);
    assert_eq!(table.ranges(), vec![(0x1000, 0x1010)]);
}

#[test]
fn the_thumb_bit_is_cleared_on_a_frame_address_only_when_the_caller_says_so() {
    let mut section = cie(4, 1, -4, 14, &def_cfa(13, 0));
    section.extend_from_slice(&fde(0, 0x1001, 0x10, &[]));
    let sections = Sections {
        debug_frame: &section,
        ..Sections::default()
    };
    let plain = crate::frame::FrameTable::parse(&sections).expect("parses");
    assert_eq!(plain.ranges(), vec![(0x1001, 0x1011)]);
    assert!(plain.unwind(0x1000).expect("no error").is_none());

    let masked = crate::frame::FrameTable::parse_with(
        &sections,
        &Options {
            arm_thumb_addresses: true,
        },
    )
    .expect("parses");
    assert_eq!(masked.ranges(), vec![(0x1000, 0x1010)]);
    assert!(masked.unwind(0x1000).expect("no error").is_some());
}

#[test]
fn no_single_byte_of_damage_makes_the_frame_reader_panic() {
    let mut program = def_cfa(13, 0);
    program.push(0x41);
    program.push(0x0e);
    program.extend_from_slice(&uleb(16));
    program.push(0x87);
    program.extend_from_slice(&uleb(2));
    program.push(0x0a);
    program.push(0x0b);
    program.push(0x01);
    program.extend_from_slice(&0x1010u32.to_le_bytes());
    program.push(0x0f);
    program.extend_from_slice(&uleb(2));
    program.extend_from_slice(&[0x91, 0x7f]);
    let mut section = cie(4, 1, -4, 14, &def_cfa(13, 0));
    section.extend_from_slice(&fde(0, 0x1000, 0x40, &program));

    let mut damaged = section.clone();
    for index in 0..section.len() {
        for bit in [0x01u8, 0x40, 0x80, 0xff] {
            damaged[index] = section[index] ^ bit;
            let sections = Sections {
                debug_frame: &damaged,
                ..Sections::default()
            };
            if let Ok(table) = crate::frame::FrameTable::parse(&sections) {
                let _ = table.rows();
                for address in [0u64, 0x1000, 0x1008, 0x1010, 0x103f, u64::MAX] {
                    let _ = table.unwind(address);
                }
            }
        }
        damaged[index] = section[index];
    }
}

#[test]
fn arithmetic_over_absurd_header_fields_saturates_rather_than_overflowing() {
    let mut program = Vec::new();
    program.push(0x87);
    program.extend_from_slice(&uleb(4));
    program.extend_from_slice(&[0x04, 0xff, 0xff, 0xff, 0xff]);
    program.push(0x0e);
    program.extend_from_slice(&uleb(8));
    let mut section = cie(4, u64::MAX, i64::MIN / 2, 14, &def_cfa(13, 0));
    section.extend_from_slice(&fde(0, u32::MAX - 4, u32::MAX, &program));
    let table = frames(&section);
    assert_eq!(table.len(), 1);
    let (start, end) = table.ranges()[0];
    assert!(end >= start, "a range that wrapped would describe every address");

    let row = table
        .unwind(start)
        .expect("no error")
        .expect("a row at the range's own start");
    assert_eq!(
        row.register(7),
        crate::RegisterRule::Offset(i64::MIN),
        "a factored offset times an absurd alignment saturates instead of wrapping"
    );
}


/// `DW_CFA_offset register, factored`, in the primary encoding a real prologue uses.
fn cfa_offset(register: u8, factored: u64) -> Vec<u8> {
    let mut out = vec![0x80 | (register & 0x3f)];
    out.extend_from_slice(&uleb(factored));
    out
}

/// The ARMv6-M prologue whose CFI contradicts itself, as two instructions of stack movement:
///
///     28c: b530  push {r4, r5, lr}    SP -= 12
///     28e: 46d6  mov  lr, r10
///     290: b500  push {lr}            SP -= 4, and the frame is 16
///
/// `second_push_frame` is what the producer declared the frame to be after the second push. The
/// case this reproduces emits no `DW_CFA_def_cfa_offset` there at all, which leaves 12 standing;
/// passing 16 is the one-instruction repair, and is what makes this pair a red proof rather than a
/// demonstration.
fn armv6m_double_push(second_push_frame: Option<u64>) -> Vec<u8> {
    let mut program = Vec::new();
    program.push(0x40 | 1);
    program.push(0x0e);
    program.extend_from_slice(&uleb(12));
    program.extend_from_slice(&cfa_offset(4, 3));
    program.extend_from_slice(&cfa_offset(5, 2));
    program.extend_from_slice(&cfa_offset(14, 1));
    program.push(0x40 | 2);
    if let Some(frame) = second_push_frame {
        program.push(0x0e);
        program.extend_from_slice(&uleb(frame));
    }
    program.extend_from_slice(&cfa_offset(10, 4));
    let mut section = cie(4, 2, -4, 14, &def_cfa(13, 0));
    section.extend_from_slice(&fde(0, 0x28c, 0x20, &program));
    section
}

#[test]
fn a_saved_register_below_the_stack_pointer_is_refused_as_a_frame() {
    let section = armv6m_double_push(None);
    let table = frames(&section);

    let before = table.unwind(0x28e).expect("no error").expect("a row");
    assert!(
        before.describes_a_possible_frame(13),
        "the row after the first push is consistent and must not be refused"
    );

    let after = table.unwind(0x292).expect("no error").expect("a row");
    assert_eq!(
        after.cfa,
        crate::CfaRule::RegisterOffset {
            register: 13,
            offset: 12
        },
        "the CFA the producer left standing"
    );
    assert_eq!(after.register(10), crate::RegisterRule::Offset(-16));
    assert!(
        !after.describes_a_possible_frame(13),
        "12 + -16 is below the stack pointer, so the row cannot describe a frame"
    );
}

#[test]
fn the_same_rows_under_the_frame_the_prologue_actually_builds_are_possible() {
    let section = armv6m_double_push(Some(16));
    let table = frames(&section);
    let after = table.unwind(0x292).expect("no error").expect("a row");
    assert_eq!(after.register(10), crate::RegisterRule::Offset(-16));
    assert!(
        after.describes_a_possible_frame(13),
        "16 + -16 is the stack pointer itself, which is the lowest slot a frame can use"
    );
}

#[test]
fn a_frame_pointer_cfa_is_reported_possible_rather_than_guessed_at() {
    let mut program = def_cfa(7, 12);
    program.extend_from_slice(&cfa_offset(10, 4));
    let mut section = cie(4, 2, -4, 14, &def_cfa(13, 0));
    section.extend_from_slice(&fde(0, 0x28c, 0x20, &program));
    let table = frames(&section);
    let row = table.unwind(0x28c).expect("no error").expect("a row");
    assert_eq!(row.register(10), crate::RegisterRule::Offset(-16));
    assert!(
        row.describes_a_possible_frame(13),
        "a CFA computed from another register is not checkable this way"
    );
}

#[test]
fn a_row_carries_the_range_of_the_whole_function_and_not_only_its_own() {
    let section = armv6m_double_push(Some(16));
    let table = frames(&section);
    let row = table.unwind(0x292).expect("no error").expect("a row");
    assert_eq!(row.function, (0x28c, 0x2ac), "the FDE's whole range");
    assert_eq!(row.start, 0x292, "and the row starts where the rules changed");
    assert!(
        row.start > row.function.0,
        "which is not the same address, or this test would prove nothing"
    );
}

#[test]
fn a_split_cursor_can_report_its_position_in_the_slice_it_came_from() {
    let bytes: Vec<u8> = (0..64u8).collect();
    let mut section = Cursor::new(&bytes);
    section.take(11).expect("a header");
    let mut unit = section.split(20).expect("a unit body");
    assert_eq!(unit.offset(), 0, "the body counts from itself");
    assert_eq!(
        unit.origin_offset(),
        11,
        "and knows where that was in the section"
    );
    unit.take(5).expect("five bytes of the body");
    assert_eq!(unit.offset(), 5);
    assert_eq!(unit.origin_offset(), 16, "both advance together");
}

#[test]
fn a_split_of_a_split_accumulates_rather_than_restarting() {
    let bytes: Vec<u8> = (0..64u8).collect();
    let mut section = Cursor::new(&bytes);
    section.take(4).expect("past the length");
    let mut unit = section.split(40).expect("a unit");
    unit.take(7).expect("past the unit header");
    let inner = unit.split(10).expect("an entry");
    assert_eq!(inner.offset(), 0);
    assert_eq!(inner.origin_offset(), 11, "4 into the section, then 7 into the unit");
}

#[test]
fn a_cursor_over_a_whole_section_has_no_origin_to_add() {
    let bytes = [0u8; 8];
    let mut whole = Cursor::new(&bytes);
    whole.take(3).expect("three bytes");
    assert_eq!(whole.offset(), whole.origin_offset());
    let at = Cursor::at(&bytes, 5).expect("a position");
    assert_eq!(at.origin_offset(), 5);
}

use crate::locals::{self, FrameBase, Locals, Place};

/// The abbreviation codes [`UnitBuilder`] emits, declared once so the table and the entries cannot
/// disagree about which attributes an entry carries.
const ABBREV: &[u8] = &[
    1, 0x11, 1, 0x11, 0x01, 0, 0,
    2, 0x2e, 1, 0x03, 0x08, 0x11, 0x01, 0x12, 0x06, 0x40, 0x18, 0, 0,
    3, 0x05, 0, 0x03, 0x08, 0x02, 0x18, 0, 0,
    4, 0x34, 0, 0x03, 0x08, 0x02, 0x18, 0, 0,
    5, 0x0b, 1, 0, 0,
    6, 0x05, 0, 0x31, 0x13, 0x02, 0x18, 0, 0,
    7, 0x2e, 1, 0x03, 0x08, 0, 0,
    8, 0x34, 0, 0x03, 0x08, 0x02, 0x17, 0, 0,
    0,
];

/// A DWARF 4, 32-bit `.debug_info` unit, built entry by entry.
///
/// Written here rather than taken from a producer: every test below turns on a shape a producer
/// emits only sometimes -- a forward abstract origin, a discarded subprogram overlapping a live one
/// -- and waiting for a sample that happens to contain one is how those go untested.
struct UnitBuilder {
    entries: Vec<u8>,
}

impl UnitBuilder {
    fn new() -> Self {
        let mut entries = Vec::new();
        entries.push(1u8);
        entries.extend_from_slice(&0u32.to_le_bytes());
        UnitBuilder { entries }
    }

    /// The unit-relative offset the NEXT entry will have -- what a `DW_FORM_ref4` naming it holds.
    ///
    /// 11 is the DWARF 4 32-bit header: length 4, version 2, abbreviation offset 4, address size 1.
    fn next_offset(&self) -> u32 {
        11 + self.entries.len() as u32
    }

    fn subprogram(&mut self, name: &str, low: u32, len: u32, frame_base: &[u8]) -> &mut Self {
        self.entries.push(2);
        self.entries.extend_from_slice(name.as_bytes());
        self.entries.push(0);
        self.entries.extend_from_slice(&low.to_le_bytes());
        self.entries.extend_from_slice(&len.to_le_bytes());
        self.entries.push(frame_base.len() as u8);
        self.entries.extend_from_slice(frame_base);
        self
    }

    fn declaration(&mut self, name: &str) -> &mut Self {
        self.entries.push(7);
        self.entries.extend_from_slice(name.as_bytes());
        self.entries.push(0);
        self
    }

    fn parameter(&mut self, name: &str, location: &[u8]) -> &mut Self {
        self.entries.push(3);
        self.entries.extend_from_slice(name.as_bytes());
        self.entries.push(0);
        self.entries.push(location.len() as u8);
        self.entries.extend_from_slice(location);
        self
    }

    fn variable(&mut self, name: &str, location: &[u8]) -> &mut Self {
        self.entries.push(4);
        self.entries.extend_from_slice(name.as_bytes());
        self.entries.push(0);
        self.entries.push(location.len() as u8);
        self.entries.extend_from_slice(location);
        self
    }

    /// A variable whose location is an offset into `.debug_loc` rather than an expression.
    fn variable_at_list(&mut self, name: &str, offset: u32) -> &mut Self {
        self.entries.push(8);
        self.entries.extend_from_slice(name.as_bytes());
        self.entries.push(0);
        self.entries.extend_from_slice(&offset.to_le_bytes());
        self
    }

    /// A parameter that carries no name and points at the entry at `origin` for one.
    fn inherited_parameter(&mut self, origin: u32, location: &[u8]) -> &mut Self {
        self.entries.push(6);
        self.entries.extend_from_slice(&origin.to_le_bytes());
        self.entries.push(location.len() as u8);
        self.entries.extend_from_slice(location);
        self
    }

    fn block(&mut self) -> &mut Self {
        self.entries.push(5);
        self
    }

    fn close(&mut self) -> &mut Self {
        self.entries.push(0);
        self
    }

    fn finish(&mut self) -> Vec<u8> {
        self.entries.push(0);
        let mut out = Vec::new();
        let body_len = 2 + 4 + 1 + self.entries.len();
        out.extend_from_slice(&(body_len as u32).to_le_bytes());
        out.extend_from_slice(&4u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.push(4);
        out.extend_from_slice(&self.entries);
        out
    }
}

fn locals_of(info: &[u8]) -> Sections<'_> {
    Sections {
        debug_info: info,
        debug_abbrev: ABBREV,
        ..Sections::default()
    }
}

/// The expression shapes this reader may act on, and the ones it must decline.
///
/// # A RECOGNIZED OPCODE WITH ANYTHING AFTER IT IS A DIFFERENT EXPRESSION
///
/// `DW_OP_reg4` says the value is in r4. `DW_OP_reg4, DW_OP_stack_value` says the value IS that
/// number and lives nowhere, and `DW_OP_reg4, DW_OP_piece 4` says r4 holds four bytes of a larger
/// value. Reading only the first opcode makes all three the same answer, of the right width and the
/// wrong meaning -- so the WHOLE expression has to match, not its head.
#[test]
fn an_expression_is_classified_only_when_the_whole_of_it_is_one_place() {
    assert_eq!(locals::classify(&[0x91, 0x10]), Place::FrameOffset(16));
    assert_eq!(locals::classify(&[0x91, 0x70]), Place::FrameOffset(-16));
    assert_eq!(locals::classify(&[0x54]), Place::Register(4));
    assert_eq!(
        locals::classify(&[0x70, 0x00]),
        Place::RegisterOffset { register: 0, offset: 0 }
    );
    assert_eq!(
        locals::classify(&[0x03, 0x00, 0x01, 0x00, 0x20]),
        Place::Address(0x2000_0100)
    );
    assert_eq!(locals::classify(&[]), Place::Nowhere);

    let stack_value: &[u8] = &[0x54, 0x9f];
    let piece: &[u8] = &[0x93, 0x04, 0x42, 0x9f, 0x93, 0x04];
    let deref: &[u8] = &[0x91, 0x10, 0x06];
    assert_eq!(locals::classify(stack_value), Place::Expression(stack_value));
    assert_eq!(locals::classify(piece), Place::Expression(piece));
    assert_eq!(locals::classify(deref), Place::Expression(deref));
}

/// `DW_AT_frame_base` decides what a frame offset means, and only two shapes are answerable.
#[test]
fn a_frame_base_is_a_register_or_the_call_frame_address_and_nothing_else() {
    assert_eq!(locals::classify_frame_base(&[0x5d]), FrameBase::Register(13));
    assert_eq!(locals::classify_frame_base(&[0x57]), FrameBase::Register(7));
    assert_eq!(locals::classify_frame_base(&[0x9c]), FrameBase::CallFrameCfa);
    let composite: &[u8] = &[0x91, 0x00, 0x9f];
    assert_eq!(
        locals::classify_frame_base(composite),
        FrameBase::Expression(composite)
    );
}

/// An inlined call's parameters carry no name and point at the abstract instance that does.
///
/// # THE REFERENCE COUNTS FROM THE UNIT HEADER AND THE CURSOR STARTS AT THE UNIT BODY
///
/// This is the arithmetic that was wrong first: a unit is read through a cursor over its own bytes,
/// whose offset restarts past the initial length field, while a `DW_FORM_ref4` counts from the
/// header. Without the shift every reference resolved to whichever entry was recorded first -- real
/// names against the wrong variables, which is not a shape anybody would file a bug about.
#[test]
fn a_parameter_with_no_name_takes_the_one_its_abstract_origin_carries() {
    let mut builder = UnitBuilder::new();
    builder.declaration("abstract");
    let origin = builder.next_offset();
    builder.parameter("theRealName", &[0x54]);
    builder.close();
    builder.subprogram("live", 0x100, 0x40, &[0x5d]);
    builder.inherited_parameter(origin, &[0x55]);
    builder.close();
    let info = builder.finish();

    let sections = locals_of(&info);
    let locals = Locals::parse(&sections).expect("parses");
    let program = locals.at(0x110).expect("the live subprogram covers it");
    assert_eq!(program.name, Some(&b"live"[..]));
    assert_eq!(program.locals.len(), 1);
    assert_eq!(
        program.locals[0].name,
        Some(&b"theRealName"[..]),
        "the name comes from the abstract instance, not from the concrete entry"
    );
    assert_eq!(program.locals[0].place_at(0x110), Place::Register(5));
}

/// A local's depth counts the blocks between it and its subprogram, not every entry back to the
/// compilation unit -- which is what a naive count of the open-entry stack gives.
#[test]
fn depth_counts_the_blocks_below_the_subprogram_and_not_the_unit_above_it() {
    let mut builder = UnitBuilder::new();
    builder.subprogram("f", 0x100, 0x40, &[0x5d]);
    builder.variable("atTop", &[0x54]);
    builder.block();
    builder.variable("inOne", &[0x55]);
    builder.block();
    builder.variable("inTwo", &[0x56]);
    builder.close();
    builder.close();
    builder.close();
    let info = builder.finish();

    let sections = locals_of(&info);
    let locals = Locals::parse(&sections).expect("parses");
    let program = locals.at(0x100).expect("covered");
    let depths: Vec<(Option<&[u8]>, u16)> =
        program.locals.iter().map(|l| (l.name, l.depth)).collect();
    assert_eq!(
        depths,
        vec![
            (Some(&b"atTop"[..]), 0),
            (Some(&b"inOne"[..]), 1),
            (Some(&b"inTwo"[..]), 2),
        ]
    );
}

/// A subprogram the linker discarded overlaps the live code it was removed from, and it is SHORTER,
/// so the innermost-wins rule picks it.
///
/// See `Locals::parse_within`. The discriminator is the executable section ranges rather than the
/// address being zero, because zero is a real image offset on a Cortex-M.
#[test]
fn a_discarded_subprogram_overlapping_a_live_one_is_dropped_by_the_code_ranges() {
    let mut builder = UnitBuilder::new();
    builder.subprogram("discarded", 0x0, 0x60, &[0x5d]);
    builder.parameter("ghost", &[0x50]);
    builder.close();
    builder.subprogram("live", 0x50, 0x200, &[0x5d]);
    builder.parameter("real", &[0x51]);
    builder.close();
    let info = builder.finish();
    let sections = locals_of(&info);

    let unfiltered = Locals::parse(&sections).expect("parses");
    assert_eq!(
        unfiltered.at(0x54).and_then(|p| p.name),
        Some(&b"discarded"[..]),
        "without the ranges the shorter, discarded entry wins -- which is the defect"
    );

    let code = [(0x50u64, 0x250u64)];
    let filtered = Locals::parse_within(&sections, &code).expect("parses");
    assert_eq!(filtered.at(0x54).and_then(|p| p.name), Some(&b"live"[..]));
    assert_eq!(filtered.at(0x54).map(|p| p.locals.len()), Some(1));

    let unanswerable = Locals::parse_within(&sections, &[]).expect("parses");
    assert_eq!(unanswerable.subprograms().len(), 2);
}

/// A DWARF 4 location list: ranges relative to a base, a base-selection entry, and a terminator.
#[test]
fn a_debug_loc_list_applies_only_over_its_own_ranges() {
    let mut section = Vec::new();
    section.extend_from_slice(&0x100u32.to_le_bytes());
    section.extend_from_slice(&0x120u32.to_le_bytes());
    section.extend_from_slice(&1u16.to_le_bytes());
    section.push(0x54);
    section.extend_from_slice(&u32::MAX.to_le_bytes());
    section.extend_from_slice(&0x1000u32.to_le_bytes());
    section.extend_from_slice(&0x200u32.to_le_bytes());
    section.extend_from_slice(&0x240u32.to_le_bytes());
    section.extend_from_slice(&1u16.to_le_bytes());
    section.push(0x55);
    section.extend_from_slice(&0u32.to_le_bytes());
    section.extend_from_slice(&0u32.to_le_bytes());

    let mut builder = UnitBuilder::new();
    builder.subprogram("f", 0x100, 0x2000, &[0x5d]);
    builder.variable_at_list("moves", 0);
    builder.close();
    let info = builder.finish();
    let sections = Sections {
        debug_info: &info,
        debug_abbrev: ABBREV,
        debug_loc: &section,
        ..Sections::default()
    };
    let locals = Locals::parse(&sections).expect("parses");
    let program = locals.at(0x100).expect("covered");
    let local = &program.locals[0];
    assert!(local.is_range_described());
    assert_eq!(local.place_at(0x100), Place::Register(4));
    assert_eq!(local.place_at(0x11f), Place::Register(4));
    assert_eq!(
        local.place_at(0x120),
        Place::Nowhere,
        "one past the end of a range is outside it"
    );
    assert_eq!(
        local.place_at(0x1200),
        Place::Register(5),
        "the second range is relative to the base the selection entry set"
    );
    assert_eq!(local.place_at(0x300), Place::Nowhere);
}

/// A DWARF 5 location list, which is the same idea in an encoding that shares no bytes with it.
#[test]
fn a_debug_loclists_list_reads_the_kinds_it_can_and_stops_at_one_it_cannot() {
    let mut section = Vec::new();
    section.push(0x06);
    section.extend_from_slice(&0x1000u32.to_le_bytes());
    section.push(0x04);
    section.push(0x00);
    section.push(0x20);
    section.push(1);
    section.push(0x57);
    section.push(0x08);
    section.extend_from_slice(&0x2000u32.to_le_bytes());
    section.push(0x10);
    section.push(1);
    section.push(0x58);
    section.push(0x00);

    let locals::Location::List(ranges) = locals::loclists_at(&section, 0, 4, 0) else {
        panic!("expected a list");
    };
    assert_eq!(
        ranges,
        vec![
            (0x1000, 0x1020, &[0x57u8][..]),
            (0x2000, 0x2010, &[0x58u8][..]),
        ]
    );

    let indexed = [0x02u8, 0x00, 0x01, 1, 0x59, 0x00];
    assert!(matches!(
        locals::loclists_at(&indexed, 0, 4, 0),
        locals::Location::None
    ));
}

/// A file can point an entry's origin at itself, and a reader that trusts the chain does not return.
#[test]
fn an_origin_that_points_at_itself_terminates() {
    let mut builder = UnitBuilder::new();
    builder.subprogram("f", 0x100, 0x40, &[0x5d]);
    let here = builder.next_offset();
    builder.inherited_parameter(here, &[0x54]);
    builder.close();
    let info = builder.finish();
    let sections = locals_of(&info);
    let locals = Locals::parse(&sections).expect("parses");
    let program = locals.at(0x100).expect("covered");
    assert_eq!(program.locals[0].name, None, "a cycle names nothing");
    assert_eq!(program.locals[0].place_at(0x100), Place::Register(4));
}
