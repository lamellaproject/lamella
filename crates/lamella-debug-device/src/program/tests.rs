//! The ELF producer, driven over ELFs these tests build, so what it answers is checked without a
//! toolchain and without a board.

use super::*;
use lamella_aot::dwarf::{inline_string, sleb128, uleb128};

const DW_LNS_COPY: u8 = 0x01;
const DW_LNS_ADVANCE_PC: u8 = 0x02;
const DW_LNS_ADVANCE_LINE: u8 = 0x03;
const DW_LNE_END_SEQUENCE: u8 = 0x01;
const DW_LNE_SET_ADDRESS: u8 = 0x02;
const DW_LNCT_PATH: u64 = 1;
const DW_LNCT_DIRECTORY_INDEX: u64 = 2;
const DW_FORM_STRING: u64 = 0x08;
const DW_FORM_UDATA: u64 = 0x0f;

/// A `.debug_line` section holding one sequence: two rows, at `start` and `start + 2`, on lines 42
/// and 43.
///
/// The addresses are ABSOLUTE and written by this function, which is the whole point: a linked ELF
/// from any toolchain carries absolute addresses, and the arithmetic under test is the subtraction
/// that turns them into the image-relative offsets a backend indexes.
fn line_section(file: &str, start: u32) -> Vec<u8> {
    let mut program = Vec::new();
    program.extend_from_slice(&[0x00, 0x05, DW_LNE_SET_ADDRESS]);
    program.extend_from_slice(&start.to_le_bytes());
    program.push(DW_LNS_ADVANCE_LINE);
    sleb128(&mut program, 41);
    program.push(DW_LNS_COPY);
    program.push(DW_LNS_ADVANCE_PC);
    uleb128(&mut program, 2);
    program.push(DW_LNS_ADVANCE_LINE);
    sleb128(&mut program, 1);
    program.push(DW_LNS_COPY);
    program.push(DW_LNS_ADVANCE_PC);
    uleb128(&mut program, 2);
    program.extend_from_slice(&[0x00, 0x01, DW_LNE_END_SEQUENCE]);

    let mut after_header_length = Vec::new();
    after_header_length.push(1);
    after_header_length.push(1);
    after_header_length.push(1);
    after_header_length.push((-5i8) as u8);
    after_header_length.push(14);
    after_header_length.push(13);
    after_header_length.extend_from_slice(&[0, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1]);
    after_header_length.push(1);
    uleb128(&mut after_header_length, DW_LNCT_PATH);
    uleb128(&mut after_header_length, DW_FORM_STRING);
    uleb128(&mut after_header_length, 1);
    inline_string(&mut after_header_length, "");
    after_header_length.push(2);
    uleb128(&mut after_header_length, DW_LNCT_PATH);
    uleb128(&mut after_header_length, DW_FORM_STRING);
    uleb128(&mut after_header_length, DW_LNCT_DIRECTORY_INDEX);
    uleb128(&mut after_header_length, DW_FORM_UDATA);
    uleb128(&mut after_header_length, 2);
    for _ in 0..2 {
        inline_string(&mut after_header_length, file);
        uleb128(&mut after_header_length, 0);
    }

    let mut body = Vec::new();
    body.extend_from_slice(&5u16.to_le_bytes());
    body.push(4);
    body.push(0);
    body.extend_from_slice(&(after_header_length.len() as u32).to_le_bytes());
    body.extend_from_slice(&after_header_length);
    body.extend_from_slice(&program);

    let mut section = (body.len() as u32).to_le_bytes().to_vec();
    section.extend_from_slice(&body);
    section
}

/// A debuggable ARM Thumb executable whose code sits at `text_addr` and whose line table describes
/// `line_start`.
fn debuggable(text_addr: u32, line_start: u32) -> Vec<u8> {
    let text = [0x00, 0xBF, 0x00, 0xBF, 0x00, 0xBF, 0x00, 0xBF];
    let line = line_section("Blink.swift", line_start);
    lamella_elf::write_debuggable_executable(
        lamella_elf::Machine::Arm,
        &text,
        0,
        text_addr,
        true,
        &[(".debug_line", &line)],
    )
}

/// The offsets a backend indexes are relative to the image base, not absolute addresses.
///
/// **THIS IS THE FAILURE THAT LOOKS LIKE A WORKING DEBUGGER.** `DeviceBackend` subtracts a base
/// from a PC and indexes the table; absolute addresses in the table would put every lookup
/// megabytes past its end, and the session would report "no source line here" for every address
/// rather than reporting an error. **A fixture based at 0 cannot tell the two conventions apart**,
/// so this one is linked where an STM32 runs.
#[test]
fn the_offsets_are_relative_to_the_image_base() {
    let elf = debuggable(0x0800_0100, 0x0800_0100);
    let program = from_elf(&elf).expect("the fixture carries .debug_line");

    assert!(program.image_base <= 0x0800_0100, "the base comes from the ELF, not from a flag");
    let lines = &program.lines;
    assert!(!lines.is_empty(), "the line table was read");
    let expected = 0x0800_0100 - program.image_base;
    assert_eq!(
        lines.iter().map(|row| row.line).collect::<Vec<_>>(),
        vec![42, 43],
        "both rows survived, on the lines the program set"
    );
    assert_eq!(
        lines[0].offset, expected,
        "the first row sits at the image-relative offset of its address"
    );
    assert_eq!(lines[1].offset, expected + 2, "and the second is two bytes past it");
    assert_eq!(program.files, vec!["Blink.swift"], "one unit, one file, interned once");
}

/// Rows from different files keep their own file, and the program carries a TABLE of them.
///
/// **THIS IS THE CASE A SINGLE-FILE PROGRAM CANNOT EXHIBIT, WHICH IS WHY IT SHIPPED.** An AOT C#
/// program has one source file, so taking the first row's filename and applying it to the whole
/// table was indistinguishable from correct. A Swift image declares fourteen files in one unit and
/// interleaves them at instruction granularity; under one filename, a stop reports a real line
/// number against the wrong file -- an answer that looks right and cannot be checked.
///
/// The fixture is two sequences at different addresses naming two different files, which is the
/// smallest shape that can tell the two models apart.
#[test]
fn rows_from_two_files_keep_their_own_file() {
    let first = line_section("Blink.swift", 0x0800_0100);
    let second = line_section("Mmio.swift", 0x0800_0110);
    let mut both = first;
    both.extend_from_slice(&second);
    let elf = lamella_elf::write_debuggable_executable(
        lamella_elf::Machine::Arm,
        &[0x00u8, 0xBF].repeat(16),
        0,
        0x0800_0100,
        true,
        &[(".debug_line", &both)],
    );

    let program = from_elf(&elf).expect("both sequences parse");
    assert_eq!(
        program.files.len(),
        2,
        "two files, interned once each: {:?}",
        program.files
    );
    let file_of = |offset: u32| {
        let row = program.lines.iter().find(|row| row.offset == offset).expect("row present");
        program.files[row.file as usize].clone()
    };
    let base = program.image_base;
    assert_eq!(file_of(0x0800_0100 - base), "Blink.swift");
    assert_eq!(
        file_of(0x0800_0110 - base),
        "Mmio.swift",
        "the second sequence's rows keep the file THEY came from, not the first one seen"
    );
}

/// Debug information for code the LINKER REMOVED is not part of the program.
///
/// # THE DEFECT THIS EXISTS FOR, AND WHY NO FIXTURE FOUND IT
///
/// `--gc-sections` discards a section nothing references, and a relocation against a symbol in a
/// discarded section resolves to ZERO. The `.debug_line` rows and `DW_TAG_subprogram` entries that
/// described that code stay in the file, now all naming address 0 -- with real file names, real
/// line numbers and real function names. **Nothing in their contents says they are dead.**
///
/// A part whose flash begins at zero -- a SAM D21, an nRF51 -- puts every one of them on top of the
/// image base, where the "below the image" check cannot see them and a breakpoint request that
/// finds one arms a comparator at the vector table's first word. The client reports it VERIFIED.
///
/// The test is that the address is not in an executable SECTION, not that it is zero: a Cortex-M
/// image legitimately begins at zero, so the fixture is based there on purpose -- based anywhere
/// else it cannot tell the two rules apart.
#[test]
fn rows_for_code_the_linker_discarded_are_not_part_of_the_program() {
    const TEXT: u32 = 0x54;
    const ABOVE_THE_CODE: u32 = TEXT + 0x1000;
    let mut sections = line_section("Discarded.swift", 0);
    sections.extend_from_slice(&line_section("Tombstone.swift", ABOVE_THE_CODE));
    sections.extend_from_slice(&line_section("Blink.swift", TEXT));
    let elf = lamella_elf::write_debuggable_executable(
        lamella_elf::Machine::Arm,
        &[0x00u8, 0xBF].repeat(16),
        0,
        TEXT,
        true,
        &[(".debug_line", &sections)],
    );

    let program = from_elf(&elf).expect("the live sequence is enough to debug");
    assert_eq!(program.image_base, TEXT, "the image begins where the code does");
    assert_eq!(
        program.files,
        vec!["Blink.swift"],
        "a file named ONLY by discarded rows is not a file this program has: {:?}",
        program.files
    );
    assert!(
        program.lines.iter().all(|row| u64::from(row.offset) < 0x1000),
        "every surviving row is in the code and none is the tombstone: {:?}",
        program.lines
    );
    assert_eq!(program.lines.len(), 2, "and the live sequence's own two rows all survived");
}

/// A `.debug_abbrev` + `.debug_info` pair describing one unit and the subprograms given.
///
/// DWARF 4 rather than 5, because that is the version the C and Swift toolchains here emit for
/// `.debug_info`, and because the entry forms it needs (`DW_FORM_addr`, `DW_FORM_data4`) are the
/// ones a real producer uses at that version.
fn info_sections(functions: &[(&str, u32, u32)]) -> (Vec<u8>, Vec<u8>) {
    const DW_TAG_COMPILE_UNIT: u64 = 0x11;
    const DW_TAG_SUBPROGRAM: u64 = 0x2e;
    const DW_AT_NAME: u64 = 0x03;
    const DW_AT_LOW_PC: u64 = 0x11;
    const DW_AT_HIGH_PC: u64 = 0x12;
    const DW_FORM_ADDR: u64 = 0x01;
    const DW_FORM_DATA4: u64 = 0x06;

    let mut abbrev = Vec::new();
    uleb128(&mut abbrev, 1);
    uleb128(&mut abbrev, DW_TAG_COMPILE_UNIT);
    abbrev.push(1);
    uleb128(&mut abbrev, DW_AT_NAME);
    uleb128(&mut abbrev, DW_FORM_STRING);
    abbrev.extend_from_slice(&[0, 0]);
    uleb128(&mut abbrev, 2);
    uleb128(&mut abbrev, DW_TAG_SUBPROGRAM);
    abbrev.push(0);
    uleb128(&mut abbrev, DW_AT_NAME);
    uleb128(&mut abbrev, DW_FORM_STRING);
    uleb128(&mut abbrev, DW_AT_LOW_PC);
    uleb128(&mut abbrev, DW_FORM_ADDR);
    uleb128(&mut abbrev, DW_AT_HIGH_PC);
    uleb128(&mut abbrev, DW_FORM_DATA4);
    abbrev.extend_from_slice(&[0, 0]);
    abbrev.push(0);

    let mut body = Vec::new();
    body.extend_from_slice(&4u16.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.push(4);
    uleb128(&mut body, 1);
    inline_string(&mut body, "unit.swift");
    for &(name, low, length) in functions {
        uleb128(&mut body, 2);
        inline_string(&mut body, name);
        body.extend_from_slice(&low.to_le_bytes());
        body.extend_from_slice(&length.to_le_bytes());
    }
    body.push(0);

    let mut info = (body.len() as u32).to_le_bytes().to_vec();
    info.extend_from_slice(&body);
    (abbrev, info)
}

/// A discarded row is not evidence that the debug information belongs to another image.
///
/// # THE REFUSAL THAT TOOK OUT AN ENTIRE BOARD FAMILY
///
/// Refusing the whole program at the first row below the image base is right for a table that
/// describes some other build, and wrong for the one shape `--gc-sections` produces on every part
/// that does not boot from zero: a discarded row's address is 0, and an STM32 image is based at
/// `0x08000000`, so every correctly built STM32 image was refused outright with a message about
/// the wrong program. `samples/nucleo-l476rg-swift` and `samples/stm32f746-disco-swift` could not
/// be opened at all.
///
/// The two are still told apart, and by a WIDER test than the one this replaces: rows outside the
/// code are dropped first, and the program is refused only if the table had rows and NONE of them
/// survived. Debug information whose addresses are all ABOVE the image failed no check before.
#[test]
fn a_discarded_row_below_the_image_does_not_refuse_the_program() {
    const TEXT: u32 = 0x0800_0100;
    let discarded = line_section("Discarded.swift", 0);
    let live = line_section("Blink.swift", TEXT);
    let mut both = discarded;
    both.extend_from_slice(&live);
    let elf = lamella_elf::write_debuggable_executable(
        lamella_elf::Machine::Arm,
        &[0x00u8, 0xBF].repeat(16),
        0,
        TEXT,
        true,
        &[(".debug_line", &both)],
    );

    let program = from_elf(&elf).expect("a discarded row is not a mismatched image");
    assert_eq!(program.files, vec!["Blink.swift"], "{:?}", program.files);
    assert_eq!(program.lines.len(), 2, "the live sequence's rows are all there");
}

/// A file that cannot say what its code is must not be able to crash the reader.
///
/// # THE FILTER THAT DROPS A ROW BELOW THE BASE IS ALSO WHAT KEEPS THE ARITHMETIC IN RANGE
///
/// A row's offset is `address - base`, and the executable-section filter is what normally stops a
/// row below the base from reaching it. That filter DELIBERATELY passes everything when the file
/// names no executable section, because an unanswerable question must not become a negative answer
/// -- and those two rules meet at a subtraction that then underflows. In a debug build that is a
/// panic in a crate whose callers are a debug adapter and a CLI; in release it wraps to a value
/// `u32::try_from` happens to reject, so the same input is a crash or a silent skip depending on
/// how the caller was built.
///
/// **This is latent rather than live.** `from_elf` refuses anything that is not `ET_EXEC` with a
/// `PT_LOAD`, so a relocatable object never reaches it, and every image this toolchain produces
/// flags `.text` executable. What reaches it is a malformed or unusual producer -- which is exactly
/// the input a debugger is handed and cannot vet.
#[test]
fn a_row_below_the_base_does_not_panic_when_the_file_names_no_code() {
    /// Clears `SHF_EXECINSTR` on a section, leaving it allocated at the same address -- an image
    /// that loads and runs, and that answers "which of my sections hold instructions" with none.
    fn without_the_executable_flag(elf: &[u8], name: &str) -> Vec<u8> {
        let mut out = elf.to_vec();
        let rd32 = |b: &[u8], o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let rd16 = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        let shoff = rd32(&out, 32) as usize;
        let shentsize = rd16(&out, 46) as usize;
        let shnum = rd16(&out, 48) as usize;
        let strtab = rd32(&out, shoff + rd16(&out, 50) as usize * shentsize + 16) as usize;
        for i in 0..shnum {
            let header = shoff + i * shentsize;
            let at = strtab + rd32(&out, header) as usize;
            let end = at + out[at..].iter().position(|&b| b == 0).expect("terminated");
            if &out[at..end] == name.as_bytes() {
                let flags = rd32(&out, header + 8) & !0x4;
                out[header + 8..header + 12].copy_from_slice(&flags.to_le_bytes());
                return out;
            }
        }
        panic!("no section named {name}");
    }

    const TEXT: u32 = 0x0800_0100;
    let mut both = line_section("Discarded.swift", 0);
    both.extend_from_slice(&line_section("Blink.swift", TEXT));
    let elf = lamella_elf::write_debuggable_executable(
        lamella_elf::Machine::Arm,
        &[0x00u8, 0xBF].repeat(16),
        0,
        TEXT,
        true,
        &[(".debug_line", &both)],
    );
    let elf = without_the_executable_flag(&elf, ".text");
    assert!(
        lamella_elf::executable_ranges(&elf).is_empty(),
        "the fixture's whole point is a file that names no executable section"
    );

    let program = from_elf(&elf).expect("a file that cannot name its code is still debuggable");
    assert_eq!(program.files, vec!["Blink.swift"], "{:?}", program.files);
    assert!(
        program.lines.iter().all(|row| row.offset < 0x1000),
        "no row was offset by a wrapped subtraction: {:?}",
        program.lines
    );
}

/// The entry is the function whose range CONTAINS the entry address.
///
/// # THE NEAREST PRECEDING FUNCTION IS A CONFIDENT WRONG ANSWER
///
/// They differ whenever the entry point has no subprogram of its own, which is the ordinary case
/// for a C floor built without `-g`: the nearest-preceding rule then names whatever function
/// happens to end just before it. On `samples/stm32f746-disco-swift` it named
/// `OUTLINED_FUNCTION_12` -- a function the compiler invented, ending fourteen bytes before the
/// entry it was credited with -- and a session opens by reporting that name.
///
/// `?` is the honest answer when nothing contains the address, and it is what this fixture asks
/// for: the entry sits in the gap between two real functions.
#[test]
fn the_entry_is_the_function_containing_it_and_not_the_nearest_one_before() {
    const TEXT: u32 = 0x0800_0100;
    let (abbrev, info) = info_sections(&[
        ("before_the_entry", TEXT, 8),
        ("holds_the_entry", TEXT + 16, 8),
    ]);
    let line = line_section("Blink.swift", TEXT);
    let build = |entry_offset: u32| {
        lamella_elf::write_debuggable_executable(
            lamella_elf::Machine::Arm,
            &[0x00u8, 0xBF].repeat(16),
            entry_offset,
            TEXT,
            true,
            &[(".debug_line", &line), (".debug_abbrev", &abbrev), (".debug_info", &info)],
        )
    };

    let inside = from_elf(&build(18)).expect("parses");
    assert_eq!(
        inside.entry, "holds_the_entry",
        "the entry is four bytes into the second function, and `before_the_entry` starts earlier"
    );

    let between = from_elf(&build(12)).expect("parses");
    assert_eq!(
        between.entry, "?",
        "no function covers that address, and naming the nearest one would be a guess"
    );
}

/// A symbol table and its string table: the null entry, then one `Elf32_Sym` per
/// `(name, value, size, st_info)`, each defined in section 1.
fn symbol_table(entries: &[(&str, u32, u32, u8)]) -> (Vec<u8>, Vec<u8>) {
    let mut strings = vec![0u8];
    let mut table = vec![0u8; 16];
    for &(name, value, size, info) in entries {
        let name_at = strings.len() as u32;
        strings.extend_from_slice(name.as_bytes());
        strings.push(0);
        table.extend_from_slice(&name_at.to_le_bytes());
        table.extend_from_slice(&value.to_le_bytes());
        table.extend_from_slice(&size.to_le_bytes());
        table.push(info);
        table.push(0);
        table.extend_from_slice(&1u16.to_le_bytes());
    }
    (table, strings)
}

/// The same file, with the `.symtab` and `.strtab` sections the writer carried as plain data typed as
/// a symbol table and the string table it links, the way a linker writes them.
fn typed_as_a_symbol_table(mut elf: Vec<u8>) -> Vec<u8> {
    let word = |elf: &[u8], at: usize| u32::from_le_bytes([elf[at], elf[at + 1], elf[at + 2], elf[at + 3]]);
    let half = |elf: &[u8], at: usize| u16::from_le_bytes([elf[at], elf[at + 1]]);
    let headers = word(&elf, 32) as usize;
    let count = usize::from(half(&elf, 48));
    let header = |index: usize| headers + index * 40;
    let section_names = word(&elf, header(usize::from(half(&elf, 50))) + 16) as usize;
    let named = |elf: &[u8], wanted: &str| {
        (0..count)
            .find(|&index| {
                let at = section_names + word(elf, header(index)) as usize;
                elf[at..].split(|&byte| byte == 0).next() == Some(wanted.as_bytes())
            })
            .expect(wanted)
    };
    let (symtab, strtab) = (named(&elf, ".symtab"), named(&elf, ".strtab"));
    elf[header(symtab) + 4..header(symtab) + 8].copy_from_slice(&2u32.to_le_bytes());
    elf[header(symtab) + 24..header(symtab) + 28].copy_from_slice(&(strtab as u32).to_le_bytes());
    elf[header(strtab) + 4..header(strtab) + 8].copy_from_slice(&3u32.to_le_bytes());
    elf
}

#[test]
fn an_entry_no_subprogram_holds_is_named_by_the_function_symbol_that_holds_it() {
    const TEXT: u32 = 0x0800_0100;
    let (abbrev, info) = info_sections(&[("described", TEXT, 8)]);
    let line = line_section("Blink.swift", TEXT);
    let (symtab, strtab) = symbol_table(&[("startup_code", TEXT + 16 + 1, 8, 0x02)]);
    let build = |entry_offset: u32| {
        typed_as_a_symbol_table(lamella_elf::write_debuggable_executable(
            lamella_elf::Machine::Arm,
            &[0x00u8, 0xBF].repeat(16),
            entry_offset,
            TEXT,
            true,
            &[
                (".debug_line", &line),
                (".debug_abbrev", &abbrev),
                (".debug_info", &info),
                (".symtab", &symtab),
                (".strtab", &strtab),
            ],
        ))
    };

    let inside = from_elf(&build(18)).expect("parses");
    assert_eq!(inside.entry, "startup_code", "no subprogram holds the entry, and a symbol does");
    assert!(
        inside.names.contains(&(16, 24, String::from("startup_code"))),
        "and the symbol names its code for every frame, not only the entry: {:?}",
        inside.names
    );

    let between = from_elf(&build(12)).expect("parses");
    assert_eq!(between.entry, "?", "neither a subprogram nor a symbol holds this address");
}

#[test]
fn a_symbol_names_code_only_where_no_subprogram_does() {
    use lamella_elf::{Binding, symbols::Function};
    const BASE: u64 = 0x0800_0000;
    let mut names = vec![(0x10, 0x20, String::from("described"))];
    let symbols = [
        Function { address: 0x0800_0010, size: 0x10, binding: Binding::Global, name: "described_again" },
        Function { address: 0x0800_0018, size: 0x20, binding: Binding::Global, name: "overlaps_its_end" },
        Function { address: 0x0800_0040, size: 0x10, binding: Binding::Local, name: "undescribed" },
        Function { address: 0x07FF_FFF0, size: 0x10, binding: Binding::Global, name: "below_the_image" },
        Function { address: 0x0800_0080, size: 0x10, binding: Binding::Global, name: "not_code" },
    ];
    add_symbol_names(&mut names, &symbols, BASE, |address| address < 0x0800_0080);
    assert_eq!(
        names,
        vec![(0x10, 0x20, String::from("described")), (0x40, 0x50, String::from("undescribed"))],
        "a symbol over described code is left out even where it runs past that code; one below the \
         image or outside its code is left out too"
    );
}

#[test]
fn where_several_symbols_hold_one_address_one_rule_names_it() {
    use lamella_elf::{Binding, symbols::Function};
    const BASE: u64 = 0x0800_0000;
    let function = |name, address, size, binding| Function { address, size, binding, name };
    let symbols = [
        function("local_alias", 0x0800_0100, 0x20, Binding::Local),
        function("global_alias", 0x0800_0100, 0x20, Binding::Global),
        function("weak_alias", 0x0800_0100, 0x20, Binding::Weak),
        function("second_global_alias", 0x0800_0100, 0x20, Binding::Global),
        function("outer", 0x0800_00F0, 0x110, Binding::Local),
        function("short", 0x0800_0100, 0x08, Binding::Global),
    ];
    let mut names = Vec::new();
    add_symbol_names(&mut names, &symbols, BASE, |_| true);
    let at = |offset| name_containing(&names, offset).map(|(_, _, name)| name.as_str());
    assert_eq!(at(0x104), Some("short"), "the symbols starting nearest below, and of those the shortest");
    assert_eq!(
        at(0x110),
        Some("global_alias"),
        "a global symbol before a weak one before a local one, then the one the table lists first"
    );
    assert_eq!(at(0x1F0), Some("outer"), "only the outer symbol holds this address");
    assert_eq!(at(0x0E0), None, "and no symbol holds this one");
}

/// A subprogram for code the linker discarded is dropped, the same way a row is.
///
/// **THE RULE HAS TWO CALL SITES AND THIS IS THE SECOND ONE.** The row half is covered above; the
/// name table is what a stack frame is labelled from, and 221 of 236 entries in one real Swift
/// image were address accessors the linker had removed, all at address 0. A frame anywhere in the
/// first function would have been labelled with one of them.
#[test]
fn a_subprogram_for_discarded_code_is_dropped_too() {
    const TEXT: u32 = 0x54;
    let (abbrev, info) = info_sections(&[("discarded", 0, 12), ("live", TEXT, 8)]);
    let line = line_section("Blink.swift", TEXT);
    let elf = lamella_elf::write_debuggable_executable(
        lamella_elf::Machine::Arm,
        &[0x00u8, 0xBF].repeat(16),
        0,
        TEXT,
        true,
        &[(".debug_line", &line), (".debug_abbrev", &abbrev), (".debug_info", &info)],
    );

    let program = from_elf(&elf).expect("parses");
    assert_eq!(
        program.names.iter().map(|(_, _, name)| name.as_str()).collect::<Vec<_>>(),
        vec!["live"],
        "the discarded subprogram is not a function this image has"
    );
}

/// An ELF with no debug information is refused, and refused DISTINCTLY.
///
/// A release build reaches this, and the repair belongs to whoever built it. Reporting it as a
/// parse failure would send them looking for a corrupt file instead of a missing flag.
#[test]
fn an_image_with_no_debug_information_is_refused_rather_than_served_empty() {
    let elf = lamella_elf::write_executable_arm_thumb(&[0x00, 0xBF, 0x00, 0xBF], 0, 0x0800_0000);
    assert!(
        matches!(from_elf(&elf), Err(ProgramError::NoDebugLine)),
        "an image with no .debug_line is its own case"
    );
}

/// Bytes that are not an ELF are refused before anything else is attempted.
#[test]
fn a_file_that_is_not_an_elf_is_refused() {
    assert!(matches!(from_elf(b"MZ\x90\x00\x03\x00\x00\x00"), Err(ProgramError::NotAnElf(_))));
}

/// A line table describing addresses below the image is a mismatch, not a rounding error.
///
/// **Each half looks fine on its own.** An ELF carrying debug sections from a different link
/// answers a plausible line for every address, and a session steps through the wrong source
/// without anything going wrong. Refusing is what makes it visible.
#[test]
fn debug_information_from_another_image_is_refused() {
    let elf = debuggable(0x0800_1000, 0x0800_0100);
    match from_elf(&elf) {
        Err(ProgramError::AddressBelowImage { address, base }) => {
            assert_eq!(address, 0x0800_0100, "it names the address the table described");
            assert!(base > 0x0800_0100, "and where the image actually starts: {base:#x}");
        }
        other => panic!(
            "expected AddressBelowImage, got {:?}",
            other.map(|program| program.image_base)
        ),
    }
}


/// A prel31 offset from `place` to `target`, as an index table stores one.
fn prel31(target: u32, place: u32) -> u32 {
    target.wrapping_sub(place) & 0x7FFF_FFFF
}

/// Little-endian words, as a table section holds them.
fn words(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|word| word.to_le_bytes()).collect()
}

const INDEX: u32 = 0x0800_1000;
const EXTAB: u32 = 0x0800_2000;

/// An index table describing `0x08000100` (cannot unwind) and `0x08000200` (a table entry at
/// `EXTAB + 4`).
fn index_with_one_table_entry() -> Vec<u8> {
    words(&[
        prel31(0x0800_0100, INDEX),
        lamella_elf::ehabi::EXIDX_CANTUNWIND,
        prel31(0x0800_0200, INDEX + 8),
        prel31(EXTAB + 4, INDEX + 12),
    ])
}

#[test]
fn a_program_keeps_its_index_tables_and_only_the_sections_their_entries_point_into() {
    use lamella_elf::ehabi::{Region, Tables};
    let index = index_with_one_table_entry();
    let extab = words(&[0, 0x8084_80B0]);
    let text = vec![0u8; 0x400];
    let rodata = vec![0xAAu8; 8];
    let tables = Tables {
        indexes: vec![Region { address: INDEX, bytes: &index }],
        loaded: vec![
            Region { address: 0x0800_0000, bytes: &text },
            Region { address: INDEX, bytes: &index },
            Region { address: EXTAB, bytes: &extab },
            Region { address: 0x0800_3000, bytes: &rodata },
        ],
    };
    let kept = UnwindTables::from_tables(&tables, vec![(0x0800_0000, 0x0800_0400)]);
    assert_eq!(kept.indexes, vec![(INDEX, index.clone())]);
    assert_eq!(
        kept.entries,
        vec![(EXTAB, extab.clone())],
        "the code, the index itself and a section no entry points into are not copied"
    );
    assert_eq!(kept.code, vec![(0x0800_0000, 0x0800_0400)]);
    assert!(!kept.is_empty());
}

#[test]
fn an_index_table_that_cannot_be_searched_is_not_kept() {
    use lamella_elf::ehabi::{Region, Tables, EXIDX_CANTUNWIND};
    let index = words(&[
        prel31(0x0800_0200, INDEX),
        EXIDX_CANTUNWIND,
        prel31(0x0800_0100, INDEX + 8),
        EXIDX_CANTUNWIND,
    ]);
    let tables = Tables {
        indexes: vec![Region { address: INDEX, bytes: &index }],
        loaded: vec![Region { address: INDEX, bytes: &index }],
    };
    let kept = UnwindTables::from_tables(&tables, Vec::new());
    assert!(kept.is_empty(), "an index out of order answers the wrong function for some address");
}

#[test]
fn an_address_is_described_by_the_function_containing_it_and_only_inside_the_code() {
    use lamella_elf::ehabi::Description;
    let kept = UnwindTables {
        indexes: vec![(
            INDEX,
            words(&[
                prel31(0x0800_0100, INDEX),
                0x80A8_B0B0,
                prel31(0x0800_0200, INDEX + 8),
                lamella_elf::ehabi::EXIDX_CANTUNWIND,
            ]),
        )],
        entries: Vec::new(),
        code: vec![(0x0800_0000, 0x0800_0300)],
    };

    let described = kept.describe(0x0800_0150).expect("inside the first function");
    assert_eq!(described.entry.function, 0x0800_0100);
    assert_eq!(
        described.description,
        Description::Compact { personality: 0, table: None, instructions: vec![0xA8, 0xB0, 0xB0] }
    );
    assert_eq!(kept.describe(0x0800_0250).map(|d| d.description), Some(Description::CantUnwind));
    assert_eq!(kept.describe(0x0800_00F0), None, "below the first function");
    assert_eq!(
        kept.describe(0x0800_0350),
        None,
        "past the code: the index does not say where its last function ends"
    );

    let unbounded = UnwindTables { code: Vec::new(), ..kept };
    assert_eq!(
        unbounded.describe(0x0800_0350).map(|d| d.description),
        Some(Description::CantUnwind),
        "a file that named no code cannot bound the last function, and is not refused for it"
    );
}

fn named<'a>(name: &'a [u8], linkage: Option<&'a [u8]>) -> lamella_dwarf::info::Function<'a> {
    lamella_dwarf::info::Function {
        name,
        linkage_name: linkage,
        low_pc: 0x100,
        high_pc: 0x200,
        decl_file: None,
        decl_line: None,
        unit_offset: 0,
        stmt_list: None,
        comp_dir: None,
    }
}

/// `DW_AT_name` is the source spelling and a source spelling is not unique, so a table built from it
/// alone carries the same name at two address ranges -- two methods called `write` are then
/// indistinguishable in a frame. The qualified linkage name is what distinguishes them.
#[test]
fn a_qualified_linkage_name_is_what_a_frame_reports() {
    let uart = named(b"write", Some(b"Nrf51Uart.write"));
    let gpio = named(b"write", Some(b"Nrf51Gpio.write"));
    assert_eq!(reported_name(&uart), "Nrf51Uart.write");
    assert_eq!(reported_name(&gpio), "Nrf51Gpio.write");
    assert_ne!(
        reported_name(&uart),
        reported_name(&gpio),
        "the whole point is that these stop being the same string"
    );
}

/// A `::` qualifier counts too: the separator is the language's, and the relationship being tested
/// is that one name is the other with a scope in front of it.
#[test]
fn a_double_colon_qualifier_counts_as_one() {
    assert_eq!(
        reported_name(&named(b"write", Some(b"Nrf51Uart::write"))),
        "Nrf51Uart::write"
    );
}

/// **A LINKAGE NAME IS NOT ALWAYS A READABLE QUALIFIER.** It is whatever the language's linkage
/// conventions say, which for several of them is a mangled symbol encoding the types and the arity.
/// Answering an ambiguous name with an unreadable one is not an improvement, so a mangled name is
/// left alone -- and the reader is no worse off than before this rule existed.
#[test]
fn a_mangled_linkage_name_is_not_put_in_a_frame() {
    assert_eq!(
        reported_name(&named(b"write", Some(b"$s4Test10Nrf51UartV5writeyySSF"))),
        "write"
    );
    assert_eq!(
        reported_name(&named(b"write", Some(b"_ZN9Nrf51Uart5writeEv"))),
        "write"
    );
}

/// What is shown always ENDS with what the source called it, so nothing a reader knew is lost.
#[test]
fn what_is_reported_always_ends_with_the_source_name() {
    for (name, linkage) in [
        (&b"write"[..], Some(&b"Nrf51Uart.write"[..])),
        (b"write", Some(b"$s4Test5writeF")),
        (b"write", None),
        (b"appMain", Some(b"appMain")),
        (b"write", Some(b"prefixwrite")),
    ] {
        let reported = reported_name(&named(name, linkage));
        assert!(
            reported.ends_with(&String::from_utf8_lossy(name).into_owned()),
            "{reported} must end with the source name"
        );
    }
}

/// A qualifier is a SCOPE, not a prefix that happens to end in the same letters. `prefixwrite` is a
/// different function, and taking it would name a frame after something else entirely.
#[test]
fn a_name_that_merely_ends_in_the_source_name_is_not_a_qualifier() {
    assert_eq!(
        reported_name(&named(b"write", Some(b"prefixwrite"))),
        "write"
    );
}

/// No linkage name, and a linkage name equal to the source name, both leave today's answer alone --
/// which is what keeps this rule from being a regression where the producer emitted nothing useful.
#[test]
fn an_absent_or_identical_linkage_name_changes_nothing() {
    assert_eq!(reported_name(&named(b"appMain", None)), "appMain");
    assert_eq!(
        reported_name(&named(b"appMain", Some(b"appMain"))),
        "appMain"
    );
}

/// A linkage name carrying bytes that are not printable is not put in front of a person.
#[test]
fn an_unprintable_linkage_name_is_refused() {
    assert_eq!(
        reported_name(&named(b"write", Some(b"Nrf51\x00Uart.write"))),
        "write"
    );
}
