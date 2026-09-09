//! `.debug_info` and `.debug_abbrev`: the debugging information entries, walked for the one thing
//! this tier needs from them -- which function occupies which addresses.

use alloc::vec::Vec;

use crate::cursor::{Cursor, Format};
use crate::form::{self, Context, Value};
use crate::{DwarfError, Sections};

const DW_TAG_SUBPROGRAM: u64 = 0x2e;

const DW_AT_NAME: u64 = 0x03;
const DW_AT_STMT_LIST: u64 = 0x10;
const DW_AT_LOW_PC: u64 = 0x11;
const DW_AT_HIGH_PC: u64 = 0x12;
const DW_AT_COMP_DIR: u64 = 0x1b;
const DW_AT_DECL_FILE: u64 = 0x3a;
const DW_AT_DECL_LINE: u64 = 0x3b;
const DW_AT_LINKAGE_NAME: u64 = 0x6e;
const DW_AT_STR_OFFSETS_BASE: u64 = 0x72;
const DW_AT_ADDR_BASE: u64 = 0x73;

/// `DW_FORM_addr`, the only form that makes `DW_AT_high_pc` an address rather than a length.
const DW_FORM_ADDR: u64 = 0x01;
/// `DW_FORM_implicit_const`, whose value is stored in the abbreviation declaration.
const DW_FORM_IMPLICIT_CONST: u64 = 0x21;

/// One subprogram: a name and the addresses it occupies.
#[derive(Debug, Clone, Copy)]
pub struct Function<'a> {
    /// The name as the producer spelled it in `DW_AT_name`.
    pub name: &'a [u8],
    /// The name a linker would see, from `DW_AT_linkage_name`, when the producer emitted one.
    pub linkage_name: Option<&'a [u8]>,
    /// The first address of the function.
    pub low_pc: u64,
    /// One past the function's last address.
    pub high_pc: u64,
    /// The file the function was declared in, as an index into its unit's line-program file table.
    pub decl_file: Option<u64>,
    /// The line the function was declared on.
    pub decl_line: Option<u32>,
    /// The offset of the compilation unit this came from, within `.debug_info`.
    pub unit_offset: usize,
    /// The offset within `.debug_line` of that unit's line program, from `DW_AT_stmt_list`.
    pub stmt_list: Option<u64>,
    /// The unit's `DW_AT_comp_dir`, which resolves a relative path in its line table.
    pub comp_dir: Option<&'a [u8]>,
}

/// One abbreviation declaration: what an entry citing this code contains.
///
pub(crate) struct Abbrev {
    pub(crate) code: u64,
    pub(crate) tag: u64,
    pub(crate) has_children: bool,
    /// `(attribute, form, implicit value)`. The value is present only for
    /// `DW_FORM_implicit_const`, whose value is declared here and stored nowhere in the entry.
    pub(crate) attributes: Vec<(u64, u64, i64)>,
}

/// Parses one abbreviation table, the one starting at `offset` within `.debug_abbrev`.
pub(crate) fn abbrev_table(section: &[u8], offset: u64) -> Result<Vec<Abbrev>, DwarfError> {
    let offset = usize::try_from(offset).map_err(|_| DwarfError::Truncated)?;
    let mut cursor = Cursor::at(section, offset)?;
    let mut out = Vec::new();
    loop {
        if cursor.is_empty() {
            break;
        }
        let code = cursor.uleb128()?;
        if code == 0 {
            break;
        }
        let tag = cursor.uleb128()?;
        let has_children = cursor.u8()? != 0;
        let mut attributes = Vec::new();
        loop {
            let attribute = cursor.uleb128()?;
            let form = cursor.uleb128()?;
            let implicit = if form == DW_FORM_IMPLICIT_CONST {
                cursor.sleb128()?
            } else {
                0
            };
            if attribute == 0 && form == 0 {
                break;
            }
            attributes.push((attribute, form, implicit));
        }
        out.push(Abbrev {
            code,
            tag,
            has_children,
            attributes,
        });
    }
    Ok(out)
}

/// Every subprogram described in `.debug_info`, in the order the entries appear.
///
/// A unit whose abbreviation table is missing or unreadable is skipped rather than failing the
/// whole walk. A linked image mixes objects, and one object contributing debug info this crate
/// cannot read is not a reason to report that the others described no functions.
pub fn functions<'a>(sections: &Sections<'a>) -> Result<Vec<Function<'a>>, DwarfError> {
    let mut out = Vec::new();
    let mut cursor = Cursor::new(sections.debug_info);
    while !cursor.is_empty() {
        if cursor.remaining() < 4 {
            break;
        }
        let unit_offset = cursor.offset();
        let (unit_length, format) = cursor.initial_length()?;
        if unit_length == 0 {
            break;
        }
        let length = usize::try_from(unit_length).map_err(|_| DwarfError::Truncated)?;
        let mut unit = cursor.split(length)?;
        if let Ok(mut found) = unit_functions(&mut unit, unit_offset, format, sections) {
            out.append(&mut found);
        }
    }
    Ok(out)
}

/// Walks one compilation unit's entries.
fn unit_functions<'a>(
    unit: &mut Cursor<'a>,
    unit_offset: usize,
    format: Format,
    sections: &Sections<'a>,
) -> Result<Vec<Function<'a>>, DwarfError> {
    let version = unit.u16()?;
    if !(2..=5).contains(&version) {
        return Err(DwarfError::UnsupportedUnitVersion(version));
    }
    let (abbrev_offset, address_size) = if version >= 5 {
        let _unit_type = unit.u8()?;
        let address_size = unit.u8()?;
        (unit.offset_of(format)?, address_size)
    } else {
        let abbrev_offset = unit.offset_of(format)?;
        (abbrev_offset, unit.u8()?)
    };

    let abbrevs = abbrev_table(sections.debug_abbrev, abbrev_offset)?;

    let mut context = Context::new(address_size, format);
    let mut probe = unit.clone();
    if let Ok(bases) = unit_bases(&mut probe, &abbrevs, &context, sections) {
        if let Some(base) = bases.0 {
            context.str_offsets_base = base;
        }
        if let Some(base) = bases.1 {
            context.addr_base = base;
        }
    }

    let mut out = Vec::new();
    let mut comp_dir = None;
    let mut stmt_list = None;
    while !unit.is_empty() {
        let code = unit.uleb128()?;
        if code == 0 {
            continue;
        }
        let Some(abbrev) = abbrevs.iter().find(|a| a.code == code) else {
            return Err(DwarfError::UnknownAbbreviation(code));
        };
        let _ = abbrev.has_children;

        let mut name = None;
        let mut linkage_name = None;
        let mut low_pc = None;
        let mut high_pc = None;
        let mut high_pc_is_address = false;
        let mut decl_file = None;
        let mut decl_line = None;
        for &(attribute, form, implicit) in &abbrev.attributes {
            let value = if form == DW_FORM_IMPLICIT_CONST {
                Value::Signed(implicit)
            } else {
                form::read_with(unit, form, &context, sections)?
            };
            match attribute {
                DW_AT_NAME => name = value.as_bytes(),
                DW_AT_LINKAGE_NAME => linkage_name = value.as_bytes(),
                DW_AT_LOW_PC => low_pc = value.as_u64(),
                DW_AT_HIGH_PC => {
                    high_pc = value.as_u64();
                    high_pc_is_address = form == DW_FORM_ADDR;
                }
                DW_AT_DECL_FILE => decl_file = value.as_u64(),
                DW_AT_DECL_LINE => decl_line = value.as_u64().and_then(|v| u32::try_from(v).ok()),
                DW_AT_COMP_DIR => comp_dir = value.as_bytes().or(comp_dir),
                DW_AT_STMT_LIST => stmt_list = value.as_u64().or(stmt_list),
                _ => {}
            }
        }

        if abbrev.tag != DW_TAG_SUBPROGRAM {
            continue;
        }
        let (Some(name), Some(low_pc), Some(high_pc)) = (name, low_pc, high_pc) else {
            continue;
        };
        let high_pc = if high_pc_is_address {
            high_pc
        } else {
            low_pc.saturating_add(high_pc)
        };
        out.push(Function {
            name,
            linkage_name,
            low_pc,
            high_pc,
            decl_file,
            decl_line,
            unit_offset,
            stmt_list,
            comp_dir,
        });
    }
    Ok(out)
}

/// Reads the first entry of a unit for its `DW_AT_str_offsets_base` and `DW_AT_addr_base` only.
fn unit_bases<'a>(
    unit: &mut Cursor<'a>,
    abbrevs: &[Abbrev],
    context: &Context,
    sections: &Sections<'a>,
) -> Result<(Option<u64>, Option<u64>), DwarfError> {
    let code = unit.uleb128()?;
    let Some(abbrev) = abbrevs.iter().find(|a| a.code == code) else {
        return Ok((None, None));
    };
    let mut str_offsets_base = None;
    let mut addr_base = None;
    for &(attribute, form, implicit) in &abbrev.attributes {
        let value = if form == DW_FORM_IMPLICIT_CONST {
            Value::Signed(implicit)
        } else {
            form::read_with(unit, form, context, sections)?
        };
        match attribute {
            DW_AT_STR_OFFSETS_BASE => str_offsets_base = value.as_u64(),
            DW_AT_ADDR_BASE => addr_base = value.as_u64(),
            _ => {}
        }
    }
    Ok((str_offsets_base, addr_base))
}
