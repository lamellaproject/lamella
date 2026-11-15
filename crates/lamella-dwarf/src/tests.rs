//! Unit tests over hand-built sections.

use alloc::vec;
use alloc::vec::Vec;

use crate::cursor::{Cursor, Format};
use crate::line::{self, Row};
use crate::{DebugInfo, DeviceRow, DwarfError, Options, Sections};
use crate::{ScalarType, scalar_type};

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
        "the shape an AOT program of one source file has"
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
    9, 0x2e, 1, 0x47, 0x13, 0x11, 0x01, 0x12, 0x06, 0, 0,
    10, 0x2e, 1, 0x31, 0x13, 0x11, 0x01, 0x12, 0x06, 0, 0,
    11, 0x05, 0, 0x03, 0x08, 0x02, 0x17, 0, 0,
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

    /// A subprogram definition with no name of its own, occupying `[low, low + len)` and naming
    /// itself through `reference`: its specification under abbreviation 9, its abstract origin
    /// under 10.
    fn referring_subprogram(&mut self, abbrev: u8, reference: u32, low: u32, len: u32) -> &mut Self {
        self.entries.push(abbrev);
        self.entries.extend_from_slice(&reference.to_le_bytes());
        self.entries.extend_from_slice(&low.to_le_bytes());
        self.entries.extend_from_slice(&len.to_le_bytes());
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

    /// A parameter whose location is an offset into `.debug_loc` rather than an expression.
    fn parameter_at_list(&mut self, name: &str, offset: u32) -> &mut Self {
        self.entries.push(11);
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

/// `DW_OP_litN; DW_OP_stack_value` is the value N itself, stored nowhere (DWARF 5 sections 2.5.1.1
/// and 2.6.1.1.4), for each N the literal operations encode, 0 to 31.
///
/// # ONLY THOSE TWO BYTES, WHOLE
///
/// `DW_OP_lit0` alone is a location: the memory at address 0. A `DW_OP_piece` after the
/// `DW_OP_stack_value` makes the number four bytes of a larger value, and `DW_OP_stack_value` alone
/// has no number to give. The operations either side of the literals in table 7.9, `DW_OP_skip`
/// and `DW_OP_reg0`, are not literals. Each of these is carried whole.
#[test]
fn a_literal_made_the_value_is_that_constant_and_nothing_near_it_is() {
    for literal in 0u8..32 {
        assert_eq!(
            locals::classify(&[0x30 + literal, 0x9f]),
            Place::Constant(u64::from(literal)),
            "DW_OP_lit{literal}"
        );
    }
    let address_zero: &[u8] = &[0x30];
    let a_piece: &[u8] = &[0x31, 0x9f, 0x93, 0x04];
    let no_literal: &[u8] = &[0x9f];
    let twice: &[u8] = &[0x30, 0x9f, 0x9f];
    let below: &[u8] = &[0x2f, 0x9f];
    let above: &[u8] = &[0x50, 0x9f];
    for carried in [address_zero, a_piece, no_literal, twice, below, above] {
        assert_eq!(locals::classify(carried), Place::Expression(carried), "{carried:02x?}");
    }
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
/// A unit is read through a cursor over its own bytes, whose offset restarts past the initial
/// length field, while a `DW_FORM_ref4` counts from the header. The shift between the two is what
/// this checks: without it a reference resolves to whichever entry was recorded first, which puts
/// real names against the wrong variables rather than failing.
/// A definition with no name of its own takes its declaration's, through `DW_AT_specification`.
/// That is how a method defined outside its class is described, and a reader that does not follow
/// the reference leaves every such body nameless in the function table.
#[test]
fn a_definition_named_only_by_its_specification_takes_the_declarations_name() {
    let mut builder = UnitBuilder::new();
    let declaration = builder.next_offset();
    builder.declaration("Blink");
    builder.close();
    builder.referring_subprogram(9, declaration, 0x200, 0x20);
    builder.close();
    let info = builder.finish();

    let functions = crate::info::functions(&locals_of(&info)).expect("parses");
    assert_eq!(
        functions.len(),
        1,
        "the declaration occupies no addresses, so it is not a function"
    );
    assert_eq!(functions[0].name, &b"Blink"[..]);
    assert_eq!((functions[0].low_pc, functions[0].high_pc), (0x200, 0x220));
}

/// A concrete instance takes its name through `DW_AT_abstract_origin`, and a reference to an entry
/// LATER in the unit resolves as well as one to an earlier, because names are resolved once the unit
/// has been read.
#[test]
fn an_instance_named_by_an_abstract_origin_later_in_the_unit_takes_that_name() {
    let mut builder = UnitBuilder::new();
    let declaration = builder.next_offset() + 14;
    builder.referring_subprogram(10, declaration, 0x300, 0x10);
    builder.close();
    builder.declaration("inlined");
    builder.close();
    let info = builder.finish();

    let functions = crate::info::functions(&locals_of(&info)).expect("parses");
    assert_eq!(functions.len(), 1);
    assert_eq!(functions[0].name, &b"inlined"[..]);
}

/// A reference that leads to no name -- here, back to the entry itself -- leaves the definition out
/// as a nameless one always was, and the walk returns rather than following the circle.
#[test]
fn a_definition_whose_reference_leads_back_to_itself_is_left_out_and_the_walk_returns() {
    let mut builder = UnitBuilder::new();
    let itself = builder.next_offset();
    builder.referring_subprogram(9, itself, 0x400, 0x10);
    builder.close();
    builder.subprogram("named", 0x500, 0x10, &[0x5d]);
    builder.close();
    let info = builder.finish();

    let functions = crate::info::functions(&locals_of(&info)).expect("parses");
    let names: Vec<&[u8]> = functions.iter().map(|function| function.name).collect();
    assert_eq!(names, vec![&b"named"[..]]);
}

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
    let filtered = Locals::parse_within(&sections, &code, &Options::default()).expect("parses");
    assert_eq!(filtered.at(0x54).and_then(|p| p.name), Some(&b"live"[..]));
    assert_eq!(filtered.at(0x54).map(|p| p.locals.len()), Some(1));

    let unanswerable = Locals::parse_within(&sections, &[], &Options::default()).expect("parses");
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

/// **A SUBPROGRAM WHOSE PRODUCER SET THE THUMB BIT IN ITS `DW_AT_low_pc`** covers its first
/// instruction once the target says its code addresses carry the bit, and keeps its length. Left
/// set, the range begins one byte past the function, so at its first instruction no subprogram is
/// found and a stop on the function's first line shows no locals.
#[test]
fn a_thumb_bit_in_a_subprograms_low_pc_is_cleared_where_the_target_says_its_addresses_carry_it() {
    let mut builder = UnitBuilder::new();
    builder.subprogram("<Main>$", 0x1007_846d, 0x12, &[0x57]);
    builder.parameter("args", &[0x50]);
    builder.close();
    let info = builder.finish();
    let sections = locals_of(&info);
    let code = [(0x1000_0000u64, 0x1020_0000u64)];

    let as_stored = Locals::parse_within(&sections, &code, &Options::default()).expect("parses");
    assert!(as_stored.at(0x1007_846c).is_none(), "as stored, the range starts past the function");

    let thumb = Options { arm_thumb_addresses: true };
    let locals = Locals::parse_within(&sections, &code, &thumb).expect("parses");
    let program = locals.at(0x1007_846c).expect("the function's first instruction is covered");
    assert_eq!(program.name, Some(&b"<Main>$"[..]));
    assert_eq!((program.low_pc, program.high_pc), (0x1007_846c, 0x1007_847e), "its length is kept");
    assert_eq!(program.frame_base, FrameBase::Register(7));
    assert_eq!(program.locals[0].name, Some(&b"args"[..]));
    assert_eq!(program.locals[0].place_at(0x1007_846c), Place::Register(0));
    assert!(locals.at(0x1007_847e).is_none(), "one past its end is outside it");
}

/// **AND FROM A LOCATION LIST'S RANGES WITH IT**: each range moves by what its start was, so the
/// instruction at a boundary belongs to the range it begins rather than to the one before it, and
/// the instruction after the last range is in none.
#[test]
fn a_thumb_bit_in_a_location_lists_ranges_is_cleared_with_it() {
    let mut section = Vec::new();
    for (start, end, register) in [(0x1007_8481u32, 0x1007_848fu32, 0x52u8), (0x1007_848f, 0x1007_853d, 0x54)] {
        section.extend_from_slice(&start.to_le_bytes());
        section.extend_from_slice(&end.to_le_bytes());
        section.extend_from_slice(&1u16.to_le_bytes());
        section.push(register);
    }
    section.extend_from_slice(&0u32.to_le_bytes());
    section.extend_from_slice(&0u32.to_le_bytes());

    let mut builder = UnitBuilder::new();
    builder.subprogram("f", 0x1007_8481, 0xbc, &[0x5d]);
    builder.variable_at_list("moves", 0);
    builder.close();
    let info = builder.finish();
    let sections = Sections {
        debug_info: &info,
        debug_abbrev: ABBREV,
        debug_loc: &section,
        ..Sections::default()
    };

    let as_stored = Locals::parse_with(&sections, &Options::default()).expect("parses");
    let local = &as_stored.at(0x1007_8482).expect("covered").locals[0];
    assert_eq!(
        local.place_at(0x1007_848e),
        Place::Register(2),
        "as stored, the instruction at the boundary is the earlier range's"
    );

    let locals = Locals::parse_with(&sections, &Options { arm_thumb_addresses: true }).expect("parses");
    let local = &locals.at(0x1007_8480).expect("the first instruction is covered").locals[0];
    assert_eq!(local.place_at(0x1007_8480), Place::Register(2));
    assert_eq!(local.place_at(0x1007_848c), Place::Register(2));
    assert_eq!(local.place_at(0x1007_848e), Place::Register(4), "the boundary begins the second range");
    assert_eq!(local.place_at(0x1007_853a), Place::Register(4));
    assert_eq!(local.place_at(0x1007_853c), Place::Nowhere, "past the last range");
}

/// Where a function's parameters reach their homes: the start of each parameter's first range past
/// the entry, the latest of them. A parameter described by one expression never moves, so a function
/// whose parameters are all described so has no such address, and neither has one with none.
#[test]
fn the_parameters_reach_their_homes_where_their_location_lists_move_them() {
    let mut section = Vec::new();
    let mut list = |ranges: &[(u32, u32, &[u8])]| {
        let offset = section.len() as u32;
        for (start, end, expression) in ranges {
            section.extend_from_slice(&start.to_le_bytes());
            section.extend_from_slice(&end.to_le_bytes());
            section.extend_from_slice(&(expression.len() as u16).to_le_bytes());
            section.extend_from_slice(expression);
        }
        section.extend_from_slice(&[0; 8]);
        offset
    };
    let args = list(&[(0x9000_0000, 0x9000_0014, &[0x50]), (0x9000_0014, 0x9000_004c, &[0x7b, 0x74])]);
    let later = list(&[(0x9000_0000, 0x9000_0010, &[0x51]), (0x9000_0010, 0x9000_004c, &[0x7b, 0x78])]);

    let mut builder = UnitBuilder::new();
    builder.subprogram("<Main>$", 0x9000_0000, 0x4c, &[0x57]);
    builder.parameter_at_list("args", args);
    builder.parameter_at_list("count", later);
    builder.variable("a", &[0x7b, 0x70]);
    builder.close();
    builder.subprogram("Add", 0x9000_0050, 0x20, &[0x57]);
    builder.parameter("x", &[0x50]);
    builder.close();
    builder.subprogram("Tick", 0x9000_0070, 0x10, &[0x57]);
    builder.variable("t", &[0x7b, 0x70]);
    builder.close();
    let info = builder.finish();
    let sections = Sections {
        debug_info: &info,
        debug_abbrev: ABBREV,
        debug_loc: &section,
        ..Sections::default()
    };
    let locals = Locals::parse(&sections).expect("parses");

    let main = locals.at(0x9000_0000).expect("covered");
    assert_eq!(main.parameters_at_home(), Some(0x9000_0014), "the latest parameter's move, not the first");
    assert_eq!(locals.at(0x9000_0050).expect("covered").parameters_at_home(), None, "one expression never moves");
    assert_eq!(locals.at(0x9000_0070).expect("covered").parameters_at_home(), None, "no parameter, no address");
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

/// **A LOCAL DESCRIBED AS ZERO UNTIL ITS FIRST STORE** reads as the constant 0 over that range of
/// its DWARF 5 location list and as its frame slot from the store on, whether or not its type is
/// described; before the list's first range, and past its last, it is nowhere.
#[test]
fn a_local_zero_until_its_first_store_reads_as_the_constant_then_as_its_slot() {
    const LOW: u32 = 0x100;
    let at = |offset: u32| u64::from(LOW + offset);
    let mut loclists = vec![0u8; 4];
    loclists.extend_from_slice(&5u16.to_le_bytes());
    loclists.extend_from_slice(&[4, 0]);
    loclists.extend_from_slice(&0u32.to_le_bytes());
    let mut list = |slot: u8| {
        let offset = loclists.len() as u32;
        loclists.push(0x08);
        loclists.extend_from_slice(&(LOW + 4).to_le_bytes());
        loclists.push(0x0c);
        loclists.extend_from_slice(&[2, 0x30, 0x9f]);
        loclists.push(0x08);
        loclists.extend_from_slice(&(LOW + 0x10).to_le_bytes());
        loclists.push(0x30);
        loclists.extend_from_slice(&[2, 0x7d, slot]);
        loclists.push(0x00);
        offset
    };
    let n_list = list(8);
    let label_list = list(12);
    let length = (loclists.len() - 4) as u32;
    loclists[..4].copy_from_slice(&length.to_le_bytes());

    let abbrev = [
        1, 0x11, 1, 0x03, 0x08, 0, 0,
        2, 0x2e, 1, 0x03, 0x08, 0x11, 0x01, 0x12, 0x06, 0x40, 0x18, 0, 0,
        3, 0x24, 0, 0x03, 0x08, 0x3e, 0x0b, 0x0b, 0x0b, 0, 0,
        4, 0x34, 0, 0x03, 0x08, 0x02, 0x17, 0x49, 0x13, 0, 0,
        5, 0x34, 0, 0x03, 0x08, 0x02, 0x17, 0, 0,
        0,
    ];
    let mut entries = vec![1];
    entries.extend_from_slice(b"p.cs\0");
    let int = 12 + entries.len() as u32;
    entries.push(3);
    entries.extend_from_slice(b"int\0");
    entries.extend_from_slice(&[0x05, 4]);
    entries.push(2);
    entries.extend_from_slice(b"f\0");
    entries.extend_from_slice(&LOW.to_le_bytes());
    entries.extend_from_slice(&0x40u32.to_le_bytes());
    entries.extend_from_slice(&[1, 0x5d]);
    entries.push(4);
    entries.extend_from_slice(b"n\0");
    entries.extend_from_slice(&n_list.to_le_bytes());
    entries.extend_from_slice(&int.to_le_bytes());
    entries.push(5);
    entries.extend_from_slice(b"label\0");
    entries.extend_from_slice(&label_list.to_le_bytes());
    entries.extend_from_slice(&[0, 0]);
    let mut info = ((2 + 1 + 1 + 4 + entries.len()) as u32).to_le_bytes().to_vec();
    info.extend_from_slice(&5u16.to_le_bytes());
    info.extend_from_slice(&[0x01, 4]);
    info.extend_from_slice(&0u32.to_le_bytes());
    info.extend_from_slice(&entries);

    let sections = Sections {
        debug_info: &info,
        debug_abbrev: &abbrev,
        debug_loclists: &loclists,
        ..Sections::default()
    };
    let locals = Locals::parse(&sections).expect("parses");
    let program = locals.at(at(4)).expect("covered");
    let [n, label] = &program.locals[..] else {
        panic!("two locals: {:?}", program.locals);
    };
    assert_eq!(n.name, Some(&b"n"[..]));
    assert_eq!(n.place_at(at(0)), Place::Nowhere, "before the list's first range");
    assert_eq!(n.place_at(at(4)), Place::Constant(0), "from the stop on the entry");
    assert_eq!(n.place_at(at(0xf)), Place::Constant(0), "to the first store");
    assert_eq!(n.place_at(at(0x10)), Place::RegisterOffset { register: 13, offset: 8 }, "its slot from the store");
    assert_eq!(n.place_at(at(0x3f)), Place::RegisterOffset { register: 13, offset: 8 });
    assert_eq!(n.place_at(at(0x40)), Place::Nowhere, "past the list's last range");
    let int = crate::types::scalar_type(&sections, n.type_offset.expect("typed")).expect("a scalar");
    assert_eq!((int.encoding, int.byte_size), (0x05, 4));

    assert_eq!(label.name, Some(&b"label"[..]));
    assert_eq!(label.type_offset, None);
    assert_eq!(label.place_at(at(4)), Place::Constant(0));
    assert_eq!(label.place_at(at(0x10)), Place::RegisterOffset { register: 13, offset: 12 });
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

/// A DWARF 4 unit around `body`, its abbreviations at offset 0 and its addresses four bytes wide.
fn unit_of(body: &[u8]) -> Vec<u8> {
    let mut unit = ((2 + 4 + 1 + body.len()) as u32).to_le_bytes().to_vec();
    unit.extend_from_slice(&4u16.to_le_bytes());
    unit.extend_from_slice(&0u32.to_le_bytes());
    unit.push(4);
    unit.extend_from_slice(body);
    unit
}

/// A variable's type is read through the names and qualifiers around it to the scalar it is, an
/// enumeration as its underlying type, and a pointer as no scalar; and the reference a variable
/// makes, counted from its own unit, is kept as an offset within the whole section.
#[test]
fn a_variables_type_is_read_through_names_and_qualifiers_to_the_scalar_it_is() {
    const TYPES_ABBREV: &[u8] = &[
        1, 0x11, 1, 0, 0,
        2, 0x2e, 1, 0x03, 0x08, 0x11, 0x01, 0x12, 0x06, 0, 0,
        3, 0x34, 0, 0x03, 0x08, 0x02, 0x18, 0x49, 0x13, 0, 0,
        4, 0x24, 0, 0x03, 0x08, 0x3e, 0x0b, 0x0b, 0x0b, 0, 0,
        5, 0x16, 0, 0x03, 0x08, 0x49, 0x13, 0, 0,
        6, 0x26, 0, 0x49, 0x13, 0, 0,
        7, 0x04, 0, 0x03, 0x08, 0x0b, 0x0b, 0x49, 0x13, 0, 0,
        8, 0x0f, 0, 0x0b, 0x0b, 0x49, 0x13, 0, 0,
        0,
    ];
    let first = unit_of(&[1, 4, b'b', b'y', b't', b'e', 0, 0x08, 1, 0]);
    let mut body = vec![1u8];
    let here = |body: &Vec<u8>| (11 + body.len()) as u32;
    let int = here(&body);
    body.extend_from_slice(&[4, b'i', b'n', b't', 0, 0x05, 4]);
    let number = here(&body);
    body.push(5);
    body.extend_from_slice(b"Number\0");
    body.extend_from_slice(&int.to_le_bytes());
    let constant = here(&body);
    body.push(6);
    body.extend_from_slice(&number.to_le_bytes());
    let day = here(&body);
    body.push(7);
    body.extend_from_slice(b"Day\0");
    body.push(4);
    body.extend_from_slice(&int.to_le_bytes());
    let pointer = here(&body);
    body.extend_from_slice(&[8, 4]);
    body.extend_from_slice(&int.to_le_bytes());
    body.push(2);
    body.extend_from_slice(b"f\0");
    body.extend_from_slice(&0x1000u32.to_le_bytes());
    body.extend_from_slice(&0x10u32.to_le_bytes());
    for (name, location, kind) in [(b"n\0", &[0x91u8, 0x70][..], constant), (b"d\0", &[0x50][..], day), (b"p\0", &[0x51][..], pointer)] {
        body.push(3);
        body.extend_from_slice(name);
        body.push(location.len() as u8);
        body.extend_from_slice(location);
        body.extend_from_slice(&kind.to_le_bytes());
    }
    body.extend_from_slice(&[0, 0]);
    let mut info = first.clone();
    let second = info.len() as u64;
    info.extend_from_slice(&unit_of(&body));
    let sections = Sections { debug_info: &info, debug_abbrev: TYPES_ABBREV, ..Sections::default() };
    let absolute = |relative: u32| second + u64::from(relative);

    let locals = Locals::parse(&sections).expect("parses");
    let f = locals.at(0x1000).expect("covered");
    assert_eq!(f.locals[0].type_offset, Some(absolute(constant)), "counted from the section, not the unit");
    let int_type = ScalarType { name: Some(&b"int"[..]), encoding: 0x05, byte_size: 4, enumeration: false };
    assert_eq!(scalar_type(&sections, absolute(constant)), Some(int_type), "through const and typedef to int");
    assert_eq!(
        scalar_type(&sections, absolute(day)),
        Some(ScalarType { name: Some(&b"Day"[..]), encoding: 0x05, byte_size: 4, enumeration: true }),
        "an enumeration as its underlying type, under its own name"
    );
    assert_eq!(scalar_type(&sections, absolute(pointer)), None, "a pointer is no scalar");
    assert_eq!(
        scalar_type(&sections, 12),
        Some(ScalarType { name: Some(&b"byte"[..]), encoding: 0x08, byte_size: 1, enumeration: false }),
        "a type in the first unit"
    );
}
