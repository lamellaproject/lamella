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
            names.push((offset, end, String::from_utf8_lossy(function.name).into_owned()));
        }
    }
    names.sort_by_key(|&(offset, _, _)| offset);

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
        .map_or_else(
            || String::from("?"),
            |f| String::from_utf8_lossy(f.name).into_owned(),
        );

    Ok(DebugProgram {
        image: flat.bytes,
        image_base: flat.base,
        lines,
        names,
        files,
        entry,
        frames,
        locals,
    })
}

#[cfg(test)]
mod tests;
