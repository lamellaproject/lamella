//! The functions a linked image's symbol table names, read.
//!
//! A symbol table gives a name, an address and a size to each function a linker kept. That is the
//! name to show for code the debug information does not describe -- startup code a toolchain emits
//! without a subprogram entry, a runtime helper, a routine written in assembly -- where the
//! alternative is no name at all.
//!
//! Only function symbols are read, and only those that name code in the file: a function symbol
//! with no size covers no instruction, and one with no section refers to code defined elsewhere.
//!
//! Sources: the generic ELF specification's symbol table, and *ELF for the Arm Architecture*
//! (AAELF32, release 2025Q4), "Symbol Values", for the bit a Thumb function's value carries.

use crate::Binding;
use alloc::vec::Vec;

/// `SHT_SYMTAB`, the type of a section holding a symbol table.
const SHT_SYMTAB: u32 = 2;
/// `SHT_STRTAB`, the type of a section holding strings. A symbol table's `sh_link` names one.
const SHT_STRTAB: u32 = 3;
/// `STT_FUNC`, in the low four bits of `st_info`: the symbol names a function.
const STT_FUNC: u8 = 2;
/// `SHN_LORESERVE`, the first reserved section index. A symbol whose index is at or above it is in
/// no section of the file -- an absolute value, a common block -- so it names no code here.
const SHN_LORESERVE: u16 = 0xFF00;

/// A function an image's symbol table names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Function<'a> {
    /// The address of the function's first instruction. On Arm the table sets bit zero of a Thumb
    /// function's value (AAELF32 2025Q4, "Symbol Values"), and this address does not carry it: it
    /// says where the code is, and which instruction set the code is in is a separate fact.
    pub address: u32,
    /// How many bytes of code the symbol covers, `st_size`. Never zero.
    pub size: u32,
    /// The symbol's binding.
    pub binding: Binding,
    /// The name as the string table spells it -- mangled, where the producer mangled it.
    pub name: &'a str,
}

/// Every function symbol in `elf` that names code, in the order its symbol tables list them.
///
/// They are read from a little-endian ELF32 file's `SHT_SYMTAB` sections, each through the string
/// table its `sh_link` names. An entry is kept when its type is `STT_FUNC`, its size is not zero, it
/// is defined in a section of the file, its binding is local, global or weak, and its name is
/// non-empty UTF-8 that ends inside the string table. A file that is not a little-endian ELF32 file,
/// or that has no symbol table, names no functions.
#[must_use]
pub fn functions(elf: &[u8]) -> Vec<Function<'_>> {
    let mut found = Vec::new();
    if elf.len() < 20 || elf[..4] != [0x7F, b'E', b'L', b'F'] || elf[4] != 1 || elf[5] != 1 {
        return found;
    }
    let arm = u16::from_le_bytes([elf[18], elf[19]]) == crate::ehabi::EM_ARM;
    let headers = crate::section_headers(elf);
    for table in headers.iter().filter(|header| header.kind == SHT_SYMTAB) {
        let strings = headers
            .get(table.link as usize)
            .filter(|header| header.kind == SHT_STRTAB)
            .and_then(|header| header.data);
        let (Some(entries), Some(strings)) = (table.data, strings) else {
            continue;
        };
        found.extend(
            entries
                .chunks_exact(crate::SYM_SIZE)
                .filter_map(|entry| function(entry, strings, arm)),
        );
    }
    found
}

/// One `Elf32_Sym`, as a [`Function`] when it names code in the file.
fn function<'a>(entry: &[u8], strings: &'a [u8], arm: bool) -> Option<Function<'a>> {
    let word =
        |at: usize| u32::from_le_bytes([entry[at], entry[at + 1], entry[at + 2], entry[at + 3]]);
    let (name, value, size, info) = (word(0), word(4), word(8), entry[12]);
    let section = u16::from_le_bytes([entry[14], entry[15]]);
    let defined = section != crate::SHN_UNDEF && section < SHN_LORESERVE;
    if info & 0x0F != STT_FUNC || size == 0 || !defined {
        return None;
    }
    let binding = match info >> 4 {
        0 => Binding::Local,
        1 => Binding::Global,
        2 => Binding::Weak,
        _ => return None,
    };
    let rest = strings.get(name as usize..)?;
    let end = rest.iter().position(|&byte| byte == 0)?;
    let name = core::str::from_utf8(&rest[..end]).ok().filter(|name| !name.is_empty())?;
    let address = if arm { value & !1 } else { value };
    Some(Function { address, size, binding, name })
}

#[cfg(test)]
mod tests;
