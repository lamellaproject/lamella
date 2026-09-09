//! DWARF debug-info READING: the line-number program and subprogram ranges.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

pub mod cursor;
pub mod form;
pub mod frame;
pub mod info;
pub mod line;
pub mod locals;

#[cfg(test)]
mod tests;

pub use cursor::Format;
pub use frame::{CfaRule, CieSummary, FrameTable, RegisterRule, UnwindRow};
pub use info::Function;
pub use locals::{FrameBase, Local, Locals, Place, Subprogram};
pub use line::{FileEntry, LineProgram, Row};

/// The debug sections a reader needs, borrowed from whatever container held them.
///
/// Every field defaults to empty, so a caller supplies what its container had and a section that is
/// absent resolves to nothing found rather than to a parse failure.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sections<'a> {
    /// `.debug_line` -- the line-number programs.
    pub debug_line: &'a [u8],
    /// `.debug_line_str` -- strings referenced by line tables (`DW_FORM_line_strp`).
    pub debug_line_str: &'a [u8],
    /// `.debug_str` -- strings referenced by debugging information entries.
    pub debug_str: &'a [u8],
    /// `.debug_str_offsets` -- the indirection `DW_FORM_strx` resolves through.
    pub debug_str_offsets: &'a [u8],
    /// `.debug_addr` -- the indirection `DW_FORM_addrx` resolves through.
    pub debug_addr: &'a [u8],
    /// `.debug_abbrev` -- the abbreviation tables the entries in `.debug_info` are encoded against.
    pub debug_abbrev: &'a [u8],
    /// `.debug_info` -- the debugging information entries.
    pub debug_info: &'a [u8],
    /// `.debug_frame` -- the call-frame information a stack walk unwinds through.
    pub debug_frame: &'a [u8],
    /// `.debug_loc` -- the DWARF 4 location lists a variable's place moves through.
    pub debug_loc: &'a [u8],
    /// `.debug_loclists` -- the same thing at DWARF 5, in a different encoding.
    ///
    /// Both, because one image carries both: an Embedded Swift image mixes version 4 and version 5
    /// units, and which section a location offset means is decided by the version of the unit that
    /// named it rather than by the attribute.
    pub debug_loclists: &'a [u8],
}

impl<'a> Sections<'a> {
    /// Fills in whichever field `name` is the section for, ignoring a name that is not one.
    ///
    /// A container hands its sections over one at a time, and the alternative -- a caller matching
    /// on names itself -- puts the spelling of every section name in every consumer.
    pub fn set(&mut self, name: &str, bytes: &'a [u8]) {
        match name {
            ".debug_line" => self.debug_line = bytes,
            ".debug_line_str" => self.debug_line_str = bytes,
            ".debug_str" => self.debug_str = bytes,
            ".debug_str_offsets" => self.debug_str_offsets = bytes,
            ".debug_addr" => self.debug_addr = bytes,
            ".debug_abbrev" => self.debug_abbrev = bytes,
            ".debug_info" => self.debug_info = bytes,
            ".debug_frame" => self.debug_frame = bytes,
            ".debug_loc" => self.debug_loc = bytes,
            ".debug_loclists" => self.debug_loclists = bytes,
            _ => {}
        }
    }
}

/// Target facts a reader cannot get from the debug sections themselves.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Whether code addresses on this target may carry the ARM Thumb bit in bit 0.
    ///
    /// An ARM function symbol for Thumb code has bit 0 set -- that is how the processor is told
    /// which instruction set to enter -- and a relocation that resolves a `DW_LNE_set_address`
    /// operand against such a symbol carries the bit into the line table. The bit is not part of
    /// the address; the instruction is at the even address.
    ///
    /// A reader that leaves it set is off by one on every row of such a table, and every lookup
    /// then quietly returns the PREVIOUS row's line rather than failing. Which producers set it is
    /// not a matter of opinion and cannot be assumed either way: the AOT backend in this tree emits
    /// even addresses throughout, and an LLVM-family toolchain building for the same target need
    /// not. So it is the caller's to declare, from the container's machine type, rather than
    /// guessed from the bytes.
    pub arm_thumb_addresses: bool,
}

/// What went wrong reading a section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DwarfError {
    /// A read ran past the end of the section or of a unit within it.
    Truncated,
    /// A LEB128 ran longer than any value it could encode.
    MalformedLeb128,
    /// An initial length in the range the format reserves, which no producer writes.
    ReservedInitialLength(u32),
    /// A line-table version this crate does not read. Versions 2 through 5 are read.
    UnsupportedLineVersion(u16),
    /// A compilation-unit version this crate does not read. Versions 2 through 5 are read.
    UnsupportedUnitVersion(u16),
    /// An address size other than 1, 2, 4 or 8 bytes.
    UnsupportedAddressSize(u8),
    /// A unit using segmented addresses, which no target this reader serves uses.
    SegmentedAddresses,
    /// A `line_range` of zero, which every special opcode would divide by.
    ZeroLineRange,
    /// A `maximum_operations_per_instruction` of zero, which every address advance would divide by.
    ZeroMaximumOperations,
    /// An `opcode_base` of zero, which would leave no byte introducing an extended opcode.
    ZeroOpcodeBase,
    /// An attribute form this crate cannot read, and therefore cannot step over either.
    UnknownForm(u64),
    /// `DW_FORM_implicit_const` where a value cannot come from: its value is in the abbreviation.
    ImplicitConstInData,
    /// An abbreviation code an entry named that its unit's abbreviation table does not declare.
    UnknownAbbreviation(u64),
    /// A call frame instruction this crate does not know.
    ///
    /// **A CFI PROGRAM CANNOT BE RESUMED PAST AN UNKNOWN OPCODE**, because the operands are encoded
    /// per instruction and there is no length to skip -- so the next byte read is an operand
    /// interpreted as an opcode, and the rows after it are invented. Refusing is the only option
    /// that does not produce a plausible frame.
    UnknownFrameInstruction(u8),
}

/// A source position, resolved out of a line table.
#[derive(Debug, Clone, Copy)]
pub struct Location<'a> {
    /// The file's path as the unit stored it, which is usually a bare name.
    pub file: &'a [u8],
    /// The directory the unit put that file in, when its index named one.
    pub directory: Option<&'a [u8]>,
    /// The source line, or 0 for code that belongs to no line.
    pub line: u32,
    /// The source column, or 0 when the producer recorded none.
    pub column: u32,
    /// Whether the address is a recommended breakpoint location for a statement.
    pub is_stmt: bool,
    /// The address the row this came from begins at, which is at or below the address asked about.
    pub address: u64,
}

impl Location<'_> {
    /// The file's path with its directory joined on, when it had one.
    ///
    /// The separator is `/` whatever the producing host used, which is what a debugger prints and
    /// what a path stored by a Windows-hosted compiler needs in order to be opened anywhere else.
    /// Bytes that are not UTF-8 are replaced rather than dropped, so a path is never silently
    /// shortened into a different one.
    #[must_use]
    pub fn path(&self) -> String {
        join_path(self.file, self.directory)
    }
}

/// Joins a stored file path onto the directory its entry named, when the join applies.
///
/// The separator is `/` whatever the producing host used, which is what a debugger prints and what
/// a path stored by a Windows-hosted compiler needs in order to be opened anywhere else. Bytes that
/// are not UTF-8 are replaced rather than dropped, so a path is never silently shortened into a
/// different one.
pub(crate) fn join_path(file: &[u8], directory: Option<&[u8]>) -> String {
    let name = String::from_utf8_lossy(file);
    match directory {
        Some(directory) if !directory.is_empty() && !is_absolute(file) => {
            let mut out = String::from_utf8_lossy(directory).into_owned();
            if !out.ends_with('/') && !out.ends_with('\\') {
                out.push('/');
            }
            out.push_str(&name);
            out
        }
        _ => name.into_owned(),
    }
}

/// Whether a stored path already names a root, in which case its directory entry does not apply.
fn is_absolute(path: &[u8]) -> bool {
    match path {
        [b'/', ..] | [b'\\', ..] => true,
        [drive, b':', sep, ..] => drive.is_ascii_alphabetic() && (*sep == b'\\' || *sep == b'/'),
        _ => false,
    }
}

/// One row of the table a native debug backend indexes: where the code is, and what it came from.
///
/// Ordering is by offset first, so a sort puts the table in the order a backend searches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeviceRow {
    /// The row's address, less the base the image is loaded at.
    pub offset: u32,
    /// The source line, or 0 for code that belongs to no line.
    pub line: u32,
    /// Which of [`DeviceLines::files`] that line is in.
    pub file: u32,
}

/// A whole program's source lines, as offsets into a loaded image.
///
/// This is the shape a native debug backend indexes: it holds a PC, subtracts the address the image
/// was loaded at, and looks the remainder up. The subtraction happens once, here, so that a caller
/// is not left holding two address conventions and a rule about which one it has.
///
/// # A row's file is per row, and a program's is not
///
/// Rows from different source files interleave at instruction granularity, and a single function
/// routinely spans several of them: inlined code keeps the position it was written at, not the
/// position it was inlined into. So the file belongs to the row.
///
/// A table that instead holds one filename for a whole program is right for exactly one shape of
/// program -- a single-file one -- and wrong for every other in a way that cannot be seen from
/// outside: it reports a real line number against a plausible file that the line is not in. Nothing
/// errors, and the reader has no way to tell. [`Self::single_file`] is the question to ask before
/// narrowing this to one name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceLines {
    /// Every source file the rows name, in the order the rows first named them.
    pub files: Vec<String>,
    /// The rows, ascending by offset and deduplicated.
    pub rows: Vec<DeviceRow>,
    /// The lowest address the line table named that was below the image base, if it named any.
    ///
    /// Such a row cannot be an offset into the image, so it is left out of [`Self::rows`] and
    /// reported here instead of being decided on. It has two causes with opposite repairs, and
    /// which one it is depends on the producer rather than on anything visible here: a line table
    /// belonging to a different program than the image, which is a mismatch worth refusing; or a
    /// tombstone a linker left behind for code it discarded, which is ordinary and worth skipping.
    pub below_base: Option<u64>,
}

impl DeviceLines {
    /// The one file every row came from, or `None` when they came from more than one.
    ///
    /// A consumer whose model holds a single filename for a program asks this, and gets `None`
    /// exactly when narrowing to one name would misattribute something.
    #[must_use]
    pub fn single_file(&self) -> Option<&str> {
        match self.files.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }

    /// The path row `row` came from.
    #[must_use]
    pub fn file_of(&self, row: &DeviceRow) -> Option<&str> {
        self.files
            .get(usize::try_from(row.file).ok()?)
            .map(String::as_str)
    }
}

/// One row's address span: what it describes, and where the next row takes over.
#[derive(Debug, Clone, Copy)]
struct Span {
    start: u64,
    end: u64,
    unit: usize,
    row: usize,
    /// The index of the first row of the sequence this row belongs to. A walk back from a row that
    /// names no source line stops here: the row before a sequence describes different code, placed
    /// somewhere else entirely by the linker.
    sequence_start: usize,
}

/// A parsed set of debug sections, ready to answer a debugger's questions.
#[derive(Debug, Clone)]
pub struct DebugInfo<'a> {
    programs: Vec<LineProgram<'a>>,
    functions: Vec<Function<'a>>,
    /// Every row's address span, sorted by start address, for the address lookup.
    spans: Vec<Span>,
}

impl<'a> DebugInfo<'a> {
    /// Parses the line-number programs and the subprogram entries in `sections`.
    ///
    /// Addresses are reported exactly as the sections store them. Use [`Self::parse_with`] on a
    /// target whose addresses carry tag bits.
    pub fn parse(sections: &Sections<'a>) -> Result<Self, DwarfError> {
        Self::parse_with(sections, &Options::default())
    }

    /// Parses `sections` against what the caller knows about the target.
    pub fn parse_with(sections: &Sections<'a>, options: &Options) -> Result<Self, DwarfError> {
        let mut programs = line::programs(sections)?;
        let mut functions = info::functions(sections)?;
        if options.arm_thumb_addresses {
            for program in &mut programs {
                for row in &mut program.rows {
                    row.address &= !1;
                }
            }
            for function in &mut functions {
                let masked = function.low_pc & !1;
                function.high_pc = function.high_pc.saturating_sub(function.low_pc - masked);
                function.low_pc = masked;
            }
        }
        let spans = spans(&programs);
        Ok(DebugInfo {
            programs,
            functions,
            spans,
        })
    }

    /// The line-number programs, in section order.
    #[must_use]
    pub fn programs(&self) -> &[LineProgram<'a>] {
        &self.programs
    }

    /// The subprogram entries found in `.debug_info`.
    #[must_use]
    pub fn functions(&self) -> &[Function<'a>] {
        &self.functions
    }

    /// The source position an address came from: the row whose span covers it.
    ///
    /// A row covers from its own address up to the next row's, and a sequence's closing row bounds
    /// the row before it without describing anything itself. An address in no sequence -- code from
    /// an object built without debug info, which in a linked image is most of it -- resolves to
    /// nothing.
    #[must_use]
    pub fn location_for_address(&self, address: u64) -> Option<Location<'a>> {
        let index = self.spans.partition_point(|span| span.start <= address);
        let span = self.spans[..index]
            .iter()
            .rev()
            .find(|span| address < span.end)?;
        let program = self.programs.get(span.unit)?;
        let row = program.rows.get(span.row)?;
        Some(self.locate(program, row))
    }

    /// Every row whose span covers an address.
    ///
    /// In a well-formed table this returns at most one: DWARF 5 section 6.2.5 requires that
    /// sequences do not overlap, so one address is described once. More than one means the debug
    /// info contradicts itself, and which of them a debugger reports is then a choice the format
    /// does not make for it. [`Self::location_for_address`] takes the narrowest of them; this
    /// reports every candidate, so a caller can tell a single description from a contradiction.
    #[must_use]
    pub fn locations_for_address(&self, address: u64) -> Vec<Location<'a>> {
        let index = self.spans.partition_point(|span| span.start <= address);
        let mut out = Vec::new();
        for span in self.spans[..index].iter().rev() {
            if address >= span.end {
                continue;
            }
            let (Some(program), Some(row)) = (
                self.programs.get(span.unit),
                self.programs
                    .get(span.unit)
                    .and_then(|program| program.rows.get(span.row)),
            ) else {
                continue;
            };
            out.push(self.locate(program, row));
        }
        out
    }

    /// The last source line at or before an address that names one, within the same sequence.
    ///
    /// [`Self::location_for_address`] is the faithful answer and this is the useful one, and the
    /// two differ exactly where a producer emitted a row with line 0. DWARF gives line 0 the
    /// meaning "this address belongs to no source line", which optimizing producers emit freely for
    /// code no source construct produced -- register shuffling a compiler inserted, a call sequence
    /// belonging to no statement. A debugger that reports "no source" there leaves a user stepping
    /// through a function into blank frames, so the convention is to keep attributing such code to
    /// the last line that did name a position. gdb goes further and drops the line-0 rows outright,
    /// which is the same answer reached by discarding the information rather than by keeping it.
    ///
    /// The walk never leaves the sequence the address is in. Sequences are placed independently by
    /// the linker, so the row before a sequence describes code that is nowhere near it.
    #[must_use]
    pub fn nearest_line_for_address(&self, address: u64) -> Option<Location<'a>> {
        let index = self.spans.partition_point(|span| span.start <= address);
        let span = self.spans[..index]
            .iter()
            .rev()
            .find(|span| address < span.end)?;
        let program = self.programs.get(span.unit)?;
        for row_index in (span.sequence_start..=span.row).rev() {
            let row = program.rows.get(row_index)?;
            if row.line != 0 && !row.end_sequence {
                return Some(self.locate(program, row));
            }
        }
        None
    }

    /// Every address a source position lowered to.
    ///
    /// `file` matches by trailing path components, so `Program.cs` finds a unit that stored the
    /// same file under a full path and `src/Program.cs` does not match `other/src/Program.cs`'s
    /// sibling directory. Only rows that are statement boundaries are returned: a breakpoint goes
    /// where a statement starts, and the other rows of a line are addresses in the middle of it.
    #[must_use]
    pub fn addresses_for_line(&self, file: &str, line: u32) -> Vec<u64> {
        let mut out = Vec::new();
        for program in &self.programs {
            for row in &program.rows {
                if row.end_sequence || row.line != line || !row.is_stmt {
                    continue;
                }
                let Some(path) = program.path(row.file) else {
                    continue;
                };
                if path_matches(&path, file) {
                    out.push(row.address);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The whole line matrix as offsets into an image loaded at `base`.
    ///
    /// Every row of every unit, less the closing rows that name no code, less the rows whose file
    /// the unit did not declare, less anything below `base` -- see [`DeviceLines::below_base`].
    /// The result is sorted and deduplicated, so two units describing the same address the same way
    /// contribute one row.
    ///
    /// This is a whole-image walk rather than a query: a debug backend loads the table once at the
    /// start of a session and indexes it thereafter, which is why nothing here is lazy.
    #[must_use]
    pub fn device_lines(&self, base: u64) -> DeviceLines {
        let mut files: Vec<String> = Vec::new();
        let mut rows: Vec<DeviceRow> = Vec::new();
        let mut below_base: Option<u64> = None;
        for program in &self.programs {
            let mut resolved: Vec<Option<Option<u32>>> = Vec::new();
            resolved.resize(program.files.len(), None);
            for row in &program.rows {
                if row.end_sequence {
                    continue;
                }
                if row.address < base {
                    below_base = Some(match below_base {
                        Some(lowest) if lowest <= row.address => lowest,
                        _ => row.address,
                    });
                    continue;
                }
                let Ok(index) = usize::try_from(row.file) else {
                    continue;
                };
                let Some(slot) = resolved.get_mut(index) else {
                    continue;
                };
                let known = match *slot {
                    Some(known) => known,
                    None => {
                        let known = match program.path(row.file) {
                            Some(path) => {
                                let position = match files.iter().position(|seen| *seen == path) {
                                    Some(position) => position,
                                    None => {
                                        files.push(path);
                                        files.len() - 1
                                    }
                                };
                                u32::try_from(position).ok()
                            }
                            None => None,
                        };
                        *slot = Some(known);
                        known
                    }
                };
                let (Some(file), Ok(offset)) = (known, u32::try_from(row.address - base)) else {
                    continue;
                };
                rows.push(DeviceRow {
                    offset,
                    line: row.line,
                    file,
                });
            }
        }
        rows.sort_unstable();
        rows.dedup();
        DeviceLines {
            files,
            rows,
            below_base,
        }
    }

    /// The subprogram of that name, if `.debug_info` described one.
    #[must_use]
    pub fn function(&self, name: &str) -> Option<&Function<'a>> {
        self.functions
            .iter()
            .find(|function| function.name == name.as_bytes())
    }

    /// Where a breakpoint on a function belongs: the address its prologue ends at.
    ///
    /// The function's entry is the wrong answer and the difference is not cosmetic. A prologue is
    /// code no source construct produced, so stopping at the entry stops before the arguments are
    /// where the debug info says they are. A producer marks the right row with `prologue_end`; when
    /// none is marked, the second row of the function's sequence is the conventional fallback, and
    /// the entry itself is used only when the function has one row.
    #[must_use]
    pub fn breakpoint_for_function(&self, name: &str) -> Option<u64> {
        let function = self.function(name)?;
        let mut first: Option<u64> = None;
        let mut second: Option<u64> = None;
        for program in &self.programs {
            for row in &program.rows {
                if row.end_sequence || row.address < function.low_pc || row.address >= function.high_pc
                {
                    continue;
                }
                if row.prologue_end {
                    return Some(row.address);
                }
                match first {
                    None => first = Some(row.address),
                    Some(start) if row.address > start => {
                        second = Some(match second {
                            Some(existing) if existing < row.address => existing,
                            _ => row.address,
                        });
                    }
                    _ => {}
                }
            }
        }
        second.or(first).or(Some(function.low_pc))
    }

    fn locate(&self, program: &LineProgram<'a>, row: &Row) -> Location<'a> {
        let entry = program.file(row.file);
        Location {
            file: entry.map_or(&b""[..], |entry| entry.path),
            directory: entry.and_then(|entry| program.directory(entry)),
            line: row.line,
            column: row.column,
            is_stmt: row.is_stmt,
            address: row.address,
        }
    }
}

/// Builds the address-ordered span list every address lookup binary-searches.
fn spans(programs: &[LineProgram<'_>]) -> Vec<Span> {
    let mut spans = Vec::new();
    for (unit, program) in programs.iter().enumerate() {
        let mut start = 0usize;
        for (index, row) in program.rows.iter().enumerate() {
            if !row.end_sequence {
                continue;
            }
            let mut ordered: Vec<usize> = (start..=index).collect();
            ordered.sort_by_key(|&i| program.rows[i].address);

            let mut group_start = 0usize;
            while group_start < ordered.len() {
                let address = program.rows[ordered[group_start]].address;
                let mut group_end = group_start;
                while group_end < ordered.len()
                    && program.rows[ordered[group_end]].address == address
                {
                    group_end += 1;
                }
                let group = &ordered[group_start..group_end];
                let representative = group
                    .iter()
                    .rev()
                    .find(|&&i| program.rows[i].is_stmt && !program.rows[i].end_sequence)
                    .or_else(|| group.iter().rev().find(|&&i| !program.rows[i].end_sequence));
                if let (Some(&row_index), Some(&next)) =
                    (representative, ordered.get(group_end))
                {
                    spans.push(Span {
                        start: address,
                        end: program.rows[next].address,
                        unit,
                        row: row_index,
                        sequence_start: start,
                    });
                }
                group_start = group_end;
            }
            start = index + 1;
        }
    }
    spans.sort_by_key(|span| span.start);
    spans
}

/// Whether `path` ends with `query` on a path-component boundary, comparing both separators alike.
fn path_matches(path: &str, query: &str) -> bool {
    let path: Vec<&str> = path.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    let query: Vec<&str> = query.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    if query.is_empty() || query.len() > path.len() {
        return false;
    }
    path[path.len() - query.len()..] == query[..]
}
