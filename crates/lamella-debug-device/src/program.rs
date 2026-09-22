//! What a device debug session needs to know about the program it is about to debug, and the two
//! ways of finding it out.

/// Everything a device debug session needs about a program, however it was produced.
///
/// The tables are ascending by offset and the offsets are relative to [`Self::image_base`], which
/// is what [`crate::DeviceBackend`] expects.
pub struct DebugProgram {
    /// The bytes to write to the part.
    pub image: Vec<u8>,
    /// Where those bytes are mapped for execution. **A PC minus this indexes the tables below.**
    pub image_base: u32,
    /// The source map, ascending by offset and deduplicated. Each row names its own file.
    pub lines: Vec<crate::LineRow>,
    /// `(offset, end offset, function name)`, ascending.
    ///
    /// **THE END IS CARRIED BECAUSE A PROGRAM HAS MORE FUNCTIONS THAN NAMES.** Only a subset of an
    /// image's code has a subprogram entry -- a C floor built without `-g`, a compiler-generated
    /// thunk, an assembly stub -- and a lookup that took the nearest preceding name would label
    /// every one of those with whatever function happened to end before it.
    pub names: Vec<(u32, u32, String)>,
    /// The files the rows index.
    ///
    /// **A LIST AND NOT ONE PATH.** A C# AOT program has one entry here and a Swift image has
    /// fourteen in a single compilation unit, interleaved at instruction granularity -- see
    /// [`crate::LineRow`].
    pub files: Vec<String>,
    /// The function a session reports itself stopped in before it has run anything.
    pub entry: String,
    /// `.debug_frame`, from which a call stack is computed. **Empty is ordinary** -- a producer
    /// need not emit it, and a session then reports the one frame it can see without unwinding.
    pub frames: Vec<u8>,
    /// The sections a variables pane is read from. **Empty is ordinary** for the same reason.
    pub locals: LocalSections,
    /// The Arm exception-handling tables, from which a call stack is computed where `.debug_frame`
    /// has no row. **Empty is ordinary** for the same reason.
    pub unwind: UnwindTables,
}

/// The debug sections local variables are read from, owned.
///
/// **HELD AS BYTES AND PARSED PER REQUEST**, for the reason [`crate::DeviceBackend`] gives about
/// `.debug_frame`: the reader borrows the sections, so an owned table would be a self-reference,
/// and a variables request is a human-scale event whose cost is bounded by the image.
///
/// `code` is carried beside them because the discriminator that tells a live subprogram from one
/// the linker discarded lives in the ELF's section flags rather than in DWARF -- and DWARF is all
/// the reader can see.
#[derive(Default, Clone)]
pub struct LocalSections {
    /// `.debug_info`.
    pub info: Vec<u8>,
    /// `.debug_abbrev`.
    pub abbrev: Vec<u8>,
    /// `.debug_str`.
    pub str_: Vec<u8>,
    /// `.debug_str_offsets`.
    pub str_offsets: Vec<u8>,
    /// `.debug_addr`.
    pub addr: Vec<u8>,
    /// `.debug_loc`, the DWARF 4 location lists.
    pub loc: Vec<u8>,
    /// `.debug_loclists`, the DWARF 5 ones. Both, because one image carries both.
    pub loclists: Vec<u8>,
    /// The image's executable address ranges, `[start, end)`.
    pub code: Vec<(u64, u64)>,
}

impl LocalSections {
    /// Whether nothing here can answer a question about a variable.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.info.is_empty() || self.abbrev.is_empty()
    }

    /// A borrowed view for the reader.
    #[must_use]
    pub fn view(&self) -> lamella_dwarf::Sections<'_> {
        let mut sections = lamella_dwarf::Sections::default();
        sections.set(".debug_info", &self.info);
        sections.set(".debug_abbrev", &self.abbrev);
        sections.set(".debug_str", &self.str_);
        sections.set(".debug_str_offsets", &self.str_offsets);
        sections.set(".debug_addr", &self.addr);
        sections.set(".debug_loc", &self.loc);
        sections.set(".debug_loclists", &self.loclists);
        sections
    }
}

/// The Arm exception-handling tables a program carries, owned: its index tables, and the loaded
/// sections their table entries are in.
///
/// **HELD AS BYTES AND SEARCHED PER STOP**, for the reason [`crate::DeviceBackend`] gives about
/// `.debug_frame`: the reader borrows what it reads, and a stop is a human-scale event.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UnwindTables {
    /// Every index table that can be searched, as `(address, bytes)`.
    pub indexes: Vec<(u32, Vec<u8>)>,
    /// Each loaded section holding a table entry an index points at, as `(address, bytes)`.
    pub entries: Vec<(u32, Vec<u8>)>,
    /// The image's executable address ranges, `[start, end)`.
    ///
    /// An index records where each function starts and not where the last one ends, so outside
    /// these ranges no address is taken to be described. Empty when the file named none, and then
    /// nothing is excluded.
    pub code: Vec<(u64, u64)>,
}

/// What the index tables say about the function containing an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Described {
    /// The function's index entry.
    pub(crate) entry: lamella_elf::ehabi::IndexEntry,
    /// What its table entry says.
    pub(crate) description: lamella_elf::ehabi::Description,
}

impl UnwindTables {
    /// The tables of a linked ELF; see [`Self::from_tables`].
    #[must_use]
    pub fn from_elf(bytes: &[u8]) -> Self {
        Self::from_tables(&lamella_elf::ehabi::tables(bytes), lamella_elf::executable_ranges(bytes))
    }

    /// Keeps every index table in `tables` that can be searched, and each loaded section holding a
    /// table entry one of them points at, with the image's executable ranges `code`.
    ///
    /// An index table that cannot be searched -- out of order, or not whole entries -- is left out: a
    /// lookup in one answers the wrong function for some address, and nothing about that answer says
    /// so.
    #[must_use]
    pub fn from_tables(tables: &lamella_elf::ehabi::Tables<'_>, code: Vec<(u64, u64)>) -> Self {
        let mut kept = UnwindTables { code, ..Self::default() };
        for region in &tables.indexes {
            let Ok(index) = lamella_elf::ehabi::IndexTable::new(*region) else {
                continue;
            };
            kept.indexes.push((region.address, region.bytes.to_vec()));
            for entry in index.entries() {
                let lamella_elf::ehabi::Content::Table(address) = entry.content else {
                    continue;
                };
                let Some(section) = tables.loaded.iter().find(|section| section.word(address).is_some())
                else {
                    continue;
                };
                if !kept.entries.iter().any(|(start, _)| *start == section.address) {
                    kept.entries.push((section.address, section.bytes.to_vec()));
                }
            }
        }
        kept
    }

    /// Whether the program carries no index table.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.indexes.is_empty()
    }

    /// What the index tables say about the function containing `address`, where one does.
    ///
    /// `None` below every function, and outside the image's code where the image names its code.
    pub(crate) fn describe(&self, address: u32) -> Option<Described> {
        let in_code = self.code.is_empty()
            || self
                .code
                .iter()
                .any(|&(start, end)| u64::from(address) >= start && u64::from(address) < end);
        if !in_code {
            return None;
        }
        let loaded: Vec<lamella_elf::ehabi::Region<'_>> = self
            .entries
            .iter()
            .map(|(start, bytes)| lamella_elf::ehabi::Region { address: *start, bytes })
            .collect();
        let mut best: Option<lamella_elf::ehabi::IndexEntry> = None;
        for (start, bytes) in &self.indexes {
            let region = lamella_elf::ehabi::Region { address: *start, bytes };
            let Ok(index) = lamella_elf::ehabi::IndexTable::new(region) else {
                continue;
            };
            let Some(entry) = index.lookup(address) else {
                continue;
            };
            if best.is_none_or(|chosen| entry.function > chosen.function) {
                best = Some(entry);
            }
        }
        let entry = best?;
        let description = lamella_elf::ehabi::describe(&entry, &loaded).ok()?;
        Some(Described { entry, description })
    }
}

/// Why a program could not be prepared for debugging.
#[derive(Debug)]
pub enum ProgramError {
    /// The bytes are not an ELF this crate can flatten.
    NotAnElf(lamella_elf::ElfError),
    /// The ELF carries no `.debug_line`, so nothing can map an address to a source line.
    ///
    /// **A SEPARATE CASE FROM A PARSE FAILURE, because the repair is different and belongs to the
    /// person who built the program**: a release build stripped of debug info reaches here, and
    /// telling them the file is corrupt would send them to the wrong place entirely.
    NoDebugLine,
    /// `.debug_line` is present and could not be read.
    Dwarf(lamella_dwarf::DwarfError),
    /// The line table describes addresses below where the image is loaded, so the two do not
    /// describe the same program.
    AddressBelowImage {
        /// The lowest address the line table names.
        address: u64,
        /// Where the image begins.
        base: u32,
    },
}

impl core::fmt::Display for ProgramError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ProgramError::NotAnElf(error) => write!(f, "not a loadable ELF: {error:?}"),
            ProgramError::NoDebugLine => f.write_str(
                "the ELF carries no .debug_line section, so no address can be mapped to a source \
                 line -- build it with debug information",
            ),
            ProgramError::Dwarf(error) => write!(f, ".debug_line could not be read: {error:?}"),
            ProgramError::AddressBelowImage { address, base } => write!(
                f,
                "the line table describes address {address:#x}, below the image base {base:#x} -- \
                 the debug information and the image are not the same program"
            ),
        }
    }
}

/// The entry of `names` that holds `offset`, where the table is ordered as [`add_symbol_names`] leaves
/// it: the containing entry that starts nearest below `offset`, and among entries starting together,
/// the one last in the table.
///
/// **NOT THE NEAREST PRECEDING NAME.** Most of an image can have no subprogram entry, and the
/// nearest-preceding rule labels every such stretch with whatever function ended before it.
pub(crate) fn name_containing(names: &[(u32, u32, String)], offset: u32) -> Option<&(u32, u32, String)> {
    names
        .iter()
        .rev()
        .find(|&&(start, end, _)| start <= offset && offset < end)
}

/// The name to report for `function`: the QUALIFIED one the producer emitted, when what it emitted
/// is a qualified form of the source name, and the source name otherwise.
///
/// **`DW_AT_name` IS THE SOURCE SPELLING, AND A SOURCE SPELLING IS NOT UNIQUE.** Two methods called
/// `write` on two different types are both `write` there, so a table built from it alone carries the
/// same name at two address ranges and a stack frame cannot say which one it is in. The producer
/// already emitted the answer in `DW_AT_linkage_name` and this consumer was dropping it.
///
/// **IT IS TAKEN ONLY WHEN IT IS A QUALIFIED FORM OF THE SAME NAME** -- printable, and ending in the
/// source name after a `.` or `::`. A linkage name is whatever the language's linkage conventions
/// say, which for several of them is a mangled symbol that names the types and the arity as well;
/// putting one of those in a stack frame would answer an ambiguous name with an unreadable one. The
/// test is that what is shown always ENDS with what the source called it, so the reader loses
/// nothing and gains the qualifier when there is one.
fn reported_name(function: &lamella_dwarf::info::Function<'_>) -> String {
    let source = String::from_utf8_lossy(function.name).into_owned();
    let Some(linkage) = function.linkage_name else {
        return source;
    };
    if linkage
        .iter()
        .any(|byte| !byte.is_ascii_graphic() && *byte != b' ')
    {
        return source;
    }
    let linkage = String::from_utf8_lossy(linkage).into_owned();
    let qualifies =
        !source.is_empty() && linkage.len() > source.len() && linkage.ends_with(&source) && {
            let before = &linkage[..linkage.len() - source.len()];
            before.ends_with('.') || before.ends_with("::")
        };
    if qualifies { linkage } else { source }
}

/// Adds a name for code no subprogram describes from the image's function `symbols`, and orders the
/// table for [`name_containing`].
///
/// A symbol names code only where it overlaps no subprogram, so the debug information names every
/// function it describes and a symbol names only what it leaves unnamed. A symbol below the image,
/// or outside its code, is left out as a subprogram there would be. Where several symbols hold one
/// address, the name found for it is the symbol that starts nearest below the address, then the
/// shortest, then a global symbol before a weak one before a local one, then the one the table lists
/// first. A name is the table's own spelling, mangled or not.
fn add_symbol_names(
    names: &mut Vec<(u32, u32, String)>,
    symbols: &[lamella_elf::symbols::Function<'_>],
    base: u64,
    is_code: impl Fn(u64) -> bool,
) {
    let described = names.len();
    let mut named: Vec<(u32, u32, u8, usize, &str)> = Vec::new();
    for (position, symbol) in symbols.iter().enumerate() {
        let address = u64::from(symbol.address);
        if address < base || !is_code(address) {
            continue;
        }
        let Ok(start) = u32::try_from(address - base) else {
            continue;
        };
        let Some(end) = start.checked_add(symbol.size) else {
            continue;
        };
        if names[..described].iter().any(|&(from, to, _)| start < to && from < end) {
            continue;
        }
        let rank = match symbol.binding {
            lamella_elf::Binding::Global => 0,
            lamella_elf::Binding::Weak => 1,
            lamella_elf::Binding::Local => 2,
        };
        named.push((start, end, rank, position, symbol.name));
    }
    named.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then((b.1 - b.0).cmp(&(a.1 - a.0)))
            .then(b.2.cmp(&a.2))
            .then(b.3.cmp(&a.3))
    });
    names.extend(named.into_iter().map(|(start, end, _, _, name)| (start, end, String::from(name))));
    names.sort_by_key(|&(offset, _, _)| offset);
}

/// Prepares a linked ELF -- one a Swift, RHU or C toolchain produced -- for a device debug session.
///
/// # THE THUMB BIT IS TAKEN OFF HERE AND NOWHERE ELSE
///
/// On ARM, a function symbol for Thumb code carries bit 0 set, and a relocation resolved against
/// such a symbol carries it into the line table and into the ELF's entry address. **The bit is not
/// part of the address, and a reader that leaves it set is off by one on every row** -- every
/// lookup then quietly answers the previous row's line. `lamella_dwarf` takes the machine as an
/// option for exactly this reason and cannot see the ELF header itself, so the container is read
/// here: `EM_ARM` is 40, at byte 18 of every ELF header.
///
/// # DEBUG INFORMATION FOR CODE THE LINKER REMOVED IS DROPPED
///
/// A link with `--gc-sections` discards sections nothing references, and a relocation against a
/// symbol in a discarded section resolves to zero -- so the rows and subprogram entries that
/// described that code survive in the file, all of them naming address 0. Their contents give no
/// sign of it: a real file, a real line number, a real function name. On a part whose flash begins
/// at zero they land on top of the image base, where a breakpoint request would find one and arm a
/// comparator on the vector table.
///
/// A row or a subprogram is kept only if its address falls inside a section the file marks as
/// executable. When the file carries no section headers at all, nothing is dropped: an unanswerable
/// question is not a negative answer.
///
/// # Errors
/// See [`ProgramError`]. An ELF with no debug information is refused rather than served with empty
/// tables: a session that attaches and then reports no source line for every address looks like a
/// broken debugger, and the cause is one sentence long.
pub fn from_elf(bytes: &[u8]) -> Result<DebugProgram, ProgramError> {
    let flat = lamella_elf::flat_image(bytes).map_err(ProgramError::NotAnElf)?;

    let mut sections = lamella_dwarf::Sections::default();
    let mut has_line = false;
    let mut frames: Vec<u8> = Vec::new();
    let mut locals = LocalSections {
        code: lamella_elf::executable_ranges(bytes),
        ..Default::default()
    };
    for (name, data) in lamella_elf::debug_sections(bytes) {
        if name == ".debug_line" && !data.is_empty() {
            has_line = true;
        }
        match name {
            ".debug_frame" => frames = data.to_vec(),
            ".debug_info" => locals.info = data.to_vec(),
            ".debug_abbrev" => locals.abbrev = data.to_vec(),
            ".debug_str" => locals.str_ = data.to_vec(),
            ".debug_str_offsets" => locals.str_offsets = data.to_vec(),
            ".debug_addr" => locals.addr = data.to_vec(),
            ".debug_loc" => locals.loc = data.to_vec(),
            ".debug_loclists" => locals.loclists = data.to_vec(),
            _ => {}
        }
        sections.set(name, data);
    }
    if !has_line {
        return Err(ProgramError::NoDebugLine);
    }

    let options = lamella_dwarf::Options {
        arm_thumb_addresses: bytes.get(18..20) == Some(&[40, 0]),
    };
    let info =
        lamella_dwarf::DebugInfo::parse_with(&sections, &options).map_err(ProgramError::Dwarf)?;

    let base = u64::from(flat.base);

    let code = lamella_elf::executable_ranges(bytes);
    let is_code = |address: u64| {
        code.is_empty() || code.iter().any(|&(start, end)| address >= start && address < end)
    };

    let mut files: Vec<String> = Vec::new();
    let mut lines: Vec<crate::LineRow> = Vec::new();
    let mut described = 0usize;
    let mut lowest = u64::MAX;
    for program in info.programs() {
        for row in &program.rows {
            if row.end_sequence {
                continue;
            }
            described += 1;
            lowest = lowest.min(row.address);
            if !is_code(row.address) {
                continue;
            }
            let Some(offset) = row.address.checked_sub(base) else {
                continue;
            };
            let Ok(offset) = u32::try_from(offset) else {
                continue;
            };
            let Some(entry) = program.file(row.file) else {
                continue;
            };
            let path = lamella_dwarf::Location {
                file: entry.path,
                directory: program.directory(entry),
                line: row.line,
                column: row.column,
                is_stmt: row.is_stmt,
                address: row.address,
            }
            .path();
            let file = match files.iter().position(|seen| *seen == path) {
                Some(index) => index,
                None => {
                    files.push(path);
                    files.len() - 1
                }
            };
            let Ok(file) = u32::try_from(file) else {
                continue;
            };
            lines.push(crate::LineRow { offset, line: row.line, file });
        }
    }
    if described > 0 && lines.is_empty() {
        return Err(ProgramError::AddressBelowImage { address: lowest, base: flat.base });
    }
    lines.sort_unstable_by_key(|row| (row.offset, row.line, row.file));
    lines.dedup();

    let mut names: Vec<(u32, u32, String)> = Vec::new();
    for function in info.functions() {
        if function.high_pc <= function.low_pc || function.low_pc < base {
            continue;
        }
        if !is_code(function.low_pc) {
            continue;
        }
        if let (Ok(offset), Ok(end)) =
            (u32::try_from(function.low_pc - base), u32::try_from(function.high_pc - base))
        {
            names.push((offset, end, reported_name(function)));
        }
    }
    add_symbol_names(&mut names, &lamella_elf::symbols::functions(bytes), base, is_code);

    let entry_address = bytes
        .get(24..28)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .map(|address| if options.arm_thumb_addresses { address & !1 } else { address })
        .unwrap_or(flat.base);
    let entry = info
        .functions()
        .iter()
        .find(|f| {
            f.high_pc > f.low_pc
                && f.low_pc <= u64::from(entry_address)
                && u64::from(entry_address) < f.high_pc
        })
        .map(reported_name)
        .or_else(|| {
            let offset = u32::try_from(u64::from(entry_address).checked_sub(base)?).ok()?;
            name_containing(&names, offset).map(|(_, _, name)| name.clone())
        })
        .unwrap_or_else(|| String::from("?"));

    Ok(DebugProgram {
        image: flat.bytes,
        image_base: flat.base,
        lines,
        names,
        files,
        entry,
        frames,
        locals,
        unwind: UnwindTables::from_elf(bytes),
    })
}

#[cfg(test)]
mod tests;
