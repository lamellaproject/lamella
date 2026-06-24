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
}

/// A symbol's type -- the low nibble of `st_info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolType {
    /// `STT_NOTYPE`.
    NoType,
    /// `STT_FUNC` -- a function entry point.
    Func,
}

/// Where a symbol is defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolSection {
    /// Defined in this object's `.text` (`st_shndx` = the `.text` section index).
    Text,
    /// Undefined here -- the linker resolves it from another object (`SHN_UNDEF`).
    Undefined,
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

/// Emits an ELF32 relocatable object (`ET_REL`) holding `text` as `.text`, `symbols` in `.symtab`,
/// and `relocations` in `.rela.text`. `machine` sets `e_machine`; output is little-endian. A
/// relocation's `symbol` indexes the `symbols` slice (the writer maps it to the final symbol-table
/// index). Pass an empty `relocations` for a leaf object with no external references.
pub fn write_relocatable_object(
    machine: Machine,
    text: &[u8],
    symbols: &[Symbol],
    relocations: &[Relocation],
) -> Vec<u8> {
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
            Binding::Global => {
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
            let st_name = strtab.len() as u32;
            strtab.extend_from_slice(sym.name.as_bytes());
            strtab.push(0);
            let bind: u8 = match sym.binding {
                Binding::Local => 0,
                Binding::Global => 1,
            };
            let typ: u8 = match sym.kind {
                SymbolType::NoType => 0,
                SymbolType::Func => 2,
            };
            let st_info = (bind << 4) | (typ & 0xf);
            let st_shndx = match sym.section {
                SymbolSection::Text => TEXT_SHNDX,
                SymbolSection::Undefined => SHN_UNDEF,
            };
            symtab.extend_from_slice(&sym_entry(st_name, sym.value, sym.size, st_info, st_shndx));
        }
    }

    let mut rela: Vec<u8> = Vec::new();
    for r in relocations {
        let r_info = (final_index[r.symbol as usize] << 8) | (r.kind & 0xff);
        push_u32(&mut rela, r.offset);
        push_u32(&mut rela, r_info);
        push_u32(&mut rela, r.addend as u32);
    }

    let has_rela = !relocations.is_empty();
    let rela_idx = 2u32;
    let symtab_idx = if has_rela { 3 } else { 2 };
    let strtab_idx = symtab_idx + 1;
    let shstrtab_idx = (strtab_idx + 1) as u16;
    let section_count = if has_rela { 6u16 } else { 5 };

    let mut shstrtab: Vec<u8> = alloc::vec![0];
    let text_name = add_name(&mut shstrtab, ".text");
    let rela_name = if has_rela {
        add_name(&mut shstrtab, ".rela.text")
    } else {
        0
    };
    let symtab_name = add_name(&mut shstrtab, ".symtab");
    let strtab_name = add_name(&mut shstrtab, ".strtab");
    let shstrtab_name = add_name(&mut shstrtab, ".shstrtab");

    let text_off = EHDR_SIZE;
    let mut cursor = text_off + text.len() as u32;
    let rela_off = align4(cursor);
    if has_rela {
        cursor = rela_off + rela.len() as u32;
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
                offset: rela_off,
                size: rela.len() as u32,
                link: symtab_idx,
                info: TEXT_SHNDX as u32,
                addralign: 4,
                entsize: RELA_SIZE as u32,
            },
        );
    }
    push_shdr(
        &mut out,
        &Shdr {
            name: symtab_name,
            typ: 2,
            flags: 0,
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
            offset: shstrtab_off,
            size: shstrtab.len() as u32,
            link: 0,
            info: 0,
            addralign: 1,
            entsize: 0,
        },
    );
    let _ = rela_idx;
    out
}

/// Emits a minimal ELF32 EXECUTABLE (`ET_EXEC`): one `PT_LOAD` segment mapping the whole file at
/// `base` (read + execute), with `e_entry` at `base + headers + entry_offset`. Runnable under a
/// user-mode loader (e.g. `qemu-<arch>`). The linked `text` must be correct for this `base` --
/// PC-relative code (what our linker produces) is, regardless of `base`. `base` must be page-aligned
/// (a multiple of `p_align` = 0x1000) so the file-offset-0 mapping satisfies the loader.
pub fn write_executable(machine: Machine, text: &[u8], entry_offset: u32, base: u32) -> Vec<u8> {
    const PHDR_SIZE: u32 = 32;
    let text_off = EHDR_SIZE + PHDR_SIZE;
    let total = text_off + text.len() as u32;

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(&[0x7f, b'E', b'L', b'F']);
    out.extend_from_slice(&[1, 1, 1, 0]);
    out.extend_from_slice(&[0u8; 8]);
    push_u16(&mut out, 2);
    push_u16(&mut out, machine.e_machine());
    push_u32(&mut out, 1);
    push_u32(&mut out, base + text_off + entry_offset);
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
    push_u32(&mut out, total);
    push_u32(&mut out, total);
    push_u32(&mut out, 0x4 | 0x1);
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

/// The fields of one `Elf32_Shdr` we set (`sh_addr` is always 0 in a relocatable object).
struct Shdr {
    name: u32,
    typ: u32,
    flags: u32,
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
    push_u32(v, 0);
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
}

/// A relocation parsed from an object's `.rela.text`.
#[derive(Debug, Clone, Copy)]
pub struct ParsedRelocation {
    /// `r_offset` within `.text`.
    pub offset: u32,
    /// The index into [`Object::symbols`] of the referenced symbol.
    pub symbol: u32,
    /// The relocation type (the low byte of `r_info`).
    pub kind: u32,
    /// `r_addend`.
    pub addend: i32,
}

/// A parsed ELF32 relocatable object.
#[derive(Debug, Clone)]
pub struct Object {
    /// The target machine.
    pub machine: Machine,
    /// The `.text` section bytes.
    pub text: Vec<u8>,
    /// The symbols, in symbol-table order (index 0 is the null symbol).
    pub symbols: Vec<ParsedSymbol>,
    /// The `.text` relocations.
    pub relocations: Vec<ParsedRelocation>,
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
const SH_OFFSET: usize = 16;
const SH_SIZE: usize = 20;
const SH_LINK: usize = 24;

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
    let e_shstrndx = rd_u16(bytes, 50)? as usize;
    let sh = |i: usize, field: usize| rd_u32(bytes, e_shoff + i * 40 + field);

    let shstr_off = sh(e_shstrndx, SH_OFFSET)? as usize;
    let section_name = |i: usize| -> Result<&str, ElfError> {
        rd_cstr(bytes, shstr_off + sh(i, SH_NAME)? as usize)
    };

    let (mut symtab_i, mut text_i, mut rela_i) = (None, None, None);
    for i in 0..e_shnum {
        match sh(i, SH_TYPE)? {
            2 => symtab_i = Some(i),
            4 => rela_i = Some(i),
            1 if section_name(i)? == ".text" => text_i = Some(i),
            _ => {}
        }
    }
    let symtab_i = symtab_i.ok_or(ElfError::MissingSymbolTable)?;

    let text = if let Some(ti) = text_i {
        let off = sh(ti, SH_OFFSET)? as usize;
        let size = sh(ti, SH_SIZE)? as usize;
        bytes
            .get(off..off + size)
            .ok_or(ElfError::Truncated)?
            .to_vec()
    } else {
        Vec::new()
    };

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
        let binding = if st_info >> 4 == 1 {
            Binding::Global
        } else {
            Binding::Local
        };
        let kind = if st_info & 0xf == 2 {
            SymbolType::Func
        } else {
            SymbolType::NoType
        };
        symbols.push(ParsedSymbol {
            name: String::from(rd_cstr(bytes, strtab_off + st_name)?),
            value: st_value,
            size: st_size,
            binding,
            kind,
            defined: st_shndx != SHN_UNDEF,
        });
    }

    let mut relocations = Vec::new();
    if let Some(ri) = rela_i {
        let off = sh(ri, SH_OFFSET)? as usize;
        let size = sh(ri, SH_SIZE)? as usize;
        for r in 0..size / RELA_SIZE {
            let base = off + r * RELA_SIZE;
            let r_info = rd_u32(bytes, base + 4)?;
            relocations.push(ParsedRelocation {
                offset: rd_u32(bytes, base)?,
                symbol: r_info >> 8,
                kind: r_info & 0xff,
                addend: rd_u32(bytes, base + 8)? as i32,
            });
        }
    }

    Ok(Object {
        machine,
        text,
        symbols,
        relocations,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
