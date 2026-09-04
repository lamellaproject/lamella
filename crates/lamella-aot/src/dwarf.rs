//! DWARF 5 generation for AOT-compiled code -- the debug info the backend produces for ITS OWN
//! output, as opposed to the external LLVM DWARF the linker carries through unchanged.

use alloc::string::String;
use alloc::vec::Vec;

use crate::debugmap::SourceLine;

/// One function's source-line rows, ready for a line-number program sequence.
#[derive(Debug, Clone)]
pub struct FunctionLines<'a> {
    /// The function's name, as a debugger should show it. Used by [`compilation_unit`]; the line
    /// program itself describes addresses, not names.
    pub name: &'a str,
    /// The source file the function lowered from.
    pub file: &'a str,
    /// The function's rows, each a code offset RELATIVE TO THE FUNCTION paired with the source
    /// position it lowered from, sorted by offset (what [`crate::debugmap::source_lines`] returns).
    pub rows: &'a [SourceLine],
    /// The function's code size in bytes. A sequence must end at the address one past its last
    /// instruction (DWARF 5 section 6.2.5.3), so without this the final row would appear to extend
    /// over whatever the linker placed next.
    pub code_size: u32,
}

/// A generated debug section: its name, its bytes, and the references it could not resolve itself.
///
/// The two relocation lists are kept apart on purpose -- they resolve in the two DIFFERENT address
/// spaces of DWARF 5 section 7.3.1, and merging them is exactly how debug info ends up looking
/// plausible while pointing at nothing.
#[derive(Debug, Clone)]
pub struct GeneratedSection {
    /// The ELF section name (`.debug_line`, ...).
    pub name: &'static str,
    /// The section's bytes.
    pub data: Vec<u8>,
    /// `(byte offset within `data`, index into the `functions` slice)` -- a 32-bit absolute
    /// reference to that function's code. The emitter relocates each against the function's symbol,
    /// which is what gives it the image's load address.
    pub code_relocs: Vec<(u32, usize)>,
    /// `(byte offset within `data`, target section name)` -- a 32-bit reference to another debug
    /// section, resolved SECTION-RELATIVE. A debug section is never loaded, so no load address ever
    /// belongs in one of these.
    pub section_relocs: Vec<(u32, &'static str)>,
}


const DW_LNS_COPY: u8 = 0x01;
const DW_LNS_ADVANCE_PC: u8 = 0x02;
const DW_LNS_ADVANCE_LINE: u8 = 0x03;
const DW_LNS_SET_FILE: u8 = 0x04;
const DW_LNS_SET_COLUMN: u8 = 0x05;

const DW_LNE_END_SEQUENCE: u8 = 0x01;
const DW_LNE_SET_ADDRESS: u8 = 0x02;

const DW_LNCT_PATH: u8 = 0x1;
const DW_LNCT_DIRECTORY_INDEX: u8 = 0x2;

/// `DW_FORM_string` -- the string is inline, NUL-terminated. Chosen over `DW_FORM_line_strp` so a
/// line program needs no `.debug_line_str` beside it: one section, no cross-section reference, and
/// nothing for the linker to resolve. It costs duplication of repeated file names, which a later
/// string-table pass can reclaim.
const DW_FORM_STRING: u8 = 0x08;
/// `DW_FORM_udata` -- an unsigned LEB128 constant.
const DW_FORM_UDATA: u8 = 0x0f;

/// The smallest value a special opcode can add to the `line` register, and the count of values it
/// can add (DWARF 5 section 6.2.5.1). Together they let one byte encode the common case of "advance
/// a few bytes of code, move a few source lines"; anything outside the window falls back to an
/// explicit `DW_LNS_advance_line`.
const LINE_BASE: i64 = -5;
const LINE_RANGE: i64 = 14;
/// The number assigned to the first special opcode -- one past the highest standard opcode, so all
/// twelve standard opcodes stay available.
const OPCODE_BASE: i64 = 13;

/// The number of LEB128 operands each standard opcode takes, in opcode order 1..=12. A consumer
/// that does not know an opcode uses this to skip it, so it must match table 7.25 exactly.
const STANDARD_OPCODE_LENGTHS: [u8; 12] = [0, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1];

/// Builds the `.debug_line` section for a set of functions: one line-number program (DWARF 5
/// section 6.2) holding one SEQUENCE per function.
///
/// A sequence per function, rather than one sequence over the whole image, is what makes the result
/// survive `--gc-sections`: each begins with its own relocated `DW_LNE_set_address`, so a function
/// the linker drops takes its own sequence's address with it instead of leaving the rest of the
/// program describing code that moved.
///
/// Returns an empty section when no function has any rows -- there is nothing to describe, and an
/// empty program is not the same as a well-formed one describing nothing.
#[must_use]
pub fn line_program(functions: &[FunctionLines]) -> GeneratedSection {
    let mut data = Vec::new();
    let mut code_relocs = Vec::new();
    if functions.iter().all(|f| f.rows.is_empty()) {
        return GeneratedSection {
            name: ".debug_line",
            data,
            code_relocs,
            section_relocs: Vec::new(),
        };
    }

    let files = file_table(functions);

    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&5u16.to_le_bytes());
    data.push(4);
    data.push(0);
    let header_length_at = data.len();
    data.extend_from_slice(&0u32.to_le_bytes());
    let after_header_length = data.len();
    data.push(1);
    data.push(1);
    data.push(1);
    data.push(LINE_BASE as u8);
    data.push(LINE_RANGE as u8);
    data.push(OPCODE_BASE as u8);
    data.extend_from_slice(&STANDARD_OPCODE_LENGTHS);

    data.push(1);
    uleb128(&mut data, u64::from(DW_LNCT_PATH));
    uleb128(&mut data, u64::from(DW_FORM_STRING));
    uleb128(&mut data, 1);
    data.extend_from_slice(b".\0");

    data.push(2);
    uleb128(&mut data, u64::from(DW_LNCT_PATH));
    uleb128(&mut data, u64::from(DW_FORM_STRING));
    uleb128(&mut data, u64::from(DW_LNCT_DIRECTORY_INDEX));
    uleb128(&mut data, u64::from(DW_FORM_UDATA));
    uleb128(&mut data, files.len() as u64);
    for file in &files {
        data.extend_from_slice(file.as_bytes());
        data.push(0);
        uleb128(&mut data, 0);
    }

    let header_length = (data.len() - after_header_length) as u32;
    data[header_length_at..header_length_at + 4].copy_from_slice(&header_length.to_le_bytes());

    for (index, func) in functions.iter().enumerate() {
        if func.rows.is_empty() {
            continue;
        }
        data.push(0);
        uleb128(&mut data, 5);
        data.push(DW_LNE_SET_ADDRESS);
        code_relocs.push((data.len() as u32, index));
        data.extend_from_slice(&0u32.to_le_bytes());

        data.push(DW_LNS_SET_FILE);
        uleb128(&mut data, file_index(&files, func.file));

        let mut address: u32 = 0;
        let mut line: i64 = 1;
        let mut column: u32 = 0;
        for row in func.rows {
            if row.col != column {
                data.push(DW_LNS_SET_COLUMN);
                uleb128(&mut data, u64::from(row.col));
                column = row.col;
            }
            let advance = row.addr.saturating_sub(address);
            emit_row(&mut data, advance, i64::from(row.line) - line);
            address = row.addr;
            line = i64::from(row.line);
        }

        let tail = func.code_size.saturating_sub(address);
        if tail > 0 {
            data.push(DW_LNS_ADVANCE_PC);
            uleb128(&mut data, u64::from(tail));
        }
        data.push(0);
        uleb128(&mut data, 1);
        data.push(DW_LNE_END_SEQUENCE);
    }

    let unit_length = (data.len() - 4) as u32;
    data[0..4].copy_from_slice(&unit_length.to_le_bytes());
    GeneratedSection {
        name: ".debug_line",
        data,
        code_relocs,
        section_relocs: Vec::new(),
    }
}

/// Appends one matrix row: advance the address by `advance` bytes and the line by `line_inc`, then
/// append. Uses a one-byte SPECIAL opcode where the pair fits in the window the header configured,
/// and falls back to explicit standard opcodes where it does not (DWARF 5 section 6.2.5.1).
fn emit_row(data: &mut Vec<u8>, advance: u32, line_inc: i64) {
    let mut line_inc = line_inc;
    if !(LINE_BASE..LINE_BASE + LINE_RANGE).contains(&line_inc) {
        data.push(DW_LNS_ADVANCE_LINE);
        sleb128(data, line_inc);
        line_inc = 0;
    }
    let mut advance = advance;
    if special_opcode(line_inc, advance) > 255 {
        data.push(DW_LNS_ADVANCE_PC);
        uleb128(data, u64::from(advance));
        advance = 0;
    }
    let opcode = special_opcode(line_inc, advance);
    debug_assert!(
        (OPCODE_BASE..=255).contains(&opcode),
        "a normalized row always encodes as a special opcode"
    );
    let _ = DW_LNS_COPY;
    data.push(opcode as u8);
}

/// The special opcode encoding a given (line delta, address advance) pair -- DWARF 5 section
/// 6.2.5.1. Returned as `i64` so an out-of-range result is visible to the caller rather than
/// wrapping into a valid-looking opcode.
fn special_opcode(line_inc: i64, advance: u32) -> i64 {
    (line_inc - LINE_BASE) + LINE_RANGE * i64::from(advance) + OPCODE_BASE
}

/// Appends an unsigned LEB128 (DWARF 5 section 7.6): seven bits per byte, little-endian, with the
/// high bit set on every byte but the last.
pub fn uleb128(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            return;
        }
    }
}

/// Appends a signed LEB128 (DWARF 5 section 7.6). The terminating condition is sign-aware: the
/// value is complete once the remaining bits are all copies of the sign bit AND the last emitted
/// byte's bit 6 agrees with that sign -- without the second half, `-64` would encode as a positive
/// number.
pub fn sleb128(out: &mut Vec<u8>, mut value: i64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        let sign_bit_set = byte & 0x40 != 0;
        let done = (value == 0 && !sign_bit_set) || (value == -1 && sign_bit_set);
        out.push(if done { byte } else { byte | 0x80 });
        if done {
            return;
        }
    }
}

/// Appends a NUL-terminated string -- `DW_FORM_string`'s representation.
pub fn inline_string(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
    out.push(0);
}

/// The file-name table in EMITTED order: the primary source file at index 0, then the distinct
/// source files across `functions` in first-appearance order -- so the primary one appears twice.
///
/// **THE DUPLICATE IS LOAD-BEARING.** DWARF 5 numbers file entries from 0 and makes entry 0 the
/// compilation file (section 6.2.4), but the line program's `file` register still has an INITIAL
/// VALUE OF 1 (table 6.4). A table with a single entry is therefore well formed and yet has a
/// starting state naming an entry that does not exist, and a reader that resolves that state
/// eagerly rejects the whole table rather than the one row. Emitting the primary file at both 0
/// and 1 leaves no index a reader can reach out of range.
///
/// Shared by the line program and the compilation unit deliberately: a subprogram's
/// `DW_AT_decl_file` is an index into the LINE program's file table, so two independently built
/// lists that happened to differ would make every declaration name the wrong file.
fn file_table<'a>(functions: &[FunctionLines<'a>]) -> Vec<&'a str> {
    let mut files: Vec<&str> = Vec::new();
    for f in functions {
        if !files.contains(&f.file) {
            files.push(f.file);
        }
    }
    if let Some(primary) = files.first().copied() {
        files.insert(0, primary);
    }
    files
}

/// `name`'s index in [`file_table`]'s output, searched from 1 -- entry 0 is the duplicate of the
/// primary file, and an index the initial state can also produce is the one shape worth not
/// emitting. A file not in the table falls back to 1, the primary.
fn file_index(files: &[&str], name: &str) -> u64 {
    files
        .iter()
        .skip(1)
        .position(|f| *f == name)
        .map_or(1, |i| i as u64 + 1)
}


const DW_TAG_COMPILE_UNIT: u64 = 0x11;
const DW_TAG_SUBPROGRAM: u64 = 0x2e;

const DW_AT_NAME: u64 = 0x03;
const DW_AT_STMT_LIST: u64 = 0x10;
const DW_AT_LOW_PC: u64 = 0x11;
const DW_AT_HIGH_PC: u64 = 0x12;
const DW_AT_LANGUAGE: u64 = 0x13;
const DW_AT_COMP_DIR: u64 = 0x1b;
const DW_AT_PRODUCER: u64 = 0x25;
const DW_AT_DECL_FILE: u64 = 0x3a;
const DW_AT_DECL_LINE: u64 = 0x3b;
const DW_AT_EXTERNAL: u64 = 0x3f;

const DW_FORM_ADDR: u64 = 0x01;
const DW_FORM_DATA2: u64 = 0x05;
const DW_FORM_DATA4: u64 = 0x06;
const DW_FORM_DATA1: u64 = 0x0b;
const DW_FORM_SEC_OFFSET: u64 = 0x17;
/// `DW_FORM_flag_present` -- the attribute's PRESENCE is its value, so it occupies no bytes in the
/// entry at all. Only the abbreviation records it.
const DW_FORM_FLAG_PRESENT: u64 = 0x19;

const DW_CHILDREN_NO: u8 = 0x00;
const DW_CHILDREN_YES: u8 = 0x01;

/// `DW_UT_compile` -- a full, non-split compilation unit.
const DW_UT_COMPILE: u8 = 0x01;

/// The abbreviation codes this module emits. Values are arbitrary but must match between the two
/// sections, so they are named once.
const ABBREV_COMPILE_UNIT: u64 = 1;
const ABBREV_SUBPROGRAM: u64 = 2;

/// `DW_LANG_C_sharp` -- the registered `DW_AT_language` code for C#, default lower bound 0.
///
/// It is NOT in the DWARF 5 spec: table 7.17 ends at `DW_LANG_BLISS` (0x25). It was registered
/// afterwards through the standard's public-comment process (issue 230203.1, accepted April 2023)
/// and carried into DWARF 6. The stated motivation is worth knowing, because it is the same problem
/// we have: .NET began emitting DWARF for natively compiled executables and had no code to name the
/// language with.
///
/// The lesson recorded here rather than in a commit nobody re-reads: the published spec is not the
/// whole of the official source. Codes assigned between revisions live in the standard's own
/// registry, and reading only the PDF yields a confident, checkable, wrong answer.
const DW_LANG_C_SHARP: u64 = 0x0032;

/// Builds the `.debug_info` and `.debug_abbrev` pair describing `functions` as one compilation unit:
/// a `DW_TAG_compile_unit` with a `DW_TAG_subprogram` child per function, each carrying the
/// `low_pc`/`high_pc` a debugger needs to turn an address into a function name.
///
/// `functions` must be in CODE ORDER, because the unit's own range starts at the first one.
/// `code_span` is the distance from the first function's entry to the end of the last -- `None`
/// leaves the unit rangeless rather than claiming an extent that is not known, which a debugger
/// tolerates (it finds functions through the subprogram entries) where a wrong one it cannot.
///
/// Returns `(.debug_info, .debug_abbrev)`. Both are empty when there are no functions.
#[must_use]
pub fn compilation_unit(
    name: &str,
    producer: &str,
    code_span: Option<u32>,
    functions: &[FunctionLines],
) -> (GeneratedSection, GeneratedSection) {
    let mut info = Vec::new();
    let mut abbrev = Vec::new();
    let mut code_relocs = Vec::new();
    let mut section_relocs = Vec::new();
    if functions.is_empty() {
        return (
            GeneratedSection {
                name: ".debug_info",
                data: info,
                code_relocs,
                section_relocs,
            },
            GeneratedSection {
                name: ".debug_abbrev",
                data: abbrev,
                code_relocs: Vec::new(),
                section_relocs: Vec::new(),
            },
        );
    }
    let files = file_table(functions);

    uleb128(&mut abbrev, ABBREV_COMPILE_UNIT);
    uleb128(&mut abbrev, DW_TAG_COMPILE_UNIT);
    abbrev.push(DW_CHILDREN_YES);
    let mut unit_attrs = alloc::vec![
        (DW_AT_PRODUCER, DW_FORM_STRING as u64),
        (DW_AT_LANGUAGE, DW_FORM_DATA2),
        (DW_AT_NAME, DW_FORM_STRING as u64),
        (DW_AT_COMP_DIR, DW_FORM_STRING as u64),
        (DW_AT_STMT_LIST, DW_FORM_SEC_OFFSET),
    ];
    if code_span.is_some() {
        unit_attrs.push((DW_AT_LOW_PC, DW_FORM_ADDR));
        unit_attrs.push((DW_AT_HIGH_PC, DW_FORM_DATA4));
    }
    for (at, form) in &unit_attrs {
        uleb128(&mut abbrev, *at);
        uleb128(&mut abbrev, *form);
    }
    uleb128(&mut abbrev, 0);
    uleb128(&mut abbrev, 0);

    uleb128(&mut abbrev, ABBREV_SUBPROGRAM);
    uleb128(&mut abbrev, DW_TAG_SUBPROGRAM);
    abbrev.push(DW_CHILDREN_NO);
    for (at, form) in [
        (DW_AT_NAME, DW_FORM_STRING as u64),
        (DW_AT_DECL_FILE, DW_FORM_DATA1),
        (DW_AT_DECL_LINE, DW_FORM_DATA4),
        (DW_AT_LOW_PC, DW_FORM_ADDR),
        (DW_AT_HIGH_PC, DW_FORM_DATA4),
        (DW_AT_EXTERNAL, DW_FORM_FLAG_PRESENT),
    ] {
        uleb128(&mut abbrev, at);
        uleb128(&mut abbrev, form);
    }
    uleb128(&mut abbrev, 0);
    uleb128(&mut abbrev, 0);
    uleb128(&mut abbrev, 0);

    info.extend_from_slice(&0u32.to_le_bytes());
    info.extend_from_slice(&5u16.to_le_bytes());
    info.push(DW_UT_COMPILE);
    info.push(4);
    section_relocs.push((info.len() as u32, ".debug_abbrev"));
    info.extend_from_slice(&0u32.to_le_bytes());

    uleb128(&mut info, ABBREV_COMPILE_UNIT);
    inline_string(&mut info, producer);
    info.extend_from_slice(&(DW_LANG_C_SHARP as u16).to_le_bytes());
    inline_string(&mut info, name);
    inline_string(&mut info, ".");
    section_relocs.push((info.len() as u32, ".debug_line"));
    info.extend_from_slice(&0u32.to_le_bytes());
    if let Some(span) = code_span {
        code_relocs.push((info.len() as u32, 0));
        info.extend_from_slice(&0u32.to_le_bytes());
        info.extend_from_slice(&span.to_le_bytes());
    }

    for (index, func) in functions.iter().enumerate() {
        uleb128(&mut info, ABBREV_SUBPROGRAM);
        inline_string(&mut info, func.name);
        info.push(file_index(&files, func.file) as u8);
        let decl_line = func.rows.first().map_or(0, |row| row.line);
        info.extend_from_slice(&decl_line.to_le_bytes());
        code_relocs.push((info.len() as u32, index));
        info.extend_from_slice(&0u32.to_le_bytes());
        info.extend_from_slice(&func.code_size.to_le_bytes());
    }
    uleb128(&mut info, 0);

    let unit_length = (info.len() - 4) as u32;
    info[0..4].copy_from_slice(&unit_length.to_le_bytes());
    (
        GeneratedSection {
            name: ".debug_info",
            data: info,
            code_relocs,
            section_relocs,
        },
        GeneratedSection {
            name: ".debug_abbrev",
            data: abbrev,
            code_relocs: Vec::new(),
            section_relocs: Vec::new(),
        },
    )
}

/// The `String` form of a display name, for callers that build one per function.
#[must_use]
pub fn owned(s: &str) -> String {
    String::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn row(addr: u32, line: u32, col: u32) -> SourceLine {
        SourceLine { addr, line, col }
    }

    #[test]
    fn uleb128_matches_the_spec_examples() {
        let mut out = Vec::new();
        uleb128(&mut out, 2);
        assert_eq!(out, [2]);
        out.clear();
        uleb128(&mut out, 127);
        assert_eq!(out, [0x7f]);
        out.clear();
        uleb128(&mut out, 128);
        assert_eq!(out, [0x80, 0x01]);
        out.clear();
        uleb128(&mut out, 129);
        assert_eq!(out, [0x81, 0x01]);
        out.clear();
        uleb128(&mut out, 12857);
        assert_eq!(out, [0xb9, 0x64]);
    }

    #[test]
    fn sleb128_matches_the_spec_examples() {
        let cases: [(i64, &[u8]); 8] = [
            (2, &[2]),
            (-2, &[0x7e]),
            (127, &[0xff, 0x00]),
            (-127, &[0x81, 0x7f]),
            (128, &[0x80, 0x01]),
            (-128, &[0x80, 0x7f]),
            (129, &[0x81, 0x01]),
            (-129, &[0xff, 0x7e]),
        ];
        for (value, expected) in cases {
            let mut out = Vec::new();
            sleb128(&mut out, value);
            assert_eq!(out, expected, "sleb128({value})");
        }
    }

    #[test]
    fn sleb128_terminates_on_the_sign_bit_not_the_value() {
        let mut out = Vec::new();
        sleb128(&mut out, -64);
        assert_eq!(out, [0x40]);
        let mut out = Vec::new();
        sleb128(&mut out, 64);
        assert_eq!(out, [0xc0, 0x00], "+64 needs a second byte to stay positive");
    }

    #[test]
    fn an_empty_set_of_functions_produces_no_section() {
        let generated = line_program(&[FunctionLines {
            name: "f",
            file: "a.cs",
            rows: &[],
            code_size: 8,
        }]);
        assert!(generated.data.is_empty());
        assert!(generated.code_relocs.is_empty());
    }

    #[test]
    fn the_header_declares_dwarf_5_and_a_consistent_unit_length() {
        let rows = [row(0, 10, 5)];
        let generated = line_program(&[FunctionLines {
            name: "f",
            file: "prog.cs",
            rows: &rows,
            code_size: 4,
        }]);
        let data = &generated.data;
        let unit_length = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        assert_eq!(
            unit_length as usize,
            data.len() - 4,
            "unit_length counts everything after itself"
        );
        assert_eq!(u16::from_le_bytes([data[4], data[5]]), 5, "version");
        assert_eq!(data[6], 4, "address_size");
        assert_eq!(data[7], 0, "segment_selector_size");
        let header_length = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
        let program_start = 12 + header_length as usize;
        assert!(program_start < data.len(), "the program must be non-empty");
        assert_eq!(data[program_start], 0, "extended opcode introducer");
        assert_eq!(data[program_start + 1], 5, "extended opcode length");
        assert_eq!(data[program_start + 2], DW_LNE_SET_ADDRESS);
    }

    #[test]
    fn each_function_gets_its_own_relocated_set_address() {
        let a = [row(0, 1, 0)];
        let b = [row(0, 7, 0)];
        let generated = line_program(&[
            FunctionLines {
                name: "f",
                file: "a.cs",
                rows: &a,
                code_size: 4,
            },
            FunctionLines {
                name: "f",
                file: "b.cs",
                rows: &b,
                code_size: 4,
            },
        ]);
        assert_eq!(generated.code_relocs.len(), 2, "one per function");
        assert_eq!(generated.code_relocs[0].1, 0, "names function 0");
        assert_eq!(generated.code_relocs[1].1, 1, "names function 1");
        assert_ne!(generated.code_relocs[0].0, generated.code_relocs[1].0);
        for (offset, _) in &generated.code_relocs {
            let at = *offset as usize;
            assert_eq!(
                &generated.data[at..at + 4],
                &[0, 0, 0, 0],
                "the address is left for the linker"
            );
        }
    }

    #[test]
    fn a_function_with_no_rows_is_skipped_without_shifting_the_others() {
        let b = [row(0, 3, 0)];
        let generated = line_program(&[
            FunctionLines {
                name: "f",
                file: "a.cs",
                rows: &[],
                code_size: 4,
            },
            FunctionLines {
                name: "f",
                file: "b.cs",
                rows: &b,
                code_size: 4,
            },
        ]);
        assert_eq!(generated.code_relocs.len(), 1);
        assert_eq!(
            generated.code_relocs[0].1, 1,
            "the sole sequence names function 1, not function 0"
        );
    }

    #[test]
    fn distinct_files_are_pooled_and_named_by_index() {
        let a = [row(0, 1, 0)];
        let b = [row(0, 1, 0)];
        let c = [row(0, 1, 0)];
        let generated = line_program(&[
            FunctionLines {
                name: "f",
                file: "one.cs",
                rows: &a,
                code_size: 4,
            },
            FunctionLines {
                name: "f",
                file: "two.cs",
                rows: &b,
                code_size: 4,
            },
            FunctionLines {
                name: "f",
                file: "one.cs",
                rows: &c,
                code_size: 4,
            },
        ]);
        let count = |needle: &[u8]| {
            generated
                .data
                .windows(needle.len())
                .filter(|w| *w == needle)
                .count()
        };
        assert_eq!(
            count(b"one.cs\0"),
            2,
            "the primary file is entry 0 AND entry 1, and pooled thereafter"
        );
        assert_eq!(count(b"two.cs\0"), 1);
        assert_eq!(generated.code_relocs.len(), 3);
    }

    #[test]
    fn a_line_delta_outside_the_special_window_falls_back_to_advance_line() {
        let rows = [row(0, 1, 0), row(2, 10, 0)];
        let generated = line_program(&[FunctionLines {
            name: "f",
            file: "a.cs",
            rows: &rows,
            code_size: 4,
        }]);
        assert!(
            generated.data.contains(&DW_LNS_ADVANCE_LINE),
            "a 9-line jump needs an explicit advance_line"
        );

        let rows = [row(0, 1, 0), row(2, 9, 0)];
        let tight = line_program(&[FunctionLines {
            name: "f",
            file: "a.cs",
            rows: &rows,
            code_size: 4,
        }]);
        let program_start = 12 + u32::from_le_bytes([
            tight.data[8],
            tight.data[9],
            tight.data[10],
            tight.data[11],
        ]) as usize;
        assert!(
            !tight.data[program_start..].contains(&DW_LNS_ADVANCE_LINE),
            "an 8-line jump fits in a special opcode"
        );
    }

    #[test]
    fn a_large_address_advance_falls_back_to_advance_pc() {
        let rows = [row(0, 1, 0), row(400, 2, 0)];
        let generated = line_program(&[FunctionLines {
            name: "f",
            file: "a.cs",
            rows: &rows,
            code_size: 404,
        }]);
        let program_start = 12 + u32::from_le_bytes([
            generated.data[8],
            generated.data[9],
            generated.data[10],
            generated.data[11],
        ]) as usize;
        assert!(
            generated.data[program_start..].contains(&DW_LNS_ADVANCE_PC),
            "a 400-byte advance cannot be a special opcode"
        );
    }

    #[test]
    fn every_sequence_ends_with_end_sequence() {
        let rows = [row(0, 1, 0)];
        let generated = line_program(&[FunctionLines {
            name: "f",
            file: "a.cs",
            rows: &rows,
            code_size: 12,
        }]);
        let data = &generated.data;
        let n = data.len();
        assert_eq!(&data[n - 3..], &[0, 1, DW_LNE_END_SEQUENCE]);
        assert_eq!(data[n - 5], DW_LNS_ADVANCE_PC);
        assert_eq!(data[n - 4], 12, "the sequence closes past the last byte");
    }

    #[test]
    fn a_row_is_emitted_for_every_distinct_position() {
        let rows = [row(0, 1, 0), row(2, 2, 0), row(4, 3, 0), row(6, 4, 0)];
        let generated = line_program(&[FunctionLines {
            name: "f",
            file: "a.cs",
            rows: &rows,
            code_size: 8,
        }]);
        let program_start = 12 + u32::from_le_bytes([
            generated.data[8],
            generated.data[9],
            generated.data[10],
            generated.data[11],
        ]) as usize;
        let expected = special_opcode(1, 2) as u8;
        let emitted = generated.data[program_start..]
            .iter()
            .filter(|b| **b == expected)
            .count();
        assert_eq!(emitted, 3, "three (advance 2, line +1) steps after the first");
    }

    #[test]
    fn a_column_change_is_recorded() {
        let rows = [row(0, 1, 3), row(2, 1, 9)];
        let generated = line_program(&[FunctionLines {
            name: "f",
            file: "a.cs",
            rows: &rows,
            code_size: 4,
        }]);
        let program_start = 12 + u32::from_le_bytes([
            generated.data[8],
            generated.data[9],
            generated.data[10],
            generated.data[11],
        ]) as usize;
        const SET_ADDRESS_BYTES: usize = 7;
        const SET_FILE_BYTES: usize = 2;
        let program = &generated.data[program_start + SET_ADDRESS_BYTES + SET_FILE_BYTES..];
        let columns: Vec<u8> = program
            .windows(2)
            .filter(|w| w[0] == DW_LNS_SET_COLUMN)
            .map(|w| w[1])
            .collect();
        assert_eq!(columns, vec![3, 9], "both columns are set");
    }

    fn func<'a>(name: &'a str, file: &'a str, rows: &'a [SourceLine], size: u32) -> FunctionLines<'a> {
        FunctionLines {
            name,
            file,
            rows,
            code_size: size,
        }
    }

    #[test]
    fn the_unit_header_is_the_dwarf_5_field_order() {
        let rows = [row(0, 3, 0)];
        let (info, abbrev) = compilation_unit("p.cs", "test", None, &[func("f", "p.cs", &rows, 4)]);
        let d = &info.data;
        assert_eq!(
            u32::from_le_bytes([d[0], d[1], d[2], d[3]]) as usize,
            d.len() - 4,
            "unit_length counts everything after itself"
        );
        assert_eq!(u16::from_le_bytes([d[4], d[5]]), 5, "version");
        assert_eq!(d[6], DW_UT_COMPILE, "unit_type -- new in DWARF 5, at byte 6");
        assert_eq!(d[7], 4, "address_size follows it");
        assert_eq!(
            info.section_relocs
                .iter()
                .find(|(_, name)| *name == ".debug_abbrev")
                .map(|(at, _)| *at),
            Some(8),
            "debug_abbrev_offset sits at byte 8, not the DWARF 4 byte 6"
        );
        assert!(!abbrev.data.is_empty());
    }

    #[test]
    fn every_function_becomes_a_subprogram_with_a_relocated_low_pc() {
        let a = [row(0, 3, 0)];
        let b = [row(0, 9, 0)];
        let (info, _) = compilation_unit(
            "p.cs",
            "test",
            None,
            &[func("Program.Add", "p.cs", &a, 8), func("Program.Main", "p.cs", &b, 20)],
        );
        assert_eq!(info.code_relocs.len(), 2);
        assert_eq!(info.code_relocs[0].1, 0);
        assert_eq!(info.code_relocs[1].1, 1);
        assert_ne!(info.code_relocs[0].0, info.code_relocs[1].0);
        for (at, _) in &info.code_relocs {
            let at = *at as usize;
            assert_eq!(&info.data[at..at + 4], &[0, 0, 0, 0], "left for the linker");
            let high = u32::from_le_bytes([
                info.data[at + 4],
                info.data[at + 5],
                info.data[at + 6],
                info.data[at + 7],
            ]);
            assert!(high == 8 || high == 20, "high_pc is the function's size, got {high}");
        }
        let has = |needle: &[u8]| info.data.windows(needle.len()).any(|w| w == needle);
        assert!(has(b"Program.Add\0"));
        assert!(has(b"Program.Main\0"));
    }

    #[test]
    fn stmt_list_points_at_the_line_program_section_relative() {
        let rows = [row(0, 3, 0)];
        let (info, _) = compilation_unit("p.cs", "test", None, &[func("f", "p.cs", &rows, 4)]);
        let line_ref: Vec<_> = info
            .section_relocs
            .iter()
            .filter(|(_, name)| *name == ".debug_line")
            .collect();
        assert_eq!(line_ref.len(), 1, "the unit names its line program exactly once");
        assert!(
            !info
                .code_relocs
                .iter()
                .any(|(at, _)| *at == line_ref[0].0),
            "stmt_list must not also be relocated as code"
        );
    }

    #[test]
    fn a_unit_range_is_emitted_only_when_the_span_is_known() {
        let rows = [row(0, 3, 0)];
        let rangeless = compilation_unit("p.cs", "t", None, &[func("f", "p.cs", &rows, 4)]);
        let ranged = compilation_unit("p.cs", "t", Some(64), &[func("f", "p.cs", &rows, 4)]);
        assert_eq!(rangeless.0.code_relocs.len(), 1, "the subprogram's low_pc only");
        assert_eq!(ranged.0.code_relocs.len(), 2, "plus the unit's own low_pc");
        assert!(
            ranged.1.data.len() > rangeless.1.data.len(),
            "the abbreviation must declare the two extra attributes, or the entry cannot be parsed"
        );
    }

    #[test]
    fn decl_file_indexes_the_same_table_the_line_program_builds() {
        let a = [row(0, 1, 0)];
        let b = [row(0, 1, 0)];
        let functions = [
            func("first", "one.cs", &a, 4),
            func("second", "two.cs", &b, 4),
        ];
        let line = line_program(&functions);
        let (info, _) = compilation_unit("one.cs", "t", None, &functions);
        let pos_one = line
            .data
            .windows(7)
            .position(|w| w == b"one.cs\0")
            .expect("one.cs in the file table");
        let pos_two = line
            .data
            .windows(7)
            .position(|w| w == b"two.cs\0")
            .expect("two.cs in the file table");
        assert!(pos_one < pos_two, "first-appearance order");
        let at = info
            .data
            .windows(7)
            .position(|w| w == b"second\0")
            .expect("the subprogram name");
        assert_eq!(info.data[at + 7], 2, "`second` declares file index 2");
        let at = info
            .data
            .windows(6)
            .position(|w| w == b"first\0")
            .expect("the subprogram name");
        assert_eq!(
            info.data[at + 6], 1,
            "`first` declares file index 1 -- never 0, which the initial state also names"
        );
    }

    #[test]
    fn no_functions_produces_no_unit() {
        let (info, abbrev) = compilation_unit("p.cs", "t", None, &[]);
        assert!(info.data.is_empty());
        assert!(abbrev.data.is_empty());
    }
}
