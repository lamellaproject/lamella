//! Attribute forms: how a value is encoded, independent of what the value means.

use crate::cursor::{Cursor, Format};
use crate::{DwarfError, Sections};

/// Form codes this crate reads by name. Everything else is skipped by its encoded size.
const DW_FORM_ADDR: u64 = 0x01;
const DW_FORM_BLOCK2: u64 = 0x03;
const DW_FORM_BLOCK4: u64 = 0x04;
const DW_FORM_DATA2: u64 = 0x05;
const DW_FORM_DATA4: u64 = 0x06;
const DW_FORM_DATA8: u64 = 0x07;
const DW_FORM_STRING: u64 = 0x08;
const DW_FORM_BLOCK: u64 = 0x09;
const DW_FORM_BLOCK1: u64 = 0x0a;
const DW_FORM_DATA1: u64 = 0x0b;
const DW_FORM_FLAG: u64 = 0x0c;
const DW_FORM_SDATA: u64 = 0x0d;
const DW_FORM_STRP: u64 = 0x0e;
const DW_FORM_UDATA: u64 = 0x0f;
const DW_FORM_REF_ADDR: u64 = 0x10;
const DW_FORM_REF1: u64 = 0x11;
const DW_FORM_REF2: u64 = 0x12;
const DW_FORM_REF4: u64 = 0x13;
const DW_FORM_REF8: u64 = 0x14;
const DW_FORM_REF_UDATA: u64 = 0x15;
const DW_FORM_INDIRECT: u64 = 0x16;
const DW_FORM_SEC_OFFSET: u64 = 0x17;
const DW_FORM_EXPRLOC: u64 = 0x18;
const DW_FORM_FLAG_PRESENT: u64 = 0x19;
const DW_FORM_STRX: u64 = 0x1a;
const DW_FORM_ADDRX: u64 = 0x1b;
const DW_FORM_REF_SUP4: u64 = 0x1c;
const DW_FORM_STRP_SUP: u64 = 0x1d;
const DW_FORM_DATA16: u64 = 0x1e;
const DW_FORM_LINE_STRP: u64 = 0x1f;
const DW_FORM_REF_SIG8: u64 = 0x20;
const DW_FORM_IMPLICIT_CONST: u64 = 0x21;
const DW_FORM_LOCLISTX: u64 = 0x22;
const DW_FORM_RNGLISTX: u64 = 0x23;
const DW_FORM_REF_SUP8: u64 = 0x24;
const DW_FORM_STRX1: u64 = 0x25;
const DW_FORM_STRX2: u64 = 0x26;
const DW_FORM_STRX3: u64 = 0x27;
const DW_FORM_STRX4: u64 = 0x28;
const DW_FORM_ADDRX1: u64 = 0x29;
const DW_FORM_ADDRX2: u64 = 0x2a;
const DW_FORM_ADDRX3: u64 = 0x2b;
const DW_FORM_ADDRX4: u64 = 0x2c;

/// A decoded attribute value, in the shapes a consumer of this crate asks for.
#[derive(Debug, Clone, Copy)]
pub enum Value<'a> {
    /// An unsigned constant, a flag, or a reference expressed as one.
    Unsigned(u64),
    /// A signed constant.
    Signed(i64),
    /// A string, wherever it was stored. Not decoded: DWARF does not require UTF-8.
    Bytes(&'a [u8]),
    /// A string whose index could not be resolved because the side table it needs is absent.
    UnresolvedIndex(u64),
    /// A value read past without being interpreted.
    Skipped,
}

impl<'a> Value<'a> {
    /// The value's bytes when it is a string, and nothing otherwise.
    #[must_use]
    pub fn as_bytes(self) -> Option<&'a [u8]> {
        match self {
            Value::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// The value as an unsigned number when it is one.
    #[must_use]
    pub fn as_u64(self) -> Option<u64> {
        match self {
            Value::Unsigned(value) => Some(value),
            Value::Signed(value) => u64::try_from(value).ok(),
            _ => None,
        }
    }
}

/// The side tables a value's form may resolve through, and the sizes its unit fixes.
#[derive(Debug, Clone, Copy)]
pub struct Context {
    /// The size in bytes of a target address, from the unit header.
    pub address_size: u8,
    /// Whether section offsets in this unit are 4 or 8 bytes wide.
    pub format: Format,
    /// The offset within `.debug_str_offsets` that this unit's string indexes count from --
    /// `DW_AT_str_offsets_base`. Past the section's own header when the unit did not say.
    pub str_offsets_base: u64,
    /// The offset within `.debug_addr` that this unit's address indexes count from --
    /// `DW_AT_addr_base`.
    pub addr_base: u64,
}

impl Context {
    /// A context for a unit that named no bases, using the offset a section's own header ends at.
    ///
    /// A version 5 `.debug_str_offsets` or `.debug_addr` section begins with an 8-byte header in
    /// the 32-bit format (a `unit_length`, a version and two reserved bytes) and 16 in the 64-bit
    /// one, and the first entry follows it. That is what a unit's base attribute points at when it
    /// carries one, so it is the right default when it does not.
    #[must_use]
    pub fn new(address_size: u8, format: Format) -> Self {
        let header = u64::from(format.offset_size()) + 4;
        Context {
            address_size,
            format,
            str_offsets_base: header,
            addr_base: header,
        }
    }
}

/// Reads one value of `form`, leaving the cursor past it.
///
/// `DW_FORM_implicit_const` is not readable here because its value lives in the abbreviation rather
/// than in the data; the DIE walk supplies it directly.
pub fn read<'a>(
    cursor: &mut Cursor<'a>,
    form: u64,
    format: Format,
    sections: &Sections<'a>,
) -> Result<Value<'a>, DwarfError> {
    read_with(cursor, form, &Context::new(0, format), sections)
}

/// Reads one value of `form` against a unit's full context.
pub fn read_with<'a>(
    cursor: &mut Cursor<'a>,
    form: u64,
    context: &Context,
    sections: &Sections<'a>,
) -> Result<Value<'a>, DwarfError> {
    let format = context.format;
    match form {
        DW_FORM_ADDR => Ok(Value::Unsigned(cursor.address(context.address_size)?)),
        DW_FORM_DATA1 | DW_FORM_REF1 | DW_FORM_FLAG => Ok(Value::Unsigned(u64::from(cursor.u8()?))),
        DW_FORM_DATA2 | DW_FORM_REF2 | DW_FORM_REF_SUP4 => {
            Ok(Value::Unsigned(u64::from(cursor.u16()?)))
        }
        DW_FORM_DATA4 | DW_FORM_REF4 => Ok(Value::Unsigned(u64::from(cursor.u32()?))),
        DW_FORM_DATA8 | DW_FORM_REF8 | DW_FORM_REF_SIG8 | DW_FORM_REF_SUP8 => {
            Ok(Value::Unsigned(cursor.u64()?))
        }
        DW_FORM_DATA16 => {
            cursor.skip(16)?;
            Ok(Value::Skipped)
        }
        DW_FORM_UDATA | DW_FORM_REF_UDATA | DW_FORM_LOCLISTX | DW_FORM_RNGLISTX => {
            Ok(Value::Unsigned(cursor.uleb128()?))
        }
        DW_FORM_SDATA => Ok(Value::Signed(cursor.sleb128()?)),
        DW_FORM_FLAG_PRESENT => Ok(Value::Unsigned(1)),
        DW_FORM_STRING => Ok(Value::Bytes(cursor.cstr()?)),
        DW_FORM_STRP | DW_FORM_STRP_SUP => {
            let offset = cursor.offset_of(format)?;
            Ok(string_at(sections.debug_str, offset))
        }
        DW_FORM_LINE_STRP => {
            let offset = cursor.offset_of(format)?;
            Ok(string_at(sections.debug_line_str, offset))
        }
        DW_FORM_STRX => {
            let index = cursor.uleb128()?;
            Ok(indexed_string(index, context, sections))
        }
        DW_FORM_STRX1 => {
            let index = u64::from(cursor.u8()?);
            Ok(indexed_string(index, context, sections))
        }
        DW_FORM_STRX2 => {
            let index = u64::from(cursor.u16()?);
            Ok(indexed_string(index, context, sections))
        }
        DW_FORM_STRX3 => {
            let bytes = cursor.take(3)?;
            let index = u64::from(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]));
            Ok(indexed_string(index, context, sections))
        }
        DW_FORM_STRX4 => {
            let index = u64::from(cursor.u32()?);
            Ok(indexed_string(index, context, sections))
        }
        DW_FORM_ADDRX => {
            let index = cursor.uleb128()?;
            Ok(indexed_address(index, context, sections))
        }
        DW_FORM_ADDRX1 => {
            let index = u64::from(cursor.u8()?);
            Ok(indexed_address(index, context, sections))
        }
        DW_FORM_ADDRX2 => {
            let index = u64::from(cursor.u16()?);
            Ok(indexed_address(index, context, sections))
        }
        DW_FORM_ADDRX3 => {
            let bytes = cursor.take(3)?;
            let index = u64::from(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]));
            Ok(indexed_address(index, context, sections))
        }
        DW_FORM_ADDRX4 => {
            let index = u64::from(cursor.u32()?);
            Ok(indexed_address(index, context, sections))
        }
        DW_FORM_SEC_OFFSET | DW_FORM_REF_ADDR => Ok(Value::Unsigned(cursor.offset_of(format)?)),
        DW_FORM_EXPRLOC | DW_FORM_BLOCK => {
            let length = cursor.uleb128()?;
            Ok(Value::Bytes(cursor.take(
                usize::try_from(length).map_err(|_| DwarfError::Truncated)?,
            )?))
        }
        DW_FORM_BLOCK1 => {
            let length = usize::from(cursor.u8()?);
            Ok(Value::Bytes(cursor.take(length)?))
        }
        DW_FORM_BLOCK2 => {
            let length = usize::from(cursor.u16()?);
            Ok(Value::Bytes(cursor.take(length)?))
        }
        DW_FORM_BLOCK4 => {
            let length = usize::try_from(cursor.u32()?).map_err(|_| DwarfError::Truncated)?;
            Ok(Value::Bytes(cursor.take(length)?))
        }
        DW_FORM_INDIRECT => {
            let actual = cursor.uleb128()?;
            if actual == DW_FORM_INDIRECT {
                return Err(DwarfError::UnknownForm(actual));
            }
            read_with(cursor, actual, context, sections)
        }
        DW_FORM_IMPLICIT_CONST => Err(DwarfError::ImplicitConstInData),
        other => Err(DwarfError::UnknownForm(other)),
    }
}

/// The NUL-terminated string at `offset` in a string section.
fn string_at(section: &[u8], offset: u64) -> Value<'_> {
    let Ok(offset) = usize::try_from(offset) else {
        return Value::UnresolvedIndex(offset);
    };
    let Some(rest) = section.get(offset..) else {
        return Value::UnresolvedIndex(offset as u64);
    };
    match rest.iter().position(|&b| b == 0) {
        Some(end) => Value::Bytes(&rest[..end]),
        None => Value::UnresolvedIndex(offset as u64),
    }
}

/// Resolves a string INDEX: `.debug_str_offsets` holds the offset, `.debug_str` holds the string.
fn indexed_string<'a>(index: u64, context: &Context, sections: &Sections<'a>) -> Value<'a> {
    let width = u64::from(context.format.offset_size());
    let Some(at) = context
        .str_offsets_base
        .checked_add(index.saturating_mul(width))
        .and_then(|at| usize::try_from(at).ok())
    else {
        return Value::UnresolvedIndex(index);
    };
    let Some(slot) = sections
        .debug_str_offsets
        .get(at..at + usize::from(context.format.offset_size()))
    else {
        return Value::UnresolvedIndex(index);
    };
    let offset = match context.format {
        Format::Dwarf32 => u64::from(u32::from_le_bytes([slot[0], slot[1], slot[2], slot[3]])),
        Format::Dwarf64 => u64::from_le_bytes([
            slot[0], slot[1], slot[2], slot[3], slot[4], slot[5], slot[6], slot[7],
        ]),
    };
    string_at(sections.debug_str, offset)
}

/// Resolves an address INDEX through `.debug_addr`.
fn indexed_address(index: u64, context: &Context, sections: &Sections<'_>) -> Value<'static> {
    let width = u64::from(if context.address_size == 0 {
        4
    } else {
        context.address_size
    });
    let Some(at) = context
        .addr_base
        .checked_add(index.saturating_mul(width))
        .and_then(|at| usize::try_from(at).ok())
    else {
        return Value::UnresolvedIndex(index);
    };
    let Ok(width) = usize::try_from(width) else {
        return Value::UnresolvedIndex(index);
    };
    let Some(slot) = sections.debug_addr.get(at..at + width) else {
        return Value::UnresolvedIndex(index);
    };
    let mut value: u64 = 0;
    for (shift, byte) in slot.iter().enumerate() {
        value |= u64::from(*byte) << (shift * 8);
    }
    Value::Unsigned(value)
}
