//! The type a variable is declared with, as far as a debugger needs it to show a scalar's value: a
//! base type's encoding and size, reached through the names and qualifiers a producer wraps it in,
//! or an enumeration's, by its underlying type.
//!
//! Sources: DWARF 5 (dwarfstd.org, 2017), section 5.1 "Base Type Entries" (table 5.2, the
//! encodings), section 5.3 "Type Modifier Entries", section 5.4 "Typedef Entries" and section 5.7
//! "Enumeration Type Entries".

use crate::cursor::Cursor;
use crate::form::{self, Context, Value};
use crate::Sections;

const DW_TAG_ENUMERATION_TYPE: u64 = 0x04;
const DW_TAG_TYPEDEF: u64 = 0x16;
const DW_TAG_BASE_TYPE: u64 = 0x24;
const DW_TAG_CONST_TYPE: u64 = 0x26;
const DW_TAG_VOLATILE_TYPE: u64 = 0x35;
const DW_TAG_RESTRICT_TYPE: u64 = 0x37;

const DW_AT_NAME: u64 = 0x03;
const DW_AT_BYTE_SIZE: u64 = 0x0b;
const DW_AT_ENCODING: u64 = 0x3e;
const DW_AT_TYPE: u64 = 0x49;

const DW_FORM_IMPLICIT_CONST: u64 = 0x21;

/// `DW_ATE_boolean`.
pub const DW_ATE_BOOLEAN: u64 = 0x02;
/// `DW_ATE_float`.
pub const DW_ATE_FLOAT: u64 = 0x04;
/// `DW_ATE_signed`.
pub const DW_ATE_SIGNED: u64 = 0x05;
/// `DW_ATE_signed_char`.
pub const DW_ATE_SIGNED_CHAR: u64 = 0x06;
/// `DW_ATE_unsigned`.
pub const DW_ATE_UNSIGNED: u64 = 0x07;
/// `DW_ATE_unsigned_char`.
pub const DW_ATE_UNSIGNED_CHAR: u64 = 0x08;
/// `DW_ATE_UTF`: a character, its value a code point.
pub const DW_ATE_UTF: u64 = 0x10;

/// A scalar type, as far as showing a value of it needs: how its bits are read, and how many bytes
/// it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScalarType<'a> {
    /// The base type's name, or the enumeration's, as the producer spelled it.
    pub name: Option<&'a [u8]>,
    /// Its `DW_ATE_*` encoding: an enumeration's is its underlying type's.
    pub encoding: u64,
    /// How many bytes a value of it occupies.
    pub byte_size: u64,
    /// Whether it is an enumeration, read as its underlying type.
    pub enumeration: bool,
}

/// How many entries the walk passes through before it gives up: names and qualifiers wrap a base
/// type a few deep, and a cycle among them would otherwise never end.
const HOPS: usize = 8;

/// The scalar type of the entry at `offset` within `.debug_info`, the offset a variable's
/// `DW_AT_type` names ([`crate::Local::type_offset`]): a base type, through any typedefs and const,
/// volatile or restrict qualifiers, or an enumeration, read as its underlying type. `None` for any
/// other type -- a pointer, a structure, a class, an array -- and for an entry that cannot be read.
///
/// An enumeration that names no underlying type is a signed integer of its own size, which is what a
/// C# enumeration is unless it declares another.
#[must_use]
pub fn scalar_type<'a>(sections: &Sections<'a>, offset: u64) -> Option<ScalarType<'a>> {
    let mut offset = offset;
    let mut enumeration: Option<(Option<&'a [u8]>, Option<u64>)> = None;
    for _ in 0..HOPS {
        let entry = entry_at(sections, offset)?;
        match entry.tag {
            DW_TAG_BASE_TYPE => {
                let (encoding, byte_size) = (entry.encoding?, entry.byte_size?);
                return Some(match enumeration {
                    Some((name, size)) => ScalarType {
                        name,
                        encoding,
                        byte_size: size.unwrap_or(byte_size),
                        enumeration: true,
                    },
                    None => ScalarType { name: entry.name, encoding, byte_size, enumeration: false },
                });
            }
            DW_TAG_ENUMERATION_TYPE if enumeration.is_none() => match entry.type_offset {
                Some(next) => {
                    enumeration = Some((entry.name, entry.byte_size));
                    offset = next;
                }
                None => {
                    return Some(ScalarType {
                        name: entry.name,
                        encoding: DW_ATE_SIGNED,
                        byte_size: entry.byte_size?,
                        enumeration: true,
                    });
                }
            },
            DW_TAG_TYPEDEF | DW_TAG_CONST_TYPE | DW_TAG_VOLATILE_TYPE | DW_TAG_RESTRICT_TYPE => {
                offset = entry.type_offset?;
            }
            _ => return None,
        }
    }
    None
}

/// The attributes of one entry that a type walk reads.
struct TypeEntry<'a> {
    tag: u64,
    name: Option<&'a [u8]>,
    encoding: Option<u64>,
    byte_size: Option<u64>,
    /// Its `DW_AT_type`, as an offset within `.debug_info`.
    type_offset: Option<u64>,
}

/// The entry at `offset` within `.debug_info`, read against the unit that holds it.
fn entry_at<'a>(sections: &Sections<'a>, offset: u64) -> Option<TypeEntry<'a>> {
    let mut cursor = Cursor::new(sections.debug_info);
    while !cursor.is_empty() {
        let unit_start = cursor.origin_offset() as u64;
        let (unit_length, format) = cursor.initial_length().ok()?;
        if unit_length == 0 {
            return None;
        }
        let unit = cursor.split(usize::try_from(unit_length).ok()?).ok()?;
        let body_start = unit.origin_offset() as u64;
        if offset < body_start || offset >= body_start.saturating_add(unit_length) {
            continue;
        }
        let mut header = unit.clone();
        let version = header.u16().ok()?;
        let (abbrev_offset, address_size) = if version >= 5 {
            let _unit_type = header.u8().ok()?;
            let address_size = header.u8().ok()?;
            (header.offset_of(format).ok()?, address_size)
        } else {
            let abbrev_offset = header.offset_of(format).ok()?;
            (abbrev_offset, header.u8().ok()?)
        };
        let abbrevs = crate::info::abbrev_table(sections.debug_abbrev, abbrev_offset).ok()?;
        let mut context = Context::new(address_size, format);
        if let Ok(bases) = crate::locals::unit_bases(&mut header.clone(), &abbrevs, &context, sections) {
            if let Some(base) = bases.str_offsets_base {
                context.str_offsets_base = base;
            }
            if let Some(base) = bases.addr_base {
                context.addr_base = base;
            }
        }

        let mut entry = unit;
        entry.split(usize::try_from(offset - body_start).ok()?).ok()?;
        let code = entry.uleb128().ok()?;
        let abbrev = abbrevs.iter().find(|abbrev| abbrev.code == code)?;
        let mut found = TypeEntry {
            tag: abbrev.tag,
            name: None,
            encoding: None,
            byte_size: None,
            type_offset: None,
        };
        for &(attribute, form, implicit) in &abbrev.attributes {
            let value = if form == DW_FORM_IMPLICIT_CONST {
                Value::Signed(implicit)
            } else {
                form::read_with(&mut entry, form, &context, sections).ok()?
            };
            match attribute {
                DW_AT_NAME => found.name = value.as_bytes(),
                DW_AT_ENCODING => found.encoding = value.as_u64(),
                DW_AT_BYTE_SIZE => found.byte_size = value.as_u64(),
                DW_AT_TYPE => found.type_offset = crate::locals::section_reference(form, value, unit_start),
                _ => {}
            }
        }
        return Some(found);
    }
    None
}
