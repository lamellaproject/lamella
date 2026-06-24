//! A minimal static linker over `lamella-elf` objects

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use lamella_elf::{Object, riscv};

/// A reason linking failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// A relocation references a symbol no input object defines.
    UndefinedSymbol(String),
    /// The named entry symbol is not defined by any input object.
    MissingEntry(String),
    /// Two input objects define the same global symbol.
    DuplicateSymbol(String),
    /// A relocation type the linker does not handle yet.
    UnsupportedRelocation(u32),
}

/// A linked image: the laid-out, relocated code plus where execution starts.
#[derive(Debug, Clone)]
pub struct LinkedImage {
    /// The combined, relocated `.text` (position-independent).
    pub text: Vec<u8>,
    /// The byte offset of the entry symbol within [`LinkedImage::text`].
    pub entry_offset: u32,
    /// Every defined symbol, as `(name, offset within text)`.
    pub symbols: Vec<(String, u32)>,
}

/// Links `objects` into one image, with `entry` naming the start symbol. Each object's `.text` is
/// laid out in order (4-byte aligned), every relocation's symbol is resolved to its definition, and
/// the code is patched in place.
pub fn link(objects: &[Object], entry: &str) -> Result<LinkedImage, LinkError> {
    let mut text: Vec<u8> = Vec::new();
    let mut bases: Vec<u32> = Vec::with_capacity(objects.len());
    for obj in objects {
        while text.len() % 4 != 0 {
            text.push(0);
        }
        bases.push(text.len() as u32);
        text.extend_from_slice(&obj.text);
    }

    let mut symbols: Vec<(String, u32)> = Vec::new();
    for (oi, obj) in objects.iter().enumerate() {
        for sym in &obj.symbols {
            if sym.defined && !sym.name.is_empty() {
                if symbols.iter().any(|(n, _)| *n == sym.name) {
                    return Err(LinkError::DuplicateSymbol(sym.name.clone()));
                }
                symbols.push((sym.name.clone(), bases[oi] + sym.value));
            }
        }
    }

    for (oi, obj) in objects.iter().enumerate() {
        for r in &obj.relocations {
            let name = &obj.symbols[r.symbol as usize].name;
            let target =
                resolve(&symbols, name).ok_or_else(|| LinkError::UndefinedSymbol(name.clone()))?;
            let site = bases[oi] + r.offset;
            match r.kind {
                riscv::R_RISCV_CALL_PLT => {
                    let delta = target as i64 + r.addend as i64 - site as i64;
                    let lo12 = (delta & 0xfff) as u32;
                    let hi20 = (((delta + 0x800) >> 12) & 0xfffff) as u32;
                    patch_or(&mut text, site as usize, hi20 << 12);
                    patch_or(&mut text, site as usize + 4, (lo12 & 0xfff) << 20);
                }
                other => return Err(LinkError::UnsupportedRelocation(other)),
            }
        }
    }

    let entry_offset =
        resolve(&symbols, entry).ok_or_else(|| LinkError::MissingEntry(String::from(entry)))?;
    Ok(LinkedImage {
        text,
        entry_offset,
        symbols,
    })
}

fn resolve(symbols: &[(String, u32)], name: &str) -> Option<u32> {
    symbols.iter().find(|(n, _)| n == name).map(|&(_, a)| a)
}

fn patch_or(text: &mut [u8], off: usize, bits: u32) {
    let w = u32::from_le_bytes([text[off], text[off + 1], text[off + 2], text[off + 3]]) | bits;
    text[off..off + 4].copy_from_slice(&w.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_elf::{
        Binding, Machine, Relocation, Symbol, SymbolSection, SymbolType, read_object,
        write_relocatable_object,
    };

    fn obj(text: &[u8], syms: &[Symbol], relocs: &[Relocation]) -> Object {
        read_object(&write_relocatable_object(
            Machine::RiscV,
            text,
            syms,
            relocs,
        ))
        .unwrap()
    }

    #[test]
    fn resolves_an_external_call_across_two_objects() {
        let answer = obj(
            &[0x13, 0x05, 0xa0, 0x02, 0x67, 0x80, 0x00, 0x00],
            &[Symbol {
                name: "answer",
                value: 0,
                size: 8,
                binding: Binding::Global,
                kind: SymbolType::Func,
                section: SymbolSection::Text,
            }],
            &[],
        );
        let caller = obj(
            &[0x97, 0x00, 0x00, 0x00, 0xe7, 0x80, 0x00, 0x00],
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
                    name: "answer",
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
        let img = link(&[caller, answer], "caller").unwrap();
        assert_eq!(img.entry_offset, 0);
        let auipc = u32::from_le_bytes([img.text[0], img.text[1], img.text[2], img.text[3]]);
        let jalr = u32::from_le_bytes([img.text[4], img.text[5], img.text[6], img.text[7]]);
        assert_eq!(auipc, 0x0000_0097);
        assert_eq!(jalr, 0x0080_80e7);
        assert_eq!(
            &img.text[8..16],
            &[0x13, 0x05, 0xa0, 0x02, 0x67, 0x80, 0x00, 0x00]
        );
    }

    #[test]
    fn an_unresolved_call_is_an_error() {
        let caller = obj(
            &[0x97, 0x00, 0x00, 0x00, 0xe7, 0x80, 0x00, 0x00],
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
                    name: "missing",
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
        assert_eq!(
            link(&[caller], "caller").unwrap_err(),
            LinkError::UndefinedSymbol(String::from("missing"))
        );
    }
}
