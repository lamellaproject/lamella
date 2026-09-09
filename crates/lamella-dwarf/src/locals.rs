//! Local variables and formal parameters: what a program's own names are worth at one address.

use alloc::vec::Vec;

use crate::cursor::{Cursor, Format};
use crate::form::{self, Context, Value};
use crate::{DwarfError, Sections};

const DW_TAG_SUBPROGRAM: u64 = 0x2e;
const DW_TAG_FORMAL_PARAMETER: u64 = 0x05;
const DW_TAG_VARIABLE: u64 = 0x34;

const DW_AT_LOCATION: u64 = 0x02;
const DW_AT_NAME: u64 = 0x03;
const DW_AT_LOW_PC: u64 = 0x11;
const DW_AT_HIGH_PC: u64 = 0x12;
const DW_AT_FRAME_BASE: u64 = 0x40;
const DW_AT_ABSTRACT_ORIGIN: u64 = 0x31;
const DW_AT_SPECIFICATION: u64 = 0x47;
const DW_AT_TYPE: u64 = 0x49;
const DW_AT_STR_OFFSETS_BASE: u64 = 0x72;
const DW_AT_ADDR_BASE: u64 = 0x73;
const DW_AT_LOCLISTS_BASE: u64 = 0x8c;

const DW_FORM_ADDR: u64 = 0x01;
const DW_FORM_IMPLICIT_CONST: u64 = 0x21;
const DW_FORM_EXPRLOC: u64 = 0x18;
const DW_FORM_SEC_OFFSET: u64 = 0x17;
const DW_FORM_LOCLISTX: u64 = 0x22;
/// The reference forms whose value counts from the start of the unit rather than of the section.
const DW_FORM_REF1: u64 = 0x11;
const DW_FORM_REF2: u64 = 0x12;
const DW_FORM_REF4: u64 = 0x13;
const DW_FORM_REF8: u64 = 0x14;
const DW_FORM_REF_UDATA: u64 = 0x15;

const DW_OP_ADDR: u8 = 0x03;
const DW_OP_FBREG: u8 = 0x91;
const DW_OP_CALL_FRAME_CFA: u8 = 0x9c;
const DW_OP_ADDRX: u8 = 0xa1;
const DW_OP_REG0: u8 = 0x50;
const DW_OP_REG31: u8 = 0x6f;
const DW_OP_BREG0: u8 = 0x70;
const DW_OP_BREG31: u8 = 0x8f;

/// Where one local's value is, at one address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place<'a> {
    /// At the subprogram's frame base plus `offset` -- `DW_OP_fbreg`. The frame base itself is
    /// [`Subprogram::frame_base`], which on every image this has been measured against is the
    /// stack pointer, so this is `SP + offset` and needs nothing from the unwinder.
    FrameOffset(i64),
    /// In a register of the frame -- `DW_OP_regN`. For the frame a target is stopped in that is a
    /// live core register; for any outer frame it is only recoverable if the unwind rules restored
    /// it, which is the caller's problem and not this crate's.
    Register(u64),
    /// At `register + offset`, from `DW_OP_bregN` -- a frame or object pointer with a displacement.
    RegisterOffset {
        /// The DWARF register number.
        register: u64,
        /// A signed byte displacement from its value.
        offset: i64,
    },
    /// At a fixed address in the image -- `DW_OP_addr` or `DW_OP_addrx`. A static, or a local the
    /// compiler promoted to one.
    Address(u64),
    /// An expression this crate does not classify, carried whole.
    ///
    /// **A CALLER MUST REFUSE THIS RATHER THAN SKIP IT.** The commonest shape behind it is
    /// `DW_OP_piece`: a value split across several places, where reading only the first piece
    /// yields a number of the right width and the wrong value. Reporting the variable as absent is
    /// also wrong -- absent means the producer said nothing, and here it said something specific
    /// that this reader declines to act on.
    Expression(&'a [u8]),
    /// The producer described no location covering this address.
    ///
    /// Ordinary and not an error: a variable has no place before its first assignment or after its
    /// last use, and an optimizing compiler emits exactly that.
    Nowhere,
}

/// How a subprogram's frame base is computed, which is what `DW_OP_fbreg` counts from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameBase<'a> {
    /// The value of a register -- `DW_OP_regN`.
    ///
    /// **This is what Embedded Swift emits**: every subprogram observed declares `DW_OP_reg13`, the
    /// stack pointer, and none declares anything else. It is what keeps locals independent of the
    /// unwinder -- a frame offset resolves against a register the target already has.
    Register(u64),
    /// The canonical frame address of the frame -- `DW_OP_call_frame_cfa`.
    ///
    /// A caller must get this from `.debug_frame`, so a producer choosing it couples every local to
    /// the unwind table. Recognized rather than supported: this crate has no target to unwind with.
    CallFrameCfa,
    /// Any other expression, carried whole for the same reason [`Place::Expression`] is.
    Expression(&'a [u8]),
    /// The subprogram declared none, so `DW_OP_fbreg` in it counts from nothing.
    None,
}

/// One parameter or local of a subprogram.
#[derive(Debug, Clone)]
pub struct Local<'a> {
    /// The name, as the producer spelled it.
    ///
    /// `None` where the entry names itself through neither `DW_AT_name` nor an abstract origin.
    /// A pane of unnamed rows is what happens when `DW_AT_abstract_origin` is not followed: an
    /// inlined call's parameters carry no name of their own and point at the abstract instance
    /// that does.
    pub name: Option<&'a [u8]>,
    /// Whether the entry was a formal parameter rather than a variable.
    pub is_parameter: bool,
    /// The offset within `.debug_info` of this entry's `DW_AT_type`, when it declared one.
    pub type_offset: Option<u64>,
    /// How deeply the entry was nested in lexical blocks below the subprogram. Zero is the
    /// subprogram's own scope.
    pub depth: u16,
    location: Location<'a>,
}

impl<'a> Local<'a> {
    /// Where this local's value is when the program counter is at `address`.
    ///
    /// A single expression applies wherever the variable is in scope; a location list applies only
    /// over the ranges it names, and outside them the answer is [`Place::Nowhere`].
    #[must_use]
    pub fn place_at(&self, address: u64) -> Place<'a> {
        match &self.location {
            Location::Single(expression) => classify(expression),
            Location::List(ranges) => ranges
                .iter()
                .find(|&&(start, end, _)| address >= start && address < end)
                .map_or(Place::Nowhere, |&(_, _, expression)| classify(expression)),
            Location::None => Place::Nowhere,
        }
    }

    /// Whether the producer described this local's location with a list of ranges rather than one
    /// expression -- which is the difference between a value that moves as the function runs and
    /// one that does not.
    #[must_use]
    pub fn is_range_described(&self) -> bool {
        matches!(self.location, Location::List(_))
    }

    /// Every range this local has a location over, for a caller reporting what an image contains
    /// rather than asking about one address.
    #[must_use]
    pub fn ranges(&self) -> &[(u64, u64, &'a [u8])] {
        match &self.location {
            Location::List(ranges) => ranges,
            _ => &[],
        }
    }
}

/// A location as the producer expressed it, before an address picks one.
#[derive(Debug, Clone)]
pub(crate) enum Location<'a> {
    /// One expression, in force wherever the entry is in scope.
    Single(&'a [u8]),
    /// `(start, end, expression)`, resolved from `.debug_loc` or `.debug_loclists` at parse time so
    /// that asking about an address needs nothing but this.
    List(Vec<(u64, u64, &'a [u8])>),
    /// No `DW_AT_location`, or one this reader could not resolve to either shape.
    None,
}

/// One subprogram and the names it can see.
#[derive(Debug, Clone)]
pub struct Subprogram<'a> {
    /// The name, following `DW_AT_abstract_origin` and `DW_AT_specification` when the defining
    /// entry carries none of its own.
    pub name: Option<&'a [u8]>,
    /// The first address of the subprogram.
    pub low_pc: u64,
    /// One past its last address.
    pub high_pc: u64,
    /// What `DW_OP_fbreg` in this subprogram counts from.
    pub frame_base: FrameBase<'a>,
    /// Its parameters and locals, in the order the producer declared them -- which for parameters
    /// is the order they are passed in.
    pub locals: Vec<Local<'a>>,
}

impl<'a> Subprogram<'a> {
    /// Whether `address` is inside this subprogram.
    #[must_use]
    pub fn contains(&self, address: u64) -> bool {
        address >= self.low_pc && address < self.high_pc
    }
}

/// Every subprogram in an image that described any local, with those locals.
#[derive(Debug, Clone, Default)]
pub struct Locals<'a> {
    subprograms: Vec<Subprogram<'a>>,
}

impl<'a> Locals<'a> {
    /// Reads `.debug_info` for subprograms and their parameters and variables.
    ///
    /// # Errors
    /// [`DwarfError`] where the section itself cannot be walked. A single UNIT that cannot be read
    /// is skipped rather than failing the image, for the reason [`crate::info::functions`] gives:
    /// a linked image mixes objects and one unreadable contributor is not a reason to report that
    /// the others described nothing.
    pub fn parse(sections: &Sections<'a>) -> Result<Self, DwarfError> {
        let mut subprograms = Vec::new();
        let mut cursor = Cursor::new(sections.debug_info);
        while !cursor.is_empty() {
            if cursor.remaining() < 4 {
                break;
            }
            let (unit_length, format) = cursor.initial_length()?;
            if unit_length == 0 {
                break;
            }
            let length = usize::try_from(unit_length).map_err(|_| DwarfError::Truncated)?;
            let mut unit = cursor.split(length)?;
            if let Ok(mut found) = unit_locals(&mut unit, format, sections) {
                subprograms.append(&mut found);
            }
        }
        Ok(Locals { subprograms })
    }

    /// The same, keeping only subprograms that lie inside one of `code` -- the image's executable
    /// section ranges.
    ///
    /// # AN IMAGE DESCRIBES SUBPROGRAMS THE LINKER REMOVED, AND THEY OVERLAP THE ONES IT KEPT
    ///
    /// `--gc-sections` resolves a relocation against a discarded symbol to zero, so a removed
    /// function's entry survives with `low_pc` 0 and its real length. On a Cortex-M image, whose
    /// code begins at zero, those ranges land ON TOP of live functions -- and a discarded body is
    /// usually SHORTER than the live function it covers, so [`Self::at`] picks it. Measured on
    /// `samples/hello`: asking for the locals at `appMain`'s first instruction answers with a
    /// discarded subprogram spanning `0x0..0x5c`, whose parameters are real entries describing
    /// registers that hold something else entirely.
    ///
    /// **This is not an address-is-zero check and must not become one.** Zero is a legitimate
    /// image offset on these parts -- the vector table lives there. The discriminator is whether an
    /// address falls in a section flagged executable, which is what `code` carries.
    ///
    /// **An EMPTY `code` keeps everything**, because a file that lists no executable section
    /// could not answer the question, and an unanswerable question must not become a negative
    /// answer -- a stripped executable would otherwise report that it contains no subprograms.
    ///
    /// # Errors
    /// As [`Self::parse`].
    pub fn parse_within(
        sections: &Sections<'a>,
        code: &[(u64, u64)],
    ) -> Result<Self, DwarfError> {
        let mut locals = Self::parse(sections)?;
        if !code.is_empty() {
            locals.subprograms.retain(|program| {
                code.iter()
                    .any(|&(start, end)| program.low_pc >= start && program.low_pc < end)
            });
        }
        Ok(locals)
    }

    /// The subprogram covering `address`, or `None` where no entry describes it.
    ///
    /// The INNERMOST is chosen where entries overlap -- a subprogram nested in another, which
    /// DWARF permits -- because that is the scope the names belong to.
    #[must_use]
    pub fn at(&self, address: u64) -> Option<&Subprogram<'a>> {
        self.subprograms
            .iter()
            .filter(|program| program.contains(address))
            .min_by_key(|program| program.high_pc.saturating_sub(program.low_pc))
    }

    /// Every subprogram read, for a caller reporting on an image rather than an address.
    #[must_use]
    pub fn subprograms(&self) -> &[Subprogram<'a>] {
        &self.subprograms
    }

    /// How many subprograms carried locals.
    #[must_use]
    pub fn len(&self) -> usize {
        self.subprograms.len()
    }

    /// Whether the image described no locals at all -- ordinary for a program built without them.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.subprograms.is_empty()
    }
}

/// Classifies one location expression into a place, or carries it whole.
///
/// The classification is deliberately narrow: an expression is recognized only when the WHOLE of it
/// is one of these shapes. `DW_OP_fbreg 16` names a place; `DW_OP_fbreg 16, DW_OP_deref` names the
/// thing at that place, which is a different value, and treating the two alike reports a pointer's
/// target as the pointer.
pub(crate) fn classify(expression: &[u8]) -> Place<'_> {
    let Some((&opcode, rest)) = expression.split_first() else {
        return Place::Nowhere;
    };
    let mut cursor = Cursor::new(rest);
    let place = match opcode {
        DW_OP_ADDR => match cursor.address(4) {
            Ok(address) => Place::Address(address),
            Err(_) => return Place::Expression(expression),
        },
        DW_OP_ADDRX => match cursor.uleb128() {
            Ok(_) => return Place::Expression(expression),
            Err(_) => return Place::Expression(expression),
        },
        DW_OP_FBREG => match cursor.sleb128() {
            Ok(offset) => Place::FrameOffset(offset),
            Err(_) => return Place::Expression(expression),
        },
        DW_OP_REG0..=DW_OP_REG31 => Place::Register(u64::from(opcode - DW_OP_REG0)),
        DW_OP_BREG0..=DW_OP_BREG31 => match cursor.sleb128() {
            Ok(offset) => Place::RegisterOffset {
                register: u64::from(opcode - DW_OP_BREG0),
                offset,
            },
            Err(_) => return Place::Expression(expression),
        },
        _ => return Place::Expression(expression),
    };
    if cursor.is_empty() {
        place
    } else {
        Place::Expression(expression)
    }
}

/// Classifies a `DW_AT_frame_base` expression.
pub(crate) fn classify_frame_base(expression: &[u8]) -> FrameBase<'_> {
    match expression {
        [DW_OP_CALL_FRAME_CFA] => FrameBase::CallFrameCfa,
        [single] if (DW_OP_REG0..=DW_OP_REG31).contains(single) => {
            FrameBase::Register(u64::from(single - DW_OP_REG0))
        }
        _ => FrameBase::Expression(expression),
    }
}

/// What one entry contributed, kept while its unit is walked so references can be resolved.
struct Entry<'a> {
    /// The entry's offset from the start of its unit, which is what a `DW_FORM_ref4` names.
    unit_relative: usize,
    name: Option<&'a [u8]>,
    /// The unit-relative offset of the entry this one takes its name from, when it named one.
    origin: Option<usize>,
}

/// Walks one compilation unit for its subprograms and their locals.
fn unit_locals<'a>(
    unit: &mut Cursor<'a>,
    format: Format,
    sections: &Sections<'a>,
) -> Result<Vec<Subprogram<'a>>, DwarfError> {
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
    let abbrevs = crate::info::abbrev_table(sections.debug_abbrev, abbrev_offset)?;

    let mut context = Context::new(address_size, format);
    let mut loclists_base = None;
    let mut unit_low_pc = 0u64;
    {
        let mut probe = unit.clone();
        if let Ok(bases) = unit_bases(&mut probe, &abbrevs, &context, sections) {
            if let Some(base) = bases.str_offsets_base {
                context.str_offsets_base = base;
            }
            if let Some(base) = bases.addr_base {
                context.addr_base = base;
            }
            loclists_base = bases.loclists_base;
            unit_low_pc = bases.low_pc.unwrap_or(0);
        }
    }

    let header_length_size = match format {
        Format::Dwarf32 => 4,
        Format::Dwarf64 => 12,
    };

    let mut out: Vec<Subprogram<'a>> = Vec::new();
    let mut entries: Vec<Entry<'a>> = Vec::new();
    let mut open: Vec<u64> = Vec::new();
    let mut current: Option<(usize, usize)> = None;
    let mut pending: Vec<(usize, usize, Option<usize>)> = Vec::new();

    while !unit.is_empty() {
        let entry_offset = unit.offset();
        let code = unit.uleb128()?;
        if code == 0 {
            if let Some(tag) = open.pop() {
                if tag == DW_TAG_SUBPROGRAM {
                    current = None;
                }
            }
            continue;
        }
        let Some(abbrev) = abbrevs.iter().find(|a| a.code == code) else {
            return Err(DwarfError::UnknownAbbreviation(code));
        };

        let mut name = None;
        let mut origin = None;
        let mut low_pc = None;
        let mut high_pc = None;
        let mut high_pc_is_address = false;
        let mut frame_base = FrameBase::None;
        let mut type_offset = None;
        let mut location = Location::None;
        for &(attribute, form, implicit) in &abbrev.attributes {
            let value = if form == DW_FORM_IMPLICIT_CONST {
                Value::Signed(implicit)
            } else {
                form::read_with(unit, form, &context, sections)?
            };
            match attribute {
                DW_AT_NAME => name = value.as_bytes(),
                DW_AT_ABSTRACT_ORIGIN | DW_AT_SPECIFICATION => {
                    origin = unit_relative_reference(form, value);
                }
                DW_AT_LOW_PC => low_pc = value.as_u64(),
                DW_AT_HIGH_PC => {
                    high_pc = value.as_u64();
                    high_pc_is_address = form == DW_FORM_ADDR;
                }
                DW_AT_TYPE => type_offset = value.as_u64(),
                DW_AT_FRAME_BASE => {
                    if let Some(bytes) = value.as_bytes() {
                        frame_base = classify_frame_base(bytes);
                    }
                }
                DW_AT_LOCATION => {
                    location = read_location(
                        form,
                        value,
                        version,
                        address_size,
                        format,
                        loclists_base,
                        unit_low_pc,
                        sections,
                    );
                }
                _ => {}
            }
        }

        let unit_relative = entry_offset + header_length_size;
        entries.push(Entry {
            unit_relative,
            name,
            origin,
        });

        match abbrev.tag {
            DW_TAG_SUBPROGRAM => {
                if let (Some(low), Some(high)) = (low_pc, high_pc) {
                    let high = if high_pc_is_address {
                        high
                    } else {
                        low.saturating_add(high)
                    };
                    out.push(Subprogram {
                        name,
                        low_pc: low,
                        high_pc: high,
                        frame_base,
                        locals: Vec::new(),
                    });
                    if abbrev.has_children {
                        current = Some((out.len() - 1, open.len()));
                    }
                }
            }
            DW_TAG_FORMAL_PARAMETER | DW_TAG_VARIABLE => {
                if let Some((index, opened_at)) = current {
                    let depth =
                        u16::try_from(open.len().saturating_sub(opened_at + 1)).unwrap_or(u16::MAX);
                    pending.push((index, out[index].locals.len(), origin));
                    out[index].locals.push(Local {
                        name,
                        is_parameter: abbrev.tag == DW_TAG_FORMAL_PARAMETER,
                        type_offset,
                        depth,
                        location,
                    });
                }
            }
            _ => {}
        }
        if abbrev.has_children {
            open.push(abbrev.tag);
        }
    }

    for (subprogram, local, origin) in pending {
        if out[subprogram].locals[local].name.is_some() {
            continue;
        }
        if let Some(origin) = origin {
            out[subprogram].locals[local].name = resolve_name(&entries, origin, 0);
        }
    }
    Ok(out)
}

/// Follows an origin reference to the entry that carries the name.
///
/// A chain is legal -- a concrete inlined instance points at an abstract one, which may itself be a
/// specification -- so this follows it, with a bound. **The bound is not defensiveness about depth:
/// a damaged or hostile file can point an entry at itself**, and a reader that trusts the chain
/// then does not return.
fn resolve_name<'a>(entries: &[Entry<'a>], offset: usize, depth: u8) -> Option<&'a [u8]> {
    if depth > 8 {
        return None;
    }
    let entry = entries.iter().find(|e| e.unit_relative == offset)?;
    if let Some(name) = entry.name {
        return Some(name);
    }
    resolve_name(entries, entry.origin?, depth + 1)
}

/// A reference expressed as an offset from the start of its unit, which is what the `DW_FORM_ref*`
/// family means -- `DW_FORM_ref_addr` counts from the section instead and is not resolved here.
fn unit_relative_reference(form: u64, value: Value<'_>) -> Option<usize> {
    match form {
        DW_FORM_REF1 | DW_FORM_REF2 | DW_FORM_REF4 | DW_FORM_REF8 | DW_FORM_REF_UDATA => {
            usize::try_from(value.as_u64()?).ok()
        }
        _ => None,
    }
}

/// The attributes of a unit's own entry that its later entries are read against.
#[derive(Default)]
struct UnitBases {
    str_offsets_base: Option<u64>,
    addr_base: Option<u64>,
    loclists_base: Option<u64>,
    low_pc: Option<u64>,
}

/// Reads the first entry of a unit for the bases and the base address a location list counts from.
fn unit_bases<'a>(
    unit: &mut Cursor<'a>,
    abbrevs: &[crate::info::Abbrev],
    context: &Context,
    sections: &Sections<'a>,
) -> Result<UnitBases, DwarfError> {
    let code = unit.uleb128()?;
    let Some(abbrev) = abbrevs.iter().find(|a| a.code == code) else {
        return Ok(UnitBases::default());
    };
    let mut bases = UnitBases::default();
    for &(attribute, form, implicit) in &abbrev.attributes {
        let value = if form == DW_FORM_IMPLICIT_CONST {
            Value::Signed(implicit)
        } else {
            form::read_with(unit, form, context, sections)?
        };
        match attribute {
            DW_AT_STR_OFFSETS_BASE => bases.str_offsets_base = value.as_u64(),
            DW_AT_ADDR_BASE => bases.addr_base = value.as_u64(),
            DW_AT_LOCLISTS_BASE => bases.loclists_base = value.as_u64(),
            DW_AT_LOW_PC => bases.low_pc = value.as_u64(),
            _ => {}
        }
    }
    Ok(bases)
}

/// Turns a `DW_AT_location` attribute into the shape [`Local::place_at`] searches.
#[allow(clippy::too_many_arguments)]
fn read_location<'a>(
    form: u64,
    value: Value<'a>,
    version: u16,
    address_size: u8,
    format: Format,
    loclists_base: Option<u64>,
    unit_low_pc: u64,
    sections: &Sections<'a>,
) -> Location<'a> {
    match form {
        DW_FORM_EXPRLOC => value.as_bytes().map_or(Location::None, Location::Single),
        DW_FORM_SEC_OFFSET => {
            let Some(offset) = value.as_u64() else {
                return Location::None;
            };
            if version >= 5 {
                loclists_at(sections.debug_loclists, offset, address_size, unit_low_pc)
            } else {
                loc_at(sections.debug_loc, offset, address_size, unit_low_pc)
            }
        }
        DW_FORM_LOCLISTX => {
            let Some(index) = value.as_u64() else {
                return Location::None;
            };
            let base = loclists_base.unwrap_or(LOCLISTS_HEADER_SIZE);
            let Some(offset) = loclists_index(sections.debug_loclists, base, index, format) else {
                return Location::None;
            };
            loclists_at(sections.debug_loclists, offset, address_size, unit_low_pc)
        }
        _ => Location::None,
    }
}

/// The size of a DWARF 32 `.debug_loclists` header: length, version, address size, segment selector
/// size, offset entry count.
const LOCLISTS_HEADER_SIZE: u64 = 12;

/// Resolves a `DW_FORM_loclistx` index through the offset table that follows the section header.
fn loclists_index(section: &[u8], base: u64, index: u64, format: Format) -> Option<u64> {
    let width = u64::from(format.offset_size());
    let at = usize::try_from(base.checked_add(index.checked_mul(width)?)?).ok()?;
    let mut cursor = Cursor::at(section, at).ok()?;
    let offset = cursor.offset_of(format).ok()?;
    base.checked_add(offset)
}

/// Reads a DWARF 4 `.debug_loc` list.
///
/// Entries are pairs of addresses. An all-ones first value is a base address selection rather than
/// a range, and a pair of zeroes ends the list -- so a reader that treats every pair as a range
/// stops at the wrong place and reports ranges built from a base it never applied.
pub(crate) fn loc_at<'a>(
    section: &'a [u8],
    offset: u64,
    address_size: u8,
    unit_low_pc: u64,
) -> Location<'a> {
    let Ok(at) = usize::try_from(offset) else {
        return Location::None;
    };
    let Ok(mut cursor) = Cursor::at(section, at) else {
        return Location::None;
    };
    let all_ones = match address_size {
        8 => u64::MAX,
        _ => u64::from(u32::MAX),
    };
    let mut base = unit_low_pc;
    let mut ranges = Vec::new();
    while let (Ok(first), Ok(second)) =
        (cursor.address(address_size), cursor.address(address_size))
    {
        if first == 0 && second == 0 {
            break;
        }
        if first == all_ones {
            base = second;
            continue;
        }
        let Ok(length) = cursor.u16() else {
            break;
        };
        let Ok(expression) = cursor.take(usize::from(length)) else {
            break;
        };
        ranges.push((
            base.saturating_add(first),
            base.saturating_add(second),
            expression,
        ));
    }
    if ranges.is_empty() {
        Location::None
    } else {
        Location::List(ranges)
    }
}

const DW_LLE_END_OF_LIST: u8 = 0x00;
const DW_LLE_BASE_ADDRESSX: u8 = 0x01;
const DW_LLE_STARTX_ENDX: u8 = 0x02;
const DW_LLE_STARTX_LENGTH: u8 = 0x03;
const DW_LLE_OFFSET_PAIR: u8 = 0x04;
const DW_LLE_DEFAULT_LOCATION: u8 = 0x05;
const DW_LLE_BASE_ADDRESS: u8 = 0x06;
const DW_LLE_START_END: u8 = 0x07;
const DW_LLE_START_LENGTH: u8 = 0x08;

/// Reads a DWARF 5 `.debug_loclists` list.
///
/// The indexed kinds -- `startx_endx`, `startx_length`, `base_addressx` -- name addresses through
/// `.debug_addr` rather than carrying them. Resolving those needs the unit's `DW_AT_addr_base`, and
/// an entry this cannot place ENDS the list rather than being skipped: the entries after it are
/// still readable, but reporting some of a variable's ranges as all of them says the variable is
/// absent over addresses where it is not.
pub(crate) fn loclists_at<'a>(
    section: &'a [u8],
    offset: u64,
    address_size: u8,
    unit_low_pc: u64,
) -> Location<'a> {
    let Ok(at) = usize::try_from(offset) else {
        return Location::None;
    };
    let Ok(mut cursor) = Cursor::at(section, at) else {
        return Location::None;
    };
    let mut base = unit_low_pc;
    let mut ranges = Vec::new();
    while let Ok(kind) = cursor.u8() {
        match kind {
            DW_LLE_END_OF_LIST => break,
            DW_LLE_BASE_ADDRESS => match cursor.address(address_size) {
                Ok(address) => base = address,
                Err(_) => break,
            },
            DW_LLE_OFFSET_PAIR => {
                let (Ok(start), Ok(end)) = (cursor.uleb128(), cursor.uleb128()) else {
                    break;
                };
                let Some(expression) = counted_expression(&mut cursor) else {
                    break;
                };
                ranges.push((
                    base.saturating_add(start),
                    base.saturating_add(end),
                    expression,
                ));
            }
            DW_LLE_START_END => {
                let (Ok(start), Ok(end)) =
                    (cursor.address(address_size), cursor.address(address_size))
                else {
                    break;
                };
                let Some(expression) = counted_expression(&mut cursor) else {
                    break;
                };
                ranges.push((start, end, expression));
            }
            DW_LLE_START_LENGTH => {
                let (Ok(start), Ok(length)) = (cursor.address(address_size), cursor.uleb128())
                else {
                    break;
                };
                let Some(expression) = counted_expression(&mut cursor) else {
                    break;
                };
                ranges.push((start, start.saturating_add(length), expression));
            }
            DW_LLE_DEFAULT_LOCATION => {
                let Some(expression) = counted_expression(&mut cursor) else {
                    break;
                };
                ranges.push((0, u64::MAX, expression));
            }
            DW_LLE_BASE_ADDRESSX | DW_LLE_STARTX_ENDX | DW_LLE_STARTX_LENGTH => break,
            _ => break,
        }
    }
    if ranges.is_empty() {
        Location::None
    } else {
        Location::List(ranges)
    }
}

/// A ULEB128 length followed by that many bytes of expression.
fn counted_expression<'a>(cursor: &mut Cursor<'a>) -> Option<&'a [u8]> {
    let length = cursor.uleb128().ok()?;
    cursor.take(usize::try_from(length).ok()?).ok()
}
