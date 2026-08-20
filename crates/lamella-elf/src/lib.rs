//! ELF object reading + writing for the Lamella linker (`lamella-link`) and the AOT backend.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// RISC-V ELF relocation type numbers (the `r_info` low byte), from the RISC-V ELF psABI.
pub mod riscv {
    /// `R_RISCV_32` -- a 32-bit absolute reference, `S + A`.
    pub const R_RISCV_32: u32 = 1;
    /// `R_RISCV_CALL_PLT` -- a PC-relative `auipc`+`jalr` call to `symbol`; applies to the auipc.
    pub const R_RISCV_CALL_PLT: u32 = 19;
    /// `R_RISCV_PCREL_HI20` -- the high 20 bits of a PC-relative reference (an `auipc`).
    pub const R_RISCV_PCREL_HI20: u32 = 23;
    /// `R_RISCV_PCREL_LO12_I` -- the low 12 bits of a PC-relative reference (an I-type).
    pub const R_RISCV_PCREL_LO12_I: u32 = 24;
    /// `R_RISCV_HI20` -- the high 20 bits of an ABSOLUTE reference (a `lui`), `S + A`. The medlow code
    /// model's address materialization (e.g. loading a function pointer); needs the link's `text_base`.
    pub const R_RISCV_HI20: u32 = 26;
    /// `R_RISCV_LO12_I` -- the low 12 bits of an ABSOLUTE reference in an I-type (`addi`/load), `S + A`.
    pub const R_RISCV_LO12_I: u32 = 27;
    /// `R_RISCV_LO12_S` -- the low 12 bits of an ABSOLUTE reference in an S-type (`store`), `S + A`.
    pub const R_RISCV_LO12_S: u32 = 28;
    /// `R_RISCV_RELAX` -- a linker-relaxation hint paired with a real relocation; nothing to patch.
    pub const R_RISCV_RELAX: u32 = 51;
    /// A lamella-private 32-bit data relocation, `S + A - P` -- the RISC-V twin of
    /// [`super::arm::R_LAMELLA_REL_DESC`]. It stores a signed, placement-invariant relative offset into
    /// a data word: a type descriptor's vtable/itable slot holds `(method_entry - type_desc)` (the
    /// addend absorbs `slot_addr - type_desc` so `S + A - P` reduces to that), so a `--gc-sections`
    /// re-layout that moves the slot and its target leaves it correct. RISC-V has no interworking bit,
    /// so unlike ARM it needs no Thumb-bit care. Same private number as the ARM constant (112); the
    /// linker keys off `(machine, kind)`, so the shared value never clashes with a standard RISC-V type.
    pub const R_LAMELLA_REL_DESC: u32 = 112;
}

/// ARM (AArch32) ELF relocation type numbers (the `r_info` low byte), from "ELF for the ARM
/// Architecture" (the ARM ELF ABI). ARM objects conventionally use `SHT_REL` (`.rel.text`, an
/// implicit addend in the instruction field), unlike RISC-V's `SHT_RELA`; the linker handles both.
pub mod arm {
    /// `R_ARM_ABS32` -- a 32-bit absolute reference, `(S + A) | T`.
    pub const R_ARM_ABS32: u32 = 2;
    /// `R_ARM_THM_CALL` -- a Thumb `BL`/`BLX` call (the 32-bit T1 `BL`): `((S + A) | T) - P`, the
    /// 24-bit signed halfword-scaled offset in the S:J1:J2:imm10:imm11 swizzle. A call emitted by a
    /// Thumb code generator becomes one of these.
    pub const R_ARM_THM_CALL: u32 = 10;
    /// `R_ARM_CALL` -- an A32 (ARM-state) `BL`/`BLX` call: `((S + A) | T) - P`, a 24-bit signed
    /// word-scaled offset in bits[23:0].
    pub const R_ARM_CALL: u32 = 28;
    /// `R_ARM_THM_JUMP24` -- a Thumb `B.W` (T4) unconditional branch: `(S + A) - P`, the SAME 24-bit
    /// halfword-scaled offset swizzle as `R_ARM_THM_CALL`; the instruction differs only in the second
    /// halfword's bit 14 (`B.W` = 0, `BL` = 1). A Rust/GCC thumbv7em tail call becomes one.
    pub const R_ARM_THM_JUMP24: u32 = 30;
    /// `R_ARM_THM_MOVW_ABS_NC` -- the LOW 16 bits of `(S + A) | T` written into a Thumb-2 `MOVW`
    /// (the 16-bit immediate split imm4:i:imm3:imm8). Paired with `MOVT_ABS` to materialize a 32-bit
    /// absolute address; the thumbv7em toolchain uses this pair instead of a literal pool.
    pub const R_ARM_THM_MOVW_ABS_NC: u32 = 47;
    /// `R_ARM_THM_MOVT_ABS` -- the HIGH 16 bits of `S + A` written into a Thumb-2 `MOVT` (same
    /// imm4:i:imm3:imm8 split). The upper half of the `MOVW`/`MOVT` absolute-address pair.
    pub const R_ARM_THM_MOVT_ABS: u32 = 48;
    /// A lamella-private 32-bit data relocation, `S + A - P` -- like `R_ARM_REL32` but WITHOUT the
    /// interworking `| T` (Thumb-bit) forcing. It stores a signed, placement-invariant relative offset
    /// into a data word. Its one use is a vtable slot: the value stored is `(method_entry - type_desc)`
    /// (the addend absorbs `slot_addr - type_desc` so `S + A - P` reduces to `method_entry - type_desc`),
    /// and the AOT dispatch code re-applies the Thumb bit at run time (`type_desc + slot + 1`), so the
    /// stored value must stay Thumb-bit-free -- which `R_ARM_REL32`'s `| T` would spoil. Numbered in the
    /// ARM ELF ABI private range (`R_ARM_PRIVATE_0` = 112), so it never clashes with a standard type.
    pub const R_LAMELLA_REL_DESC: u32 = 112;
}

/// A target machine, selecting the ELF `e_machine`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Machine {
    /// RISC-V (`EM_RISCV` = 243).
    RiscV,
    /// 32-bit ARM (`EM_ARM` = 40).
    Arm,
}

impl Machine {
    fn e_machine(self) -> u16 {
        match self {
            Machine::RiscV => 243,
            Machine::Arm => 40,
        }
    }
}

/// A symbol's binding -- the high nibble of `st_info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding {
    /// `STB_LOCAL` -- not visible to the linker outside this object.
    Local,
    /// `STB_GLOBAL` -- visible to the linker across objects.
    Global,
    /// `STB_WEAK` -- a global definition a strong (global) one overrides, and which does not conflict
    /// with another weak definition of the same name (the first wins). compiler_builtins emits its
    /// `__aeabi_*` soft-float helpers this way.
    Weak,
}

/// A symbol's type -- the low nibble of `st_info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolType {
    /// `STT_NOTYPE`.
    NoType,
    /// `STT_FUNC` -- a function entry point.
    Func,
    /// `STT_SECTION` -- names a SECTION rather than a location inside one. Its `st_name` is 0 (the
    /// name lives in the section header, not the string table), which is why a purely name-keyed
    /// linker cannot resolve DWARF: LLVM emits every `.debug_*` cross-section reference against one
    /// of these. `llvm-readelf` DISPLAYS a name for them -- synthesized from the section headers --
    /// so they look name-addressable in a dump when they are not.
    Section,
}

/// The prefix on a canonical type-descriptor's data symbol name (`__lamella_typedesc_<handle>`). The AOT
/// backend NAMES descriptor symbols with it and the linker's gc-sections COLLECTS them by it (a descriptor
/// unreachable from the entry is dropped, so its vtable relocations cannot pin an otherwise-trimmed
/// method); shared here so the two crates agree on one string.
pub const TYPE_DESC_PREFIX: &str = "__lamella_typedesc_";

/// The prefix on a per-method GC stack-map record's data symbol (`__lamella_smrec_<function symbol>`).
/// The AOT backend emits one METHOD_SLOTS record per safepoint-bearing function; the record is
/// code-UNreferenced (only the collector reads it at runtime), so the linker applies the INVERSE
/// gc-sections rule: a record survives if and only if the function its name carries survives (a plain
/// data-keep would leave a dropped function's record with a dangling `func_addr` relocation, and a
/// reachability-keep would drop every record). Shared here so backend and linker agree on one string.
pub const STACKMAP_RECORD_PREFIX: &str = "__lamella_smrec_";

/// The CARRIED section holding per-function RETURN-ADDRESS stack-map fragments, from which
/// `lamella-link` synthesizes the whole-program [`STACKMAP_BLOB_SYMBOL`] map.
///
/// **THIS IS A DIFFERENT MAP FROM [`STACKMAP_RECORD_PREFIX`], NOT A SECOND ENCODING OF IT.** A
/// `__lamella_smrec_*` record is per-METHOD and answers "how do I step past this frame"; this map is
/// per-SAFEPOINT and answers "which slots are roots at this exact PC". One is a table and the other
/// is an index into the code; neither substitutes for the other.
///
/// **NOT `SHF_ALLOC`, which is the whole point of the vehicle**: a fragment is an input the linker
/// consumes and never a byte the target flashes, so the synthesized map costs what the SURVIVING
/// functions need rather than what the compiler happened to see. Emitting fragments into `.text`
/// instead would pay for every surviving function twice.
///
/// The section is a concatenation of self-delimiting fragments, each little-endian:
///
/// ```text
///   u32  name_len          the owning FUNCTION symbol's name length
///   u8   name[name_len]    the name, padded with zeros to a 4-byte boundary
///   u32  entry_count
///   repeat entry_count:
///     u32  rel_pc          the safepoint's return address RELATIVE TO ITS FUNCTION's start
///     u32  tail_len        byte length of the opaque tail
///     u8   tail[tail_len]  the entry's encoded bytes AFTER its `return_pc` word, verbatim,
///                          padded with zeros to a 4-byte boundary
/// ```
///
/// The padding is the FRAGMENT's, not the map's. An entry's tail is an even number of bytes but
/// not always a multiple of four, and `tail_len` is what the linker copies -- so the padding keeps
/// the next fragment word-aligned while the synthesized map keeps the exact unpadded layout
/// the GC ABI defines for a stack map.
///
/// **THE TAIL IS DELIBERATELY OPAQUE, AND THAT IS A DRIFT DEFENCE RATHER THAN LAZINESS.** The
/// linker rebases `rel_pc` into the map's key and copies the tail through, so it never learns the
/// entry's internal shape (`nrefs`, `ntagged`, the root arrays). One encoder writes that shape and
/// one walker reads it; a linker that parsed it would be a third party to drift from.
pub const STACKMAP_GCMAP_SECTION: &str = ".lamella_gcmap";

/// The whole-program return-address stack map a collector binary-searches (`__lamella_gc_stackmaps`),
/// synthesized by `lamella-link` from [`STACKMAP_GCMAP_SECTION`] over the functions that SURVIVED
/// dead-stripping. Its wire format is the GC ABI's and is unchanged by where it is
/// built.
pub const STACKMAP_BLOB_SYMBOL: &str = "__lamella_gc_stackmaps";

/// The image's `.text` start (`__lamella_text_base`), the base a collector subtracts from a runtime
/// return address to get a [`STACKMAP_BLOB_SYMBOL`] lookup key. Defined by `lamella-link` at image
/// offset 0 whenever it synthesizes the map.
///
/// **THE LINKER DEFINES IT AND THE BACKEND DELIBERATELY DOES NOT.** A backend-emitted definition
/// is an offset into ONE object, which stops being the image's `.text` start the moment a second
/// object links ahead of it -- and a zero-size symbol does not survive `garbage_collect`.
pub const TEXT_BASE_SYMBOL: &str = "__lamella_text_base";

/// Whether a PROGBITS, non-`SHF_ALLOC` section is one the linker CARRIES through the link as itself
/// -- the DWARF [`DEBUG_SECTION_PREFIX`] family, plus [`STACKMAP_GCMAP_SECTION`]. Anything else
/// non-allocated is dropped, as it was before carrying existed.
#[must_use]
pub fn is_carried_section(name: &str) -> bool {
    name.starts_with(DEBUG_SECTION_PREFIX) || name == STACKMAP_GCMAP_SECTION
}

/// The prefix on a GLOBAL-roots stack-map record's data symbol (`__lamella_smstat_<asmhash>`) -- the
/// mode-2 STATICS record naming one assembly's static region's ref-bearing words. Its `func_addr`
/// word carries an `R_ARM_ABS32` relocation against the assembly's region symbol
/// ([`STATICS_BASE_PREFIX`]), so the walker reads the linker-placed base. Always kept; the linker
/// recognizes it so the gather pass tables it alongside the per-method records.
pub const STACKMAP_STATICS_PREFIX: &str = "__lamella_smstat_";

/// The prefix on a managed assembly's STATIC-REGION symbol (`__lamella_statics_<asmhash>`, the
/// suffix EXACTLY eight lowercase hex digits -- fnv1a32 of the assembly's CIL bytes, the same hash
/// that prefixes a library object's internal symbols). The AOT backend references it UNDEFINED from
/// every `ldsfld`/`stsfld` pool word (addend = the field's dense slot offset) and from the mode-2
/// statics record's base word, carrying the region's byte size in the reference's `st_size`;
/// `lamella-link` lays each referenced region out in a RAM window, defines the symbol, and brackets
/// the span with [`STATICS_START_SYMBOL`]/[`STATICS_END_SYMBOL`]. Word 0 of every region is
/// RESERVED (dense slots start at 1): offset 0 is the MIR-level EH-tag marker, split out to
/// [`EH_TAG_SYMBOL`].
pub const STATICS_BASE_PREFIX: &str = "__lamella_statics_";

/// The ONE VES-global in-flight exception word, shared by EVERY assembly's throw/catch lowering
/// (`__lamella_eh_tag`). Splitting it out of the per-assembly static regions is what keeps EH
/// working across assemblies: a corlib `throw` and a program `catch` must read the SAME word, so
/// it cannot be "row 0 of the thrower's region". `lamella-link` defines it as the ENTRY object's
/// region word 0 (reserved by the dense layout, and covered by that record's row-0 ManagedPtr
/// root), falling back to the first laid region / a standalone word.
pub const EH_TAG_SYMBOL: &str = "__lamella_eh_tag";

/// The symbol at the start of the linker-laid statics RAM span (every region plus the EH word).
/// A boot stub zeroes `[start, end)` before calling the entry -- C# statics are zero-initialized
/// by the CLI spec, and `.cctor`s (chained by the entry's startup) do the rest. Not 8 hex digits,
/// so the region matcher never mistakes it for a region reference.
pub const STATICS_START_SYMBOL: &str = "__lamella_statics_start";

/// The symbol just past the linker-laid statics RAM span (see [`STATICS_START_SYMBOL`]).
pub const STATICS_END_SYMBOL: &str = "__lamella_statics_end";

/// The symbol bracketing the gathered stack-map pointer table's first word (its `u32` record count).
/// `lamella-link` defines it (Global) on any image whose objects carry stack-map records; the
/// runtime-support walker declares it extern with a WEAK empty-table fallback for images without one.
pub const STACKMAP_START_SYMBOL: &str = "__lamella_stackmaps_start";

/// The symbol just past the gathered stack-map pointer table (see [`STACKMAP_START_SYMBOL`]).
pub const STACKMAP_END_SYMBOL: &str = "__lamella_stackmaps_end";

/// The section-name prefix of the DWARF debug family (`.debug_info`, `.debug_abbrev`, `.debug_str`,
/// `.debug_line`, ...). A section so named is CARRIED through the link -- kept as itself, with its
/// own bytes and its own relocations -- instead of being merged into [`Object::text`] or dropped.
///
/// Matching by PREFIX rather than an enumerated list is deliberate: it covers the whole DWARF 5
/// family, the split-DWARF `.dwo` variants, vendor extensions, and whatever a later DWARF version
/// adds, with no code change. Debug sections are not `SHF_ALLOC`, so they cost the target no memory;
/// they exist only in the linked artifact a debugger reads.
pub const DEBUG_SECTION_PREFIX: &str = ".debug_";

/// One section carried through the link verbatim rather than merged into the code blob -- today the
/// DWARF `.debug_*` family (see [`DEBUG_SECTION_PREFIX`]), as [`read_object`] parsed it.
///
/// A carried section has its OWN address space, which is what distinguishes it from `.text`/`.rodata`
/// (those merge into one blob at one load address). The linker concatenates same-named contributions
/// across objects and relocates within the result, so a reference from `.debug_info` to `.debug_abbrev`
/// resolves to "that contribution to the combined section" while a reference to a function resolves to
/// its virtual address -- the two-address-space rule DWARF 5 s7.3.1 lays out.
///
/// The WRITER's counterpart is [`Section`], which borrows its bytes and names its relocations'
/// symbols by the index they have in the writer's `symbols` slice.
#[derive(Debug, Clone)]
pub struct ParsedSection {
    /// The section name (e.g. `.debug_info`), which is also what the linker groups contributions by.
    pub name: String,
    /// `sh_flags` -- carried so the linker can reproduce the section's character in an output object.
    pub flags: u32,
    /// `sh_addralign` (at least 1); the alignment this contribution needs in the combined section.
    pub addralign: u32,
    /// The section's bytes.
    pub data: Vec<u8>,
    /// The section's own relocations, with `offset` relative to THIS section (not to `.text`).
    pub relocations: Vec<ParsedRelocation>,
}

/// One section to EMIT beside `.text` -- the writer's counterpart to [`ParsedSection`], and how the
/// AOT backend hands its own DWARF to [`write_relocatable_object_with_sections`].
///
/// `relocations` are relative to THIS section and name their symbols by index into the writer's
/// `symbols` slice, the same space `.text`'s relocations use -- so one symbol serves a `.debug_info`
/// reference to a function and a `.debug_line` reference to the same function alike.
#[derive(Debug, Clone, Copy)]
pub struct Section<'a> {
    /// The section name (e.g. `.debug_info`). A name under [`DEBUG_SECTION_PREFIX`] is what makes
    /// [`read_object`] carry it back rather than merge or drop it.
    pub name: &'a str,
    /// `sh_flags`. 0 for a debug section: NOT `SHF_ALLOC`, so it costs the target no memory.
    pub flags: u32,
    /// `sh_addralign` (at least 1).
    pub addralign: u32,
    /// The section's bytes.
    pub data: &'a [u8],
    /// The section's own relocations, `offset` relative to this section's start.
    pub relocations: &'a [Relocation],
}

/// Where a symbol is defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolSection {
    /// Defined in this object's `.text` (`st_shndx` = the `.text` section index).
    Text,
    /// Undefined here -- the linker resolves it from another object (`SHN_UNDEF`).
    Undefined,
    /// Defined in one of the emitted [`Section`]s, by its index into the writer's `sections` slice;
    /// the symbol's `value` is an offset within THAT section.
    ///
    /// This is the handle a DWARF cross-section reference resolves through: `.debug_info` naming
    /// `.debug_abbrev` is section-relative, in an address space with no load address in it at all
    /// (DWARF 5 s7.3.1). A symbol here must not be given `.text`'s space by mistake -- that is
    /// exactly the confusion that yields debug info which looks plausible and points at nothing.
    InSection(u32),
}

/// One symbol to place in `.symtab`.
#[derive(Debug, Clone, Copy)]
pub struct Symbol<'a> {
    /// The symbol name (copied into `.strtab`).
    pub name: &'a str,
    /// `st_value` -- for a `.text` symbol, its byte offset within `.text`.
    pub value: u32,
    /// `st_size` -- the symbol's size in bytes (0 if unknown).
    pub size: u32,
    /// The binding.
    pub binding: Binding,
    /// The type.
    pub kind: SymbolType,
    /// The defining section.
    pub section: SymbolSection,
}

/// One `.rela.text` relocation: patch the `.text` site at `offset` to reference `symbol`.
#[derive(Debug, Clone, Copy)]
pub struct Relocation {
    /// `r_offset` -- the byte offset within `.text` of the instruction(s) to patch.
    pub offset: u32,
    /// The index, into the `symbols` slice passed to the writer, of the referenced symbol.
    pub symbol: u32,
    /// The relocation type (an `R_<arch>_*` number; the low byte of `r_info`).
    pub kind: u32,
    /// `r_addend` -- the constant added in the relocation's calculation.
    pub addend: i32,
}

const SHN_UNDEF: u16 = 0;
const TEXT_SHNDX: u16 = 1;
const EHDR_SIZE: u32 = 52;
const SHDR_SIZE: u16 = 40;
const SYM_SIZE: usize = 16;
const RELA_SIZE: usize = 12;
const REL_SIZE: usize = 8;

/// Emits an ELF32 relocatable object (`ET_REL`) holding `text` as `.text`, `symbols` in `.symtab`,
/// and `relocations` in `.rela.text`. `machine` sets `e_machine`; output is little-endian. A
/// relocation's `symbol` indexes the `symbols` slice (the writer maps it to the final symbol-table
/// index). Pass an empty `relocations` for a leaf object with no external references.
///
/// Emits no sections beyond `.text`; see [`write_relocatable_object_with_sections`] to emit DWARF
/// alongside it. Every object without debug info goes through here, so its bytes are unchanged by
/// the existence of that path.
pub fn write_relocatable_object(
    machine: Machine,
    text: &[u8],
    symbols: &[Symbol],
    relocations: &[Relocation],
) -> Vec<u8> {
    write_relocatable_object_with_sections(machine, text, symbols, relocations, &[])
}

/// As [`write_relocatable_object`], plus `sections` emitted beside `.text` -- the DWARF the AOT
/// backend generates for its own code.
///
/// Each section keeps its own bytes and its own relocations (in a `.rela.<name>` of its own), and a
/// [`SymbolSection::InSection`] symbol lets one section reference another. That is the whole
/// mechanism DWARF needs: `.debug_info` names `.debug_abbrev` through a section symbol, and names a
/// FUNCTION through an ordinary `.text` symbol, and the linker keeps those two in the separate
/// address spaces DWARF 5 s7.3.1 requires.
///
/// With an empty `sections` the output is byte-for-byte what [`write_relocatable_object`] has always
/// produced -- the layout below degenerates to the original five-or-six-section one rather than
/// reproducing it, so the two cannot drift apart.
pub fn write_relocatable_object_with_sections(
    machine: Machine,
    text: &[u8],
    symbols: &[Symbol],
    relocations: &[Relocation],
    sections: &[Section],
) -> Vec<u8> {
    let has_rela = !relocations.is_empty();
    let mut next_idx = if has_rela { 3u32 } else { 2 };
    let mut sec_idx: Vec<u32> = Vec::with_capacity(sections.len());
    let mut sec_rela_idx: Vec<Option<u32>> = Vec::with_capacity(sections.len());
    for sec in sections {
        sec_idx.push(next_idx);
        next_idx += 1;
        if sec.relocations.is_empty() {
            sec_rela_idx.push(None);
        } else {
            sec_rela_idx.push(Some(next_idx));
            next_idx += 1;
        }
    }
    let symtab_idx = next_idx;
    let strtab_idx = symtab_idx + 1;
    let shstrtab_idx = (strtab_idx + 1) as u16;
    let section_count = shstrtab_idx + 1;

    let local_count = symbols
        .iter()
        .filter(|s| s.binding == Binding::Local)
        .count();
    let mut local_cursor = 1u32;
    let mut global_cursor = 1 + local_count as u32;
    let mut final_index = alloc::vec![0u32; symbols.len()];
    for (i, sym) in symbols.iter().enumerate() {
        match sym.binding {
            Binding::Local => {
                final_index[i] = local_cursor;
                local_cursor += 1;
            }
            Binding::Global | Binding::Weak => {
                final_index[i] = global_cursor;
                global_cursor += 1;
            }
        }
    }
    let first_global = 1 + local_count as u32;

    let mut strtab: Vec<u8> = alloc::vec![0];
    let mut symtab: Vec<u8> = Vec::new();
    symtab.extend_from_slice(&[0u8; SYM_SIZE]);
    for want_local in [true, false] {
        for sym in symbols
            .iter()
            .filter(|s| (s.binding == Binding::Local) == want_local)
        {
            let st_name = if sym.name.is_empty() {
                0
            } else {
                let at = strtab.len() as u32;
                strtab.extend_from_slice(sym.name.as_bytes());
                strtab.push(0);
                at
            };
            let bind: u8 = match sym.binding {
                Binding::Local => 0,
                Binding::Global => 1,
                Binding::Weak => 2,
            };
            let typ: u8 = match sym.kind {
                SymbolType::NoType => 0,
                SymbolType::Func => 2,
                SymbolType::Section => 3,
            };
            let st_info = (bind << 4) | (typ & 0xf);
            let st_shndx = match sym.section {
                SymbolSection::Text => TEXT_SHNDX,
                SymbolSection::Undefined => SHN_UNDEF,
                SymbolSection::InSection(i) => sec_idx[i as usize] as u16,
            };
            symtab.extend_from_slice(&sym_entry(st_name, sym.value, sym.size, st_info, st_shndx));
        }
    }

    let rela = encode_rela(relocations, &final_index);
    let sec_rela: Vec<Vec<u8>> = sections
        .iter()
        .map(|s| encode_rela(s.relocations, &final_index))
        .collect();

    let mut shstrtab: Vec<u8> = alloc::vec![0];
    let text_name = add_name(&mut shstrtab, ".text");
    let rela_name = if has_rela {
        add_name(&mut shstrtab, ".rela.text")
    } else {
        0
    };
    let mut sec_name: Vec<u32> = Vec::with_capacity(sections.len());
    let mut sec_rela_name: Vec<u32> = Vec::with_capacity(sections.len());
    for (i, sec) in sections.iter().enumerate() {
        sec_name.push(add_name(&mut shstrtab, sec.name));
        sec_rela_name.push(if sec_rela_idx[i].is_some() {
            let at = shstrtab.len() as u32;
            shstrtab.extend_from_slice(b".rela");
            shstrtab.extend_from_slice(sec.name.as_bytes());
            shstrtab.push(0);
            at
        } else {
            0
        });
    }
    let symtab_name = add_name(&mut shstrtab, ".symtab");
    let strtab_name = add_name(&mut shstrtab, ".strtab");
    let shstrtab_name = add_name(&mut shstrtab, ".shstrtab");

    let text_off = EHDR_SIZE;
    let mut cursor = text_off + text.len() as u32;
    let rela_off = align4(cursor);
    if has_rela {
        cursor = rela_off + rela.len() as u32;
    }
    let mut sec_off: Vec<u32> = Vec::with_capacity(sections.len());
    let mut sec_rela_off: Vec<u32> = Vec::with_capacity(sections.len());
    for (i, sec) in sections.iter().enumerate() {
        let at = align_up(cursor, sec.addralign.max(1));
        sec_off.push(at);
        cursor = at + sec.data.len() as u32;
        let rela_at = align4(cursor);
        sec_rela_off.push(rela_at);
        if sec_rela_idx[i].is_some() {
            cursor = rela_at + sec_rela[i].len() as u32;
        }
    }
    let symtab_off = align4(cursor);
    let strtab_off = symtab_off + symtab.len() as u32;
    let shstrtab_off = strtab_off + strtab.len() as u32;
    let shoff = align4(shstrtab_off + shstrtab.len() as u32);

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(&[0x7f, b'E', b'L', b'F']);
    out.extend_from_slice(&[1, 1, 1, 0]);
    out.extend_from_slice(&[0u8; 8]);
    push_u16(&mut out, 1);
    push_u16(&mut out, machine.e_machine());
    push_u32(&mut out, 1);
    push_u32(&mut out, 0);
    push_u32(&mut out, 0);
    push_u32(&mut out, shoff);
    push_u32(&mut out, 0);
    push_u16(&mut out, EHDR_SIZE as u16);
    push_u16(&mut out, 0);
    push_u16(&mut out, 0);
    push_u16(&mut out, SHDR_SIZE);
    push_u16(&mut out, section_count);
    push_u16(&mut out, shstrtab_idx);
    out.extend_from_slice(text);
    if has_rela {
        pad_to(&mut out, rela_off);
        out.extend_from_slice(&rela);
    }
    for (i, sec) in sections.iter().enumerate() {
        pad_to(&mut out, sec_off[i]);
        out.extend_from_slice(sec.data);
        if sec_rela_idx[i].is_some() {
            pad_to(&mut out, sec_rela_off[i]);
            out.extend_from_slice(&sec_rela[i]);
        }
    }
    pad_to(&mut out, symtab_off);
    out.extend_from_slice(&symtab);
    out.extend_from_slice(&strtab);
    out.extend_from_slice(&shstrtab);
    pad_to(&mut out, shoff);
    push_shdr(&mut out, &Shdr::null());
    push_shdr(
        &mut out,
        &Shdr {
            name: text_name,
            typ: 1,
            flags: 0x2 | 0x4,
            addr: 0,
            offset: text_off,
            size: text.len() as u32,
            link: 0,
            info: 0,
            addralign: 4,
            entsize: 0,
        },
    );
    if has_rela {
        push_shdr(
            &mut out,
            &Shdr {
                name: rela_name,
                typ: 4,
                flags: 0,
                addr: 0,
                offset: rela_off,
                size: rela.len() as u32,
                link: symtab_idx,
                info: TEXT_SHNDX as u32,
                addralign: 4,
                entsize: RELA_SIZE as u32,
            },
        );
    }
    for (i, sec) in sections.iter().enumerate() {
        push_shdr(
            &mut out,
            &Shdr {
                name: sec_name[i],
                typ: 1,
                flags: sec.flags,
                addr: 0,
                offset: sec_off[i],
                size: sec.data.len() as u32,
                link: 0,
                info: 0,
                addralign: sec.addralign.max(1),
                entsize: 0,
            },
        );
        if sec_rela_idx[i].is_some() {
            push_shdr(
                &mut out,
                &Shdr {
                    name: sec_rela_name[i],
                    typ: 4,
                    flags: 0,
                    addr: 0,
                    offset: sec_rela_off[i],
                    size: sec_rela[i].len() as u32,
                    link: symtab_idx,
                    info: sec_idx[i],
                    addralign: 4,
                    entsize: RELA_SIZE as u32,
                },
            );
        }
    }
    push_shdr(
        &mut out,
        &Shdr {
            name: symtab_name,
            typ: 2,
            flags: 0,
            addr: 0,
            offset: symtab_off,
            size: symtab.len() as u32,
            link: strtab_idx,
            info: first_global,
            addralign: 4,
            entsize: SYM_SIZE as u32,
        },
    );
    push_shdr(
        &mut out,
        &Shdr {
            name: strtab_name,
            typ: 3,
            flags: 0,
            addr: 0,
            offset: strtab_off,
            size: strtab.len() as u32,
            link: 0,
            info: 0,
            addralign: 1,
            entsize: 0,
        },
    );
    push_shdr(
        &mut out,
        &Shdr {
            name: shstrtab_name,
            typ: 3,
            flags: 0,
            addr: 0,
            offset: shstrtab_off,
            size: shstrtab.len() as u32,
            link: 0,
            info: 0,
            addralign: 1,
            entsize: 0,
        },
    );
    out
}

/// Encodes a run of relocations as `Elf32_Rela` entries: `r_offset`, `r_info` = (symbol << 8) | type,
/// `r_addend`. `final_index` maps a caller-facing symbol index to its post-reorder symtab index.
fn encode_rela(relocations: &[Relocation], final_index: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(relocations.len() * RELA_SIZE);
    for r in relocations {
        let r_info = (final_index[r.symbol as usize] << 8) | (r.kind & 0xff);
        push_u32(&mut out, r.offset);
        push_u32(&mut out, r_info);
        push_u32(&mut out, r.addend as u32);
    }
    out
}

/// The file offset (and, since the file maps at `base`, the `base`-relative virtual offset) of
/// `.text` in a [`write_executable`] image: the 52-byte ELF header plus the 32-byte program header.
/// So `.text` offset 0 lives at virtual address `base + EXEC_TEXT_OFFSET` -- what an absolute
/// relocation needs (`lamella_link::link_at_base`).
pub const EXEC_TEXT_OFFSET: u32 = EHDR_SIZE + 32;

/// Emits a minimal ELF32 EXECUTABLE (`ET_EXEC`): one `PT_LOAD` segment mapping the whole file at
/// `base` (read + execute), with `e_entry` at `base + headers + entry_offset`. Runnable under a
/// user-mode loader (e.g. `qemu-<arch>`). The linked `text` must be correct for this `base` --
/// PC-relative code (what `lamella_link` produces) is, regardless of `base`; absolute relocations need
/// the matching `lamella_link::link_at_base`. `base` must be page-aligned (a multiple of `p_align`
/// = 0x1000) so the file-offset-0 mapping satisfies the loader.
pub fn write_executable(machine: Machine, text: &[u8], entry_offset: u32, base: u32) -> Vec<u8> {
    write_executable_impl(machine, text, entry_offset, base, false, None)
}

/// As [`write_executable`], but for an ARM Thumb entry: `e_entry` gets its low bit set so the loader
/// (the Linux/`qemu-arm` ELF loader keys ARM-vs-Thumb start state off `e_entry & 1`) enters Thumb
/// state. The AArch32 code generator emits Thumb (thumbv6m), so a hosted ARM image starts here.
pub fn write_executable_arm_thumb(text: &[u8], entry_offset: u32, base: u32) -> Vec<u8> {
    write_executable_impl(Machine::Arm, text, entry_offset, base, true, None)
}

/// As [`write_executable_arm_thumb`], but the load segment is WRITABLE and extends to cover a
/// zero-filled heap region at `base + heap_offset` of `heap_size` bytes (a `.bss`-style reservation).
/// A program with a fixed-address bump allocator (the runtime-allocator stand-in) writes its objects
/// there. `heap_offset` must clear the code, which sits right after the headers.
pub fn write_executable_arm_thumb_with_heap(
    text: &[u8],
    entry_offset: u32,
    base: u32,
    heap_offset: u32,
    heap_size: u32,
) -> Vec<u8> {
    write_executable_impl(
        Machine::Arm,
        text,
        entry_offset,
        base,
        true,
        Some((heap_offset, heap_size)),
    )
}

/// Emits an `ET_EXEC` like [`write_executable`], but WITH a section table carrying the linked
/// `.debug_*` sections -- the artifact a debugger opens.
///
/// This is the outlet for the linker's DWARF passthrough: `lamella_link::LinkedImage` comes back
/// with the debug sections concatenated and relocated, and until they are written into a container
/// with section headers, nothing can read them. The loaded image is UNAFFECTED -- the `PT_LOAD`
/// segment still covers only the headers plus `.text`, so the debug bytes ride along in the file
/// and cost the target nothing. Flash the same `.text`; hand a debugger this.
///
/// `debug` is `(section name, bytes)`, taken straight from `LinkedImage::debug_sections`.
/// `entry_thumb` sets `e_entry`'s low bit, as [`write_executable_arm_thumb`] does.
///
/// No `.symtab` is emitted. DWARF already carries the function names and addresses a source-level
/// debugger needs, and a PARTIAL symbol table would be worse than none on ARM: correct disassembly
/// depends on the `$t`/`$a` mapping symbols, which are per-object locals the linker does not retain.
pub fn write_debuggable_executable(
    machine: Machine,
    text: &[u8],
    entry_offset: u32,
    base: u32,
    entry_thumb: bool,
    debug: &[(&str, &[u8])],
) -> Vec<u8> {
    const PHDR_SIZE: u32 = 32;
    let text_off = EHDR_SIZE + PHDR_SIZE;
    let loaded = text_off + text.len() as u32;
    let entry = (base + text_off + entry_offset) | entry_thumb as u32;

    let mut shstrtab: Vec<u8> = alloc::vec![0];
    let text_name = add_name(&mut shstrtab, ".text");
    let debug_names: Vec<u32> = debug
        .iter()
        .map(|(n, _)| add_name(&mut shstrtab, n))
        .collect();
    let shstrtab_name = add_name(&mut shstrtab, ".shstrtab");

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(&[0x7f, b'E', b'L', b'F']);
    out.extend_from_slice(&[1, 1, 1, 0]);
    out.extend_from_slice(&[0u8; 8]);
    push_u16(&mut out, 2);
    push_u16(&mut out, machine.e_machine());
    push_u32(&mut out, 1);
    push_u32(&mut out, entry);
    push_u32(&mut out, EHDR_SIZE);
    let e_shoff_at = out.len();
    push_u32(&mut out, 0);
    push_u32(&mut out, 0);
    push_u16(&mut out, EHDR_SIZE as u16);
    push_u16(&mut out, PHDR_SIZE as u16);
    push_u16(&mut out, 1);
    push_u16(&mut out, SHDR_SIZE);
    push_u16(&mut out, debug.len() as u16 + 3);
    push_u16(&mut out, debug.len() as u16 + 2);
    push_u32(&mut out, 1);
    push_u32(&mut out, 0);
    push_u32(&mut out, base);
    push_u32(&mut out, base);
    push_u32(&mut out, loaded);
    push_u32(&mut out, loaded);
    push_u32(&mut out, 0x4 | 0x1);
    push_u32(&mut out, 0x1000);

    out.extend_from_slice(text);
    let mut debug_at: Vec<u32> = Vec::with_capacity(debug.len());
    for (_, data) in debug {
        let at = align4(out.len() as u32);
        pad_to(&mut out, at);
        debug_at.push(at);
        out.extend_from_slice(data);
    }
    let shstrtab_off = align4(out.len() as u32);
    pad_to(&mut out, shstrtab_off);
    out.extend_from_slice(&shstrtab);
    let shoff = align4(out.len() as u32);
    pad_to(&mut out, shoff);
    out[e_shoff_at..e_shoff_at + 4].copy_from_slice(&shoff.to_le_bytes());

    push_shdr(&mut out, &Shdr::null());
    push_shdr(
        &mut out,
        &Shdr {
            name: text_name,
            typ: 1,
            flags: 0x2 | 0x4,
            addr: base + text_off,
            offset: text_off,
            size: text.len() as u32,
            link: 0,
            info: 0,
            addralign: 4,
            entsize: 0,
        },
    );
    for (i, (_, data)) in debug.iter().enumerate() {
        push_shdr(
            &mut out,
            &Shdr {
                name: debug_names[i],
                typ: 1,
                flags: 0,
                addr: 0,
                offset: debug_at[i],
                size: data.len() as u32,
                link: 0,
                info: 0,
                addralign: 1,
                entsize: 0,
            },
        );
    }
    push_shdr(
        &mut out,
        &Shdr {
            name: shstrtab_name,
            typ: 3,
            flags: 0,
            addr: 0,
            offset: shstrtab_off,
            size: shstrtab.len() as u32,
            link: 0,
            info: 0,
            addralign: 1,
            entsize: 0,
        },
    );
    out
}

fn write_executable_impl(
    machine: Machine,
    text: &[u8],
    entry_offset: u32,
    base: u32,
    entry_thumb: bool,
    heap: Option<(u32, u32)>,
) -> Vec<u8> {
    const PHDR_SIZE: u32 = 32;
    let text_off = EHDR_SIZE + PHDR_SIZE;
    let total = text_off + text.len() as u32;
    let entry = (base + text_off + entry_offset) | entry_thumb as u32;

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(&[0x7f, b'E', b'L', b'F']);
    out.extend_from_slice(&[1, 1, 1, 0]);
    out.extend_from_slice(&[0u8; 8]);
    push_u16(&mut out, 2);
    push_u16(&mut out, machine.e_machine());
    push_u32(&mut out, 1);
    push_u32(&mut out, entry);
    push_u32(&mut out, EHDR_SIZE);
    push_u32(&mut out, 0);
    push_u32(&mut out, 0);
    push_u16(&mut out, EHDR_SIZE as u16);
    push_u16(&mut out, PHDR_SIZE as u16);
    push_u16(&mut out, 1);
    push_u16(&mut out, 0);
    push_u16(&mut out, 0);
    push_u16(&mut out, 0);
    push_u32(&mut out, 1);
    push_u32(&mut out, 0);
    push_u32(&mut out, base);
    push_u32(&mut out, base);
    let (memsz, flags) = match heap {
        Some((offset, size)) => (offset + size, 0x4 | 0x2 | 0x1),
        None => (total, 0x4 | 0x1),
    };
    push_u32(&mut out, total);
    push_u32(&mut out, memsz);
    push_u32(&mut out, flags);
    push_u32(&mut out, 0x1000);
    out.extend_from_slice(text);
    out
}

fn push_u16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn push_u32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn align4(x: u32) -> u32 {
    (x + 3) & !3
}

/// Rounds `x` up to a multiple of `align`, which must be a power of two and at least 1.
fn align_up(x: u32, align: u32) -> u32 {
    x.div_ceil(align) * align
}

fn pad_to(v: &mut Vec<u8>, off: u32) {
    while (v.len() as u32) < off {
        v.push(0);
    }
}

/// Appends a NUL-terminated name to a string table and returns its starting offset.
fn add_name(strtab: &mut Vec<u8>, name: &str) -> u32 {
    let off = strtab.len() as u32;
    strtab.extend_from_slice(name.as_bytes());
    strtab.push(0);
    off
}

/// Builds one 16-byte `Elf32_Sym`.
fn sym_entry(
    st_name: u32,
    st_value: u32,
    st_size: u32,
    st_info: u8,
    st_shndx: u16,
) -> [u8; SYM_SIZE] {
    let mut e = [0u8; SYM_SIZE];
    e[0..4].copy_from_slice(&st_name.to_le_bytes());
    e[4..8].copy_from_slice(&st_value.to_le_bytes());
    e[8..12].copy_from_slice(&st_size.to_le_bytes());
    e[12] = st_info;
    e[13] = 0;
    e[14..16].copy_from_slice(&st_shndx.to_le_bytes());
    e
}

/// The fields of one `Elf32_Shdr` we set. `addr` is 0 in a relocatable object (nothing is placed
/// yet) and in any non-allocated section; an executable's `.text` carries its load address there.
struct Shdr {
    name: u32,
    typ: u32,
    flags: u32,
    addr: u32,
    offset: u32,
    size: u32,
    link: u32,
    info: u32,
    addralign: u32,
    entsize: u32,
}

impl Shdr {
    fn null() -> Shdr {
        Shdr {
            name: 0,
            typ: 0,
            flags: 0,
            addr: 0,
            offset: 0,
            size: 0,
            link: 0,
            info: 0,
            addralign: 0,
            entsize: 0,
        }
    }
}

fn push_shdr(v: &mut Vec<u8>, s: &Shdr) {
    push_u32(v, s.name);
    push_u32(v, s.typ);
    push_u32(v, s.flags);
    push_u32(v, s.addr);
    push_u32(v, s.offset);
    push_u32(v, s.size);
    push_u32(v, s.link);
    push_u32(v, s.info);
    push_u32(v, s.addralign);
    push_u32(v, s.entsize);
}

/// An error parsing an ELF object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    /// Not an ELF32, little-endian, relocatable object (bad magic / class / data / `e_type`).
    NotRelocatableElf32,
    /// The machine is not one this crate knows.
    UnknownMachine,
    /// A header, section, or table runs past the end of the input.
    Truncated,
    /// The object has no `.symtab`.
    MissingSymbolTable,
    /// Not a `!<arch>` archive (bad magic).
    NotArchive,
    /// A malformed archive member header (bad terminator, a non-decimal size, a dangling long name).
    BadArchive,
    /// Not an ELF32, little-endian EXECUTABLE (`ET_EXEC`) -- [`flat_image`] was handed something
    /// else, most often a relocatable object that has not been linked yet.
    NotExecutableElf32,
    /// The executable declares no loadable bytes: either no `PT_LOAD` segment at all, or only
    /// segments whose file size is zero. See [`flat_image`] for why this is an error.
    NoLoadableContent,
}

/// A linked executable flattened into the bytes a flasher writes, and the address they go at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlatImage {
    /// The lowest physical address any loadable segment claims -- where the image begins in the
    /// part's address space.
    pub base: u32,
    /// The image itself. Gaps between segments are zero-filled, so the bytes are contiguous from
    /// [`FlatImage::base`] and can be written in one pass.
    pub bytes: Vec<u8>,
}

/// Flatten a linked ELF32 executable into the image a flasher writes to the part.
///
/// This is what `objcopy -O binary` produces, computed in-tree so that turning a build into a
/// flashable file needs no external toolchain. Segments are taken by PHYSICAL address (`p_paddr`),
/// which is the one that matters on a microcontroller: a part whose initialized data is copied
/// from flash to RAM at startup declares a virtual address in RAM and a physical address in flash,
/// and it is the flash address the programmer needs.
///
/// # Why an empty image is an error
///
/// A link that produces no loadable content does not report itself as a failure -- the file is a
/// valid ELF, and the tools that read it are happy. Flattening it silently yields zero bytes, and
/// whatever consumes those bytes goes on to write a well-formed artifact that flashes nothing. So
/// this refuses with [`ElfError::NoLoadableContent`] rather than returning an empty image.
///
/// # Examples
///
/// ```
/// # use lamella_elf::{flat_image, write_executable_arm_thumb};
/// let elf = write_executable_arm_thumb(&[0x00, 0xBF, 0x00, 0xBF], 0, 0x1000);
/// let image = flat_image(&elf).unwrap();
/// assert_eq!(image.base, 0x1000);
/// assert!(image.bytes.ends_with(&[0x00, 0xBF, 0x00, 0xBF]));
/// ```
pub fn flat_image(bytes: &[u8]) -> Result<FlatImage, ElfError> {
    const PT_LOAD: u32 = 1;
    const ET_EXEC: u16 = 2;
    const PHDR_SIZE: usize = 32;

    let u16_at = |o: usize| -> Result<u16, ElfError> {
        bytes
            .get(o..o + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .ok_or(ElfError::Truncated)
    };
    let u32_at = |o: usize| -> Result<u32, ElfError> {
        bytes
            .get(o..o + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .ok_or(ElfError::Truncated)
    };

    if bytes.len() < EHDR_SIZE as usize
        || bytes[..4] != [0x7F, b'E', b'L', b'F']
        || bytes[4] != 1
        || bytes[5] != 1
        || u16_at(16)? != ET_EXEC
    {
        return Err(ElfError::NotExecutableElf32);
    }

    let phoff = u32_at(28)? as usize;
    let phentsize = u16_at(42)? as usize;
    let phnum = u16_at(44)? as usize;
    if phentsize < PHDR_SIZE {
        return Err(ElfError::Truncated);
    }

    let mut loads: Vec<(u32, &[u8])> = Vec::new();
    for i in 0..phnum {
        let p = phoff + i * phentsize;
        if u32_at(p)? != PT_LOAD {
            continue;
        }
        let offset = u32_at(p + 4)? as usize;
        let paddr = u32_at(p + 12)?;
        let filesz = u32_at(p + 16)? as usize;
        if filesz == 0 {
            continue;
        }
        let data = bytes
            .get(offset..offset + filesz)
            .ok_or(ElfError::Truncated)?;
        loads.push((paddr, data));
    }
    if loads.is_empty() {
        return Err(ElfError::NoLoadableContent);
    }

    let base = loads.iter().map(|(a, _)| *a).min().expect("non-empty");
    let end = loads
        .iter()
        .map(|(a, d)| u64::from(*a) + d.len() as u64)
        .max()
        .expect("non-empty");
    let span = usize::try_from(end - u64::from(base)).map_err(|_| ElfError::Truncated)?;

    let mut out = Vec::new();
    out.resize(span, 0u8);
    for (paddr, data) in loads {
        let at = (paddr - base) as usize;
        out[at..at + data.len()].copy_from_slice(data);
    }
    Ok(FlatImage { base, bytes: out })
}

/// A symbol parsed from an object's `.symtab`.
#[derive(Debug, Clone)]
pub struct ParsedSymbol {
    /// The symbol name (resolved from `.strtab`).
    pub name: String,
    /// `st_value` -- a defined `.text` symbol's offset within `.text`.
    pub value: u32,
    /// `st_size`.
    pub size: u32,
    /// The binding.
    pub binding: Binding,
    /// The type.
    pub kind: SymbolType,
    /// Whether the symbol is defined here (`st_shndx != SHN_UNDEF`).
    pub defined: bool,
    /// For a symbol defined in a CARRIED section ([`Section`]), its index into [`Object::sections`],
    /// with [`Self::value`] an offset within that section. `None` for a symbol in the merged code
    /// blob (where `value` is a [`Object::text`] offset), an undefined symbol, or an absolute one.
    ///
    /// This is what makes DWARF relocatable at all: a `.debug_*` relocation names its target with a
    /// nameless `STT_SECTION` symbol (`st_name` is 0 -- the section's name lives in the section
    /// header, not the string table), so a purely NAME-keyed linker cannot resolve it. The section
    /// index is the only handle such a symbol has.
    pub section: Option<u32>,
}

/// A relocation parsed from an object's `.rela.text` (explicit addend) or `.rel.text` (implicit).
#[derive(Debug, Clone, Copy)]
pub struct ParsedRelocation {
    /// `r_offset` within `.text`.
    pub offset: u32,
    /// The index into [`Object::symbols`] of the referenced symbol.
    pub symbol: u32,
    /// The relocation type (the low byte of `r_info`).
    pub kind: u32,
    /// `r_addend` (an explicit `RELA` addend; 0 when [`Self::implicit_addend`] is set).
    pub addend: i32,
    /// True for a `SHT_REL` relocation (`.rel.text`, the ARM C toolchain's convention): the addend
    /// is not in this entry but stored in-place in the instruction field, so a consumer that needs
    /// it extracts it from the relocated bytes. False for `SHT_RELA` (the addend is [`Self::addend`]).
    pub implicit_addend: bool,
}

/// A parsed ELF32 relocatable object.
#[derive(Debug, Clone)]
pub struct Object {
    /// The target machine.
    pub machine: Machine,
    /// The `.text` section bytes.
    pub text: Vec<u8>,
    /// `.text`'s `sh_addralign` -- the byte alignment the linker must give this object's code (4 for
    /// RISC-V and for this crate's own output, 2 for an ARM `-mthumb` toolchain's `.text`). 1 if
    /// absent.
    pub text_align: u32,
    /// The symbols, in symbol-table order (index 0 is the null symbol).
    pub symbols: Vec<ParsedSymbol>,
    /// The `.text` relocations.
    pub relocations: Vec<ParsedRelocation>,
    /// Sections carried through the link rather than merged into [`Self::text`] -- today the DWARF
    /// `.debug_*` family (see [`Section`]). Empty for an object with no debug info, which is every
    /// object the AOT backend emits today, so the code path costs nothing when it is not in use.
    pub sections: Vec<ParsedSection>,
}

fn rd_u16(bytes: &[u8], o: usize) -> Result<u16, ElfError> {
    bytes
        .get(o..o + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or(ElfError::Truncated)
}

fn rd_u32(bytes: &[u8], o: usize) -> Result<u32, ElfError> {
    bytes
        .get(o..o + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or(ElfError::Truncated)
}

fn rd_cstr(bytes: &[u8], o: usize) -> Result<&str, ElfError> {
    let rest = bytes.get(o..).ok_or(ElfError::Truncated)?;
    let end = rest
        .iter()
        .position(|&b| b == 0)
        .ok_or(ElfError::Truncated)?;
    core::str::from_utf8(&rest[..end]).map_err(|_| ElfError::Truncated)
}

const SH_NAME: usize = 0;
const SH_TYPE: usize = 4;
const SH_FLAGS: usize = 8;
const SH_OFFSET: usize = 16;
const SH_SIZE: usize = 20;
const SH_LINK: usize = 24;
const SH_INFO: usize = 28;
const SH_ADDRALIGN: usize = 32;
const SHF_WRITE: u32 = 0x1;
const SHF_ALLOC: u32 = 0x2;

/// Parses an ELF32 little-endian relocatable object (as written by [`write_relocatable_object`],
/// and, later, a C toolchain): the `.text` bytes, the symbol table (names resolved), and the
/// `.rela.text` relocations.
pub fn read_object(bytes: &[u8]) -> Result<Object, ElfError> {
    if bytes.len() < EHDR_SIZE as usize
        || bytes[0..4] != [0x7f, b'E', b'L', b'F']
        || bytes[4] != 1
        || bytes[5] != 1
    {
        return Err(ElfError::NotRelocatableElf32);
    }
    if rd_u16(bytes, 16)? != 1 {
        return Err(ElfError::NotRelocatableElf32);
    }
    let machine = match rd_u16(bytes, 18)? {
        243 => Machine::RiscV,
        40 => Machine::Arm,
        _ => return Err(ElfError::UnknownMachine),
    };
    let e_shoff = rd_u32(bytes, 32)? as usize;
    let e_shnum = rd_u16(bytes, 48)? as usize;
    let sh = |i: usize, field: usize| rd_u32(bytes, e_shoff + i * 40 + field);
    let shstrtab_off = sh(rd_u16(bytes, 50)? as usize, SH_OFFSET)? as usize;
    let sec_name = |i: usize| rd_cstr(bytes, shstrtab_off + sh(i, SH_NAME)? as usize);
    let is_unwind_metadata =
        |name: &str| name.starts_with(".eh_frame") || name.starts_with(".ARM.ex");

    let mut symtab_i = None;
    let mut section_base: Vec<Option<u32>> = Vec::new();
    section_base.resize(e_shnum, None);
    let mut text: Vec<u8> = Vec::new();
    let mut text_align = 1u32;
    #[allow(clippy::needless_range_loop)]
    for i in 0..e_shnum {
        if sh(i, SH_TYPE)? == 2 {
            symtab_i = Some(i);
        }
        let flags = sh(i, SH_FLAGS)?;
        let merge = sh(i, SH_TYPE)? == 1
            && flags & SHF_ALLOC != 0
            && flags & SHF_WRITE == 0
            && !is_unwind_metadata(sec_name(i)?);
        if !merge {
            continue;
        }
        let align = sh(i, SH_ADDRALIGN)?.max(1);
        text_align = text_align.max(align);
        while text.len() as u32 % align != 0 {
            text.push(0);
        }
        section_base[i] = Some(text.len() as u32);
        let off = sh(i, SH_OFFSET)? as usize;
        let size = sh(i, SH_SIZE)? as usize;
        text.extend_from_slice(bytes.get(off..off + size).ok_or(ElfError::Truncated)?);
    }
    let symtab_i = symtab_i.ok_or(ElfError::MissingSymbolTable)?;

    let mut sections: Vec<ParsedSection> = Vec::new();
    let mut carried_of: Vec<Option<u32>> = Vec::new();
    carried_of.resize(e_shnum, None);
    #[allow(clippy::needless_range_loop)]
    for i in 0..e_shnum {
        let name = sec_name(i)?;
        if sh(i, SH_TYPE)? != 1
            || sh(i, SH_FLAGS)? & SHF_ALLOC != 0
            || !is_carried_section(name)
        {
            continue;
        }
        let off = sh(i, SH_OFFSET)? as usize;
        let size = sh(i, SH_SIZE)? as usize;
        carried_of[i] = Some(sections.len() as u32);
        sections.push(ParsedSection {
            name: String::from(name),
            flags: sh(i, SH_FLAGS)?,
            addralign: sh(i, SH_ADDRALIGN)?.max(1),
            data: bytes
                .get(off..off + size)
                .ok_or(ElfError::Truncated)?
                .to_vec(),
            relocations: Vec::new(),
        });
    }

    let strtab_off = sh(sh(symtab_i, SH_LINK)? as usize, SH_OFFSET)? as usize;
    let symtab_off = sh(symtab_i, SH_OFFSET)? as usize;
    let symtab_size = sh(symtab_i, SH_SIZE)? as usize;
    let mut symbols = Vec::new();
    for s in 0..symtab_size / SYM_SIZE {
        let base = symtab_off + s * SYM_SIZE;
        let st_name = rd_u32(bytes, base)? as usize;
        let st_value = rd_u32(bytes, base + 4)?;
        let st_size = rd_u32(bytes, base + 8)?;
        let st_info = *bytes.get(base + 12).ok_or(ElfError::Truncated)?;
        let st_shndx = rd_u16(bytes, base + 14)?;
        let binding = match st_info >> 4 {
            1 => Binding::Global,
            2 => Binding::Weak,
            _ => Binding::Local,
        };
        let kind = match st_info & 0xf {
            2 => SymbolType::Func,
            3 => SymbolType::Section,
            _ => SymbolType::NoType,
        };
        let carried = carried_of.get(st_shndx as usize).copied().flatten();
        let rebase = match carried {
            Some(_) => 0,
            None => section_base
                .get(st_shndx as usize)
                .copied()
                .flatten()
                .unwrap_or(0),
        };
        symbols.push(ParsedSymbol {
            name: String::from(rd_cstr(bytes, strtab_off + st_name)?),
            value: st_value + rebase,
            size: st_size,
            binding,
            kind,
            defined: st_shndx != SHN_UNDEF,
            section: carried,
        });
    }

    let mut relocations = Vec::new();
    for ri in 0..e_shnum {
        let implicit = match sh(ri, SH_TYPE)? {
            4 => false,
            9 => true,
            _ => continue,
        };
        let applies_to = sh(ri, SH_INFO)? as usize;
        let target = match section_base.get(applies_to).copied().flatten() {
            Some(base) => RelocTarget::Text(base),
            None => match carried_of.get(applies_to).copied().flatten() {
                Some(idx) => RelocTarget::Carried(idx),
                None => continue,
            },
        };
        let off = sh(ri, SH_OFFSET)? as usize;
        let size = sh(ri, SH_SIZE)? as usize;
        let entsize = if implicit { REL_SIZE } else { RELA_SIZE };
        for r in 0..size / entsize {
            let base = off + r * entsize;
            let r_info = rd_u32(bytes, base + 4)?;
            let parsed = ParsedRelocation {
                offset: rd_u32(bytes, base)?
                    + match target {
                        RelocTarget::Text(b) => b,
                        RelocTarget::Carried(_) => 0,
                    },
                symbol: r_info >> 8,
                kind: r_info & 0xff,
                addend: if implicit {
                    0
                } else {
                    rd_u32(bytes, base + 8)? as i32
                },
                implicit_addend: implicit,
            };
            match target {
                RelocTarget::Text(_) => relocations.push(parsed),
                RelocTarget::Carried(idx) => sections[idx as usize].relocations.push(parsed),
            }
        }
    }

    Ok(Object {
        machine,
        text,
        text_align,
        symbols,
        relocations,
        sections,
    })
}

/// What a relocation section applies to: the merged code blob (at the given rebase offset) or a
/// carried section (by its [`Object::sections`] index).
#[derive(Clone, Copy)]
enum RelocTarget {
    Text(u32),
    Carried(u32),
}

/// One object member of an archive: its name and the parsed object.
#[derive(Debug, Clone)]
pub struct ArchiveMember {
    /// The member's file name (e.g. `memcpy.o`).
    pub name: String,
    /// The member parsed as an ELF object.
    pub object: Object,
}

/// A parsed `ar` archive (`.a`): its object members, in file order. The symbol-index (`/`) and
/// long-name (`//`) bookkeeping members are consumed during parsing, not exposed.
#[derive(Debug, Clone)]
pub struct Archive {
    /// The object members.
    pub members: Vec<ArchiveMember>,
}

const AR_MAGIC: &[u8] = b"!<arch>\n";
const AR_HEADER_SIZE: usize = 60;

/// Parses a System V / GNU `ar` archive: the `!<arch>` magic, then 60-byte member headers each
/// followed by an even-padded payload. The `/` symbol index is skipped (this crate scans each
/// member's own symbol table); the `//` long-name table resolves members named `/<offset>`. Every
/// other member is parsed as an ELF object. (Thin archives, which reference external files, are not
/// supported.)
pub fn read_archive(bytes: &[u8]) -> Result<Archive, ElfError> {
    if bytes.len() < AR_MAGIC.len() || &bytes[..AR_MAGIC.len()] != AR_MAGIC {
        return Err(ElfError::NotArchive);
    }
    let mut pos = AR_MAGIC.len();
    let mut long_names: Vec<u8> = Vec::new();
    let mut members = Vec::new();
    while pos + AR_HEADER_SIZE <= bytes.len() {
        let header = &bytes[pos..pos + AR_HEADER_SIZE];
        if &header[58..60] != b"\x60\x0a" {
            return Err(ElfError::BadArchive);
        }
        let size = parse_ar_decimal(&header[48..58])?;
        let data_start = pos + AR_HEADER_SIZE;
        let data = bytes
            .get(data_start..data_start + size)
            .ok_or(ElfError::Truncated)?;
        let raw_name = trim_ar_field(&header[0..16]);
        if raw_name == b"/" || raw_name == b"/SYM64/" {
        } else if raw_name == b"//" {
            long_names = data.to_vec();
        } else if let Ok(object) = read_object(data) {
            let name = resolve_ar_name(raw_name, &long_names)?;
            members.push(ArchiveMember { name, object });
        }
        pos = data_start + size + (size & 1);
    }
    Ok(Archive { members })
}

/// Trims trailing spaces from a fixed-width `ar` header field.
fn trim_ar_field(field: &[u8]) -> &[u8] {
    let end = field.iter().rposition(|&b| b != b' ').map_or(0, |p| p + 1);
    &field[..end]
}

/// Parses a space-padded ASCII decimal `ar` header field (the member size).
fn parse_ar_decimal(field: &[u8]) -> Result<usize, ElfError> {
    let digits = trim_ar_field(field);
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(ElfError::BadArchive);
    }
    Ok(digits
        .iter()
        .fold(0usize, |n, &b| n * 10 + (b - b'0') as usize))
}

/// Resolves a member name: a `/<offset>` reference into the `//` long-name table, or a short name
/// with its GNU trailing `/` stripped.
fn resolve_ar_name(raw: &[u8], long_names: &[u8]) -> Result<String, ElfError> {
    if raw.len() > 1 && raw[0] == b'/' && raw[1..].iter().all(u8::is_ascii_digit) {
        let offset = parse_ar_decimal(&raw[1..])?;
        let rest = long_names.get(offset..).ok_or(ElfError::BadArchive)?;
        let end = rest
            .iter()
            .position(|&b| b == b'/' || b == b'\n')
            .unwrap_or(rest.len());
        return Ok(String::from_utf8_lossy(&rest[..end]).into_owned());
    }
    let name = raw.strip_suffix(b"/").unwrap_or(raw);
    Ok(String::from_utf8_lossy(name).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `sh_addr`'s byte offset in an `Elf32_Shdr`. Only the tests read it: a relocatable object's
    /// is always 0, so the parser has no use for it.
    const SH_ADDR: usize = 12;

    #[test]
    fn emits_a_well_formed_relocatable_object() {
        let text = [0x13, 0x05, 0xa0, 0x02, 0x67, 0x80, 0x00, 0x00];
        let obj = write_relocatable_object(
            Machine::RiscV,
            &text,
            &[Symbol {
                name: "answer",
                value: 0,
                size: text.len() as u32,
                binding: Binding::Global,
                kind: SymbolType::Func,
                section: SymbolSection::Text,
            }],
            &[],
        );
        assert_eq!(&obj[0..4], &[0x7f, b'E', b'L', b'F']);
        assert_eq!([obj[4], obj[5], obj[6]], [1, 1, 1]);
        assert_eq!(u16::from_le_bytes([obj[16], obj[17]]), 1);
        assert_eq!(u16::from_le_bytes([obj[18], obj[19]]), 243);
        assert_eq!(u16::from_le_bytes([obj[40], obj[41]]), 52);
        assert_eq!(u16::from_le_bytes([obj[46], obj[47]]), 40);
        assert_eq!(u16::from_le_bytes([obj[48], obj[49]]), 5);
        assert_eq!(u16::from_le_bytes([obj[50], obj[51]]), 4);
        assert_eq!(&obj[52..52 + text.len()], &text);
        let shoff = u32::from_le_bytes([obj[32], obj[33], obj[34], obj[35]]) as usize;
        assert_eq!(obj.len(), shoff + 5 * 40);
    }

    #[test]
    fn an_external_call_emits_a_rela_text_relocation() {
        let text = [0x97, 0x00, 0x00, 0x00, 0xe7, 0x80, 0x00, 0x00];
        let obj = write_relocatable_object(
            Machine::RiscV,
            &text,
            &[
                Symbol {
                    name: "caller",
                    value: 0,
                    size: text.len() as u32,
                    binding: Binding::Global,
                    kind: SymbolType::Func,
                    section: SymbolSection::Text,
                },
                Symbol {
                    name: "callee",
                    value: 0,
                    size: 0,
                    binding: Binding::Global,
                    kind: SymbolType::NoType,
                    section: SymbolSection::Undefined,
                },
            ],
            &[Relocation {
                offset: 0,
                symbol: 1,
                kind: riscv::R_RISCV_CALL_PLT,
                addend: 0,
            }],
        );
        assert_eq!(u16::from_le_bytes([obj[48], obj[49]]), 6);
        assert_eq!(u16::from_le_bytes([obj[50], obj[51]]), 5);
        let r_offset = u32::from_le_bytes([obj[60], obj[61], obj[62], obj[63]]);
        let r_info = u32::from_le_bytes([obj[64], obj[65], obj[66], obj[67]]);
        assert_eq!(r_offset, 0);
        assert_eq!(r_info >> 8, 2);
        assert_eq!(r_info & 0xff, riscv::R_RISCV_CALL_PLT);
    }

    #[test]
    fn read_object_round_trips_the_writer() {
        let text = [0x97, 0x00, 0x00, 0x00, 0xe7, 0x80, 0x00, 0x00];
        let obj_bytes = write_relocatable_object(
            Machine::RiscV,
            &text,
            &[
                Symbol {
                    name: "caller",
                    value: 0,
                    size: 8,
                    binding: Binding::Global,
                    kind: SymbolType::Func,
                    section: SymbolSection::Text,
                },
                Symbol {
                    name: "callee",
                    value: 0,
                    size: 0,
                    binding: Binding::Global,
                    kind: SymbolType::NoType,
                    section: SymbolSection::Undefined,
                },
            ],
            &[Relocation {
                offset: 0,
                symbol: 1,
                kind: riscv::R_RISCV_CALL_PLT,
                addend: 0,
            }],
        );
        let obj = read_object(&obj_bytes).unwrap();
        assert_eq!(obj.machine, Machine::RiscV);
        assert_eq!(obj.text, text);
        assert_eq!(obj.symbols.len(), 3);
        assert_eq!(obj.symbols[1].name, "caller");
        assert!(obj.symbols[1].defined);
        assert_eq!(obj.symbols[1].kind, SymbolType::Func);
        assert_eq!(obj.symbols[2].name, "callee");
        assert!(!obj.symbols[2].defined);
        assert_eq!(obj.relocations.len(), 1);
        assert_eq!(obj.relocations[0].offset, 0);
        assert_eq!(obj.relocations[0].symbol, 2);
        assert_eq!(obj.relocations[0].kind, riscv::R_RISCV_CALL_PLT);
    }

    #[test]
    fn write_executable_is_a_valid_et_exec() {
        let text = [0x13, 0x05, 0xa0, 0x02, 0x67, 0x80, 0x00, 0x00];
        let exe = write_executable(Machine::RiscV, &text, 0, 0x1_0000);
        assert_eq!(u16::from_le_bytes([exe[16], exe[17]]), 2);
        assert_eq!(u16::from_le_bytes([exe[18], exe[19]]), 243);
        assert_eq!(u16::from_le_bytes([exe[44], exe[45]]), 1);
        assert_eq!(
            u32::from_le_bytes([exe[24], exe[25], exe[26], exe[27]]),
            0x1_0000 + 84
        );
        assert_eq!(u32::from_le_bytes([exe[52], exe[53], exe[54], exe[55]]), 1);
        assert_eq!(
            u32::from_le_bytes([exe[60], exe[61], exe[62], exe[63]]),
            0x1_0000
        );
        assert_eq!(&exe[84..84 + text.len()], &text);
    }

    #[test]
    fn write_executable_arm_thumb_sets_the_entry_thumb_bit() {
        let text = [0x2a, 0x20, 0x70, 0x47];
        let exe = write_executable_arm_thumb(&text, 0, 0x1_0000);
        assert_eq!(u16::from_le_bytes([exe[16], exe[17]]), 2);
        assert_eq!(u16::from_le_bytes([exe[18], exe[19]]), 40);
        assert_eq!(
            u32::from_le_bytes([exe[24], exe[25], exe[26], exe[27]]),
            (0x1_0000 + 84) | 1
        );
    }

    /// Wraps `members` in a minimal GNU `ar` archive (short names; the mtime/uid/gid/mode header
    /// fields stay spaces, which the reader ignores).
    fn make_archive(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(b"!<arch>\n");
        for (name, data) in members {
            let mut header = [b' '; 60];
            header[..name.len()].copy_from_slice(name.as_bytes());
            header[name.len()] = b'/';
            let mut size = data.len();
            let mut digits: Vec<u8> = Vec::new();
            loop {
                digits.push(b'0' + (size % 10) as u8);
                size /= 10;
                if size == 0 {
                    break;
                }
            }
            digits.reverse();
            header[48..48 + digits.len()].copy_from_slice(&digits);
            header[58] = 0x60;
            header[59] = 0x0a;
            out.extend_from_slice(&header);
            out.extend_from_slice(data);
            if data.len() % 2 == 1 {
                out.push(b'\n');
            }
        }
        out
    }

    #[test]
    fn read_archive_parses_object_members() {
        let func = |name: &'static str, code: &[u8]| {
            write_relocatable_object(
                Machine::RiscV,
                code,
                &[Symbol {
                    name,
                    value: 0,
                    size: code.len() as u32,
                    binding: Binding::Global,
                    kind: SymbolType::Func,
                    section: SymbolSection::Text,
                }],
                &[],
            )
        };
        let answer = func("answer", &[0x13, 0x05, 0xa0, 0x02]);
        let unused = func("unused", &[0x13, 0x05, 0x00, 0x00, 0x67, 0x80, 0x00]);
        let ar = make_archive(&[("answer.o", &answer), ("unused.o", &unused)]);
        let archive = read_archive(&ar).unwrap();
        assert_eq!(archive.members.len(), 2);
        assert_eq!(archive.members[0].name, "answer.o");
        assert_eq!(archive.members[1].name, "unused.o");
        assert!(
            archive.members[0]
                .object
                .symbols
                .iter()
                .any(|s| s.name == "answer")
        );
        assert!(
            archive.members[1]
                .object
                .symbols
                .iter()
                .any(|s| s.name == "unused")
        );
    }

    #[test]
    fn read_archive_rejects_non_archive() {
        assert_eq!(
            read_archive(b"not an ar").unwrap_err(),
            ElfError::NotArchive
        );
    }

    #[test]
    fn write_debuggable_executable_carries_debug_sections_outside_the_load_segment() {
        let text = [0x2a, 0x20, 0x70, 0x47];
        let info = [0x11u8; 7];
        let abbrev = [0x22u8; 5];
        let exe = write_debuggable_executable(
            Machine::Arm,
            &text,
            0,
            0x1_0000,
            true,
            &[(".debug_info", &info), (".debug_abbrev", &abbrev)],
        );
        assert_eq!(u16::from_le_bytes([exe[16], exe[17]]), 2);
        assert_eq!(u16::from_le_bytes([exe[18], exe[19]]), 40);
        assert_eq!(u16::from_le_bytes([exe[48], exe[49]]), 5);
        assert_eq!(u16::from_le_bytes([exe[50], exe[51]]), 4);
        assert_eq!(
            u32::from_le_bytes([exe[24], exe[25], exe[26], exe[27]]),
            (0x1_0000 + 84) | 1
        );

        let p_filesz = u32::from_le_bytes([exe[68], exe[69], exe[70], exe[71]]);
        let p_memsz = u32::from_le_bytes([exe[72], exe[73], exe[74], exe[75]]);
        assert_eq!(p_filesz, 84 + text.len() as u32);
        assert_eq!(p_memsz, p_filesz);
        assert!(exe.len() as u32 > p_filesz, "debug data follows the segment");

        let shoff = u32::from_le_bytes([exe[32], exe[33], exe[34], exe[35]]) as usize;
        let shdr = |i: usize, field: usize| {
            let o = shoff + i * SHDR_SIZE as usize + field;
            u32::from_le_bytes([exe[o], exe[o + 1], exe[o + 2], exe[o + 3]])
        };
        let shstrtab_off = shdr(4, SH_OFFSET) as usize;
        let name_of = |i: usize| rd_cstr(&exe, shstrtab_off + shdr(i, SH_NAME) as usize).unwrap();

        assert_eq!(name_of(1), ".text");
        assert_eq!(shdr(1, SH_FLAGS), 0x2 | 0x4);
        assert_eq!(shdr(1, SH_ADDR), 0x1_0000 + 84);

        for (i, (name, data)) in [(".debug_info", &info[..]), (".debug_abbrev", &abbrev[..])]
            .iter()
            .enumerate()
        {
            let si = 2 + i;
            assert_eq!(name_of(si), *name);
            assert_eq!(shdr(si, SH_FLAGS), 0, "{name} must not be SHF_ALLOC");
            assert_eq!(shdr(si, SH_ADDR), 0, "{name} must have no address");
            assert_eq!(shdr(si, SH_SIZE) as usize, data.len());
            let off = shdr(si, SH_OFFSET) as usize;
            assert_eq!(&exe[off..off + data.len()], *data, "{name} bytes round-trip");
        }
    }

    #[test]
    fn emitted_debug_sections_round_trip_through_the_reader() {
        let text = [0x13, 0x05, 0xa0, 0x02, 0x67, 0x80, 0x00, 0x00];
        let abbrev = [0x11u8, 0x22, 0x33];
        let info = [0u8; 8];
        let info_relocs = [
            Relocation {
                offset: 0,
                symbol: 1,
                kind: riscv::R_RISCV_32,
                addend: 0,
            },
            Relocation {
                offset: 4,
                symbol: 0,
                kind: riscv::R_RISCV_32,
                addend: 0,
            },
        ];
        let symbols = [
            Symbol {
                name: "answer",
                value: 0,
                size: text.len() as u32,
                binding: Binding::Global,
                kind: SymbolType::Func,
                section: SymbolSection::Text,
            },
            Symbol {
                name: "",
                value: 0,
                size: 0,
                binding: Binding::Local,
                kind: SymbolType::Section,
                section: SymbolSection::InSection(0),
            },
        ];
        let sections = [
            Section {
                name: ".debug_abbrev",
                flags: 0,
                addralign: 1,
                data: &abbrev,
                relocations: &[],
            },
            Section {
                name: ".debug_info",
                flags: 0,
                addralign: 1,
                data: &info,
                relocations: &info_relocs,
            },
        ];
        let obj = read_object(&write_relocatable_object_with_sections(
            Machine::RiscV,
            &text,
            &symbols,
            &[],
            &sections,
        ))
        .expect("an object with emitted debug sections reads back");

        assert_eq!(obj.text, text, ".text is untouched by the debug sections");
        let names: Vec<&str> = obj.sections.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, [".debug_abbrev", ".debug_info"]);
        assert_eq!(obj.sections[0].data, abbrev);
        assert_eq!(obj.sections[1].data, info);
        assert!(
            obj.sections[0].relocations.is_empty(),
            "a section with no relocations gets no .rela"
        );

        let relocs = &obj.sections[1].relocations;
        assert_eq!(relocs.len(), 2);
        assert_eq!(relocs[0].offset, 0);
        assert_eq!(relocs[1].offset, 4);

        let to_abbrev = &obj.symbols[relocs[0].symbol as usize];
        assert_eq!(to_abbrev.name, "", "a section symbol is nameless");
        assert_eq!(to_abbrev.kind, SymbolType::Section);
        assert_eq!(to_abbrev.section, Some(0), "it names `.debug_abbrev`");
        assert!(to_abbrev.defined);

        let to_code = &obj.symbols[relocs[1].symbol as usize];
        assert_eq!(to_code.name, "answer");
        assert_eq!(to_code.kind, SymbolType::Func);
        assert_eq!(
            to_code.section, None,
            "a code symbol is not in a carried section"
        );
    }

    #[test]
    fn a_section_without_relocations_does_not_shift_the_others_indices() {
        let first_relocs = [Relocation {
            offset: 0,
            symbol: 0,
            kind: arm::R_ARM_ABS32,
            addend: 0,
        }];
        let last_relocs = [Relocation {
            offset: 4,
            symbol: 0,
            kind: arm::R_ARM_ABS32,
            addend: 0,
        }];
        let data = [0u8; 8];
        let sections = [
            Section {
                name: ".debug_info",
                flags: 0,
                addralign: 1,
                data: &data,
                relocations: &first_relocs,
            },
            Section {
                name: ".debug_abbrev",
                flags: 0,
                addralign: 1,
                data: &data,
                relocations: &[],
            },
            Section {
                name: ".debug_line",
                flags: 0,
                addralign: 1,
                data: &data,
                relocations: &last_relocs,
            },
        ];
        let obj = read_object(&write_relocatable_object_with_sections(
            Machine::Arm,
            &[0u8; 4],
            &[Symbol {
                name: "f",
                value: 0,
                size: 4,
                binding: Binding::Global,
                kind: SymbolType::Func,
                section: SymbolSection::Text,
            }],
            &[],
            &sections,
        ))
        .expect("an object with a relocation-free middle section reads back");
        let by_name = |n: &str| {
            obj.sections
                .iter()
                .find(|s| s.name == n)
                .unwrap_or_else(|| panic!("no {n}"))
        };
        assert_eq!(by_name(".debug_info").relocations.len(), 1);
        assert_eq!(by_name(".debug_info").relocations[0].offset, 0);
        assert!(by_name(".debug_abbrev").relocations.is_empty());
        assert_eq!(by_name(".debug_line").relocations.len(), 1);
        assert_eq!(by_name(".debug_line").relocations[0].offset, 4);
    }
}
