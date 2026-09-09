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
    /// The method's OPENING source position -- `(line, column)` -- or `None` when it has none.
    ///
    /// It is deliberately not one of `rows`, because it is not a position the lowered code
    /// reaches. `rows` describe emitted instructions; a function's PROLOGUE is code no source
    /// construct produced, so nothing maps to it and the first row begins some bytes in. That
    /// uncovered span starts at the function's entry, which is the address a consumer resolves
    /// `<function>` to, so this is what keeps the entry from being a hole.
    pub entry: Option<(u32, u32)>,
    /// The function's named locals, in slot order -- what a debugger lists in its locals window.
    ///
    /// A local carries a NAME and a TYPE here; where it LIVES is in `locations`, joined by slot.
    pub locals: &'a [crate::debugmap::LocalSlot],
    /// Where each local lives, over which spans of this function's code -- joined to `locals` by
    /// [`crate::debugmap::LocalSlot::slot`] and NOT by position, because the two lists are filtered
    /// differently (a slot the code generator cannot place has no entry here at all).
    ///
    /// A local with no entry, or with an empty range list, is emitted with a name and a type and no
    /// `DW_AT_location`, which a debugger reads as "not available" -- the honest answer where this
    /// backend cannot say, and the one it gave for every local before locations existed.
    pub locations: &'a [LocalLocation],
    /// The method's named parameters, in argument order -- what a debugger shows as the function's
    /// signature and in its `info args`.
    pub params: &'a [crate::debugmap::LocalSlot],
    /// Where each parameter lives, joined to `params` by slot exactly as `locations` is to `locals`.
    pub param_locations: &'a [LocalLocation],
}

/// One span of a function's code over which a local lives at one fixed place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalRange {
    /// The span's first byte, relative to the function's entry.
    pub start: u32,
    /// One past the span's last byte, relative to the function's entry.
    pub end: u32,
    /// Where the local is over that span: this many bytes above the stack pointer.
    ///
    /// A frame slot rather than a register, and that is the whole of what this backend will claim.
    /// See `arm32::SpilledHomes`: a spilled slot belongs to one value for the length of the
    /// function, while a register is handed on to the next value the moment the first one dies --
    /// so a register-homed local would read correct until it went dead and then confidently
    /// report somebody else's number.
    pub frame_offset: u16,
}

/// Where one local lives across a function: a list of spans, each with the place it is at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalLocation {
    /// The CIL local slot, joining this to a [`crate::debugmap::LocalSlot`] of the same slot.
    pub slot: u16,
    /// The spans, in increasing address order and non-overlapping. Empty means "nothing to say",
    /// which is emitted as no `DW_AT_location` at all rather than as an empty list.
    pub ranges: Vec<LocalRange>,
}

/// A generated debug section: its name, its bytes, and the references it could not resolve itself.
///
/// The two relocation lists are kept apart on purpose -- they resolve in the two DIFFERENT address
/// spaces of DWARF 5 section 7.3.1, and merging them is exactly how debug info ends up looking
/// plausible while pointing at nothing.
///
/// # THE ADDEND LIVES IN THE FIELD, AND [`site_addend`] IS HOW IT GETS OUT
///
/// A relocation site here is a 4-byte field written with the offset the reference needs FROM its
/// target: `low_pc` writes 0 because it means the function's own entry, and a location list's
/// range writes its start because it means that many bytes into the function. The object writer
/// emits `.rela` -- an explicit addend -- so that number has to be copied into the relocation
/// rather than left where DWARF conventionally puts it.
///
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
/// `DW_LNS_set_prologue_end` -- marks the row a breakpoint on the FUNCTION belongs at. The register
/// it sets is reset after every appended row, so it applies to the next row only.
const DW_LNS_SET_PROLOGUE_END: u8 = 0x0a;

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
/// `unit` is the compilation unit's primary source file -- the same string the caller hands
/// [`compilation_unit`] as its `name`. DWARF 5 section 6.2.4 requires file entry 0 to match the
/// unit's `DW_AT_name` exactly, so it is taken as an argument here rather than inferred from the
/// first function: two lists that agree because they were built the same way agree by luck.
///
/// Returns an empty section when no function has any rows -- there is nothing to describe, and an
/// empty program is not the same as a well-formed one describing nothing.
#[must_use]
pub fn line_program(unit: &str, functions: &[FunctionLines]) -> GeneratedSection {
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

    let files = file_table(unit, functions);
    let directories = directory_table(&files);

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
    uleb128(&mut data, directories.len() as u64);
    for directory in &directories {
        data.extend_from_slice(directory.as_bytes());
        data.push(0);
    }

    data.push(2);
    uleb128(&mut data, u64::from(DW_LNCT_PATH));
    uleb128(&mut data, u64::from(DW_FORM_STRING));
    uleb128(&mut data, u64::from(DW_LNCT_DIRECTORY_INDEX));
    uleb128(&mut data, u64::from(DW_FORM_UDATA));
    uleb128(&mut data, files.len() as u64);
    for file in &files {
        data.extend_from_slice(split_path(file).1.as_bytes());
        data.push(0);
        uleb128(&mut data, directory_index(&directories, file));
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

        let uncovered_entry = func.rows.first().is_some_and(|first| first.addr > 0);
        if let (true, Some((entry_line, entry_column))) = (uncovered_entry, func.entry) {
            if entry_column != column {
                data.push(DW_LNS_SET_COLUMN);
                uleb128(&mut data, u64::from(entry_column));
                column = entry_column;
            }
            emit_row(&mut data, 0, i64::from(entry_line) - line);
            line = i64::from(entry_line);
        }

        for (index, row) in func.rows.iter().enumerate() {
            if row.col != column {
                data.push(DW_LNS_SET_COLUMN);
                uleb128(&mut data, u64::from(row.col));
                column = row.col;
            }
            if index == 0 {
                data.push(DW_LNS_SET_PROLOGUE_END);
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
/// source files across `functions` in first-appearance order, so entry 0 and entry 1 both describe
/// the primary file -- identically whenever `unit` is the first function's file, which is every
/// single-file assembly.
///
/// **THE SECOND ENTRY IS LOAD-BEARING.** DWARF 5 numbers file entries from 0 and makes entry 0 the
/// compilation file (section 6.2.4), but the line program's `file` register still has an INITIAL
/// VALUE OF 1 (table 6.4). A table with a single entry is therefore well formed and yet has a
/// starting state naming an entry that does not exist, and a reader that resolves that state
/// eagerly rejects the whole table rather than the one row. Prefixing the list leaves no index a
/// reader can reach out of range.
///
///
/// `unit` heads the list because section 6.2.4 also requires entry 0 to match the unit's
/// `DW_AT_name`. A `unit` naming no function's file still belongs there -- it is the file the
/// compilation is OF, and nothing has to point at it.
///
/// Shared by the line program and the compilation unit deliberately: a subprogram's
/// `DW_AT_decl_file` is an index into the LINE program's file table, so two independently built
/// lists that happened to differ would make every declaration name the wrong file.
fn file_table<'a>(unit: &'a str, functions: &[FunctionLines<'a>]) -> Vec<&'a str> {
    let mut files: Vec<&str> = Vec::new();
    for f in functions {
        if !files.contains(&f.file) {
            files.push(f.file);
        }
    }
    if let Some(primary) = files.first().copied() {
        files.insert(0, if unit.is_empty() { primary } else { unit });
    }
    files
}

/// Splits a source path into `(directory, file name)` at its last separator, treating `/` and `\`
/// alike -- a Portable PDB document name carries the separator of the machine that compiled it, and
/// the backend reading it is not necessarily on that machine. A path with no separator has no
/// directory; one whose only separator is leading (`/x.cs`) has the root.
///
/// **STORING THE TWO HALVES SEPARATELY IS NOT A SPACE OPTIMIZATION.** A consumer JOINS a file
/// entry's directory to its name using ITS OWN host's separator, so a name field holding a whole
/// path yields, on a host whose separator differs from the compiling host's, a string containing no
/// separator that host recognizes -- one filename component, which it can neither open nor rewrite,
/// because a source-path substitution matches leading path COMPONENTS and there are none. Splitting
/// is what guarantees the joined result carries a separator the reading host knows, which is what
/// its own basename search and its path substitution both work from.
fn split_path(path: &str) -> (&str, &str) {
    match path.rfind(|c| c == '/' || c == '\\') {
        Some(0) => (&path[..1], &path[1..]),
        Some(at) => (&path[..at], &path[at + 1..]),
        None => ("", path),
    }
}

/// The line program's directory table: the distinct directories of `files` in first-appearance
/// order, so entry 0 is the primary file's -- which is what makes it the COMPILATION directory
/// section 6.2.4 requires there, and the string the unit repeats as `DW_AT_comp_dir`.
///
/// A file carrying no directory is filed under `"."`, which is what this whole table used to be.
/// A unit built from bare file names therefore emits exactly what it emitted before.
fn directory_table<'a>(files: &[&'a str]) -> Vec<&'a str> {
    let mut directories: Vec<&'a str> = Vec::new();
    for file in files {
        let directory = directory_of(*file);
        if !directories.contains(&directory) {
            directories.push(directory);
        }
    }
    if directories.is_empty() {
        directories.push(".");
    }
    directories
}

/// `file`'s directory as [`directory_table`] files it: `"."` when the path carries none.
fn directory_of(file: &str) -> &str {
    match split_path(file).0 {
        "" => ".",
        directory => directory,
    }
}

/// The index into [`directory_table`]'s output of the directory holding `file`. A directory somehow
/// absent from it falls back to 0, the compilation directory -- the one entry that always exists.
fn directory_index(directories: &[&str], file: &str) -> u64 {
    let directory = directory_of(file);
    directories
        .iter()
        .position(|d| *d == directory)
        .unwrap_or(0) as u64
}

/// `name`'s index in [`file_table`]'s output, searched from 1 -- entry 0 is the unit's own primary
/// file, and an index the line program's initial state can also produce is the one shape worth not
/// emitting. A file not in the table falls back to 1, the first function's.
fn file_index(files: &[&str], name: &str) -> u64 {
    files
        .iter()
        .skip(1)
        .position(|f| *f == name)
        .map_or(1, |i| i as u64 + 1)
}


const DW_TAG_COMPILE_UNIT: u64 = 0x11;
const DW_TAG_SUBPROGRAM: u64 = 0x2e;
const DW_TAG_BASE_TYPE: u64 = 0x24;
const DW_TAG_VARIABLE: u64 = 0x34;
/// `DW_TAG_formal_parameter` -- a declared parameter, which is a DIFFERENT tag from a variable and
/// not a stylistic distinction. A debugger builds the function's SIGNATURE from these children and
/// lists them under `info args`; described as variables instead, the parameters would appear in the
/// locals pane and the function would still print as taking none.
const DW_TAG_FORMAL_PARAMETER: u64 = 0x05;

const DW_AT_BYTE_SIZE: u64 = 0x0b;
const DW_AT_ENCODING: u64 = 0x3e;
/// A reference to the type DIE describing this entry, as an offset from the start of this unit's
/// header. `DW_FORM_ref4` measures from the UNIT, not from the section: an emitter that writes a
/// section offset here produces a file every structural check accepts, in which every variable
/// names the wrong type.
const DW_AT_TYPE: u64 = 0x49;
const DW_FORM_REF4: u64 = 0x13;

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
/// `DW_AT_frame_base` -- the expression a `DW_OP_fbreg` location in this subprogram would be
/// measured from, and the base a debugger reports as the frame's own address.
///
/// THIS BACKEND'S OWN LOCATIONS DO NOT GO THROUGH IT. They are written as `DW_OP_breg13` with an
/// offset, which names the register directly; see [`DW_OP_BREG13`] for why. What this attribute is
/// still for is everything that asks where the FRAME is rather than where a variable is -- and
/// third-party DWARF in the same image (the runtime-support archive) does use `DW_OP_fbreg`, so a
/// consumer needs both paths regardless of which one this backend emits.
const DW_AT_FRAME_BASE: u64 = 0x40;
/// `DW_AT_location` -- where the object described by this entry lives. Written as a
/// `DW_FORM_sec_offset` into `.debug_loclists`, never as an inline expression: a local's home is a
/// property of a PROGRAM COUNTER here, not of the whole function, and one expression cannot say
/// "at sp+40 from here to there and nowhere else".
const DW_AT_LOCATION: u64 = 0x02;
/// `DW_LLE_start_length` (DWARF 5 table 7.10) -- an address, a ULEB128 byte length, and a counted
/// location description. The address form rather than an offset pair because a code address in a
/// relocatable object HAS to come from a relocation, and this is the entry kind that carries one.
const DW_LLE_START_LENGTH: u8 = 0x08;
/// `DW_LLE_end_of_list` -- the terminator every location list needs.
const DW_LLE_END_OF_LIST: u8 = 0x00;
/// `DW_OP_breg13` -- "the address in r13, plus this signed offset". A frame slot, addressed from
/// the stack pointer directly.
///
/// # WHY NOT `DW_OP_fbreg`, WHICH THE `DW_AT_frame_base` ABOVE WOULD ALLOW
///
/// `DW_OP_fbreg` measures from whatever `DW_AT_frame_base` evaluates to, so every local in the
/// function inherits any mistake in that one attribute -- and it is the attribute a future change
/// (a real frame pointer, a `DW_OP_call_frame_cfa`) is most likely to move. `DW_OP_breg13` names
/// the register it means, so a location stays correct however the frame base is later spelled.
/// It is also what LLVM emits for a plain stack slot on this target, measured on its own output.
const DW_OP_BREG13: u8 = 0x7d;
/// `DW_FORM_exprloc` -- a ULEB128 length followed by that many bytes of DWARF expression.
const DW_FORM_EXPRLOC: u64 = 0x18;
/// `DW_OP_reg13`, the ARM stack pointer: the whole of this back end's frame-base expression.
///
/// # WHY NOT `DW_OP_call_frame_cfa`, WHICH IS WHAT A HOST COMPILER USUALLY EMITS
///
/// `DW_OP_call_frame_cfa` makes every local's address depend on the call-frame information being
/// correct, so a defect in the unwind table becomes a wrong VALUE in a variable pane rather than a
/// short backtrace. Naming the stack pointer directly keeps the two independent: a debugger resolves
/// a local with one register read, and the coupling to the unwinder is one value per outer frame
/// instead of a dependency.
///
/// It also matches what the other producer this tree reads is doing -- measured across sixteen
/// images, every subprogram in all of them emits this operator and nothing else -- so the consumer
/// side needs no second path.
const DW_OP_REG13: u8 = 0x5d;

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
/// A `DW_TAG_base_type` DIE: one per DISTINCT type any local in the unit is declared with.
const ABBREV_BASE_TYPE: u64 = 3;
/// A `DW_TAG_variable` whose type this backend can name.
const ABBREV_VARIABLE: u64 = 4;
/// A `DW_TAG_variable` whose type it cannot -- a reference or a value type. It carries the name
/// alone, which DWARF permits and which is the answer that does not mislead.
const ABBREV_VARIABLE_UNTYPED: u64 = 5;
/// A `DW_TAG_variable` whose type this backend can name AND whose home it can point at.
const ABBREV_VARIABLE_LOCATED: u64 = 6;
/// A `DW_TAG_variable` whose home this backend can point at but whose type it cannot name.
const ABBREV_VARIABLE_UNTYPED_LOCATED: u64 = 7;
/// The same four shapes again for `DW_TAG_formal_parameter`. Four more abbreviations rather than a
/// shared one with a tag field, because an abbreviation IS a tag plus a fixed attribute list -- that
/// is what lets an entry carry no codes of its own.
const ABBREV_PARAMETER: u64 = 8;
const ABBREV_PARAMETER_UNTYPED: u64 = 9;
const ABBREV_PARAMETER_LOCATED: u64 = 10;
const ABBREV_PARAMETER_UNTYPED_LOCATED: u64 = 11;

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
/// Returns `(.debug_info, .debug_abbrev, .debug_loclists)`. All three are empty when there are no
/// functions, and the third is empty when no local's home could be placed.
#[must_use]
pub fn compilation_unit(
    name: &str,
    producer: &str,
    code_span: Option<u32>,
    functions: &[FunctionLines],
) -> (GeneratedSection, GeneratedSection, GeneratedSection) {
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
            GeneratedSection {
                name: ".debug_loclists",
                data: Vec::new(),
                code_relocs: Vec::new(),
                section_relocs: Vec::new(),
            },
        );
    }
    let (loclists, list_at) = location_lists(functions);
    let files = file_table(name, functions);
    let directories = directory_table(&files);

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
    abbrev.push(DW_CHILDREN_YES);
    for (at, form) in [
        (DW_AT_NAME, DW_FORM_STRING as u64),
        (DW_AT_DECL_FILE, DW_FORM_DATA1),
        (DW_AT_DECL_LINE, DW_FORM_DATA4),
        (DW_AT_LOW_PC, DW_FORM_ADDR),
        (DW_AT_HIGH_PC, DW_FORM_DATA4),
        (DW_AT_FRAME_BASE, DW_FORM_EXPRLOC),
        (DW_AT_EXTERNAL, DW_FORM_FLAG_PRESENT),
    ] {
        uleb128(&mut abbrev, at);
        uleb128(&mut abbrev, form);
    }
    uleb128(&mut abbrev, 0);
    uleb128(&mut abbrev, 0);

    uleb128(&mut abbrev, ABBREV_BASE_TYPE);
    uleb128(&mut abbrev, DW_TAG_BASE_TYPE);
    abbrev.push(DW_CHILDREN_NO);
    for (at, form) in [
        (DW_AT_NAME, DW_FORM_STRING as u64),
        (DW_AT_ENCODING, DW_FORM_DATA1),
        (DW_AT_BYTE_SIZE, DW_FORM_DATA1),
    ] {
        uleb128(&mut abbrev, at);
        uleb128(&mut abbrev, form);
    }
    uleb128(&mut abbrev, 0);
    uleb128(&mut abbrev, 0);

    uleb128(&mut abbrev, ABBREV_VARIABLE);
    uleb128(&mut abbrev, DW_TAG_VARIABLE);
    abbrev.push(DW_CHILDREN_NO);
    for (at, form) in [(DW_AT_NAME, DW_FORM_STRING as u64), (DW_AT_TYPE, DW_FORM_REF4)] {
        uleb128(&mut abbrev, at);
        uleb128(&mut abbrev, form);
    }
    uleb128(&mut abbrev, 0);
    uleb128(&mut abbrev, 0);

    uleb128(&mut abbrev, ABBREV_VARIABLE_UNTYPED);
    uleb128(&mut abbrev, DW_TAG_VARIABLE);
    abbrev.push(DW_CHILDREN_NO);
    uleb128(&mut abbrev, DW_AT_NAME);
    uleb128(&mut abbrev, DW_FORM_STRING as u64);
    uleb128(&mut abbrev, 0);
    uleb128(&mut abbrev, 0);

    uleb128(&mut abbrev, ABBREV_VARIABLE_LOCATED);
    uleb128(&mut abbrev, DW_TAG_VARIABLE);
    abbrev.push(DW_CHILDREN_NO);
    for (at, form) in [
        (DW_AT_NAME, DW_FORM_STRING as u64),
        (DW_AT_LOCATION, DW_FORM_SEC_OFFSET),
        (DW_AT_TYPE, DW_FORM_REF4),
    ] {
        uleb128(&mut abbrev, at);
        uleb128(&mut abbrev, form);
    }
    uleb128(&mut abbrev, 0);
    uleb128(&mut abbrev, 0);

    uleb128(&mut abbrev, ABBREV_VARIABLE_UNTYPED_LOCATED);
    uleb128(&mut abbrev, DW_TAG_VARIABLE);
    abbrev.push(DW_CHILDREN_NO);
    for (at, form) in [
        (DW_AT_NAME, DW_FORM_STRING as u64),
        (DW_AT_LOCATION, DW_FORM_SEC_OFFSET),
    ] {
        uleb128(&mut abbrev, at);
        uleb128(&mut abbrev, form);
    }
    uleb128(&mut abbrev, 0);
    uleb128(&mut abbrev, 0);

    for (code, tag, attrs) in [
        (
            ABBREV_PARAMETER,
            DW_TAG_FORMAL_PARAMETER,
            alloc::vec![(DW_AT_NAME, DW_FORM_STRING as u64), (DW_AT_TYPE, DW_FORM_REF4)],
        ),
        (
            ABBREV_PARAMETER_UNTYPED,
            DW_TAG_FORMAL_PARAMETER,
            alloc::vec![(DW_AT_NAME, DW_FORM_STRING as u64)],
        ),
        (
            ABBREV_PARAMETER_LOCATED,
            DW_TAG_FORMAL_PARAMETER,
            alloc::vec![
                (DW_AT_NAME, DW_FORM_STRING as u64),
                (DW_AT_LOCATION, DW_FORM_SEC_OFFSET),
                (DW_AT_TYPE, DW_FORM_REF4),
            ],
        ),
        (
            ABBREV_PARAMETER_UNTYPED_LOCATED,
            DW_TAG_FORMAL_PARAMETER,
            alloc::vec![
                (DW_AT_NAME, DW_FORM_STRING as u64),
                (DW_AT_LOCATION, DW_FORM_SEC_OFFSET),
            ],
        ),
    ] {
        uleb128(&mut abbrev, code);
        uleb128(&mut abbrev, tag);
        abbrev.push(DW_CHILDREN_NO);
        for (at, form) in attrs {
            uleb128(&mut abbrev, at);
            uleb128(&mut abbrev, form);
        }
        uleb128(&mut abbrev, 0);
        uleb128(&mut abbrev, 0);
    }

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
    inline_string(&mut info, split_path(files[0]).1);
    inline_string(&mut info, directories[0]);
    section_relocs.push((info.len() as u32, ".debug_line"));
    info.extend_from_slice(&0u32.to_le_bytes());
    if let Some(span) = code_span {
        code_relocs.push((info.len() as u32, 0));
        info.extend_from_slice(&0u32.to_le_bytes());
        info.extend_from_slice(&span.to_le_bytes());
    }

    let mut base_types: Vec<(crate::debugmap::BaseType, u32)> = Vec::new();
    for func in functions {
        for local in func.params.iter().chain(func.locals) {
            let Some(ty) = local.ty else { continue };
            if base_types.iter().any(|(seen, _)| *seen == ty) {
                continue;
            }
            let offset = info.len() as u32;
            uleb128(&mut info, ABBREV_BASE_TYPE);
            inline_string(&mut info, ty.name);
            info.push(ty.encoding);
            info.push(ty.byte_size);
            base_types.push((ty, offset));
        }
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
        uleb128(&mut info, 1);
        info.push(DW_OP_REG13);

        for (is_param, entries) in [(true, func.params), (false, func.locals)] {
        for local in entries {
            let type_offset = local
                .ty
                .and_then(|ty| base_types.iter().find(|(seen, _)| *seen == ty))
                .map(|&(_, offset)| offset);
            let location = list_at
                .iter()
                .find(|&&(function, param, slot, _)| {
                    function == index && param == is_param && slot == local.slot
                })
                .map(|&(.., at)| at);
            match (type_offset, location) {
                (Some(offset), Some(at)) => {
                    uleb128(&mut info, if is_param { ABBREV_PARAMETER_LOCATED } else { ABBREV_VARIABLE_LOCATED });
                    inline_string(&mut info, &local.name);
                    section_relocs.push((info.len() as u32, ".debug_loclists"));
                    info.extend_from_slice(&at.to_le_bytes());
                    info.extend_from_slice(&offset.to_le_bytes());
                }
                (None, Some(at)) => {
                    uleb128(&mut info, if is_param { ABBREV_PARAMETER_UNTYPED_LOCATED } else { ABBREV_VARIABLE_UNTYPED_LOCATED });
                    inline_string(&mut info, &local.name);
                    section_relocs.push((info.len() as u32, ".debug_loclists"));
                    info.extend_from_slice(&at.to_le_bytes());
                }
                (Some(offset), None) => {
                    uleb128(&mut info, if is_param { ABBREV_PARAMETER } else { ABBREV_VARIABLE });
                    inline_string(&mut info, &local.name);
                    info.extend_from_slice(&offset.to_le_bytes());
                }
                (None, None) => {
                    uleb128(&mut info, if is_param { ABBREV_PARAMETER_UNTYPED } else { ABBREV_VARIABLE_UNTYPED });
                    inline_string(&mut info, &local.name);
                }
            }
        }
        }
        uleb128(&mut info, 0);
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
        loclists,
    )
}

/// Builds `.debug_loclists` for `functions`, and says where each local's list starts:
/// `(function index, CIL local slot, byte offset of that list within the section)`.
///
/// # THE ADDRESSES COME FROM RELOCATIONS, LIKE EVERY OTHER CODE REFERENCE HERE
///
/// A `DW_LLE_start_length` entry names a real address, and there is no such thing until the image
/// has a load address. So each one is emitted as a zero word plus a relocation against its
/// function's symbol, with the range's function-relative start left in the field as the addend --
/// the same shape `DW_AT_low_pc` uses, and the reason this module never decides where code lands.
fn location_lists(functions: &[FunctionLines]) -> (GeneratedSection, Vec<(usize, bool, u16, u32)>) {
    let mut data = Vec::new();
    let mut code_relocs = Vec::new();
    let mut list_at: Vec<(usize, bool, u16, u32)> = Vec::new();
    if functions.iter().all(|f| {
        f.locations
            .iter()
            .chain(f.param_locations)
            .all(|l| l.ranges.is_empty())
    }) {
        return (
            GeneratedSection {
                name: ".debug_loclists",
                data,
                code_relocs,
                section_relocs: Vec::new(),
            },
            list_at,
        );
    }
    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&5u16.to_le_bytes());
    data.push(4);
    data.push(0);
    data.extend_from_slice(&0u32.to_le_bytes());

    for (index, func) in functions.iter().enumerate() {
        for (is_param, local) in func
            .param_locations
            .iter()
            .map(|l| (true, l))
            .chain(func.locations.iter().map(|l| (false, l)))
        {
            if local.ranges.is_empty() {
                continue;
            }
            list_at.push((index, is_param, local.slot, data.len() as u32));
            for range in &local.ranges {
                data.push(DW_LLE_START_LENGTH);
                code_relocs.push((data.len() as u32, index));
                data.extend_from_slice(&range.start.to_le_bytes());
                uleb128(&mut data, u64::from(range.end.saturating_sub(range.start)));
                let mut expression = Vec::new();
                expression.push(DW_OP_BREG13);
                sleb128(&mut expression, i64::from(range.frame_offset));
                uleb128(&mut data, expression.len() as u64);
                data.extend_from_slice(&expression);
            }
            data.push(DW_LLE_END_OF_LIST);
        }
    }
    let unit_length = (data.len() - 4) as u32;
    data[0..4].copy_from_slice(&unit_length.to_le_bytes());
    (
        GeneratedSection {
            name: ".debug_loclists",
            data,
            code_relocs,
            section_relocs: Vec::new(),
        },
        list_at,
    )
}

/// The addend the relocation site at `site` carries: the 4-byte field's current contents, read as a
/// signed 32-bit value.
///
/// ONE IMPLEMENTATION, because both target back ends turn these sections into ELF relocations and a
/// rule spelled out at each of them is a rule that gains its next case at one of them. See
/// [`GeneratedSection`] for why the addend is in the field in the first place.
#[must_use]
pub fn site_addend(section: &GeneratedSection, site: u32) -> i32 {
    section
        .data
        .get(site as usize..site as usize + 4)
        .map_or(0, |b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// The `String` form of a display name, for callers that build one per function.
#[must_use]
pub fn owned(s: &str) -> String {
    String::from(s)
}


/// `DW_CFA_advance_loc` -- the delta in the low six bits, multiplied by the code alignment.
const DW_CFA_ADVANCE_LOC: u8 = 0x40;
/// `DW_CFA_offset` -- the register in the low six bits, the factored offset a ULEB128 after it.
const DW_CFA_OFFSET: u8 = 0x80;
const DW_CFA_ADVANCE_LOC1: u8 = 0x02;
const DW_CFA_ADVANCE_LOC2: u8 = 0x03;
const DW_CFA_ADVANCE_LOC4: u8 = 0x04;
const DW_CFA_DEF_CFA: u8 = 0x0c;
const DW_CFA_DEF_CFA_OFFSET: u8 = 0x0e;

/// The DWARF register numbers this back end's ARM targets use for the stack and link registers.
const ARM_SP: u64 = 13;
const ARM_LR: u64 = 14;

/// Thumb instructions are two-byte aligned, so every `advance_loc` operand is a HALFWORD count and
/// the one-byte form reaches 126 bytes rather than 63.
const CODE_ALIGNMENT: u64 = 2;
/// The stack grows down and every saved register occupies a word, so a factored offset of `n` means
/// `-4n` bytes and the rules a prologue produces encode in a single byte.
const DATA_ALIGNMENT: i64 = -4;

/// One function's frame, as the emitter needs it: enough to say where the caller's stack pointer
/// was and where this function put the caller's registers.
///
/// # THESE ARE FACTS ABOUT THE EMITTED PROLOGUE, NOT ABOUT A CALL SITE
///
/// This back end already records a frame per SAFEPOINT, for the collector. Those records are not
/// these. A function with no call site has none at all, and the width they state is correct only
/// where a call exists to state it for -- the saved block they describe includes the link register
/// unconditionally, which is right exactly when the function has a call in it. Frame information
/// has to describe EVERY function, so it is built from what the prologue emitted rather than from
/// what a safepoint happened to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionFrame {
    /// The function's code size in bytes, which is the range its entry describes.
    pub code_size: u32,
    /// Where the caller's stack pointer stands after each instruction that moved it: `(offset
    /// within the function, bytes above this function's stack pointer)`, ascending by offset.
    ///
    /// EVERY movement, not only the prologue's. A frame stated once is wrong from the next stack
    /// instruction onward, and an epilogue is where that becomes dangerous: once the stack is
    /// restored, a frame address stated from the prologue points into the CALLER's frame, and the
    /// return address read through it comes out of a word belonging to somebody else. Nothing about
    /// that word says so, so the walk reports a caller the program never had instead of stopping.
    pub transitions: Vec<(u32, u32)>,
    /// The registers the prologue pushed, ASCENDING, in DWARF numbering with 14 for the link
    /// register -- which is the order a push stores them in and therefore the order of their slots.
    pub saved: Vec<u8>,
}

/// Builds the `.debug_frame` section: one common information entry, and one frame description per
/// function.
///
/// # WHY A FUNCTION WITH NO FRAME STILL GETS AN ENTRY
///
/// A leaf that never touches the stack is describable in one sentence -- the caller's stack pointer
/// is this one, and the return address is still in its register -- and that sentence is exactly what
/// a debugger may not assume on its own. An address in no entry means this producer described
/// nothing there, and a walk stops. So the cheap entry is emitted rather than skipped: it is the
/// difference between a truncated backtrace and a complete one, for about a dozen bytes.
///
/// # `.debug_frame` and not `.eh_frame`
///
/// The two carry the same table and differ in how a frame description points at its common entry
/// and how addresses are encoded. `.debug_frame` uses an absolute section offset and stores
/// addresses plainly, which is what a relocation applies to; `.eh_frame`'s relative pointer and
/// encoding byte exist so it can be read at run time without one, by an exception personality this
/// runtime does not have.
///
/// Returns an empty section for an empty list: a common entry describing no functions is a
/// well-formed way of saying nothing.
#[must_use]
pub fn frame_section(functions: &[FunctionFrame]) -> GeneratedSection {
    let mut data = Vec::new();
    let mut code_relocs = Vec::new();
    if functions.is_empty() {
        return GeneratedSection {
            name: ".debug_frame",
            data,
            code_relocs,
            section_relocs: Vec::new(),
        };
    }

    let cie_start = data.len();
    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&u32::MAX.to_le_bytes());
    data.push(4);
    data.push(0);
    data.push(4);
    data.push(0);
    uleb128(&mut data, CODE_ALIGNMENT);
    sleb128(&mut data, DATA_ALIGNMENT);
    uleb128(&mut data, ARM_LR);
    data.push(DW_CFA_DEF_CFA);
    uleb128(&mut data, ARM_SP);
    uleb128(&mut data, 0);
    pad_to_word(&mut data);
    let length = (data.len() - cie_start - 4) as u32;
    data[cie_start..cie_start + 4].copy_from_slice(&length.to_le_bytes());

    for (index, function) in functions.iter().enumerate() {
        let fde_start = data.len();
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&(cie_start as u32).to_le_bytes());
        code_relocs.push((data.len() as u32, index));
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&function.code_size.to_le_bytes());

        let mut at = 0u32;
        let mut stated = 0u32;
        let mut rules_emitted = false;
        for &(offset, cfa) in &function.transitions {
            if offset >= function.code_size || (cfa == stated && offset > 0) {
                continue;
            }
            if offset > at {
                advance_loc(&mut data, offset - at);
                at = offset;
            }
            if cfa != stated {
                data.push(DW_CFA_DEF_CFA_OFFSET);
                uleb128(&mut data, u64::from(cfa));
                stated = cfa;
            }
            if !rules_emitted && cfa > 0 {
                let saved_bytes = 4 * function.saved.len() as i64;
                for (slot, register) in function.saved.iter().enumerate() {
                    let offset = -saved_bytes + 4 * slot as i64;
                    data.push(DW_CFA_OFFSET | (register & 0x3f));
                    uleb128(&mut data, (offset / DATA_ALIGNMENT) as u64);
                }
                rules_emitted = true;
            }
        }
        pad_to_word(&mut data);
        let length = (data.len() - fde_start - 4) as u32;
        data[fde_start..fde_start + 4].copy_from_slice(&length.to_le_bytes());
    }

    GeneratedSection {
        name: ".debug_frame",
        data,
        code_relocs,
        section_relocs: Vec::new(),
    }
}

/// Appends an advance of `bytes` in the smallest encoding that holds it.
///
/// The operand is in code-alignment units, so an odd byte count cannot be expressed -- and cannot
/// arise, because every instruction on this target occupies a whole number of halfwords. One that
/// somehow was odd rounds DOWN here, which places the row early: the prologue's rules cover an
/// instruction that has not run yet, rather than the entry row covering one that has.
fn advance_loc(out: &mut Vec<u8>, bytes: u32) {
    let units = u64::from(bytes) / CODE_ALIGNMENT;
    if units <= 0x3f {
        out.push(DW_CFA_ADVANCE_LOC | units as u8);
    } else if units <= 0xff {
        out.push(DW_CFA_ADVANCE_LOC1);
        out.push(units as u8);
    } else if units <= 0xffff {
        out.push(DW_CFA_ADVANCE_LOC2);
        out.extend_from_slice(&(units as u16).to_le_bytes());
    } else {
        out.push(DW_CFA_ADVANCE_LOC4);
        out.extend_from_slice(&(units as u32).to_le_bytes());
    }
}

/// Pads an entry to a multiple of the address size with `DW_CFA_nop`, which is zero.
///
/// DWARF 5 section 6.4.1 requires it. A consumer reading the length works either way, but the
/// consumers that matter align their own walk, so an unpadded entry puts the NEXT one at an offset
/// they do not look at.
fn pad_to_word(out: &mut Vec<u8>) {
    while out.len() % 4 != 0 {
        out.push(0);
    }
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
        let generated = line_program("a.cs", &[FunctionLines {
            name: "f",
            file: "a.cs",
            rows: &[],
            code_size: 8,
            entry: None,
            locals: &[],
            locations: &[],
            params: &[],
            param_locations: &[],
        }]);
        assert!(generated.data.is_empty());
        assert!(generated.code_relocs.is_empty());
    }

    #[test]
    fn the_header_declares_dwarf_5_and_a_consistent_unit_length() {
        let rows = [row(0, 10, 5)];
        let generated = line_program("prog.cs", &[FunctionLines {
            name: "f",
            file: "prog.cs",
            rows: &rows,
            code_size: 4,
            entry: None,
            locals: &[],
            locations: &[],
            params: &[],
            param_locations: &[],
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
        let generated = line_program("a.cs", &[
            FunctionLines {
                name: "f",
                file: "a.cs",
                rows: &a,
                code_size: 4,
                entry: None,
                locals: &[],
                locations: &[],
                params: &[],
                param_locations: &[],
            },
            FunctionLines {
                name: "f",
                file: "b.cs",
                rows: &b,
                code_size: 4,
                entry: None,
                locals: &[],
                locations: &[],
                params: &[],
                param_locations: &[],
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
        let generated = line_program("a.cs", &[
            FunctionLines {
                name: "f",
                file: "a.cs",
                rows: &[],
                code_size: 4,
                entry: None,
                locals: &[],
                locations: &[],
                params: &[],
                param_locations: &[],
            },
            FunctionLines {
                name: "f",
                file: "b.cs",
                rows: &b,
                code_size: 4,
                entry: None,
                locals: &[],
                locations: &[],
                params: &[],
                param_locations: &[],
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
        let generated = line_program("one.cs", &[
            FunctionLines {
                name: "f",
                file: "one.cs",
                rows: &a,
                code_size: 4,
                entry: None,
                locals: &[],
                locations: &[],
                params: &[],
                param_locations: &[],
            },
            FunctionLines {
                name: "f",
                file: "two.cs",
                rows: &b,
                code_size: 4,
                entry: None,
                locals: &[],
                locations: &[],
                params: &[],
                param_locations: &[],
            },
            FunctionLines {
                name: "f",
                file: "one.cs",
                rows: &c,
                code_size: 4,
                entry: None,
                locals: &[],
                locations: &[],
                params: &[],
                param_locations: &[],
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
        let generated = line_program("a.cs", &[FunctionLines {
            name: "f",
            file: "a.cs",
            rows: &rows,
            code_size: 4,
            entry: None,
            locals: &[],
            locations: &[],
            params: &[],
            param_locations: &[],
        }]);
        assert!(
            generated.data.contains(&DW_LNS_ADVANCE_LINE),
            "a 9-line jump needs an explicit advance_line"
        );

        let rows = [row(0, 1, 0), row(2, 9, 0)];
        let tight = line_program("a.cs", &[FunctionLines {
            name: "f",
            file: "a.cs",
            rows: &rows,
            code_size: 4,
            entry: None,
            locals: &[],
            locations: &[],
            params: &[],
            param_locations: &[],
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
        let generated = line_program("a.cs", &[FunctionLines {
            name: "f",
            file: "a.cs",
            rows: &rows,
            code_size: 404,
            entry: None,
            locals: &[],
            locations: &[],
            params: &[],
            param_locations: &[],
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
        let generated = line_program("a.cs", &[FunctionLines {
            name: "f",
            file: "a.cs",
            rows: &rows,
            code_size: 12,
            entry: None,
            locals: &[],
            locations: &[],
            params: &[],
            param_locations: &[],
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
        let generated = line_program("a.cs", &[FunctionLines {
            name: "f",
            file: "a.cs",
            rows: &rows,
            code_size: 8,
            entry: None,
            locals: &[],
            locations: &[],
            params: &[],
            param_locations: &[],
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
        let generated = line_program("a.cs", &[FunctionLines {
            name: "f",
            file: "a.cs",
            rows: &rows,
            code_size: 4,
            entry: None,
            locals: &[],
            locations: &[],
            params: &[],
            param_locations: &[],
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
            entry: None,
            locals: &[],
            locations: &[],
            params: &[],
            param_locations: &[],
        }
    }

    fn local(slot: u16, name: &str, ty: Option<crate::debugmap::BaseType>) -> crate::debugmap::LocalSlot {
        crate::debugmap::LocalSlot {
            slot,
            name: String::from(name),
            ty,
        }
    }

    #[test]
    fn a_local_with_two_homes_becomes_a_location_list_the_variable_points_at() {
        let rows = [row(0, 3, 0)];
        let int = crate::debugmap::base_type_of(&lamella_metadata::SigType::I4).unwrap();
        let locals = [local(0, "n", Some(int))];
        let locations = [LocalLocation {
            slot: 0,
            ranges: alloc::vec![
                LocalRange { start: 4, end: 0x20, frame_offset: 8 },
                LocalRange { start: 0x20, end: 0x40, frame_offset: 72 },
            ],
        }];
        let mut f = func("f", "p.cs", &rows, 0x40);
        f.locals = &locals;
        f.locations = &locations;
        let (info, _, loclists) = compilation_unit("p.cs", "t", None, &[f]);

        assert_eq!(&loclists.data[4..12], &[5, 0, 4, 0, 0, 0, 0, 0]);

        let list = &loclists.data[12..];
        assert_eq!(list[0], 0x08, "first entry is DW_LLE_start_length");
        assert_eq!(&list[1..5], &4u32.to_le_bytes(), "the range start is left in the field");
        assert_eq!(list[5], 0x1c, "length: 0x20 - 4");
        assert_eq!(list[6], 2, "a two-byte expression");
        assert_eq!(&list[7..9], &[0x7d, 8], "DW_OP_breg13, +8");
        assert_eq!(list[9], 0x08, "second entry");
        assert_eq!(&list[10..14], &0x20u32.to_le_bytes());
        assert_eq!(list[14], 0x20, "length: 0x40 - 0x20");
        assert_eq!(list[15], 3, "a three-byte expression: 72 needs two SLEB128 bytes");
        assert_eq!(&list[16..19], &[0x7d, 0xc8, 0x00], "DW_OP_breg13, +72");
        assert_eq!(list[19], 0x00, "DW_LLE_end_of_list");

        let (site, _) = loclists.code_relocs[0];
        assert_eq!(site_addend(&loclists, site), 4);
        let (site, _) = loclists.code_relocs[1];
        assert_eq!(site_addend(&loclists, site), 0x20);

        assert!(
            info.section_relocs.iter().any(|&(_, target)| target == ".debug_loclists"),
            "a located variable must reference the list section by relocation"
        );
    }

    #[test]
    fn a_local_with_nothing_to_say_about_its_home_carries_no_location() {
        let rows = [row(0, 3, 0)];
        let int = crate::debugmap::base_type_of(&lamella_metadata::SigType::I4).unwrap();
        let locals = [local(0, "n", Some(int))];
        let locations = [LocalLocation { slot: 0, ranges: Vec::new() }];
        let mut f = func("f", "p.cs", &rows, 0x40);
        f.locals = &locals;
        f.locations = &locations;
        let (info, _, loclists) = compilation_unit("p.cs", "t", None, &[f]);
        assert!(loclists.data.is_empty(), "no lists means no section, not an empty unit");
        assert!(
            !info.section_relocs.iter().any(|&(_, target)| target == ".debug_loclists"),
            "a variable with no home must not reference a list that does not exist"
        );
    }

    #[test]
    fn a_location_joins_to_its_local_by_slot_and_not_by_position() {
        let rows = [row(0, 3, 0)];
        let int = crate::debugmap::base_type_of(&lamella_metadata::SigType::I4).unwrap();
        let locals = [local(0, "first", Some(int)), local(2, "second", Some(int))];
        let locations = [LocalLocation {
            slot: 2,
            ranges: alloc::vec![LocalRange { start: 0, end: 0x10, frame_offset: 4 }],
        }];
        let mut f = func("f", "p.cs", &rows, 0x40);
        f.locals = &locals;
        f.locations = &locations;
        let (info, _, _) = compilation_unit("p.cs", "t", None, &[f]);
        let at = |needle: &str| {
            info.data
                .windows(needle.len() + 1)
                .position(|w| &w[..needle.len()] == needle.as_bytes() && w[needle.len()] == 0)
                .expect("the name is in the unit")
        };
        assert_eq!(info.data[at("first") - 1], 4, "`first` has no home, so no location");
        assert_eq!(info.data[at("second") - 1], 6, "`second` has one, and it is its own");
    }

    #[test]
    fn a_locals_type_is_emitted_once_however_many_locals_share_it() {
        let rows = [row(0, 3, 0)];
        let int = crate::debugmap::base_type_of(&lamella_metadata::SigType::I4).unwrap();
        let boolean = crate::debugmap::base_type_of(&lamella_metadata::SigType::Boolean).unwrap();
        let locals = [
            local(0, "a", Some(int)),
            local(1, "b", Some(int)),
            local(2, "c", Some(boolean)),
        ];
        let mut f = func("f", "p.cs", &rows, 4);
        f.locals = &locals;
        let (info, ..) = compilation_unit("p.cs", "t", None, &[f]);
        let base_type_abbrevs = info
            .data
            .windows(2)
            .filter(|w| w[0] == 3 && w[1] == b'i')
            .count();
        assert_eq!(
            base_type_abbrevs, 1,
            "two `int` locals must share one DW_TAG_base_type entry, not get one each"
        );
        let has = |needle: &[u8]| {
            info.data
                .windows(needle.len())
                .any(|w| w == needle)
        };
        assert!(has(b"int\0"), "the int base type is named");
        assert!(has(b"bool\0"), "the bool base type is named");
        assert!(has(b"a\0") && has(b"b\0") && has(b"c\0"), "every local is named");
    }

    #[test]
    fn a_locals_type_reference_points_at_a_base_type_inside_this_unit() {
        let rows = [row(0, 3, 0)];
        let wide = crate::debugmap::base_type_of(&lamella_metadata::SigType::I8).unwrap();
        let locals = [local(0, "w", Some(wide))];
        let mut f = func("f", "p.cs", &rows, 4);
        f.locals = &locals;
        let (info, ..) = compilation_unit("p.cs", "t", None, &[f]);
        let at = info
            .data
            .windows(2)
            .position(|w| w == b"w\0")
            .expect("the local is named");
        let start = at + 2;
        let target = u32::from_le_bytes([
            info.data[start],
            info.data[start + 1],
            info.data[start + 2],
            info.data[start + 3],
        ]) as usize;
        assert_eq!(info.data[target], 3, "the reference lands on a base type abbreviation");
        assert_eq!(&info.data[target + 1..target + 6], b"long\0");
        assert_eq!(info.data[target + 6], 0x05, "DW_ATE_signed, as a literal");
        assert_eq!(info.data[target + 7], 8, "eight bytes wide");
    }

    #[test]
    fn a_local_whose_type_this_backend_cannot_name_keeps_its_name_and_gets_no_type() {
        let rows = [row(0, 3, 0)];
        assert!(crate::debugmap::base_type_of(&lamella_metadata::SigType::Object).is_none());
        let locals = [local(0, "thing", None)];
        let mut f = func("f", "p.cs", &rows, 4);
        f.locals = &locals;
        let (info, ..) = compilation_unit("p.cs", "t", None, &[f]);
        let at = info
            .data
            .windows(6)
            .position(|w| w == b"thing\0")
            .expect("the local is still named");
        assert_eq!(info.data[at + 6], 0, "no DW_AT_type follows an untyped local");
    }

    #[test]
    fn the_unit_header_is_the_dwarf_5_field_order() {
        let rows = [row(0, 3, 0)];
        let (info, abbrev, _) = compilation_unit("p.cs", "test", None, &[func("f", "p.cs", &rows, 4)]);
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
        let (info, ..) = compilation_unit(
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
        let (info, ..) = compilation_unit("p.cs", "test", None, &[func("f", "p.cs", &rows, 4)]);
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
        let line = line_program("one.cs", &functions);
        let (info, ..) = compilation_unit("one.cs", "t", None, &functions);
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
        let (info, abbrev, _) = compilation_unit("p.cs", "t", None, &[]);
        assert!(info.data.is_empty());
        assert!(abbrev.data.is_empty());
    }

    #[test]
    fn a_path_splits_at_its_last_separator_of_either_kind() {
        assert_eq!(split_path("Q:\\src\\hello.cs"), ("Q:\\src", "hello.cs"));
        assert_eq!(split_path("/opt/build/hello.cs"), ("/opt/build", "hello.cs"));
        assert_eq!(split_path("src/hello.cs"), ("src", "hello.cs"));
        assert_eq!(split_path("/hello.cs"), ("/", "hello.cs"));
        assert_eq!(split_path("hello.cs"), ("", "hello.cs"));
        assert_eq!(directory_of("hello.cs"), ".");
        assert_eq!(split_path("Q:\\src/hello.cs"), ("Q:\\src", "hello.cs"));
    }

    #[test]
    fn a_file_entry_is_a_bare_name_and_a_directory_index_not_a_whole_path() {
        let rows = [row(0, 3, 0)];
        let generated = line_program(
            "Q:\\w\\hello.cs",
            &[func("Program.Main", "Q:\\w\\hello.cs", &rows, 8)],
        );
        let has = |needle: &[u8]| generated.data.windows(needle.len()).any(|w| w == needle);
        assert!(has(b"Q:\\w\0"), "the directory is a directory-table entry");
        assert!(has(b"hello.cs\0"), "the file entry is the bare name");
        assert!(
            !has(b"Q:\\w\\hello.cs\0"),
            "no entry may carry the whole path -- that is the defect, and it parses clean"
        );
    }

    #[test]
    fn the_units_comp_dir_is_the_line_tables_directory_zero() {
        let rows = [row(0, 3, 0)];
        let functions = [func("Program.Main", "Q:\\w\\hello.cs", &rows, 8)];
        let (info, ..) = compilation_unit("Q:\\w\\hello.cs", "t", None, &functions);
        let line = line_program("Q:\\w\\hello.cs", &functions);
        let has = |data: &[u8], needle: &[u8]| data.windows(needle.len()).any(|w| w == needle);
        assert!(
            has(&info.data, b"hello.cs\0Q:\\w\0"),
            "DW_AT_name is the bare name and DW_AT_comp_dir the directory, in that order"
        );
        assert!(
            !has(&info.data, b"hello.cs\0.\0"),
            "a literal \".\" comp_dir beside an absolute name is what made the join unusable"
        );
        assert!(
            has(&line.data, b"Q:\\w\0"),
            "and the line table declares the identical directory"
        );
    }

    #[test]
    fn two_directories_are_two_entries_and_a_shared_basename_stays_two_files() {
        let a = [row(0, 1, 0)];
        let b = [row(0, 1, 0)];
        let functions = [
            func("A.Main", "Q:\\one\\Program.cs", &a, 4),
            func("B.Main", "Q:\\two\\Program.cs", &b, 4),
        ];
        let files = file_table("Q:\\one\\Program.cs", &functions);
        assert_eq!(
            files,
            [
                "Q:\\one\\Program.cs",
                "Q:\\one\\Program.cs",
                "Q:\\two\\Program.cs"
            ],
            "entry 0 is the unit's own file and the distinct list still starts at 1"
        );
        let directories = directory_table(&files);
        assert_eq!(directories, ["Q:\\one", "Q:\\two"], "the primary's directory heads it");
        assert_eq!(directory_index(&directories, "Q:\\one\\Program.cs"), 0);
        assert_eq!(directory_index(&directories, "Q:\\two\\Program.cs"), 1);
        assert_eq!(file_index(&files, "Q:\\one\\Program.cs"), 1);
        assert_eq!(file_index(&files, "Q:\\two\\Program.cs"), 2);
    }

    #[test]
    fn a_unit_naming_a_file_no_function_lowered_from_still_heads_the_table() {
        let a = [row(0, 1, 0)];
        let functions = [func("Helper.Run", "helper.cs", &a, 4)];
        let files = file_table("main.cs", &functions);
        assert_eq!(files, ["main.cs", "helper.cs"]);
        assert_eq!(file_index(&files, "helper.cs"), 1, "still index 1");
    }

    /// The bytes of one function's line-number PROGRAM, with the sequence preamble skipped: a
    /// program is a variable-length instruction stream, so a raw scan cannot tell an opcode from an
    /// operand, and every assertion below is on an exact expected sequence rather than a search.
    fn program_of(generated: &GeneratedSection) -> Vec<u8> {
        let header_length = u32::from_le_bytes([
            generated.data[8],
            generated.data[9],
            generated.data[10],
            generated.data[11],
        ]) as usize;
        const SET_ADDRESS_BYTES: usize = 7;
        const SET_FILE_BYTES: usize = 2;
        generated.data[12 + header_length + SET_ADDRESS_BYTES + SET_FILE_BYTES..].to_vec()
    }

    /// The one-byte special opcode for `(line delta, address advance)`, computed the way a READER
    /// does (DWARF 5 section 6.2.5.1) rather than copied out of a run of the encoder.
    fn special(line_inc: i64, advance: u32) -> u8 {
        u8::try_from((line_inc - LINE_BASE) + LINE_RANGE * i64::from(advance) + OPCODE_BASE)
            .expect("the test's own rows all encode in one byte")
    }

    #[test]
    fn a_prologue_row_covers_the_bytes_before_the_first_described_instruction() {
        let rows = [row(8, 3, 0)];
        let generated = line_program(
            "p.cs",
            &[FunctionLines {
                name: "Program.Main",
                file: "p.cs",
                rows: &rows,
                code_size: 16,
                entry: Some((2, 0)),
                locals: &[],
                locations: &[],
                params: &[],
                param_locations: &[],
            }],
        );
        assert_eq!(
            program_of(&generated),
            vec![
                special(1, 0),
                DW_LNS_SET_PROLOGUE_END,
                special(1, 8),
                DW_LNS_ADVANCE_PC,
                8,
                0,
                1,
                DW_LNE_END_SEQUENCE,
            ],
            "a prologue row at offset 0, then a prologue_end-marked first row"
        );
    }

    #[test]
    fn without_an_opening_position_there_is_no_prologue_row_but_still_a_prologue_end() {
        let rows = [row(8, 3, 0)];
        let generated = line_program(
            "p.cs",
            &[FunctionLines {
                name: "Program.Main",
                file: "p.cs",
                rows: &rows,
                code_size: 16,
                entry: None,
                locals: &[],
                locations: &[],
                params: &[],
                param_locations: &[],
            }],
        );
        assert_eq!(
            program_of(&generated),
            vec![
                DW_LNS_SET_PROLOGUE_END,
                special(2, 8),
                DW_LNS_ADVANCE_PC,
                8,
                0,
                1,
                DW_LNE_END_SEQUENCE,
            ],
        );
    }

    #[test]
    fn a_function_whose_first_row_is_already_at_its_entry_gains_no_row() {
        let rows = [row(0, 3, 0)];
        let generated = line_program(
            "p.cs",
            &[FunctionLines {
                name: "Program.Main",
                file: "p.cs",
                rows: &rows,
                code_size: 16,
                entry: Some((2, 0)),
                locals: &[],
                locations: &[],
                params: &[],
                param_locations: &[],
            }],
        );
        assert_eq!(
            program_of(&generated),
            vec![
                DW_LNS_SET_PROLOGUE_END,
                special(2, 0),
                DW_LNS_ADVANCE_PC,
                16,
                0,
                1,
                DW_LNE_END_SEQUENCE,
            ],
        );
    }

    #[test]
    fn the_prologue_rows_column_is_set_when_the_opening_has_one() {
        let rows = [row(8, 3, 9)];
        let generated = line_program(
            "p.cs",
            &[FunctionLines {
                name: "Program.Main",
                file: "p.cs",
                rows: &rows,
                code_size: 16,
                entry: Some((2, 24)),
                locals: &[],
                locations: &[],
                params: &[],
                param_locations: &[],
            }],
        );
        assert_eq!(
            program_of(&generated),
            vec![
                DW_LNS_SET_COLUMN,
                24,
                special(1, 0),
                DW_LNS_SET_COLUMN,
                9,
                DW_LNS_SET_PROLOGUE_END,
                special(1, 8),
                DW_LNS_ADVANCE_PC,
                8,
                0,
                1,
                DW_LNE_END_SEQUENCE,
            ],
        );
    }

    #[test]
    fn bare_file_names_emit_the_single_dot_directory_they_always_did() {
        let rows = [row(0, 3, 0)];
        let functions = [func("f", "p.cs", &rows, 4)];
        let files = file_table("p.cs", &functions);
        assert_eq!(directory_table(&files), ["."]);
        let (info, ..) = compilation_unit("p.cs", "t", None, &functions);
        let has = |needle: &[u8]| info.data.windows(needle.len()).any(|w| w == needle);
        assert!(has(b"p.cs\0.\0"), "DW_AT_name, then the \".\" DW_AT_comp_dir");
    }


    /// Reads a section back and answers what the table says at one address.
    fn unwind_at(section: &[u8], address: u64) -> lamella_dwarf::UnwindRow<'_> {
        let sections = lamella_dwarf::Sections {
            debug_frame: section,
            ..lamella_dwarf::Sections::default()
        };
        lamella_dwarf::FrameTable::parse(&sections)
            .expect("the emitted section parses")
            .unwind(address)
            .expect("no error")
            .expect("a row covers the address")
    }

    /// A function's frame description is relocated, so in an unlinked section every entry begins at
    /// zero. Tests place one function at a time for that reason.
    fn one(frame: FunctionFrame) -> Vec<u8> {
        frame_section(&[frame]).data
    }

    #[test]
    fn a_function_that_never_moves_the_stack_still_gets_an_entry() {
        let section = one(FunctionFrame {
            code_size: 8,
            transitions: vec![(0, 0), (0, 0)],
            saved: Vec::new(),
        });
        let row = unwind_at(&section, 0);
        assert_eq!(
            row.cfa,
            lamella_dwarf::CfaRule::RegisterOffset {
                register: 13,
                offset: 0
            },
            "the caller's stack pointer is this one"
        );
        assert_eq!(
            row.return_address(),
            lamella_dwarf::RegisterRule::Undefined,
            "no rule means the return address is where the convention leaves it, which is the \
             register it arrived in"
        );
        assert_eq!(row.function, (0, 8));
    }

    #[test]
    fn the_saved_registers_land_in_the_slots_the_push_created() {
        let section = one(FunctionFrame {
            code_size: 0x40,
            transitions: vec![(0, 0), (2, 12)],
            saved: vec![4, 5, 14],
        });
        let row = unwind_at(&section, 2);
        assert_eq!(
            row.cfa,
            lamella_dwarf::CfaRule::RegisterOffset {
                register: 13,
                offset: 12
            }
        );
        assert_eq!(row.register(4), lamella_dwarf::RegisterRule::Offset(-12));
        assert_eq!(row.register(5), lamella_dwarf::RegisterRule::Offset(-8));
        assert_eq!(row.return_address(), lamella_dwarf::RegisterRule::Offset(-4));
    }

    #[test]
    fn the_rules_do_not_apply_before_the_prologue_has_run() {
        let section = one(FunctionFrame {
            code_size: 0x40,
            transitions: vec![(0, 0), (4, 12)],
            saved: vec![4, 5, 14],
        });
        let entry = unwind_at(&section, 0);
        assert_eq!(
            entry.cfa,
            lamella_dwarf::CfaRule::RegisterOffset {
                register: 13,
                offset: 0
            },
            "the entry row, before the prologue"
        );
        assert_eq!(entry.return_address(), lamella_dwarf::RegisterRule::Undefined);
        let after = unwind_at(&section, 4);
        assert_eq!(
            after.cfa,
            lamella_dwarf::CfaRule::RegisterOffset {
                register: 13,
                offset: 12
            },
            "and the built frame, after it"
        );
    }

    #[test]
    fn a_frame_too_large_for_one_subtract_is_described_as_one_frame() {
        let section = one(FunctionFrame {
            code_size: 0x600,
            transitions: vec![(0, 0), (8, 1144+8)],
            saved: vec![7, 14],
        });
        let row = unwind_at(&section, 8);
        assert_eq!(
            row.cfa,
            lamella_dwarf::CfaRule::RegisterOffset {
                register: 13,
                offset: 1152
            }
        );
        assert_eq!(row.register(7), lamella_dwarf::RegisterRule::Offset(-8));
        assert_eq!(row.return_address(), lamella_dwarf::RegisterRule::Offset(-4));
    }

    #[test]
    fn a_prologue_past_the_one_byte_advance_reach_still_lands_on_its_own_address() {
        for prologue_end in [126u32, 128, 512, 0x1_0000] {
            let section = one(FunctionFrame {
                code_size: 0x2_0000,
                transitions: vec![(0, 0), (prologue_end, 8)],
                saved: vec![14],
            });
            let before = unwind_at(&section, u64::from(prologue_end) - 2);
            assert_eq!(
                before.cfa,
                lamella_dwarf::CfaRule::RegisterOffset {
                    register: 13,
                    offset: 0
                },
                "the entry row still applies one instruction earlier, at {prologue_end}"
            );
            let after = unwind_at(&section, u64::from(prologue_end));
            assert_eq!(
                after.cfa,
                lamella_dwarf::CfaRule::RegisterOffset {
                    register: 13,
                    offset: 8
                },
                "and the built frame begins exactly at {prologue_end}"
            );
        }
    }

    #[test]
    fn every_function_gets_its_own_description_and_its_own_relocation() {
        let section = frame_section(&[
            FunctionFrame {
                code_size: 0x10,
                transitions: vec![(0, 0), (2, 8)],
                saved: vec![14],
            },
            FunctionFrame {
                code_size: 0x20,
                transitions: vec![(0, 0), (2, 12)],
                saved: vec![4, 14],
            },
        ]);
        assert_eq!(
            section.code_relocs.iter().map(|&(_, i)| i).collect::<Vec<_>>(),
            vec![0, 1],
            "one code relocation per function, naming that function"
        );
        assert!(
            section.section_relocs.is_empty(),
            "a frame description points at its common entry by section offset, which needs no \
             relocation because both are in this section"
        );
        let sections = lamella_dwarf::Sections {
            debug_frame: &section.data,
            ..lamella_dwarf::Sections::default()
        };
        let table = lamella_dwarf::FrameTable::parse(&sections).expect("parses");
        assert_eq!(table.len(), 2);
        assert_eq!(table.cies().len(), 1, "and they share one common entry");
    }

    #[test]
    fn an_empty_list_emits_no_section_rather_than_a_common_entry_describing_nothing() {
        let section = frame_section(&[]);
        assert!(section.data.is_empty());
        assert!(section.code_relocs.is_empty());
    }

    #[test]
    fn every_entry_is_padded_to_the_address_size() {
        let frames: Vec<FunctionFrame> = (0..3)
            .map(|i| FunctionFrame {
                code_size: 0x10,
                transitions: vec![(0, 0), (2, 8)],
                saved: if i == 1 { vec![4, 14] } else { vec![14] },
            })
            .collect();
        let section = frame_section(&frames);
        assert_eq!(section.data.len() % 4, 0);
        let sections = lamella_dwarf::Sections {
            debug_frame: &section.data,
            ..lamella_dwarf::Sections::default()
        };
        let table = lamella_dwarf::FrameTable::parse(&sections).expect("parses");
        assert_eq!(table.len(), 3, "all three entries are found, so none was skipped");
    }

    #[test]
    fn the_common_rules_encode_in_the_bytes_the_format_reserves_for_them() {
        let section = one(FunctionFrame {
            code_size: 0x40,
            transitions: vec![(0, 0), (2, 12)],
            saved: vec![4, 5, 14],
        });
        let sections = lamella_dwarf::Sections {
            debug_frame: &section,
            ..lamella_dwarf::Sections::default()
        };
        let table = lamella_dwarf::FrameTable::parse(&sections).expect("parses");
        let common = table.cies()[0];
        assert_eq!(
            common.data_alignment, -4,
            "the sign belongs to the alignment, so the offsets can stay one byte each"
        );
        assert_eq!(common.code_alignment, 2, "Thumb instructions are halfwords");
        assert_eq!(common.return_address_register, 14);
        assert_eq!(
            section.len(),
            20 + 28,
            "the common entry, then one description whose rules fit in nine bytes"
        );
    }

    #[test]
    fn an_epilogue_that_gave_the_stack_back_is_described_as_having_done_so() {
        let section = one(FunctionFrame {
            code_size: 0x2a,
            transitions: vec![(0, 0), (2, 4), (4, 24), (0x26, 4)],
            saved: vec![14],
        });
        let body = unwind_at(&section, 0x10);
        assert_eq!(
            body.cfa,
            lamella_dwarf::CfaRule::RegisterOffset {
                register: 13,
                offset: 24
            },
            "the whole frame, through the body"
        );
        assert_eq!(body.return_address(), lamella_dwarf::RegisterRule::Offset(-4));

        let epilogue = unwind_at(&section, 0x26);
        assert_eq!(
            epilogue.cfa,
            lamella_dwarf::CfaRule::RegisterOffset {
                register: 13,
                offset: 4
            },
            "and four bytes once the reservation has been given back"
        );
        assert_eq!(
            epilogue.return_address(),
            lamella_dwarf::RegisterRule::Offset(-4),
            "the saved register's rule is relative to the FRAME address, so it does not move when \
             the stack pointer does and is stated once"
        );
    }

    #[test]
    fn a_second_path_through_a_function_starts_from_the_frame_the_prologue_built() {
        let section = one(FunctionFrame {
            code_size: 0x40,
            transitions: vec![(0, 0), (2, 24), (0x10, 4), (0x12, 24), (0x30, 4)],
            saved: vec![14],
        });
        for (address, want) in [(0x08, 24), (0x10, 4), (0x12, 24), (0x20, 24), (0x30, 4)] {
            let row = unwind_at(&section, address);
            assert_eq!(
                row.cfa,
                lamella_dwarf::CfaRule::RegisterOffset {
                    register: 13,
                    offset: want
                },
                "at 0x{address:02x}"
            );
        }
    }

    #[test]
    fn a_transition_that_changes_nothing_costs_no_bytes() {
        let dense = one(FunctionFrame {
            code_size: 0x40,
            transitions: vec![(0, 0), (2, 4), (2, 12), (2, 12), (2, 12)],
            saved: vec![4, 5, 14],
        });
        let sparse = one(FunctionFrame {
            code_size: 0x40,
            transitions: vec![(0, 0), (2, 12)],
            saved: vec![4, 5, 14],
        });
        assert_eq!(dense.len(), sparse.len(), "a repeated frame states nothing new");
    }

    #[test]
    fn every_subprogram_carries_a_frame_base_and_it_is_the_stack_pointer() {
        let (info, abbrev, _) = compilation_unit(
            "a.cs",
            "lamella",
            Some(0x40),
            &[FunctionLines {
                name: "M",
                file: "a.cs",
                rows: &[row(0, 7, 1)],
                code_size: 0x20,
                entry: None,
                locals: &[],
                locations: &[],
                params: &[],
                param_locations: &[],
            }],
        );

        let mut want = Vec::new();
        uleb128(&mut want, DW_AT_FRAME_BASE);
        uleb128(&mut want, DW_FORM_EXPRLOC);
        assert!(
            abbrev.data.windows(want.len()).any(|w| w == want.as_slice()),
            "the abbreviation declares DW_AT_frame_base as an exprloc"
        );

        assert!(
            info.data.windows(2).any(|w| w == [1, 0x5d]),
            "the subprogram entry carries a one-operator frame base naming the stack pointer"
        );
    }

    #[test]
    fn the_frame_base_is_one_two_byte_block_per_subprogram_and_not_one_per_unit() {
        let rows = [row(0, 7, 1)];
        for count in [1usize, 3] {
            let funcs: Vec<FunctionLines> = ["M", "N", "O"][..count]
                .iter()
                .map(|name| FunctionLines {
                    name,
                    file: "a.cs",
                    rows: &rows,
                    code_size: 0x20,
                    entry: None,
                    locals: &[],
                locations: &[],
                params: &[],
                param_locations: &[],
                })
                .collect();
            let (info, ..) = compilation_unit("a.cs", "lamella", None, &funcs);
            let blocks = info.data.windows(2).filter(|w| *w == [1, 0x5d]).count();
            assert_eq!(
                blocks, count,
                "one frame base per subprogram, not one per unit and not one per attribute"
            );
        }
    }
}
