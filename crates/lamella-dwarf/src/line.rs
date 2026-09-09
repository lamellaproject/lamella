//! The `.debug_line` line-number program: its header and its state machine.

use alloc::string::String;
use alloc::vec::Vec;

use crate::cursor::{Cursor, Format};
use crate::form;
use crate::{DwarfError, Sections};

/// Standard opcodes (DWARF 5 section 6.2.5.2).
const DW_LNS_COPY: u8 = 0x01;
const DW_LNS_ADVANCE_PC: u8 = 0x02;
const DW_LNS_ADVANCE_LINE: u8 = 0x03;
const DW_LNS_SET_FILE: u8 = 0x04;
const DW_LNS_SET_COLUMN: u8 = 0x05;
const DW_LNS_NEGATE_STMT: u8 = 0x06;
const DW_LNS_SET_BASIC_BLOCK: u8 = 0x07;
const DW_LNS_CONST_ADD_PC: u8 = 0x08;
const DW_LNS_FIXED_ADVANCE_PC: u8 = 0x09;
const DW_LNS_SET_PROLOGUE_END: u8 = 0x0a;
const DW_LNS_SET_EPILOGUE_BEGIN: u8 = 0x0b;
const DW_LNS_SET_ISA: u8 = 0x0c;

/// Extended opcodes (DWARF 5 section 6.2.5.3).
const DW_LNE_END_SEQUENCE: u8 = 0x01;
const DW_LNE_SET_ADDRESS: u8 = 0x02;
const DW_LNE_DEFINE_FILE: u8 = 0x03;
const DW_LNE_SET_DISCRIMINATOR: u8 = 0x04;

/// Line-table entry content type codes (DWARF 5 section 6.2.4.1, table 7.27).
const DW_LNCT_PATH: u64 = 0x1;
const DW_LNCT_DIRECTORY_INDEX: u64 = 0x2;

/// One row of the line-number matrix: a code address and the source position it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    /// The code address this row describes.
    pub address: u64,
    /// Index into the unit's file table. Its numbering follows the unit's version: from zero in
    /// version 5, from one before it.
    pub file: u64,
    /// The source line, or 0 for code that belongs to no line.
    pub line: u32,
    /// The source column, or 0 when the producer recorded none.
    pub column: u32,
    /// Whether this address is a recommended breakpoint location for a statement.
    pub is_stmt: bool,
    /// Whether this address is where a function's prologue ends -- the row a breakpoint on the
    /// FUNCTION belongs at, rather than at its entry.
    pub prologue_end: bool,
    /// Whether this address is where a function's epilogue begins.
    pub epilogue_begin: bool,
    /// Whether this address begins a basic block.
    pub basic_block: bool,
    /// Whether this row marks the address one past the last instruction of a sequence. Such a row
    /// carries no source position and bounds the row before it.
    pub end_sequence: bool,
    /// The instruction set this address is in, for targets with more than one.
    pub isa: u32,
    /// Distinguishes several blocks that a producer mapped to one source position.
    pub discriminator: u64,
}

/// One file named by a unit's file table.
#[derive(Debug, Clone, Copy)]
pub struct FileEntry<'a> {
    /// The file's path as stored, which is a bare name whenever `directory_index` supplies the
    /// rest. Not required to be UTF-8 and not decoded here.
    pub path: &'a [u8],
    /// Index into the unit's directory table.
    pub directory_index: u64,
}

/// One parsed line-number program: its header's tables and the rows its state machine produced.
#[derive(Debug, Clone)]
pub struct LineProgram<'a> {
    /// The unit's offset within `.debug_line`, which is what a compilation unit's
    /// `DW_AT_stmt_list` points at.
    pub offset: usize,
    /// The line table's own version, which is not the version of the rest of the debug info.
    pub version: u16,
    /// The size in bytes of an address on the target this unit describes.
    pub address_size: u8,
    /// Whether the unit uses 4-byte or 8-byte section offsets.
    pub format: Format,
    /// The directory table. In version 5 entry 0 is the compilation directory; before it, entry 0
    /// is absent and index 0 means the compilation directory implicitly.
    pub directories: Vec<&'a [u8]>,
    /// The file table, in the unit's own numbering.
    pub files: Vec<FileEntry<'a>>,
    /// The rows the program produced, in the order the program appended them.
    pub rows: Vec<Row>,
}

impl<'a> LineProgram<'a> {
    /// The file entry a row's `file` register names, resolving the version's numbering.
    #[must_use]
    pub fn file(&self, index: u64) -> Option<&FileEntry<'a>> {
        usize::try_from(index).ok().and_then(|i| self.files.get(i))
    }

    /// The directory a file entry sits in, or `None` when the index names no entry.
    ///
    /// Version 4 and earlier reserve index 0 for the compilation directory, which the table does
    /// not contain, so that index resolves to nothing here and the caller supplies
    /// `DW_AT_comp_dir`.
    #[must_use]
    pub fn directory(&self, file: &FileEntry<'a>) -> Option<&'a [u8]> {
        if self.version < 5 && file.directory_index == 0 {
            return None;
        }
        usize::try_from(file.directory_index)
            .ok()
            .and_then(|i| self.directories.get(i))
            .copied()
    }

    /// The path a row's `file` register names, with its directory joined on.
    ///
    /// Resolving a row's file takes three steps -- the entry, its directory, the join -- and a
    /// consumer that does them by hand is one more place for the rule to drift out of step.
    #[must_use]
    pub fn path(&self, index: u64) -> Option<String> {
        let entry = self.file(index)?;
        if self.version < 5 && index == 0 {
            return None;
        }
        Some(crate::join_path(entry.path, self.directory(entry)))
    }
}

/// The header fields the state machine needs, kept apart from the tables it does not.
struct Header {
    address_size: u8,
    minimum_instruction_length: u8,
    maximum_operations_per_instruction: u8,
    default_is_stmt: bool,
    line_base: i64,
    line_range: u8,
    opcode_base: u8,
    standard_opcode_lengths: Vec<u8>,
}

/// Parses every line-number program in `.debug_line`, in section order.
///
/// A unit that cannot be parsed stops the walk rather than being skipped: the next unit is found by
/// trusting the broken one's `unit_length`, so continuing past a unit that did not parse means
/// reading whatever byte that length happened to point at as a header.
pub fn programs<'a>(sections: &Sections<'a>) -> Result<Vec<LineProgram<'a>>, DwarfError> {
    let mut out = Vec::new();
    let mut cursor = Cursor::new(sections.debug_line);
    while !cursor.is_empty() {
        if cursor.remaining() < 4 {
            break;
        }
        let offset = cursor.offset();
        let (unit_length, format) = cursor.initial_length()?;
        if unit_length == 0 {
            break;
        }
        let length = usize::try_from(unit_length).map_err(|_| DwarfError::Truncated)?;
        let mut unit = cursor.split(length)?;
        out.push(program(&mut unit, offset, format, sections)?);
    }
    Ok(out)
}

/// Parses one line-number program from a cursor already confined to its `unit_length`.
fn program<'a>(
    unit: &mut Cursor<'a>,
    offset: usize,
    format: Format,
    sections: &Sections<'a>,
) -> Result<LineProgram<'a>, DwarfError> {
    let version = unit.u16()?;
    if !(2..=5).contains(&version) {
        return Err(DwarfError::UnsupportedLineVersion(version));
    }

    let (address_size, segment_selector_size) = if version >= 5 {
        (unit.u8()?, unit.u8()?)
    } else {
        (0, 0)
    };
    if segment_selector_size != 0 {
        return Err(DwarfError::SegmentedAddresses);
    }

    let header_length = unit.offset_of(format)?;
    let program_start = unit
        .offset()
        .checked_add(usize::try_from(header_length).map_err(|_| DwarfError::Truncated)?)
        .ok_or(DwarfError::Truncated)?;

    let minimum_instruction_length = unit.u8()?;
    let maximum_operations_per_instruction = if version >= 4 { unit.u8()? } else { 1 };
    let default_is_stmt = unit.u8()? != 0;
    let line_base = i64::from(unit.u8()? as i8);
    let line_range = unit.u8()?;
    let opcode_base = unit.u8()?;

    if line_range == 0 {
        return Err(DwarfError::ZeroLineRange);
    }
    if maximum_operations_per_instruction == 0 {
        return Err(DwarfError::ZeroMaximumOperations);
    }
    if opcode_base == 0 {
        return Err(DwarfError::ZeroOpcodeBase);
    }
    let standard_opcode_lengths = unit.take(usize::from(opcode_base - 1))?.to_vec();

    let (directories, files) = if version >= 5 {
        tables_v5(unit, format, sections)?
    } else {
        tables_v2(unit)?
    };

    let header = Header {
        address_size,
        minimum_instruction_length,
        maximum_operations_per_instruction,
        default_is_stmt,
        line_base,
        line_range,
        opcode_base,
        standard_opcode_lengths,
    };

    let mut body = Cursor::at(unit_bytes(unit)?, program_start)?;
    let (rows, observed_address_size) = run(&mut body, &header)?;

    Ok(LineProgram {
        offset,
        version,
        address_size: observed_address_size,
        format,
        directories,
        files,
        rows,
    })
}

/// The whole unit's bytes, so the program can be re-entered at the offset `header_length` names.
fn unit_bytes<'a>(unit: &Cursor<'a>) -> Result<&'a [u8], DwarfError> {
    Ok(unit.whole())
}

/// Version 5 directory and file tables: a typed entry format, then a count, then the entries.
fn tables_v5<'a>(
    unit: &mut Cursor<'a>,
    format: Format,
    sections: &Sections<'a>,
) -> Result<(Vec<&'a [u8]>, Vec<FileEntry<'a>>), DwarfError> {
    let directory_format = entry_format(unit)?;
    let directory_count = unit.uleb128()?;
    let mut directories = Vec::new();
    for _ in 0..directory_count {
        let entry = read_entry(unit, &directory_format, format, sections)?;
        directories.push(entry.path.unwrap_or(b""));
    }

    let file_format = entry_format(unit)?;
    let file_count = unit.uleb128()?;
    let mut files = Vec::new();
    for _ in 0..file_count {
        let entry = read_entry(unit, &file_format, format, sections)?;
        files.push(FileEntry {
            path: entry.path.unwrap_or(b""),
            directory_index: entry.directory_index,
        });
    }
    Ok((directories, files))
}

/// A `(content type, form)` pair list, as both version 5 tables are described by.
fn entry_format(unit: &mut Cursor<'_>) -> Result<Vec<(u64, u64)>, DwarfError> {
    let count = unit.u8()?;
    let mut out = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let content_type = unit.uleb128()?;
        let form = unit.uleb128()?;
        out.push((content_type, form));
    }
    Ok(out)
}

/// What one version 5 table entry contributed, ignoring the content types nothing here consumes.
struct Entry<'a> {
    path: Option<&'a [u8]>,
    directory_index: u64,
}

fn read_entry<'a>(
    unit: &mut Cursor<'a>,
    entry_format: &[(u64, u64)],
    format: Format,
    sections: &Sections<'a>,
) -> Result<Entry<'a>, DwarfError> {
    let mut entry = Entry {
        path: None,
        directory_index: 0,
    };
    for &(content_type, form) in entry_format {
        let value = form::read(unit, form, format, sections)?;
        match content_type {
            DW_LNCT_PATH => entry.path = value.as_bytes(),
            DW_LNCT_DIRECTORY_INDEX => entry.directory_index = value.as_u64().unwrap_or(0),
            _ => {}
        }
    }
    Ok(entry)
}

/// Version 2 through 4 directory and file tables: two NUL-terminated lists, each ended by an empty
/// string.
///
/// The file table is built with a placeholder at index 0. These versions number files from one and
/// reserve 0 for "the compilation unit's own primary source file", which the table does not
/// contain; keeping the slot makes an index mean the same thing in both shapes, so the state
/// machine and every consumer index one way.
fn tables_v2<'a>(unit: &mut Cursor<'a>) -> Result<(Vec<&'a [u8]>, Vec<FileEntry<'a>>), DwarfError> {
    let mut directories = Vec::new();
    directories.push(&b""[..]);
    loop {
        let name = unit.cstr()?;
        if name.is_empty() {
            break;
        }
        directories.push(name);
    }

    let mut files = Vec::new();
    files.push(FileEntry {
        path: b"",
        directory_index: 0,
    });
    loop {
        let name = unit.cstr()?;
        if name.is_empty() {
            break;
        }
        let directory_index = unit.uleb128()?;
        let _modification_time = unit.uleb128()?;
        let _file_length = unit.uleb128()?;
        files.push(FileEntry {
            path: name,
            directory_index,
        });
    }
    Ok((directories, files))
}

/// The state machine's registers (DWARF 5 table 6.4).
struct State {
    address: u64,
    op_index: u64,
    file: u64,
    line: i64,
    column: u32,
    is_stmt: bool,
    basic_block: bool,
    end_sequence: bool,
    prologue_end: bool,
    epilogue_begin: bool,
    isa: u32,
    discriminator: u64,
}

impl State {
    fn new(header: &Header) -> Self {
        State {
            address: 0,
            op_index: 0,
            file: 1,
            line: 1,
            column: 0,
            is_stmt: header.default_is_stmt,
            basic_block: false,
            end_sequence: false,
            prologue_end: false,
            epilogue_begin: false,
            isa: 0,
            discriminator: 0,
        }
    }

    fn row(&self) -> Row {
        Row {
            address: self.address,
            file: self.file,
            line: u32::try_from(self.line).unwrap_or(0),
            column: self.column,
            is_stmt: self.is_stmt,
            prologue_end: self.prologue_end,
            epilogue_begin: self.epilogue_begin,
            basic_block: self.basic_block,
            end_sequence: self.end_sequence,
            isa: self.isa,
            discriminator: self.discriminator,
        }
    }

    /// The reset every row-appending opcode performs (DWARF 5 section 6.2.5.1). These four
    /// registers describe the row just appended and must not carry into the next one.
    fn after_row(&mut self) {
        self.discriminator = 0;
        self.basic_block = false;
        self.prologue_end = false;
        self.epilogue_begin = false;
    }

    /// Advances `address` and `op_index` by an operation advance.
    ///
    /// The division is the VLIW form from DWARF 5 section 6.2.5.1. With
    /// `maximum_operations_per_instruction` of 1 -- every target this tree targets -- it reduces to
    /// adding `minimum_instruction_length` times the advance, but writing the general form is what
    /// keeps a VLIW producer's table from decoding as addresses that drift.
    fn advance(&mut self, operation_advance: u64, header: &Header) {
        let max_ops = u64::from(header.maximum_operations_per_instruction);
        let total = self.op_index.saturating_add(operation_advance);
        let bytes = u64::from(header.minimum_instruction_length).saturating_mul(total / max_ops);
        self.address = self.address.wrapping_add(bytes);
        self.op_index = total % max_ops;
    }
}

/// Runs one unit's program, appending a row wherever the program says to.
///
/// Returns the rows and the address size the unit turned out to use. Before version 5 that is not
/// in the header at all, so it is only known once a `DW_LNE_set_address` has been reached and its
/// own length has said how wide its operand was.
fn run(body: &mut Cursor<'_>, header: &Header) -> Result<(Vec<Row>, u8), DwarfError> {
    let mut rows = Vec::new();
    let mut state = State::new(header);
    let mut observed_address_size = header.address_size;

    while !body.is_empty() {
        let opcode = body.u8()?;
        if opcode >= header.opcode_base {
            let adjusted = u64::from(opcode - header.opcode_base);
            let line_range = u64::from(header.line_range);
            state.advance(adjusted / line_range, header);
            let line_advance = header.line_base
                + i64::try_from(adjusted % line_range).unwrap_or(0);
            state.line = state.line.saturating_add(line_advance);
            rows.push(state.row());
            state.after_row();
            continue;
        }
        if opcode == 0 {
            let length = body.uleb128()?;
            let length = usize::try_from(length).map_err(|_| DwarfError::Truncated)?;
            if length == 0 {
                continue;
            }
            let mut instruction = body.split(length)?;
            let sub = instruction.u8()?;
            match sub {
                DW_LNE_END_SEQUENCE => {
                    state.end_sequence = true;
                    rows.push(state.row());
                    state = State::new(header);
                }
                DW_LNE_SET_ADDRESS => {
                    let size = if header.address_size != 0 {
                        header.address_size
                    } else {
                        u8::try_from(length - 1).unwrap_or(0)
                    };
                    state.address = instruction.address(size)?;
                    state.op_index = 0;
                    observed_address_size = size;
                }
                DW_LNE_DEFINE_FILE => {
                    let _name = instruction.cstr()?;
                    let _directory_index = instruction.uleb128()?;
                    let _modification_time = instruction.uleb128()?;
                    let _file_length = instruction.uleb128()?;
                }
                DW_LNE_SET_DISCRIMINATOR => state.discriminator = instruction.uleb128()?,
                _ => {}
            }
            continue;
        }
        match opcode {
            DW_LNS_COPY => {
                rows.push(state.row());
                state.after_row();
            }
            DW_LNS_ADVANCE_PC => {
                let advance = body.uleb128()?;
                state.advance(advance, header);
            }
            DW_LNS_ADVANCE_LINE => {
                let advance = body.sleb128()?;
                state.line = state.line.saturating_add(advance);
            }
            DW_LNS_SET_FILE => state.file = body.uleb128()?,
            DW_LNS_SET_COLUMN => {
                state.column = u32::try_from(body.uleb128()?).unwrap_or(u32::MAX);
            }
            DW_LNS_NEGATE_STMT => state.is_stmt = !state.is_stmt,
            DW_LNS_SET_BASIC_BLOCK => state.basic_block = true,
            DW_LNS_CONST_ADD_PC => {
                let adjusted = u64::from(255 - header.opcode_base);
                state.advance(adjusted / u64::from(header.line_range), header);
            }
            DW_LNS_FIXED_ADVANCE_PC => {
                let advance = body.u16()?;
                state.address = state.address.wrapping_add(u64::from(advance));
                state.op_index = 0;
            }
            DW_LNS_SET_PROLOGUE_END => state.prologue_end = true,
            DW_LNS_SET_EPILOGUE_BEGIN => state.epilogue_begin = true,
            DW_LNS_SET_ISA => state.isa = u32::try_from(body.uleb128()?).unwrap_or(0),
            other => {
                let operands = header
                    .standard_opcode_lengths
                    .get(usize::from(other) - 1)
                    .copied()
                    .unwrap_or(0);
                for _ in 0..operands {
                    body.uleb128()?;
                }
            }
        }
    }
    Ok((rows, observed_address_size))
}
