//! The function-symbol reader, driven over files these tests lay out byte by byte.

use super::*;
use alloc::vec;
use alloc::vec::Vec;

/// `EM_ARM`.
const ARM: u16 = 40;
/// `EM_RISCV`.
const RISCV: u16 = 243;
/// `STT_OBJECT`, `STT_FUNC`, and the three bindings, as they sit in `st_info`.
const OBJECT: u8 = 1;
const FUNC: u8 = 2;
const LOCAL: u8 = 0;
const GLOBAL: u8 = 1 << 4;
const WEAK: u8 = 2 << 4;

/// One `Elf32_Sym`: `st_name`, `st_value`, `st_size`, `st_info`, a zero `st_other`, `st_shndx`.
fn entry(name: u32, value: u32, size: u32, info: u8, section: u16) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(16);
    bytes.extend_from_slice(&name.to_le_bytes());
    bytes.extend_from_slice(&value.to_le_bytes());
    bytes.extend_from_slice(&size.to_le_bytes());
    bytes.push(info);
    bytes.push(0);
    bytes.extend_from_slice(&section.to_le_bytes());
    bytes
}

/// A symbol table whose first entry is the null symbol every table opens with, then `entries`.
fn table(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = vec![0u8; 16];
    for entry in entries {
        bytes.extend_from_slice(entry);
    }
    bytes
}

/// A little-endian ELF32 file for `machine` whose sections are a symbol table, a string table and the
/// section-name table, in that order after the null section. `link` is the symbol table's `sh_link`.
fn image(machine: u16, symbols: &[u8], strings: &[u8], link: u32) -> Vec<u8> {
    let names: &[u8] = b"\0.symtab\0.strtab\0.shstrtab\0";
    let mut elf = vec![0u8; 52];
    elf[..4].copy_from_slice(&[0x7F, b'E', b'L', b'F']);
    elf[4] = 1;
    elf[5] = 1;
    elf[6] = 1;
    elf[18..20].copy_from_slice(&machine.to_le_bytes());
    let symbols_at = elf.len();
    elf.extend_from_slice(symbols);
    let strings_at = elf.len();
    elf.extend_from_slice(strings);
    let names_at = elf.len();
    elf.extend_from_slice(names);
    let headers_at = elf.len() as u32;
    elf[32..36].copy_from_slice(&headers_at.to_le_bytes());
    elf[46..48].copy_from_slice(&40u16.to_le_bytes());
    elf[48..50].copy_from_slice(&4u16.to_le_bytes());
    elf[50..52].copy_from_slice(&3u16.to_le_bytes());
    let sections = [
        (0u32, 0u32, 0usize, 0usize, 0u32),
        (1, 2, symbols_at, symbols.len(), link),
        (9, 3, strings_at, strings.len(), 0),
        (17, 3, names_at, names.len(), 0),
    ];
    for (name, kind, offset, size, link) in sections {
        for word in [name, kind, 0, 0, offset as u32, size as u32, link, 0, 0, 0] {
            elf.extend_from_slice(&word.to_le_bytes());
        }
    }
    elf
}

/// A little-endian ELF64 file with two section headers, the second carrying `link` as its `sh_link`.
fn elf64_with_a_link(link: u32) -> Vec<u8> {
    let mut elf = vec![0u8; 64];
    elf[..4].copy_from_slice(&[0x7F, b'E', b'L', b'F']);
    elf[4] = 2;
    elf[5] = 1;
    elf[6] = 1;
    elf[40..48].copy_from_slice(&64u64.to_le_bytes());
    elf[58..60].copy_from_slice(&64u16.to_le_bytes());
    elf[60..62].copy_from_slice(&2u16.to_le_bytes());
    elf[62..64].copy_from_slice(&0u16.to_le_bytes());
    elf.extend_from_slice(&[0u8; 64]);
    let mut second = [0u8; 64];
    second[4..8].copy_from_slice(&1u32.to_le_bytes());
    second[40..44].copy_from_slice(&link.to_le_bytes());
    elf.extend_from_slice(&second);
    elf
}

/// `main` at offset 1, `helper` at 6, `weak_one` at 13, and an empty name at 22.
const STRINGS: &[u8] = b"\0main\0helper\0weak_one\0\0";

#[test]
fn a_function_symbol_is_read_with_its_address_size_binding_and_name() {
    let symbols = table(&[entry(1, 0x0800_0101, 20, GLOBAL | FUNC, 1)]);
    assert_eq!(
        functions(&image(ARM, &symbols, STRINGS, 2)),
        vec![Function { address: 0x0800_0100, size: 20, binding: Binding::Global, name: "main" }],
        "a Thumb function's value carries bit zero, and its address does not"
    );
}

#[test]
fn only_an_arm_image_has_a_thumb_bit_to_clear() {
    let symbols = table(&[entry(1, 0x2000_0101, 20, GLOBAL | FUNC, 1)]);
    let elf = image(RISCV, &symbols, STRINGS, 2);
    let read = functions(&elf);
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].address, 0x2000_0101, "the value is the address as the table gives it");
}

#[test]
fn only_a_sized_function_defined_in_the_file_is_read() {
    let symbols = table(&[
        entry(1, 0x100, 8, GLOBAL | OBJECT, 1),
        entry(1, 0x100, 8, GLOBAL, 1),
        entry(1, 0x100, 0, GLOBAL | FUNC, 1),
        entry(1, 0x100, 8, GLOBAL | FUNC, 0),
        entry(1, 0x100, 8, GLOBAL | FUNC, 0xFFF1),
        entry(1, 0x100, 8, (10 << 4) | FUNC, 1),
        entry(22, 0x100, 8, GLOBAL | FUNC, 1),
        entry(6, 0x200, 12, LOCAL | FUNC, 1),
        entry(13, 0x300, 16, WEAK | FUNC, 2),
    ]);
    assert_eq!(
        functions(&image(RISCV, &symbols, STRINGS, 2)),
        vec![
            Function { address: 0x200, size: 12, binding: Binding::Local, name: "helper" },
            Function { address: 0x300, size: 16, binding: Binding::Weak, name: "weak_one" },
        ],
        "an object, an untyped symbol, a zero size, an undefined or absolute symbol, an unknown \
         binding and an empty name are each left out, and what is kept keeps the table's order"
    );
}

#[test]
fn a_name_that_does_not_end_inside_its_string_table_is_not_read() {
    let unterminated: &[u8] = b"\0main";
    let symbols = table(&[entry(1, 0x100, 8, GLOBAL | FUNC, 1), entry(99, 0x200, 8, GLOBAL | FUNC, 1)]);
    assert!(functions(&image(RISCV, &symbols, unterminated, 2)).is_empty());
}

#[test]
fn a_symbol_table_that_does_not_link_a_string_table_is_not_read() {
    let symbols = table(&[entry(1, 0x100, 8, GLOBAL | FUNC, 1)]);
    assert!(functions(&image(RISCV, &symbols, STRINGS, 1)).is_empty(), "linked to itself");
    assert!(functions(&image(RISCV, &symbols, STRINGS, 9)).is_empty(), "linked past the last section");
}

#[test]
fn a_symbol_table_that_is_not_a_whole_number_of_entries_reads_its_whole_entries() {
    let mut symbols = table(&[entry(1, 0x100, 8, GLOBAL | FUNC, 1)]);
    symbols.extend_from_slice(&[0xAA; 5]);
    assert_eq!(functions(&image(RISCV, &symbols, STRINGS, 2)).len(), 1);
}

#[test]
fn a_file_that_is_not_a_little_endian_elf32_file_names_no_functions() {
    let symbols = table(&[entry(1, 0x100, 8, GLOBAL | FUNC, 1)]);
    let mut elf64 = image(RISCV, &symbols, STRINGS, 2);
    elf64[4] = 2;
    assert!(functions(&elf64).is_empty(), "ELF64");
    let mut big_endian = image(RISCV, &symbols, STRINGS, 2);
    big_endian[5] = 2;
    assert!(functions(&big_endian).is_empty(), "big-endian");
    assert!(functions(b"not an ELF file at all, and long enough to be read as one if it were").is_empty());
}

#[test]
fn the_section_header_reader_reads_sh_link_in_both_classes() {
    let symbols = table(&[entry(1, 0x100, 8, GLOBAL | FUNC, 1)]);
    let elf32 = image(RISCV, &symbols, STRINGS, 2);
    assert_eq!(crate::section_headers(&elf32)[1].link, 2, "ELF32");
    let elf64 = elf64_with_a_link(7);
    assert_eq!(crate::section_headers(&elf64)[1].link, 7, "ELF64");
}
