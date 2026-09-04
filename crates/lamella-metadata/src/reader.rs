//! A navigable reader over an assembly's metadata.

use crate::bytes::Reader;
use crate::constant::{ConstantValue, decode_constant};
use crate::flags;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

use crate::heaps::{StringsHeap, read_compressed_u32};
use crate::image::{MetadataError, MetadataImage};
use crate::layout::{LayoutError, TargetLayout, TypeLayout, layout_value_type};
use crate::pe::PeImage;
use crate::rows::Tables;
use crate::signature::{
    LocalVar, MethodSig, SigType, calling, element, parse_field, parse_local_vars, parse_method,
    parse_type,
};
use crate::tables::{TableError, TablesHeader, table};
use lamella_cil::{EhKind, MethodBodyImage, instruction_offsets, read_method_body, read_method_body_sized};
use lamella_token::Token;

impl From<TableError> for MetadataError {
    fn from(_: TableError) -> MetadataError {
        MetadataError::Truncated
    }
}

/// The `CharSet` of a P/Invoke (its `DllImport` `CharSet`), decoded from the `ImplMap` MappingFlags
/// (II.23.1.8, mask `0x0006`). It selects how a `string` argument marshals to native -- `Ansi` to a
/// byte `char*`, `Unicode` to a `wchar_t*`. The calling convention (bits `0x0700`: Cdecl/StdCall/...)
/// is an x86 stack-cleanup distinction with no effect on the AOT's AAPCS targets (ARM/RISC-V), so it is
/// not modeled; `SetLastError` (`0x0040`) is likewise a later concern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharSet {
    /// Unspecified -- treated as `Ansi` by convention.
    NotSpecified,
    /// One-byte characters: a native `char*`.
    Ansi,
    /// Two-byte characters: a native `wchar_t*`.
    Unicode,
    /// Platform default (`Ansi` here; .NET historically chose per-OS).
    Auto,
}

/// A type's namespace and name (II.22.37), as borrowed `#Strings` slices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypeName<'a> {
    /// The namespace, empty for the global namespace.
    pub namespace: &'a str,
    /// The unqualified type name.
    pub name: &'a str,
}

/// One exception-handling clause of a method ([`Method::exception_clauses`]), with its
/// protected and handler regions as IL BYTE offsets (II.25.4.6) -- the form an AOT/CFG
/// lowering maps onto its basic blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExceptionClause {
    /// What the handler is, and any caught type or filter it carries.
    pub kind: ExceptionHandlerKind,
    /// IL byte offset where the protected (try) region begins.
    pub try_offset: u32,
    /// Byte length of the protected region.
    pub try_length: u32,
    /// IL byte offset where the handler region begins.
    pub handler_offset: u32,
    /// Byte length of the handler region.
    pub handler_length: u32,
}

/// What an [`ExceptionClause`] handler does (II.25.4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExceptionHandlerKind {
    /// A typed `catch`, carrying the caught type's metadata token.
    Catch(Token),
    /// A filtered handler; the filter expression begins at this IL byte offset.
    Filter {
        /// The IL byte offset where the filter expression starts.
        filter_offset: u32,
    },
    /// A `finally` handler, run on both normal exit and exception.
    Finally,
    /// A `fault` handler, run only when an exception leaves the try region.
    Fault,
}

/// The exception tag for a type's `namespace` + `name`: FNV-1a 32-bit over the full name
/// (`"System" + "." + "DivideByZeroException"`), the high bit forced so every tag is a
/// nonzero failure value. The AOT computes a catch/throw tag from a token via
/// [`Assembly::exception_tag`]; the compiler computes a base-chain vector's tags by name
/// with this same function -- both agree because the formula is one.
#[must_use]
pub fn exception_tag_for_name(namespace: &str, name: &str) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    if !namespace.is_empty() {
        hash = fnv1a32(hash, namespace.as_bytes());
        hash = fnv1a32(hash, b".");
    }
    hash = fnv1a32(hash, name.as_bytes());
    hash | 0x8000_0000
}

/// FNV-1a 32-bit, folding `bytes` into the running `hash` (seed with the FNV offset basis
/// `0x811c_9dc5`). The shared hash primitive behind every decentralized name tag -- exception tags,
/// type tags, and the AOT's interface-method tags -- so all engines derive identical tags from
/// metadata with no shared registry.
pub fn fnv1a32(hash: u32, bytes: &[u8]) -> u32 {
    let mut hash = hash;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Encodes an exception base-chain tag vector as the `<ExceptionBaseChain>` attribute value
/// blob the compiler emits: a standard custom-attribute blob (II.23.3) for a single `uint[]`
/// positional argument -- a `0x0001` prolog, the array length, the little-endian `u32` tags,
/// then a zero named-argument count. The inverse of [`Assembly::exception_base_chain`]'s
/// decode, kept here so the emit and read sides share one format.
#[must_use]
pub fn encode_exception_base_chain(tags: &[u32]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(8 + tags.len() * 4);
    blob.extend_from_slice(&0x0001u16.to_le_bytes());
    blob.extend_from_slice(&(tags.len() as u32).to_le_bytes());
    for tag in tags {
        blob.extend_from_slice(&tag.to_le_bytes());
    }
    blob.extend_from_slice(&0x0000u16.to_le_bytes());
    blob
}

/// Decodes a `<ExceptionBaseChain>` attribute value blob produced by
/// [`encode_exception_base_chain`]. `None` if the blob is malformed.
fn decode_exception_base_chain(blob: &[u8]) -> Option<Vec<u32>> {
    if blob.len() < 6 || blob[0] != 0x01 || blob[1] != 0x00 {
        return None;
    }
    let count = u32::from_le_bytes(blob[2..6].try_into().ok()?) as usize;
    let mut tags = Vec::with_capacity(count);
    let mut offset = 6;
    for _ in 0..count {
        let bytes = blob.get(offset..offset + 4)?;
        tags.push(u32::from_le_bytes(bytes.try_into().ok()?));
        offset += 4;
    }
    Some(tags)
}

/// The byte width of a primitive [`SigType`], when csc reuses such a type as a constant array
/// literal's RVA holder (a blob of exactly 1/2/4/8 bytes is typed `int8`/`int16`/`int32`/`int64`
/// rather than a synthesized `__StaticArrayInitTypeSize=N` struct). `None` for any non-primitive
/// type (a reference, pointer, native int, or value type), which is not a size-optimized holder.
fn primitive_field_size(sig: &SigType) -> Option<u32> {
    Some(match sig {
        SigType::Boolean | SigType::I1 | SigType::U1 => 1,
        SigType::Char | SigType::I2 | SigType::U2 => 2,
        SigType::I4 | SigType::U4 | SigType::R4 => 4,
        SigType::I8 | SigType::U8 | SigType::R8 => 8,
        _ => return None,
    })
}

/// A method a call token resolves to ([`Assembly::resolve_method`]): its name,
/// declaring type, and signature, plus where it is defined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMethod<'a> {
    /// The method's name.
    pub name: Option<&'a str>,
    /// The namespace and name of the type that declares it.
    pub declaring_type: Option<TypeName<'a>>,
    /// The decoded method signature.
    pub signature: Option<MethodSig>,
    /// Whether the method is defined here or referenced from elsewhere.
    pub kind: MethodKind,
}

/// Where a resolved method lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    /// Defined in this assembly: the `MethodDef` row index, which an AOT lowering
    /// maps to a compiled callee.
    Definition(u32),
    /// Referenced from another assembly (a `MemberRef`) -- e.g. a BCL method an AOT
    /// backend recognizes as an intrinsic.
    Reference,
}

/// A custom attribute applied to a target (II.22.10): the constructor it invokes
/// and its raw argument blob (decoding the blob per II.23.3 is left to callers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CustomAttribute<'a> {
    /// The token of the attribute's constructor (a `MethodDef` or `MemberRef`).
    pub constructor: Token,
    /// The raw argument blob.
    pub value: &'a [u8],
}

/// One decoded argument value of a custom attribute (II.23.3): a positional (fixed)
/// constructor argument, or a named field/property value. The set covers the elem-types
/// a `Elem` / `FieldOrPropType` can carry for the attribute surface the interpreter
/// instantiates -- the primitives, `string`, a `System.Type` (`typeof(X)`), and an enum
/// value (its underlying integer). Strings and the `typeof` type name are borrowed from the
/// blob so the decoder allocates nothing; the caller materializes them.
#[derive(Debug, Clone, PartialEq)]
pub enum AttrArg<'a> {
    /// A boolean.
    Bool(bool),
    /// A character (a UTF-16 code unit).
    Char(u16),
    /// A signed integer of 8/16/32/64 bits, sign-extended to `i64` (the value an enum
    /// argument also takes -- its underlying integer).
    Int(i64),
    /// An unsigned integer of 8/16/32/64 bits, zero-extended to `u64`.
    UInt(u64),
    /// A single-precision float.
    R4(f32),
    /// A double-precision float.
    R8(f64),
    /// A `string` argument, as the borrowed UTF-8 bytes of the blob's SerString (the blob
    /// stores attribute strings as UTF-8, II.23.3, not UTF-16 like the `Constant` heap).
    Str(&'a str),
    /// A null reference (a null `string` / `Type` / `object` argument: SerString length `0xFF`).
    Null,
    /// A `System.Type` argument (`typeof(X)`): the borrowed type NAME the blob serializes
    /// (its reflection name, e.g. `"Program"`). The caller resolves it to a type handle.
    Type(&'a str),
    /// A single-dimension array argument (`new int[]{...}` / `new string[]{...}`): the decoded
    /// elements in order. A NULL array (`NumElem == 0xFFFF_FFFF`, II.23.3) decodes as
    /// [`AttrArg::Null`], not as an empty `Array`.
    Array(Vec<AttrArg<'a>>),
}

/// One decoded named argument of a custom attribute (II.23.3): which member it sets and
/// its value.
#[derive(Debug, Clone, PartialEq)]
pub struct AttrNamed<'a> {
    /// Whether the named member is a field (`true`, sentinel `0x53`) or a property
    /// (`false`, sentinel `0x54`).
    pub is_field: bool,
    /// The member's name.
    pub name: &'a str,
    /// The value to assign.
    pub value: AttrArg<'a>,
}

/// The decoded positional and named arguments of a custom-attribute value blob.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DecodedAttribute<'a> {
    /// The fixed (constructor) arguments, in declaration order.
    pub fixed: Vec<AttrArg<'a>>,
    /// The named field/property arguments.
    pub named: Vec<AttrNamed<'a>>,
}

const SERIALIZATION_TYPE: u8 = 0x50;
const SERIALIZATION_TAGGED_OBJECT: u8 = 0x51;
const SERIALIZATION_ENUM: u8 = 0x55;

/// Reads a SerString (II.23.3): a `PackedLen`-prefixed UTF-8 string, or the single byte
/// `0xFF` for a null string. Returns `Some(None)` for the null case, `Some(Some(s))` for a
/// present string, and `None` on a malformed/truncated blob.
fn read_ser_string<'a>(reader: &mut Reader<'a>) -> Option<Option<&'a str>> {
    if reader.peek_u8().ok()? == 0xFF {
        reader.read_u8().ok()?;
        return Some(None);
    }
    let len = reader.read_compressed_u32().ok()? as usize;
    let bytes = reader.read_bytes(len).ok()?;
    Some(Some(core::str::from_utf8(bytes).ok()?))
}

/// Reads one fixed-argument value of the given element-type signature from a custom-attribute
/// blob (II.23.3). `enum_width` resolves an enum type NAME to its underlying integer
/// element-type byte (e.g. `I4`), needed because an enum value is serialized at its
/// underlying width with no inline tag. `None` on a malformed blob or an element type outside
/// the supported attribute surface.
fn read_attr_value<'a>(
    reader: &mut Reader<'a>,
    sig: &SigType,
    enum_width: &dyn Fn(&str) -> u8,
) -> Option<AttrArg<'a>> {
    Some(match sig {
        SigType::Boolean => AttrArg::Bool(reader.read_u8().ok()? != 0),
        SigType::Char => AttrArg::Char(reader.read_u16().ok()?),
        SigType::I1 => AttrArg::Int(i64::from(reader.read_u8().ok()? as i8)),
        SigType::U1 => AttrArg::UInt(u64::from(reader.read_u8().ok()?)),
        SigType::I2 => AttrArg::Int(i64::from(reader.read_u16().ok()? as i16)),
        SigType::U2 => AttrArg::UInt(u64::from(reader.read_u16().ok()?)),
        SigType::I4 => AttrArg::Int(i64::from(reader.read_u32().ok()? as i32)),
        SigType::U4 => AttrArg::UInt(u64::from(reader.read_u32().ok()?)),
        SigType::I8 => AttrArg::Int(reader.read_u64().ok()? as i64),
        SigType::U8 => AttrArg::UInt(reader.read_u64().ok()?),
        SigType::R4 => AttrArg::R4(f32::from_bits(reader.read_u32().ok()?)),
        SigType::R8 => AttrArg::R8(f64::from_bits(reader.read_u64().ok()?)),
        SigType::String => match read_ser_string(reader)? {
            Some(text) => AttrArg::Str(text),
            None => AttrArg::Null,
        },
        SigType::ValueType(_) => read_attr_value(reader, &SigType::I4, enum_width)?,
        SigType::SzArray(element) => {
            let count = reader.read_u32().ok()?;
            if count == 0xFFFF_FFFF {
                AttrArg::Null
            } else {
                let mut elements = Vec::with_capacity((count as usize).min(4096));
                for _ in 0..count {
                    elements.push(read_attr_value(reader, element, enum_width)?);
                }
                AttrArg::Array(elements)
            }
        }
        SigType::Class(_) | SigType::Object => return read_tagged_value(reader, enum_width),
        _ => return None,
    })
}

/// The [`SigType`] a one-byte `FieldOrPropType` element tag names, for the primitive tags an
/// `object`-boxed / named-argument ARRAY may carry as its element type (II.23.3). `None` for a
/// tag with no direct primitive mapping (enum/type/tagged-object elements are not modeled here).
fn tag_element_sig(tag: u8) -> Option<SigType> {
    Some(match tag {
        element::BOOLEAN => SigType::Boolean,
        element::CHAR => SigType::Char,
        element::I1 => SigType::I1,
        element::U1 => SigType::U1,
        element::I2 => SigType::I2,
        element::U2 => SigType::U2,
        element::I4 => SigType::I4,
        element::U4 => SigType::U4,
        element::I8 => SigType::I8,
        element::U8 => SigType::U8,
        element::R4 => SigType::R4,
        element::R8 => SigType::R8,
        element::STRING => SigType::String,
        _ => return None,
    })
}

/// Reads a tagged value (a `System.Type`/`object` fixed argument, a named-argument value, or a
/// boxed `object` element): a one-byte `FieldOrPropType` describing the value's type, then the
/// value (II.23.3).
fn read_tagged_value<'a>(
    reader: &mut Reader<'a>,
    enum_width: &dyn Fn(&str) -> u8,
) -> Option<AttrArg<'a>> {
    let tag = reader.read_u8().ok()?;
    match tag {
        SERIALIZATION_TYPE => match read_ser_string(reader)? {
            Some(name) => Some(AttrArg::Type(name)),
            None => Some(AttrArg::Null),
        },
        SERIALIZATION_ENUM => {
            let enum_name = read_ser_string(reader)??;
            let width_sig = match enum_width(enum_name) {
                element::I1 => SigType::I1,
                element::U1 => SigType::U1,
                element::I2 => SigType::I2,
                element::U2 => SigType::U2,
                element::U4 => SigType::U4,
                element::I8 => SigType::I8,
                element::U8 => SigType::U8,
                _ => SigType::I4,
            };
            read_attr_value(reader, &width_sig, enum_width)
        }
        SERIALIZATION_TAGGED_OBJECT => read_tagged_value(reader, enum_width),
        element::BOOLEAN => read_attr_value(reader, &SigType::Boolean, enum_width),
        element::CHAR => read_attr_value(reader, &SigType::Char, enum_width),
        element::I1 => read_attr_value(reader, &SigType::I1, enum_width),
        element::U1 => read_attr_value(reader, &SigType::U1, enum_width),
        element::I2 => read_attr_value(reader, &SigType::I2, enum_width),
        element::U2 => read_attr_value(reader, &SigType::U2, enum_width),
        element::I4 => read_attr_value(reader, &SigType::I4, enum_width),
        element::U4 => read_attr_value(reader, &SigType::U4, enum_width),
        element::I8 => read_attr_value(reader, &SigType::I8, enum_width),
        element::U8 => read_attr_value(reader, &SigType::U8, enum_width),
        element::R4 => read_attr_value(reader, &SigType::R4, enum_width),
        element::R8 => read_attr_value(reader, &SigType::R8, enum_width),
        element::STRING => read_attr_value(reader, &SigType::String, enum_width),
        element::SZARRAY => {
            let element_sig = tag_element_sig(reader.read_u8().ok()?)?;
            read_attr_value(reader, &SigType::SzArray(Box::new(element_sig)), enum_width)
        }
        _ => None,
    }
}

/// Decodes a custom-attribute value blob (II.23.3) against its constructor's parameter
/// signature: the `0x0001` prolog, one fixed argument per `ctor_params` entry (each read at
/// that parameter's type), then the named-argument count and that many `(FIELD|PROPERTY,
/// type, name, value)` records. `enum_width` maps an enum type NAME (as the blob serializes
/// it) to its underlying integer element-type byte, so an enum-typed argument is read at the
/// right width; return [`element::I4`] when the enum is unknown (the C# default). Returns the
/// decoded fixed and named arguments, or `None` if the blob is malformed (e.g. a wrong prolog
/// or a truncated value).
#[must_use]
pub fn decode_custom_attribute<'a>(
    blob: &'a [u8],
    ctor_params: &[SigType],
    enum_width: &dyn Fn(&str) -> u8,
) -> Option<DecodedAttribute<'a>> {
    let mut reader = Reader::new(blob);
    if reader.read_u16().ok()? != 0x0001 {
        return None;
    }
    let mut fixed = Vec::with_capacity(ctor_params.len());
    for param in ctor_params {
        fixed.push(read_attr_value(&mut reader, param, enum_width)?);
    }
    let named_count = reader.read_u16().unwrap_or(0);
    let mut named = Vec::with_capacity(named_count as usize);
    for _ in 0..named_count {
        let is_field = match reader.read_u8().ok()? {
            0x53 => true,
            0x54 => false,
            _ => return None,
        };
        let field_or_prop = reader.read_u8().ok()?;
        let enum_name = if field_or_prop == SERIALIZATION_ENUM {
            Some(read_ser_string(&mut reader)??)
        } else {
            None
        };
        let name = read_ser_string(&mut reader)??;
        let value = match field_or_prop {
            SERIALIZATION_TYPE => match read_ser_string(&mut reader)? {
                Some(type_name) => AttrArg::Type(type_name),
                None => AttrArg::Null,
            },
            SERIALIZATION_ENUM => read_enum_value(&mut reader, enum_name?, enum_width)?,
            SERIALIZATION_TAGGED_OBJECT | element::OBJECT => {
                read_tagged_value(&mut reader, enum_width)?
            }
            elem => read_attr_value(&mut reader, &elem_to_sig(elem)?, enum_width)?,
        };
        named.push(AttrNamed {
            is_field,
            name,
            value,
        });
    }
    Some(DecodedAttribute { fixed, named })
}

/// Maps a named-argument `FieldOrPropType` element-type byte to the signature type its value
/// is read as (the subset a custom attribute may carry, II.23.3). `None` for an unsupported
/// element type.
fn elem_to_sig(elem: u8) -> Option<SigType> {
    Some(match elem {
        element::BOOLEAN => SigType::Boolean,
        element::CHAR => SigType::Char,
        element::I1 => SigType::I1,
        element::U1 => SigType::U1,
        element::I2 => SigType::I2,
        element::U2 => SigType::U2,
        element::I4 => SigType::I4,
        element::U4 => SigType::U4,
        element::I8 => SigType::I8,
        element::U8 => SigType::U8,
        element::R4 => SigType::R4,
        element::R8 => SigType::R8,
        element::STRING => SigType::String,
        element::OBJECT => SigType::Object,
        _ => return None,
    })
}

/// Reads an enum-typed value at the enum's underlying width, resolved through `enum_width`.
fn read_enum_value<'a>(
    reader: &mut Reader<'a>,
    enum_name: &str,
    enum_width: &dyn Fn(&str) -> u8,
) -> Option<AttrArg<'a>> {
    let sig = match enum_width(enum_name) {
        element::I1 => SigType::I1,
        element::U1 => SigType::U1,
        element::I2 => SigType::I2,
        element::U2 => SigType::U2,
        element::U4 => SigType::U4,
        element::I8 => SigType::I8,
        element::U8 => SigType::U8,
        _ => SigType::I4,
    };
    read_attr_value(reader, &sig, enum_width)
}

/// A read-only, navigable view of one assembly's metadata.
#[derive(Debug, Clone, Copy)]
pub struct Assembly<'a> {
    image: MetadataImage<'a>,
    tables: Tables<'a>,
    /// The whole PE file, kept so method bodies (addressed by RVA) can be read.
    /// `None` when built from a bare metadata image.
    file: Option<&'a [u8]>,
}

impl<'a> Assembly<'a> {
    /// Reads a managed PE file into a navigable assembly.
    pub fn read(file: &'a [u8]) -> Result<Assembly<'a>, MetadataError> {
        let mut assembly = Assembly::from_image(MetadataImage::read(file)?)?;
        assembly.file = Some(file);
        Ok(assembly)
    }

    /// Builds the navigable view over an already-parsed metadata image. Method
    /// bodies are unavailable (no backing PE) when built this way.
    pub fn from_image(image: MetadataImage<'a>) -> Result<Assembly<'a>, MetadataError> {
        let header = TablesHeader::parse(image.tables())?;
        let tables = Tables::new(header)?;
        Ok(Assembly {
            image,
            tables,
            file: None,
        })
    }

    /// The underlying metadata image.
    #[must_use]
    pub fn image(&self) -> &MetadataImage<'a> {
        &self.image
    }

    /// The whole assembly file buffer this was read from, if it was read via [`Assembly::read`]
    /// (as opposed to constructed from an already-parsed image). The `#Strings`, `#US`, and method
    /// IL slices the reader hands out are sub-slices of this buffer, so a consumer can record a
    /// slice's position as a byte offset (`slice.as_ptr() - file.as_ptr()`) and resolve it later
    /// against an owned copy of these bytes.
    #[must_use]
    pub fn file(&self) -> Option<&'a [u8]> {
        self.file
    }

    /// The parsed tables.
    #[must_use]
    pub fn tables(&self) -> &Tables<'a> {
        &self.tables
    }

    fn strings(&self) -> StringsHeap<'a> {
        self.image.strings()
    }

    /// The module's name (the single `Module` row, II.22.30).
    #[must_use]
    pub fn module_name(&self) -> Option<&'a str> {
        let module = self.tables.row(table::MODULE, 1)?;
        self.strings().get(module.raw(1)).ok()
    }

    /// The number of type definitions (II.22.37).
    #[must_use]
    pub fn type_count(&self) -> u32 {
        self.tables.row_count(table::TYPE_DEF)
    }

    /// The namespace and name of the 1-based `index`-th type definition. Row 1 is
    /// the `<Module>` pseudo-type (II.22.37).
    #[must_use]
    pub fn type_name(&self, index: u32) -> Option<TypeName<'a>> {
        let row = self.tables.row(table::TYPE_DEF, index)?;
        let name = self.strings().get(row.raw(1)).ok()?;
        let namespace = self.strings().get(row.raw(2)).ok()?;
        Some(TypeName { namespace, name })
    }

    /// Iterates the type definitions by namespace and name.
    pub fn types(&self) -> impl Iterator<Item = TypeName<'a>> + '_ {
        (1..=self.type_count()).filter_map(move |index| self.type_name(index))
    }

    /// The 1-based `index`-th type definition as a navigable view.
    #[must_use]
    pub fn type_def(&self, index: u32) -> Option<TypeDef<'a>> {
        if index == 0 || index > self.type_count() {
            return None;
        }
        Some(TypeDef {
            assembly: *self,
            index,
        })
    }

    /// Iterates the type definitions as navigable views.
    pub fn type_defs(&self) -> impl Iterator<Item = TypeDef<'a>> + '_ {
        (1..=self.type_count()).filter_map(move |index| self.type_def(index))
    }

    /// The type definition with the given namespace and name, if any. The binder's
    /// basic resolution step against a reference assembly.
    #[must_use]
    pub fn find_type(&self, namespace: &str, name: &str) -> Option<TypeDef<'a>> {
        self.type_defs()
            .find(|type_def| type_def.name() == Some(TypeName { namespace, name }))
    }

    /// The first of `references` declaring `namespace.name`, as `(ordinal, TypeDef)`.
    ///
    /// **REFERENCE ORDER IS THE TIE-BREAK, AND THAT IS THE WHOLE RULE**: a name declared by two
    /// references resolves to the EARLIER one, exactly as the build was handed them. Ordinals are
    /// identity downstream -- a descriptor symbol, a statics region and a `TypeHandle` all encode
    /// one -- so the index returned here is the same index every later stage means.
    ///
    /// **IT IS A FREE FUNCTION ON PURPOSE.** Two callers need this rule from different crates: the
    /// AOT resolver, which turns a `TypeRef` into a qualified handle, and the generic collector,
    /// which has to decide whether an instantiation's definition is declared here or next door.
    /// Written twice it would be two rules that agree until one of them gains a case.
    #[must_use]
    pub fn find_in_references(
        references: &[&Assembly<'a>],
        namespace: &str,
        name: &str,
    ) -> Option<(usize, TypeDef<'a>)> {
        references
            .iter()
            .enumerate()
            .find_map(|(ordinal, reference)| {
                reference
                    .find_type(namespace, name)
                    .map(|type_def| (ordinal, type_def))
            })
    }

    /// This assembly's own simple name (the `Assembly` row, II.22.2), if present.
    #[must_use]
    pub fn assembly_name(&self) -> Option<&'a str> {
        let row = self.tables.row(table::ASSEMBLY, 1)?;
        self.strings().get(row.raw(7)).ok()
    }

    /// This assembly's own version `(major, minor, build, revision)` (the `Assembly` row, II.22.2).
    #[must_use]
    pub fn assembly_version(&self) -> (u16, u16, u16, u16) {
        self.tables
            .row(table::ASSEMBLY, 1)
            .map_or((0, 0, 0, 0), |row| {
                (
                    row.raw(1) as u16,
                    row.raw(2) as u16,
                    row.raw(3) as u16,
                    row.raw(4) as u16,
                )
            })
    }

    /// This assembly's full public key (the `Assembly.PublicKey` blob, II.22.2); empty if the
    /// assembly is unsigned. A reference assembly carries the FULL key here, and an `AssemblyRef`
    /// to it may hold either that key (with the `afPublicKey` flag) or its derived 8-byte token.
    #[must_use]
    pub fn assembly_public_key(&self) -> &'a [u8] {
        self.tables
            .row(table::ASSEMBLY, 1)
            .and_then(|row| self.image.blob().get(row.raw(6)).ok())
            .unwrap_or(&[])
    }

    /// This assembly's hash-algorithm id (the `Assembly.HashAlgId` column, II.22.2 -- an
    /// `AssemblyHashAlgorithm`, e.g. `0x8004` for SHA-1), or `0` (None) if absent.
    #[must_use]
    pub fn assembly_hash_algorithm(&self) -> u32 {
        self.tables
            .row(table::ASSEMBLY, 1)
            .map_or(0, |row| row.raw(0))
    }

    /// This assembly's flags (the `Assembly.Flags` column, II.22.2 / II.23.1.2), or `0` if absent.
    #[must_use]
    pub fn assembly_flags(&self) -> u32 {
        self.tables
            .row(table::ASSEMBLY, 1)
            .map_or(0, |row| row.raw(5))
    }

    /// This assembly's culture name (the `Assembly.Culture` column, II.22.2), or `None` for the
    /// neutral culture (a nil column).
    #[must_use]
    pub fn assembly_culture(&self) -> Option<&'a str> {
        let row = self.tables.row(table::ASSEMBLY, 1)?;
        match row.raw(8) {
            0 => None,
            offset => self.strings().get(offset).ok(),
        }
    }

    /// The 1-based `index`-th `TypeRef` (a referenced type, II.22.38).
    #[must_use]
    pub fn type_ref(&self, index: u32) -> Option<TypeRef<'a>> {
        if index == 0 || index > self.tables.row_count(table::TYPE_REF) {
            return None;
        }
        Some(TypeRef {
            assembly: *self,
            index,
        })
    }

    /// Iterates the referenced types.
    pub fn type_refs(&self) -> impl Iterator<Item = TypeRef<'a>> + '_ {
        (1..=self.tables.row_count(table::TYPE_REF)).filter_map(move |index| self.type_ref(index))
    }

    /// The 1-based `index`-th `AssemblyRef` (a referenced assembly, II.22.5).
    #[must_use]
    pub fn assembly_ref(&self, index: u32) -> Option<AssemblyRef<'a>> {
        if index == 0 || index > self.tables.row_count(table::ASSEMBLY_REF) {
            return None;
        }
        Some(AssemblyRef {
            assembly: *self,
            index,
        })
    }

    /// Iterates the referenced assemblies.
    pub fn assembly_refs(&self) -> impl Iterator<Item = AssemblyRef<'a>> + '_ {
        (1..=self.tables.row_count(table::ASSEMBLY_REF))
            .filter_map(move |index| self.assembly_ref(index))
    }

    /// The 1-based `index`-th `MemberRef` (a referenced method or field, II.22.25).
    #[must_use]
    pub fn member_ref(&self, index: u32) -> Option<MemberRef<'a>> {
        if index == 0 || index > self.tables.row_count(table::MEMBER_REF) {
            return None;
        }
        Some(MemberRef {
            assembly: *self,
            index,
        })
    }

    /// Iterates the referenced members.
    pub fn member_refs(&self) -> impl Iterator<Item = MemberRef<'a>> + '_ {
        (1..=self.tables.row_count(table::MEMBER_REF))
            .filter_map(move |index| self.member_ref(index))
    }

    /// The declared type parameters of a generic type or method, as
    /// `(number, flags, owner, name)` per `GenericParam` row (ECMA-335 4th ed, II.22.20).
    ///
    /// `owner` is the raw `TypeOrMethodDef` coded index, NOT a [`Token`]: it is a 1-bit tag
    /// (II.24.2.6) with `TypeDef` = 0 and `MethodDef` = 1, so a `TypeDef` row `r` reads as `r << 1`.
    /// It is returned encoded because decoding it here would silently invent a token for a row
    /// index that may not exist, and a caller comparing against a known owner does not need it
    /// decoded.
    ///
    /// **This reports the DECLARATIONS only.** It says a type has parameters named `T`; it does
    /// not make a signature mentioning `!0` decodable, which is a separate and larger piece of work.
    /// Reading this as "generics are supported" would be exactly the half-right reading the rest of
    /// this module is arranged to prevent.
    pub fn generic_params(&self) -> impl Iterator<Item = (u32, u32, u32, Option<&'a str>)> + '_ {
        (1..=self.tables.row_count(table::GENERIC_PARAM)).filter_map(move |index| {
            let row = self.tables.row(table::GENERIC_PARAM, index)?;
            Some((
                row.raw(0),
                row.raw(1),
                row.raw(2),
                self.image.strings().get(row.raw(3)).ok(),
            ))
        })
    }

    /// Every declared type parameter's CONSTRAINTS, keyed by `(owner, number)` where `owner` is the
    /// raw `TypeOrMethodDef` coded index exactly as [`Assembly::generic_params`] reports it.
    ///
    /// Each entry is `(flags, constraint_tokens)`: the `GenericParam` flag word (II.23.1.7, whose
    /// `0x001C` bits are `class`/`struct`/`new()`) and one `TypeDefOrRef` token per
    /// `GenericParamConstraint` row (II.22.21) naming a class, interface or type parameter.
    ///
    /// **BOTH HALVES OR NEITHER.** A constraint has two encodings and reading one is worse than
    /// reading none: a consumer that took only the rows would report `where T : class` as
    /// unconstrained, and one that took only the flags would miss every named constraint. Returning
    /// them together is what stops a caller choosing accidentally.
    ///
    /// **KEYED BY `(owner, number)`, NOT BY ROW.** `GenericParamConstraint.Owner` is a ROW index
    /// into `GenericParam`, and row order is not parameter order -- II.22.20's sort is a property
    /// the file asserts rather than one a reader may assume. Resolving through the row and then
    /// keying by the declared `Number` means a mis-sorted input yields the right constraints on the
    /// right parameter instead of a plausible permutation, which is the failure that would silently
    /// enforce `where T : IFoo` against `U`.
    #[must_use]
    pub fn generic_param_constraints(&self) -> BTreeMap<(u32, u32), (u32, Vec<Token>)> {
        let rows: Vec<(u32, u32, u32)> = self
            .generic_params()
            .map(|(number, flags, owner, _)| (owner, number, flags))
            .collect();
        let mut result: BTreeMap<(u32, u32), (u32, Vec<Token>)> = BTreeMap::new();
        for &(owner, number, flags) in &rows {
            result.entry((owner, number)).or_insert((flags, Vec::new()));
        }
        for index in 1..=self.tables.row_count(table::GENERIC_PARAM_CONSTRAINT) {
            let Some(row) = self.tables.row(table::GENERIC_PARAM_CONSTRAINT, index) else {
                continue;
            };
            let Some(&(owner, number, _)) = rows.get(row.raw(0).saturating_sub(1) as usize) else {
                continue;
            };
            let token = crate::CodedIndex::TypeDefOrRef.decode(row.raw(1));
            if token.table() == 0 {
                continue;
            }
            if let Some((_, constraints)) = result.get_mut(&(owner, number)) {
                constraints.push(token);
            }
        }
        result
    }

    /// Every generic METHOD's declared type-parameter names, keyed by the owning `MethodDef` row
    /// and ordered by the `Number` column -- the `!!0` half of `GenericParam`, where
    /// [`Assembly::type_parameter_names`] reads the `!0` half.
    ///
    /// **A CALLER THAT HAS ONLY THE TYPE HALF SEES EVERY IMPORTED GENERIC METHOD AS
    /// NON-GENERIC**, because the coded-index tag it filters on discards exactly these rows. That
    /// is not a missing feature that announces itself: an imported `T Id<T>(T)` read without this
    /// has an empty parameter list, so a call site naming `Id<int>(x)` is told the method is not
    /// generic at all -- a confidently wrong diagnostic rather than a gap.
    ///
    /// Same `Number` ordering discipline as the type half, for the same reason: II.22.20's sort is
    /// a property the file asserts rather than one a reader may assume.
    #[must_use]
    pub fn method_type_parameter_names(&self) -> BTreeMap<u32, Vec<&'a str>> {
        let mut by_owner: BTreeMap<u32, Vec<(u32, &'a str)>> = BTreeMap::new();
        for (number, _flags, owner, name) in self.generic_params() {
            if owner & 1 == 0 {
                continue;
            }
            if let Some(name) = name {
                by_owner.entry(owner >> 1).or_default().push((number, name));
            }
        }
        by_owner
            .into_iter()
            .map(|(owner, mut parameters)| {
                parameters.sort_by_key(|&(number, _)| number);
                (owner, parameters.into_iter().map(|(_, name)| name).collect())
            })
            .collect()
    }

    /// Every generic TYPE's declared type-parameter names, keyed by the owning `TypeDef` row and
    /// ordered by the `Number` column (II.22.20). Indexed once for the whole assembly, like
    /// [`Assembly::param_array_params`], so a caller walking every type does not rescan the table
    /// per type. Method type parameters are excluded -- the owner tag selects `TypeDef` only.
    ///
    /// **ORDERED BY `Number`, NEVER BY ROW ORDER.** II.22.20 requires the table sorted by owner
    /// then number, but that sort is a property the file ASSERTS rather than one a reader can
    /// assume, and `GenericParamConstraint` indexes `GenericParam` BY ROW -- so a future writer that
    /// re-sorts has to keep both true. Reading the number explicitly means a mis-sorted input yields
    /// the right names in the right order instead of a plausible permutation of them, which is the
    /// failure that would show up as a diagnostic naming `Box<U, T>`.
    #[must_use]
    pub fn type_parameter_names(&self) -> BTreeMap<u32, Vec<&'a str>> {
        let mut by_owner: BTreeMap<u32, Vec<(u32, &'a str)>> = BTreeMap::new();
        for (number, _flags, owner, name) in self.generic_params() {
            if owner & 1 != 0 {
                continue;
            }
            if let Some(name) = name {
                by_owner.entry(owner >> 1).or_default().push((number, name));
            }
        }
        by_owner
            .into_iter()
            .map(|(owner, mut parameters)| {
                parameters.sort_by_key(|&(number, _)| number);
                (owner, parameters.into_iter().map(|(_, name)| name).collect())
            })
            .collect()
    }

    /// The unmanaged import name of a P/Invoke method -- its `DllImport` entry point -- from the
    /// `ImplMap` table (II.22.22): `[MappingFlags(u16), MemberForwarded, ImportName, ImportScope]`.
    /// Returns the `ImportName` of the row whose `MemberForwarded` is this `MethodDef` rid, or `None`
    /// for an ordinary method. The AOT lowering turns a call to such a method into a `CallNative` to
    /// the returned symbol (the C ABI boundary the own-linker resolves).
    #[must_use]
    pub fn pinvoke_import(&self, method_rid: u32) -> Option<&'a str> {
        let forwarded = (method_rid << 1) | 1;
        (1..=self.tables.row_count(table::IMPL_MAP)).find_map(|index| {
            let row = self.tables.row(table::IMPL_MAP, index)?;
            if row.raw(1) == forwarded {
                self.strings().get(row.raw(2)).ok()
            } else {
                None
            }
        })
    }

    /// The unmanaged MODULE (DLL) name of a P/Invoke method -- its `DllImport` library --
    /// from the `ImplMap` row's `ImportScope` column, a `ModuleRef` (II.22.31) whose only
    /// column is the name. `None` for an ordinary method.
    #[must_use]
    pub fn pinvoke_module(&self, method_rid: u32) -> Option<&'a str> {
        let forwarded = (method_rid << 1) | 1;
        (1..=self.tables.row_count(table::IMPL_MAP)).find_map(|index| {
            let row = self.tables.row(table::IMPL_MAP, index)?;
            if row.raw(1) != forwarded {
                return None;
            }
            let scope = self.tables.row(table::MODULE_REF, row.raw(3))?;
            self.strings().get(scope.raw(0)).ok()
        })
    }

    /// The [`CharSet`] of a P/Invoke method, from its `ImplMap` MappingFlags (column 0, mask `0x0006`).
    /// `None` for an ordinary method (no `ImplMap` row). The string-marshaling layer reads this to pick
    /// the native encoding of a `string` argument.
    #[must_use]
    pub fn pinvoke_charset(&self, method_rid: u32) -> Option<CharSet> {
        let forwarded = (method_rid << 1) | 1;
        (1..=self.tables.row_count(table::IMPL_MAP)).find_map(|index| {
            let row = self.tables.row(table::IMPL_MAP, index)?;
            (row.raw(1) == forwarded).then(|| match row.raw(0) & 0x0006 {
                0x0002 => CharSet::Ansi,
                0x0004 => CharSet::Unicode,
                0x0006 => CharSet::Auto,
                _ => CharSet::NotSpecified,
            })
        })
    }

    /// The decoded signature of the `TypeSpec` named by `token` (II.23.2.14): the type a
    /// `TypeSpec` row stands for, e.g. `SigType::Array { element, rank }` for `int[,]`.
    /// `None` when `token` is not a `TypeSpec` or its blob does not decode. This is the
    /// seam an AOT lowering uses to recover an array's element type and rank from the
    /// `TypeSpec` that a `newobj`/`Get`/`Set` `MemberRef` points at.
    #[must_use]
    pub fn type_spec_signature(&self, token: Token) -> Option<SigType> {
        if token.table() != table::TYPE_SPEC {
            return None;
        }
        let row = self.tables.row(table::TYPE_SPEC, token.row())?;
        let blob = self.image.blob().get(row.raw(0)).ok()?;
        parse_type(blob).ok()
    }

    /// The `MethodDef` at 1-based `index`, or `None` if out of range.
    #[must_use]
    pub fn method(&self, index: u32) -> Option<Method<'a>> {
        if index == 0 || index > self.tables.row_count(table::METHOD_DEF) {
            return None;
        }
        Some(Method {
            assembly: *self,
            index,
        })
    }

    /// The underlying method a `MethodSpec` token (II.22.29) instantiates: its
    /// `MethodDefOrRef` (a `MethodDef` or `MemberRef`), so a generic-method call (e.g.
    /// `Array.Empty<int>()`) can be resolved through the generic definition. `None` if the
    /// token is not a `MethodSpec` or is out of range.
    #[must_use]
    pub fn method_spec_method(&self, token: Token) -> Option<Token> {
        if token.table() != table::METHOD_SPEC {
            return None;
        }
        self.tables
            .row(table::METHOD_SPEC, token.row())
            .map(|row| row.token(0))
    }

    /// The type ARGUMENTS a `MethodSpec` token supplies at its call site (II.22.29's `Instantiation`
    /// blob, II.23.2.15) -- the `int` of `Identity<int>(..)`.
    ///
    /// The companion to [`method_spec_method`](Self::method_spec_method), which answers WHICH generic
    /// method a site instantiates; this answers WITH WHAT. A consumer needs both to substitute, and
    /// having only the first is how a call binds to the open definition and leaves `!!0` unresolved.
    ///
    /// `None` if the token is not a `MethodSpec`, is out of range, or its blob does not decode --
    /// a malformed instantiation is refused rather than reported as zero arguments, which would read
    /// as a call to a non-generic method.
    #[must_use]
    pub fn method_spec_instantiation(&self, token: Token) -> Option<Vec<SigType>> {
        if token.table() != table::METHOD_SPEC {
            return None;
        }
        let row = self.tables.row(table::METHOD_SPEC, token.row())?;
        let blob = self.image.blob().get(row.raw(1)).ok()?;
        crate::signature::parse_method_spec(blob).ok()
    }

    /// The raw initializer bytes a `Field` token's RVA points at (II.22.18 `FieldRVA`) --
    /// the data backing a `RuntimeHelpers.InitializeArray` (a constant array literal). The
    /// slice is sized exactly to the field's value-type byte size (its `ClassLayout`
    /// `ClassSize`, the compiler-synthesized `__StaticArrayInitTypeSize=N`), so the caller
    /// reads a precise blob. `None` if the field has no `FieldRVA` row, no resolvable size,
    /// or the RVA is unmapped (e.g. a bare metadata image with no backing PE).
    #[must_use]
    pub fn field_rva_data(&self, field_token: Token) -> Option<&'a [u8]> {
        if field_token.table() != table::FIELD {
            return None;
        }
        let field_index = field_token.row();
        let rva = (1..=self.tables.row_count(table::FIELD_RVA))
            .filter_map(|index| self.tables.row(table::FIELD_RVA, index))
            .find(|row| row.raw(1) == field_index)
            .map(|row| row.raw(0))
            .filter(|&rva| rva != 0)?;
        let size = self.field_rva_blob_size(field_index)? as usize;
        let file = self.file?;
        PeImage::parse(file).ok()?.slice_at_rva(rva, size).ok()
    }

    /// The byte size of an RVA-backed field's initializer blob. Two shapes occur, both emitted
    /// by csc for a constant array literal `T[] a = {...}`:
    ///   * a synthesized value type (`__StaticArrayInitTypeSize=N`) -- the size is its
    ///     `ClassLayout` `ClassSize` (any blob length);
    ///   * a PRIMITIVE field reused as the holder when the blob is exactly 1/2/4/8 bytes (csc's
    ///     size optimization types it `int8`/`int16`/`int32`/`int64` rather than minting a struct)
    ///     -- the size is the primitive's width. Without this case a power-of-two-sized literal
    ///     (e.g. `byte[]{1,2,3,4}` in an `int32` field, or `int[]{42}`) had no resolvable size, so
    ///     `field_rva_data` returned `None` and `InitializeArray` silently left the array zeroed.
    /// `None` if the field's type is neither (or its value type has no `ClassLayout`).
    fn field_rva_blob_size(&self, field_index: u32) -> Option<u32> {
        let blob = self
            .tables
            .row(table::FIELD, field_index)
            .and_then(|row| self.image.blob().get(row.raw(2)).ok())?;
        match parse_field(blob).ok()? {
            SigType::ValueType(type_token) => {
                if type_token.table() != table::TYPE_DEF {
                    return None;
                }
                (1..=self.tables.row_count(table::CLASS_LAYOUT))
                    .filter_map(|index| self.tables.row(table::CLASS_LAYOUT, index))
                    .find(|row| row.raw(2) == type_token.row())
                    .map(|row| row.raw(1))
            }
            primitive => primitive_field_size(&primitive),
        }
    }

    /// Resolves a `call`/`callvirt` method token to its target: the name, declaring
    /// type, and signature, plus whether it is defined in this assembly (a
    /// `MethodDef`) or referenced from another (a `MemberRef`). This is what a
    /// consumer of decoded CIL -- an interpreter or an AOT lowering -- needs to bind
    /// a call to its callee. Returns `None` for a token that is neither.
    #[must_use]
    pub fn resolve_method(&self, token: Token) -> Option<ResolvedMethod<'a>> {
        match token.table() {
            table::METHOD_DEF => {
                let method = self.method(token.row())?;
                Some(ResolvedMethod {
                    name: method.name(),
                    declaring_type: self
                        .method_owner(token.row())
                        .and_then(|owner| owner.name()),
                    signature: method.signature(),
                    kind: MethodKind::Definition(token.row()),
                })
            }
            table::MEMBER_REF => {
                let member = self.member_ref(token.row())?;
                Some(ResolvedMethod {
                    name: member.name(),
                    declaring_type: self.type_token_name(member.parent()),
                    signature: member.method_signature(),
                    kind: MethodKind::Reference,
                })
            }
            _ => None,
        }
    }

    /// The layout of a value type (a struct or enum) named by a `TypeDef` token: its
    /// instance fields, in declaration order, placed per `target`, with the
    /// reference-offset map. Recurses into nested value-type fields. This is the one
    /// shared layout the backend (stack slots + GC stack maps) and the runtime (the
    /// flat heap + boxing) consume, so neither re-derives it. A field whose type is a
    /// value type in another assembly (a `TypeRef`) cannot be resolved here and is a
    /// `LayoutError::UnresolvedValueType`.
    pub fn value_type_layout(
        &self,
        token: Token,
        target: &TargetLayout,
    ) -> Result<TypeLayout, LayoutError> {
        if token.table() != table::TYPE_DEF {
            return Err(LayoutError::UnresolvedValueType(token));
        }
        let fields: Vec<SigType> = match self.type_def(token.row()) {
            Some(type_def) => type_def
                .fields()
                .filter(|field| !field.is_static())
                .filter_map(|field| field.signature())
                .collect(),
            None => return Err(LayoutError::UnresolvedValueType(token)),
        };
        layout_value_type(&fields, target, &|nested| {
            self.value_type_layout(nested, target).ok()
        })
    }

    /// The byte offset of an instance field within its declaring type, by the field's
    /// token -- the seam from a `Field` token to a layout offset. Finds the type that
    /// declares the field, lays it out, and returns the offset of that field's slot
    /// (in declaration order among the instance fields). `None` if the token names no
    /// instance field of a layable type. The offset is from the type's field block;
    /// a reference type's object header, if any, is the runtime's to add.
    #[must_use]
    pub fn field_offset(&self, field: Token, target: &TargetLayout) -> Option<u32> {
        if field.table() != table::FIELD {
            return None;
        }
        for type_def in self.type_defs() {
            let mut index = 0usize;
            for candidate in type_def.fields() {
                if candidate.is_static() {
                    continue;
                }
                if candidate.token() == field {
                    let layout = self.value_type_layout(type_def.token(), target).ok()?;
                    return layout.field_offsets.get(index).copied();
                }
                index += 1;
            }
        }
        None
    }

    /// The signature type of a field, by its `Field` token -- the seam from an `ldfld`/`stfld`
    /// operand to the field's type, so the lowering can type the loaded value (a reference field
    /// reads an `ObjectRef`, not an `int`). `None` if the token names no field.
    #[must_use]
    pub fn field_signature(&self, field: Token) -> Option<SigType> {
        if field.table() != table::FIELD {
            return None;
        }
        Field {
            assembly: *self,
            index: field.row(),
        }
        .signature()
    }

    /// The `TypeDef` that owns the method at `method_index`, found by the method
    /// ranges the `TypeDef.MethodList` column delimits.
    fn method_owner(&self, method_index: u32) -> Option<TypeDef<'a>> {
        (1..=self.tables.row_count(table::TYPE_DEF)).find_map(|type_index| {
            let (first, last) = self.child_range(table::TYPE_DEF, type_index, 5, table::METHOD_DEF);
            (first..last).contains(&method_index).then_some(TypeDef {
                assembly: *self,
                index: type_index,
            })
        })
    }

    /// The namespace and name of a type token -- a `TypeRef` or `TypeDef` (the two
    /// `TypeDefOrRef` cases that name a concrete type, e.g. a `MemberRef` parent, a base
    /// type, or a `newarr` element); `None` for any other table (such as a `TypeSpec`,
    /// which carries a signature rather than a name). Also normalizes a signature's
    /// `Class`/`ValueType` token to a portable name for cross-assembly matching.
    #[must_use]
    pub fn type_token_name(&self, token: Token) -> Option<TypeName<'a>> {
        match token.table() {
            table::TYPE_REF => self
                .type_ref(token.row())
                .and_then(|type_ref| type_ref.name()),
            table::TYPE_DEF => self
                .type_def(token.row())
                .and_then(|type_def| type_def.name()),
            _ => None,
        }
    }

    /// The `(namespace, name)` a type token is IDENTIFIED by, with a NESTED type's enclosing chain
    /// standing in for its namespace: `` Widget.Nested `` answers `("Lamella.Checks.Widget",
    /// "Nested")`. `None` for a token that names no type.
    ///
    /// **[`Assembly::type_token_name`] IS NOT AN IDENTITY, AND THE DIFFERENCE IS INVISIBLE UNTIL A
    /// NESTED TYPE ARRIVES.** A nested type's namespace is EMPTY on both sides of the metadata --
    /// a `TypeDef` puts its enclosing type in `NestedClass` (II.22.32) and a `TypeRef` puts it in
    /// its `ResolutionScope` (II.22.38) -- so reading the row directly answers a bare `Nested` for
    /// every nested type in an assembly, and `Widget.Nested` and `Gadget.Nested` become one key.
    /// A consumer that indexes on it either misses the type or, worse, binds it to the other one.
    ///
    /// **THE TWO TABLES MUST ANSWER ALIKE OR A CROSS-ASSEMBLY REFERENCE CANNOT FIND ITS
    /// DEFINITION**, which is the whole reason this is one function rather than a walk at each
    /// consumer: the DEFINING assembly reaches the type as a `TypeDef` and the referencing one as
    /// a `TypeRef`, and they meet only if both spell it the same way.
    ///
    /// Only the OUTERMOST type carries a namespace, so reaching one ends the walk. The walk is
    /// bounded: a malformed cyclic `NestedClass` or `ResolutionScope` cannot spin here.
    #[must_use]
    pub fn type_token_full_name(&self, token: Token) -> Option<(String, String)> {
        let own = self.type_token_name(token)?;
        let mut prefix = String::new();
        let mut current = self.enclosing_token(token);
        for _ in 0..16 {
            let Some(outer) = current else { break };
            let outer_name = self.type_token_name(outer)?;
            let mut next = String::from(outer_name.name);
            if !prefix.is_empty() {
                next.push('.');
                next.push_str(&prefix);
            }
            prefix = next;
            if !outer_name.namespace.is_empty() {
                prefix = alloc::format!("{}.{}", outer_name.namespace, prefix);
                break;
            }
            current = self.enclosing_token(outer);
        }
        let namespace = if prefix.is_empty() {
            String::from(own.namespace)
        } else {
            prefix
        };
        Some((namespace, String::from(own.name)))
    }

    /// The token of the type `token` is nested in, or `None` when it is top level. The enclosing
    /// type lives in a different place for each table -- `NestedClass` for a `TypeDef` (II.22.32),
    /// the `ResolutionScope` for a `TypeRef` (II.22.38) -- and a `ResolutionScope` that is NOT a
    /// `TypeRef` is an assembly or module, which means top level.
    fn enclosing_token(&self, token: Token) -> Option<Token> {
        match token.table() {
            table::TYPE_DEF => self
                .type_def(token.row())
                .and_then(|type_def| type_def.enclosing_type())
                .map(|outer| outer.token()),
            table::TYPE_REF => {
                let scope = self.type_ref(token.row())?.resolution_scope();
                (scope.table() == table::TYPE_REF).then_some(scope)
            }
            _ => None,
        }
    }

    /// The exception TAG for a type, for the no-GC tag-dispatch exception model: one `u32`
    /// identifying the exception type identically wherever it is named -- the throw site,
    /// every catch, and the runtime -- so the compiler, interpreter, and AOT backend never
    /// diverge. It is a deterministic FNV-1a hash of the type's full name with the high bit
    /// set, so it needs no shared table and is always a nonzero "failure" value (0 = no
    /// exception in flight). `0` if the token names no type.
    ///
    /// A per-name hash (not .NET's canonical HResults) is used because a tag-DISPATCH model
    /// needs tags UNIQUE per type, and the canonical HResults collide (e.g.
    /// ArgumentNullException and NullReferenceException are both 0x80004003). A future
    /// `[HResult(N)]` attribute will pin a chosen value; a well-known-BCL-HResult table is a
    /// later refinement layered on top.
    #[must_use]
    pub fn exception_tag(&self, type_token: Token) -> u32 {
        let Some(name) = self.type_token_name(type_token) else {
            return 0;
        };
        exception_tag_for_name(name.namespace, name.name)
    }

    /// The AOT exception base-chain tag vector for `type_token` -- `[tag(T), tag(base(T)),
    /// ..., tag(System.Exception)]`, leaf first -- read from the `<ExceptionBaseChain>`
    /// custom attribute the compiler emits on a referenced (BCL) exception type. The tags
    /// use the same formula as [`Assembly::exception_tag`].
    ///
    /// `None` for a type with no such attribute -- an in-program exception, whose chain the
    /// AOT walks itself via `extends()`. The middle-base `catch (BaseType)` subtype test is
    /// `catch_tag`'s membership in this vector (exact-match and catch-all need only the tag).
    #[must_use]
    pub fn exception_base_chain(&self, type_token: Token) -> Option<Vec<u32>> {
        for attribute in self.custom_attributes(type_token) {
            let is_chain = self
                .resolve_method(attribute.constructor)
                .and_then(|method| method.declaring_type)
                .is_some_and(|name| {
                    name.namespace.is_empty() && name.name == "<ExceptionBaseChain>"
                });
            if is_chain {
                return decode_exception_base_chain(attribute.value);
            }
        }
        None
    }

    /// The custom attributes applied to `parent`, from the `CustomAttribute`
    /// table (II.22.10): each yields the constructor token and the raw value blob.
    /// Every CustomAttribute row's constructor token (II.22.10, the `Type` column), in one table
    /// pass. A ctor is referenced ONLY from this table, never a method body, so the loader feeds
    /// these through its call-binding to make an attribute's ctor resolvable (a `const decimal`'s
    /// DecimalConstantAttribute, a `[Field]`-target attribute) even though nothing constructs it.
    pub fn custom_attribute_ctors(&self) -> impl Iterator<Item = Token> + '_ {
        (1..=self.tables.row_count(table::CUSTOM_ATTRIBUTE))
            .filter_map(move |index| self.tables.row(table::CUSTOM_ATTRIBUTE, index).map(|row| row.token(1)))
    }

    /// Whether `parent` carries the custom attribute named `namespace`.`name`.
    ///
    /// Attribute presence is asked in several crates that share no dependency but this one -- the
    /// interpreter's loader binds `[RuntimeProvided]` seams to intrinsics, and the AOT build asks the
    /// same question of the same metadata. Each grew its own copy of this walk, which is fine until
    /// the two answer differently. One implementation, so they cannot.
    ///
    /// Matches on the declaring type of the attribute's CONSTRUCTOR, which is how an attribute is
    /// identified in metadata: a `CustomAttribute` row names its ctor, not its type.
    ///
    /// **THIS IS O(CustomAttribute ROWS) AND MUST NOT BE CALLED PER MEMBER.** `custom_attributes`
    /// scans the whole table to find one parent's rows, so asking it about every method of a
    /// reference assembly is rows x members -- and on a real reference set that dominates the
    /// whole cost of loading it, by an order of magnitude over everything else put together.
    ///
    /// Use [`Assembly::tokens_with_attribute`] to index the whole assembly in ONE pass and then ask
    /// the set. This entry point is for a caller with a SINGLE token in hand.
    #[must_use]
    pub fn has_attribute(&self, parent: Token, namespace: &str, name: &str) -> bool {
        self.custom_attributes(parent).any(|attribute| {
            self.resolve_method(attribute.constructor)
                .and_then(|ctor| ctor.declaring_type)
                .is_some_and(|declaring| declaring.namespace == namespace && declaring.name == name)
        })
    }

    /// Every token carrying `[namespace.name]`, computed in ONE pass over the CustomAttribute table
    /// (II.22.10) -- the set form of [`Assembly::has_attribute`], for a caller that would otherwise
    /// ask it once per member.
    ///
    /// Same shape and the same reason as [`Assembly::param_array_params`] and
    /// [`Assembly::conditional_symbols`], which index one attribute each. Those two already carried
    /// the warning ("*catastrophic across a large reference assembly*") when three later attribute
    /// reads were written as per-member calls anyway, so this is the general form rather than a
    /// fourth special case: the attribute is a parameter, and the next one needs no new function.
    ///
    /// Keyed by TOKEN, not by row, because the parents span tables -- a `Field`, a `Property` and a
    /// `MethodDef` row all number from 1 and a row-keyed set would answer for whichever shares the
    /// number.
    #[must_use]
    pub fn tokens_with_attribute(&self, namespace: &str, name: &str) -> BTreeSet<Token> {
        let mut found = BTreeSet::new();
        for index in 1..=self.tables.row_count(table::CUSTOM_ATTRIBUTE) {
            let Some(row) = self.tables.row(table::CUSTOM_ATTRIBUTE, index) else {
                continue;
            };
            let matches = self
                .resolve_method(row.token(1))
                .and_then(|ctor| ctor.declaring_type)
                .is_some_and(|declaring| declaring.namespace == namespace && declaring.name == name);
            if matches {
                found.insert(row.token(0));
            }
        }
        found
    }

    /// The single `string` argument of every assembly-level custom attribute of type
    /// `namespace`.`name`, in table order: the shape of every declaration a build makes ABOUT the
    /// assembly it is producing (a friend, a required capability, a compilation symbol).
    ///
    /// Attributes on the Assembly manifest row (II.22.2's row 1, II.22.10's parent), so it never
    /// sees an attribute on a type or a member. An `AllowMultiple` attribute yields one entry per
    /// application and a single-application one yields at most one; a row whose blob does not decode
    /// as one string is skipped rather than reported, because a foreign attribute of a shape this
    /// reader does not know is not an error.
    ///
    /// The constructor signature is taken to be one `string` rather than resolved, because the ctor
    /// of an attribute declared in ANOTHER assembly arrives as a `MemberRef` whose signature this
    /// reader would have to decode to learn what the caller has already told it.
    #[must_use]
    pub fn assembly_attribute_strings(&self, namespace: &str, name: &str) -> Vec<&'a str> {
        let assembly_row = Token::new(table::ASSEMBLY, 1);
        let mut values = Vec::new();
        for attribute in self.custom_attributes(assembly_row) {
            let declaring = self
                .resolve_method(attribute.constructor)
                .and_then(|ctor| ctor.declaring_type);
            let matches = declaring
                .is_some_and(|declaring| declaring.namespace == namespace && declaring.name == name);
            if !matches {
                continue;
            }
            let decoded =
                decode_custom_attribute(attribute.value, &[SigType::String], &|_| element::I4);
            if let Some(decoded) = decoded
                && let Some(AttrArg::Str(value)) = decoded.fixed.first()
            {
                values.push(*value);
            }
        }
        values
    }

    /// The simple names of the assemblies this one declares as FRIENDS: the string argument of
    /// every `[assembly: System.Runtime.CompilerServices.InternalsVisibleTo("...")]` row on the
    /// Assembly manifest (II.22.2's row 1, II.22.10's parent).
    ///
    /// THE ARGUMENT IS AN ASSEMBLY DISPLAY NAME, NOT A SIMPLE NAME. A strong-named friend is
    /// spelled `"Friend, PublicKey=0024..."`, so everything from the first comma is dropped and the
    /// remainder trimmed -- a reader that compared the whole string would match no strong-named
    /// reference assembly at all, which is the majority of the .NET reference pack.
    #[must_use]
    pub fn friend_assemblies(&self) -> Vec<&'a str> {
        self.assembly_attribute_strings(
            "System.Runtime.CompilerServices",
            "InternalsVisibleToAttribute",
        )
        .into_iter()
        .map(|display_name| display_name.split(',').next().unwrap_or(display_name).trim())
        .collect()
    }

    /// Whether `method_token` is a `[Lamella.Runtime.RuntimeProvided]` seam: a method whose body the
    /// VES supplies, carrying only a placeholder in source.
    #[must_use]
    pub fn is_runtime_provided(&self, method_token: Token) -> bool {
        self.has_attribute(method_token, "Lamella.Runtime", "RuntimeProvidedAttribute")
    }

    /// Whether `method_token` declares that its COMPILED-OUT DEFAULT IS THE INTENDED ANSWER
    /// (`[Lamella.Runtime.IntendedDefault]`) -- i.e. a build that does not synthesize this seam is
    /// still correct.
    ///
    /// This is the distinction that lets a toolchain ERROR on an unsynthesized seam whose default is
    /// a silent wrong answer, while leaving alone the seams whose absence is the truth. An UNMARKED
    /// `[RuntimeProvided]` method is therefore not "assumed fine" -- it is undeclared, and a
    /// consumer enforcing this is entitled to refuse it. See the attribute's own source for the
    /// triage rule, including why intendedness that holds on only one tier does not count.
    #[must_use]
    pub fn is_intended_default(&self, method_token: Token) -> bool {
        self.has_attribute(method_token, "Lamella.Runtime", "IntendedDefaultAttribute")
    }

    /// Every `CustomAttribute` row (II.22.10) whose parent is `parent`, in table order, each
    /// carrying its constructor token and its raw value blob (II.23.3).
    ///
    /// **THIS SCANS THE WHOLE TABLE FOR ONE PARENT, so a call per member is rows x members.** It is
    /// the right entry point for a caller holding a SINGLE token; a caller asking about every member
    /// of an assembly wants [`Assembly::tokens_with_attribute`], which indexes the table in one pass.
    /// The same warning on [`Assembly::has_attribute`] records what the difference measured.
    #[must_use]
    /// Every custom attribute in the assembly, paired with the token it is attached to.
    ///
    /// [`custom_attributes`](Self::custom_attributes) filters this table for ONE parent, so asking it
    /// per member walks the whole table per member. A caller that wants every attachment -- a scan,
    /// a census -- should walk it once through here instead: the cost stops being the product of two
    /// table sizes, and every parent kind is covered without enumerating which kinds to ask about.
    pub fn all_custom_attributes(&self) -> impl Iterator<Item = (Token, CustomAttribute<'a>)> + '_ {
        let blob = self.image.blob();
        (1..=self.tables.row_count(table::CUSTOM_ATTRIBUTE)).filter_map(move |index| {
            let row = self.tables.row(table::CUSTOM_ATTRIBUTE, index)?;
            Some((
                row.token(0),
                CustomAttribute {
                    constructor: row.token(1),
                    value: blob.get(row.raw(2)).unwrap_or(&[]),
                },
            ))
        })
    }

    pub fn custom_attributes(
        &self,
        parent: Token,
    ) -> impl Iterator<Item = CustomAttribute<'a>> + '_ {
        let blob = self.image.blob();
        (1..=self.tables.row_count(table::CUSTOM_ATTRIBUTE)).filter_map(move |index| {
            let row = self.tables.row(table::CUSTOM_ATTRIBUTE, index)?;
            let owner = row.token(0);
            if owner.table() == parent.table() && owner.row() == parent.row() {
                Some(CustomAttribute {
                    constructor: row.token(1),
                    value: blob.get(row.raw(2)).unwrap_or(&[]),
                })
            } else {
                None
            }
        })
    }

    /// The `Param` row indices that carry `[System.ParamArrayAttribute]` -- a C# `params`
    /// array. Computed in ONE pass over the CustomAttribute table (II.22.10), so a caller
    /// can mark every `params` method at load without an O(rows)-per-parameter scan (which
    /// is catastrophic across a large reference assembly). `resolve_method` returns `None`
    /// for an unresolvable attribute ctor, so a foreign attribute is skipped.
    #[must_use]
    pub fn param_array_params(&self) -> BTreeSet<u32> {
        let mut params = BTreeSet::new();
        for index in 1..=self.tables.row_count(table::CUSTOM_ATTRIBUTE) {
            let Some(row) = self.tables.row(table::CUSTOM_ATTRIBUTE, index) else {
                continue;
            };
            let parent = row.token(0);
            if parent.table() != table::PARAM {
                continue;
            }
            let is_param_array = self
                .resolve_method(row.token(1))
                .and_then(|target| target.declaring_type)
                .is_some_and(|name| {
                    name.namespace == "System" && name.name == "ParamArrayAttribute"
                });
            if is_param_array {
                params.insert(parent.row());
            }
        }
        params
    }

    /// Methods marked `[System.Diagnostics.Conditional("SYMBOL")]` (24.4.2), as a map from the
    /// `MethodDef` row to its symbols. A call to such a method is omitted unless one of the
    /// symbols is defined at the call site. One pass over `CustomAttribute` (like
    /// [`Assembly::param_array_params`]) to avoid an O(rows)-per-method scan of the BCL.
    #[must_use]
    pub fn conditional_symbols(&self) -> BTreeMap<u32, Vec<Box<str>>> {
        let mut map: BTreeMap<u32, Vec<Box<str>>> = BTreeMap::new();
        for index in 1..=self.tables.row_count(table::CUSTOM_ATTRIBUTE) {
            let Some(row) = self.tables.row(table::CUSTOM_ATTRIBUTE, index) else {
                continue;
            };
            let parent = row.token(0);
            if parent.table() != table::METHOD_DEF {
                continue;
            }
            let is_conditional = self
                .resolve_method(row.token(1))
                .and_then(|target| target.declaring_type)
                .is_some_and(|name| {
                    name.namespace == "System.Diagnostics" && name.name == "ConditionalAttribute"
                });
            if !is_conditional {
                continue;
            }
            let blob = self.image.blob().get(row.raw(2)).unwrap_or(&[]);
            if let Some(symbol) = conditional_attribute_symbol(blob) {
                map.entry(parent.row()).or_default().push(symbol);
            }
        }
        map
    }

    /// The methods marked `[UnmanagedCallersOnly]` (II.22.10 scan, matched by the attribute type name
    /// regardless of namespace, so a clean-room Lamella attribute of that name works too), each mapped to
    /// its EXPORT-NAME override: the attribute's `EntryPoint` named argument if present, else `None` (the
    /// AOT then uses the method's own name). Naming a method this way makes it a global symbol the linker
    /// resolves a `CallNative` against -- so a managed method can BACK a native seam: a C# `lamella_gc_alloc`
    /// the AOT's own `new` then calls. One pass over CustomAttribute (like
    /// [`Assembly::conditional_symbols`]) to avoid an O(rows)-per-method scan of the BCL.
    #[must_use]
    pub fn unmanaged_callers_only(&self) -> BTreeMap<u32, Option<Box<str>>> {
        let mut exports = BTreeMap::new();
        for index in 1..=self.tables.row_count(table::CUSTOM_ATTRIBUTE) {
            let Some(row) = self.tables.row(table::CUSTOM_ATTRIBUTE, index) else {
                continue;
            };
            let parent = row.token(0);
            if parent.table() != table::METHOD_DEF {
                continue;
            }
            let is_unmanaged_callers_only = self
                .resolve_method(row.token(1))
                .and_then(|target| target.declaring_type)
                .is_some_and(|name| name.name == "UnmanagedCallersOnlyAttribute");
            if !is_unmanaged_callers_only {
                continue;
            }
            let blob = self.image.blob().get(row.raw(2)).unwrap_or(&[]);
            let entry_point =
                decode_custom_attribute(blob, &[], &|_| element::I4).and_then(|decoded| {
                    decoded
                        .named
                        .into_iter()
                        .find_map(|named| match named.value {
                            AttrArg::Str(s) if named.name == "EntryPoint" => {
                                Some(Box::<str>::from(s))
                            }
                            _ => None,
                        })
                });
            exports.insert(parent.row(), entry_point);
        }
        exports
    }

    /// The constant value attached to `parent` (a Field/Param/Property token),
    /// from the `Constant` table (II.22.9), or `None` if it has no constant.
    #[must_use]
    pub fn constant(&self, parent: Token) -> Option<ConstantValue> {
        for index in 1..=self.tables.row_count(table::CONSTANT) {
            let row = self.tables.row(table::CONSTANT, index)?;
            let owner = row.token(1);
            if owner.table() == parent.table() && owner.row() == parent.row() {
                let element_type = row.raw(0) as u8;
                let blob = self.image.blob().get(row.raw(2)).ok()?;
                return decode_constant(element_type, blob);
            }
        }
        None
    }

    /// The `[first, last)` child run for a type located through a map table
    /// (`PropertyMap`/`EventMap`, II.22.35/12): find the map row for the type,
    /// then take its list column to the next map row's (or the child table end).
    fn mapped_range(&self, map: u8, type_index: u32, child: u8) -> (u32, u32) {
        let map_rows = self.tables.row_count(map);
        for map_index in 1..=map_rows {
            let Some(row) = self.tables.row(map, map_index) else {
                break;
            };
            if row.raw(0) == type_index {
                let first = row.raw(1);
                let last = if map_index < map_rows {
                    self.tables
                        .row(map, map_index + 1)
                        .map_or(first, |next| next.raw(1))
                } else {
                    self.tables.row_count(child) + 1
                };
                return (first, last);
            }
        }
        (1, 1)
    }

    /// The half-open `[first, last)` range of child rows owned by an owner row,
    /// from the owner's list column and the next owner's (II.22.11 run pattern).
    fn child_range(&self, owner: u8, owner_index: u32, list_col: usize, child: u8) -> (u32, u32) {
        let first = self
            .tables
            .row(owner, owner_index)
            .map_or(1, |row| row.raw(list_col));
        let last = if owner_index < self.tables.row_count(owner) {
            self.tables
                .row(owner, owner_index + 1)
                .map_or(first, |row| row.raw(list_col))
        } else {
            self.tables.row_count(child) + 1
        };
        (first, last)
    }
}

/// The single string argument of a `[Conditional("X")]` value blob (II.23.3): the `0x0001`
/// prolog, then a `SerString` (a compressed length, then UTF-8 bytes). `None` if malformed.
fn conditional_attribute_symbol(blob: &[u8]) -> Option<Box<str>> {
    let rest = blob.get(2..)?;
    let (length, consumed) = read_compressed_u32(rest).ok()?;
    let bytes = rest.get(consumed..consumed + length as usize)?;
    core::str::from_utf8(bytes).ok().map(Box::from)
}

/// A navigable type definition (II.22.37).
#[derive(Debug, Clone, Copy)]
pub struct TypeDef<'a> {
    assembly: Assembly<'a>,
    index: u32,
}

impl<'a> TypeDef<'a> {
    /// The type's namespace and name.
    #[must_use]
    pub fn name(&self) -> Option<TypeName<'a>> {
        self.assembly.type_name(self.index)
    }

    /// This type's `TypeDef` token (e.g. to pass to [`Assembly::value_type_layout`]).
    #[must_use]
    pub fn token(&self) -> Token {
        Token::new(table::TYPE_DEF, self.index)
    }

    /// The type attribute flags (II.23.1.15).
    #[must_use]
    pub fn flags(&self) -> u32 {
        self.assembly
            .tables
            .row(table::TYPE_DEF, self.index)
            .map_or(0, |row| row.raw(0))
    }

    /// The base-class/interface token from the `Extends` column, nil for none.
    #[must_use]
    pub fn extends(&self) -> Token {
        self.assembly
            .tables
            .row(table::TYPE_DEF, self.index)
            .map_or(Token::new(0, 0), |row| row.token(3))
    }

    /// Whether the type is public.
    #[must_use]
    pub fn is_public(&self) -> bool {
        flags::type_is_public(self.flags())
    }

    /// Whether the type is nested in another type (II.23.1.15): it has no namespace of its
    /// own and is named through its enclosing type, never by a simple name.
    #[must_use]
    pub fn is_nested(&self) -> bool {
        flags::type_is_nested(self.flags())
    }

    /// Whether the type is an interface.
    #[must_use]
    pub fn is_interface(&self) -> bool {
        flags::type_is_interface(self.flags())
    }

    /// Whether this type's initializer has RELAXED timing (`beforefieldinit`) -- see
    /// [`flags::type_is_before_field_init`](crate::flags::type_is_before_field_init) for the two
    /// rules and why the difference is not observable without asking.
    #[must_use]
    pub fn is_before_field_init(&self) -> bool {
        flags::type_is_before_field_init(self.flags())
    }

    /// Whether the type is abstract.
    #[must_use]
    pub fn is_abstract(&self) -> bool {
        flags::type_is_abstract(self.flags())
    }

    /// Whether the type is sealed.
    #[must_use]
    pub fn is_sealed(&self) -> bool {
        flags::type_is_sealed(self.flags())
    }

    /// Whether the type is a value type -- it extends `System.ValueType` or `System.Enum`.
    /// Reference types (classes) extend `System.Object` or another class, so this is how the
    /// backend tells a `newobj` of a struct (built in place) from one of a class (heap-allocated).
    #[must_use]
    pub fn is_value_type(&self) -> bool {
        self.assembly
            .type_token_name(self.extends())
            .is_some_and(|base| {
                base.namespace == "System" && matches!(base.name, "ValueType" | "Enum")
            })
    }

    /// The type's fields.
    pub fn fields(&self) -> impl Iterator<Item = Field<'a>> + '_ {
        let (first, last) = self
            .assembly
            .child_range(table::TYPE_DEF, self.index, 4, table::FIELD);
        (first..last).map(move |index| Field {
            assembly: self.assembly,
            index,
        })
    }

    /// The type's methods.
    pub fn methods(&self) -> impl Iterator<Item = Method<'a>> + '_ {
        let (first, last) =
            self.assembly
                .child_range(table::TYPE_DEF, self.index, 5, table::METHOD_DEF);
        (first..last).map(move |index| Method {
            assembly: self.assembly,
            index,
        })
    }

    /// The interfaces this type implements, as `TypeDefOrRef` tokens (II.22.23).
    /// `InterfaceImpl` is a side table keyed by the implementing type, so this
    /// scans it for rows whose `Class` is this type.
    pub fn interfaces(&self) -> impl Iterator<Item = Token> + '_ {
        let index = self.index;
        (1..=self.assembly.tables.row_count(table::INTERFACE_IMPL)).filter_map(move |row_index| {
            let row = self.assembly.tables.row(table::INTERFACE_IMPL, row_index)?;
            (row.raw(0) == index).then(|| row.token(1))
        })
    }

    /// This type's explicit method overrides (II.22.27 `MethodImpl`), each as a
    /// `(body, declaration)` pair of `MethodDefOrRef` tokens: `body` is the method
    /// in this type that implements `declaration` (the overridden virtual/interface
    /// method). `MethodImpl` is a side table keyed by the implementing type's `Class`
    /// column, so this scans it for rows whose `Class` is this type -- the wiring an
    /// explicit interface member implementation (`int IA.Value()`) needs, since it is
    /// reachable only through the interface slot, never by its own (mangled) name.
    pub fn method_impls(&self) -> impl Iterator<Item = (Token, Token)> + '_ {
        let index = self.index;
        (1..=self.assembly.tables.row_count(table::METHOD_IMPL)).filter_map(move |row_index| {
            let row = self.assembly.tables.row(table::METHOD_IMPL, row_index)?;
            (row.raw(0) == index).then(|| (row.token(1), row.token(2)))
        })
    }

    /// The custom attributes applied to this type.
    pub fn custom_attributes(&self) -> impl Iterator<Item = CustomAttribute<'a>> + '_ {
        self.assembly
            .custom_attributes(Token::new(table::TYPE_DEF, self.index))
    }

    /// The type's properties (II.22.34, located through `PropertyMap`).
    pub fn properties(&self) -> impl Iterator<Item = Property<'a>> + '_ {
        let (first, last) =
            self.assembly
                .mapped_range(table::PROPERTY_MAP, self.index, table::PROPERTY);
        (first..last).map(move |index| Property {
            assembly: self.assembly,
            index,
        })
    }

    /// The type's events (II.22.13, located through `EventMap`).
    pub fn events(&self) -> impl Iterator<Item = Event<'a>> + '_ {
        let (first, last) = self
            .assembly
            .mapped_range(table::EVENT_MAP, self.index, table::EVENT);
        (first..last).map(move |index| Event {
            assembly: self.assembly,
            index,
        })
    }

    /// The type this type is nested in, if any (II.22.32).
    #[must_use]
    pub fn enclosing_type(&self) -> Option<TypeDef<'a>> {
        for index in 1..=self.assembly.tables.row_count(table::NESTED_CLASS) {
            let row = self.assembly.tables.row(table::NESTED_CLASS, index)?;
            if row.raw(0) == self.index {
                return self.assembly.type_def(row.raw(1));
            }
        }
        None
    }

    /// The types nested directly within this type (II.22.32).
    pub fn nested_types(&self) -> impl Iterator<Item = TypeDef<'a>> + '_ {
        let enclosing = self.index;
        (1..=self.assembly.tables.row_count(table::NESTED_CLASS)).filter_map(move |index| {
            let row = self.assembly.tables.row(table::NESTED_CLASS, index)?;
            (row.raw(1) == enclosing).then(|| self.assembly.type_def(row.raw(0)))?
        })
    }
}

/// A navigable property definition (II.22.34).
#[derive(Debug, Clone, Copy)]
pub struct Property<'a> {
    assembly: Assembly<'a>,
    index: u32,
}

impl<'a> Property<'a> {
    /// The property's name.
    #[must_use]
    pub fn name(&self) -> Option<&'a str> {
        let row = self.assembly.tables.row(table::PROPERTY, self.index)?;
        self.assembly.strings().get(row.raw(1)).ok()
    }

    /// This property's `Property` metadata token (e.g. for its custom attributes).
    #[must_use]
    pub fn token(&self) -> Token {
        Token::new(table::PROPERTY, self.index)
    }

    /// The property attribute flags (II.23.1.14).
    #[must_use]
    pub fn flags(&self) -> u32 {
        self.assembly
            .tables
            .row(table::PROPERTY, self.index)
            .map_or(0, |row| row.raw(0))
    }
}

/// A navigable event definition (II.22.13).
#[derive(Debug, Clone, Copy)]
pub struct Event<'a> {
    assembly: Assembly<'a>,
    index: u32,
}

impl<'a> Event<'a> {
    /// The event's name.
    #[must_use]
    pub fn name(&self) -> Option<&'a str> {
        let row = self.assembly.tables.row(table::EVENT, self.index)?;
        self.assembly.strings().get(row.raw(1)).ok()
    }

    /// The event's delegate type, as a `TypeDefOrRef` token.
    #[must_use]
    pub fn event_type(&self) -> Token {
        self.assembly
            .tables
            .row(table::EVENT, self.index)
            .map_or(Token::new(0, 0), |row| row.token(2))
    }
}

/// A navigable field definition (II.22.15).
#[derive(Debug, Clone, Copy)]
pub struct Field<'a> {
    assembly: Assembly<'a>,
    index: u32,
}

impl<'a> Field<'a> {
    /// The field's name.
    #[must_use]
    pub fn name(&self) -> Option<&'a str> {
        let row = self.assembly.tables.row(table::FIELD, self.index)?;
        self.assembly.strings().get(row.raw(1)).ok()
    }

    /// The field attribute flags (II.23.1.5).
    #[must_use]
    pub fn flags(&self) -> u32 {
        self.assembly
            .tables
            .row(table::FIELD, self.index)
            .map_or(0, |row| row.raw(0))
    }

    /// The field's decoded type signature.
    #[must_use]
    pub fn signature(&self) -> Option<SigType> {
        let row = self.assembly.tables.row(table::FIELD, self.index)?;
        let blob = self.assembly.image.blob().get(row.raw(2)).ok()?;
        parse_field(blob).ok()
    }

    /// The field's decoded type signature and its REQUIRED custom modifiers.
    ///
    /// A field carries them -- `modreq(System.Runtime.CompilerServices.IsVolatile)` is how a
    /// `volatile` field is spelled -- and II.7.1.1 makes an unrecognized one a reason to refuse the
    /// member, so a consumer asking "may I use this field" needs both halves from one decode rather
    /// than a second walk of the same blob.
    #[must_use]
    pub fn signature_with_modifiers(&self) -> Option<(SigType, alloc::vec::Vec<Token>)> {
        let row = self.assembly.tables.row(table::FIELD, self.index)?;
        let blob = self.assembly.image.blob().get(row.raw(2)).ok()?;
        crate::signature::parse_field_with_modifiers(blob).ok()
    }

    /// Whether the field is static.
    #[must_use]
    pub fn is_static(&self) -> bool {
        flags::field_is_static(self.flags())
    }

    /// This field's `Field` token.
    #[must_use]
    pub fn token(&self) -> Token {
        Token::new(table::FIELD, self.index)
    }

    /// Whether the field is a `const` literal.
    #[must_use]
    pub fn is_literal(&self) -> bool {
        flags::field_is_literal(self.flags())
    }

    /// The field's constant value, if it has one (a `const` field or enum member).
    #[must_use]
    pub fn constant(&self) -> Option<ConstantValue> {
        self.assembly.constant(Token::new(table::FIELD, self.index))
    }
}

/// A navigable method definition (II.22.26).
#[derive(Debug, Clone, Copy)]
pub struct Method<'a> {
    assembly: Assembly<'a>,
    index: u32,
}

impl<'a> Method<'a> {
    /// The method's name.
    #[must_use]
    pub fn name(&self) -> Option<&'a str> {
        let row = self.assembly.tables.row(table::METHOD_DEF, self.index)?;
        self.assembly.strings().get(row.raw(3)).ok()
    }

    /// The method's `MethodDef` row index (its metadata rid) -- the key a Portable PDB
    /// uses for sequence points and local-variable names.
    #[must_use]
    pub fn rid(&self) -> u32 {
        self.index
    }

    /// This method's `MethodDef` metadata token -- e.g. to look up its custom attributes.
    #[must_use]
    pub fn token(&self) -> Token {
        Token::new(table::METHOD_DEF, self.index)
    }

    /// The custom attributes applied to this method (II.22.10) -- e.g. `[Obsolete]` on a
    /// constructor, or a `[RuntimeProvided]` marker a loader reads to bind an intrinsic.
    pub fn custom_attributes(&self) -> impl Iterator<Item = CustomAttribute<'a>> + '_ {
        self.assembly
            .custom_attributes(Token::new(table::METHOD_DEF, self.index))
    }

    /// The method attribute flags (II.23.1.10).
    #[must_use]
    pub fn flags(&self) -> u32 {
        self.assembly
            .tables
            .row(table::METHOD_DEF, self.index)
            .map_or(0, |row| row.raw(2))
    }

    /// The method implementation flags (II.23.1.11): the `ImplFlags` column. Its
    /// `CodeTypeMask` says whether the body is CIL, native, or provided by the runtime
    /// -- the last is the conforming seam a managed BCL method crosses to a runtime
    /// intrinsic (see [`crate::flags::method_impl`]).
    #[must_use]
    pub fn impl_flags(&self) -> u32 {
        self.assembly
            .tables
            .row(table::METHOD_DEF, self.index)
            .map_or(0, |row| row.raw(1))
    }

    /// The relative virtual address of the method body, 0 for none (abstract,
    /// extern).
    #[must_use]
    pub fn rva(&self) -> u32 {
        self.assembly
            .tables
            .row(table::METHOD_DEF, self.index)
            .map_or(0, |row| row.raw(0))
    }

    /// The method's signature as raw blob bytes, undecoded.
    ///
    /// For a caller that needs to inspect a signature this build's decoder declines -- a generic one,
    /// today -- without that refusal becoming an absence. [`Self::signature`] is the decoding
    /// accessor and answers `None` for exactly those; this answers the bytes either way, and the
    /// caller does its own walk.
    #[must_use]
    pub fn signature_blob(&self) -> &'a [u8] {
        self.assembly
            .tables
            .row(table::METHOD_DEF, self.index)
            .and_then(|row| self.assembly.image.blob().get(row.raw(4)).ok())
            .unwrap_or(&[])
    }

    /// The method's decoded signature.
    #[must_use]
    pub fn signature(&self) -> Option<MethodSig> {
        let row = self.assembly.tables.row(table::METHOD_DEF, self.index)?;
        let blob = self.assembly.image.blob().get(row.raw(4)).ok()?;
        parse_method(blob).ok()
    }

    /// The method's CIL body, decoded through [`lamella_cil`]. `None` for a method
    /// with no body (abstract, extern), or when the assembly was built from a bare
    /// metadata image with no backing PE.
    #[must_use]
    pub fn body(&self) -> Option<MethodBodyImage> {
        let file = self.assembly.file?;
        let rva = self.rva();
        if rva == 0 {
            return None;
        }
        let offset = PeImage::parse(file).ok()?.rva_to_offset(rva).ok()?;
        read_method_body(file.get(offset..)?).ok()
    }

    /// The method's raw CIL body bytes -- the exact region [`body`](Self::body) decodes (header + CIL +
    /// any exception section), or `None` for a bodyless method / a bare metadata image. A caller can
    /// store these and re-decode the body lazily with [`lamella_cil::read_method_body`], which is the
    /// foundation for keeping the corlib's bodies compact in RAM until a method is actually called.
    #[must_use]
    pub fn body_bytes(&self) -> Option<&'a [u8]> {
        let file = self.assembly.file?;
        let rva = self.rva();
        if rva == 0 {
            return None;
        }
        let offset = PeImage::parse(file).ok()?.rva_to_offset(rva).ok()?;
        let rest = file.get(offset..)?;
        let (_, len) = read_method_body_sized(rest).ok()?;
        rest.get(..len)
    }

    /// The method's decoded body AND its raw bytes in a single decode -- so a loader can scan the
    /// decoded form (for token binding) yet store the compact raw bytes for lazy re-decoding, without
    /// decoding the body twice. `None` for a bodyless method / a bare metadata image.
    #[must_use]
    pub fn body_and_bytes(&self) -> Option<(MethodBodyImage, &'a [u8])> {
        let file = self.assembly.file?;
        let rva = self.rva();
        if rva == 0 {
            return None;
        }
        let offset = PeImage::parse(file).ok()?.rva_to_offset(rva).ok()?;
        let rest = file.get(offset..)?;
        let (body, len) = read_method_body_sized(rest).ok()?;
        Some((body, rest.get(..len)?))
    }

    /// The method's local-variable types, resolving the body's local-variable
    /// signature (a `StandAloneSig`, II.23.2.6). The index in the returned vector is
    /// the local's slot number. Empty when the method declares no locals (or has no
    /// body). This is what an interpreter or AOT lowering needs to type its locals,
    /// and what `lamella-dap` needs to show them.
    ///
    /// TYPES ONLY: the `pinned` constraint a `fixed` statement's holder slot carries is not in
    /// this answer. A consumer that derives a raw address from a slot and reports roots to a
    /// MOVING collector needs [`Self::local_variable_slots`] instead -- see [`LocalVar::pinned`].
    #[must_use]
    pub fn local_variables(&self) -> Vec<SigType> {
        self.local_variable_slots()
            .into_iter()
            .map(|local| local.ty)
            .collect()
    }

    /// The method's local-variable slots COMPLETE -- each slot's type AND its `pinned`
    /// constraint (II.23.2.6), indexed by slot number. Empty when the method declares no
    /// locals (or has no body).
    ///
    /// [`Self::local_variables`] is the types-only view over the same decode. This is the one a
    /// collector-facing consumer wants: a pinned slot is a C# `fixed` statement's holder, and its
    /// promise (do not relocate this object while the slot is live) is what keeps the untracked
    /// `T*` the statement hands the program valid across a collection.
    #[must_use]
    pub fn local_variable_slots(&self) -> Vec<LocalVar> {
        let Some(token) = self.body().and_then(|body| body.local_var_sig) else {
            return Vec::new();
        };
        self.assembly
            .tables
            .row(table::STAND_ALONE_SIG, token.row())
            .and_then(|row| self.assembly.image.blob().get(row.raw(0)).ok())
            .and_then(|blob| parse_local_vars(blob).ok())
            .unwrap_or_default()
    }

    /// The method's exception-handling clauses (II.25.4.6) with regions as IL BYTE offsets
    /// -- what an AOT/CFG lowering maps to its basic blocks. (`lamella-cil` decodes the EH
    /// table into instruction-index ranges; these are mapped back to byte offsets via the
    /// instruction layout, the same round-trip the body writer uses.) Empty when the method
    /// has no body or no handlers.
    #[must_use]
    pub fn exception_clauses(&self) -> Vec<ExceptionClause> {
        let Some(body) = self.body() else {
            return Vec::new();
        };
        if body.handlers.is_empty() {
            return Vec::new();
        }
        let Some(offsets) = instruction_offsets(&body.code) else {
            return Vec::new();
        };
        let byte_at = |index: u32| {
            offsets
                .get(index as usize)
                .copied()
                .unwrap_or_else(|| offsets.last().copied().unwrap_or(0))
        };
        body.handlers
            .iter()
            .map(|handler| {
                let try_offset = byte_at(handler.try_range.start);
                let handler_offset = byte_at(handler.handler_range.start);
                ExceptionClause {
                    kind: match handler.kind {
                        EhKind::Catch(token) => ExceptionHandlerKind::Catch(token),
                        EhKind::Filter { filter_start } => ExceptionHandlerKind::Filter {
                            filter_offset: byte_at(filter_start),
                        },
                        EhKind::Finally => ExceptionHandlerKind::Finally,
                        EhKind::Fault => ExceptionHandlerKind::Fault,
                    },
                    try_offset,
                    try_length: byte_at(handler.try_range.end).saturating_sub(try_offset),
                    handler_offset,
                    handler_length: byte_at(handler.handler_range.end)
                        .saturating_sub(handler_offset),
                }
            })
            .collect()
    }

    /// Whether the method is static.
    #[must_use]
    pub fn is_static(&self) -> bool {
        flags::method_is_static(self.flags())
    }

    /// Whether the method's body is provided by the runtime (II.23.1.11 `Runtime`): a
    /// bodyless method the runtime supplies. This is the conforming seam a managed BCL
    /// method crosses to a native intrinsic (the standard alternative to the
    /// non-conforming `internalcall`).
    #[must_use]
    pub fn is_runtime_impl(&self) -> bool {
        flags::method_impl_is_runtime(self.impl_flags())
    }

    /// Whether the method is virtual.
    #[must_use]
    pub fn is_virtual(&self) -> bool {
        flags::method_is_virtual(self.flags())
    }

    /// Whether the method is abstract.
    #[must_use]
    pub fn is_abstract(&self) -> bool {
        flags::method_is_abstract(self.flags())
    }

    /// The method's declared parameters (II.22.33). Note the parameter list may
    /// include a row for the return value (sequence 0).
    pub fn params(&self) -> impl Iterator<Item = Param<'a>> + '_ {
        let (first, last) =
            self.assembly
                .child_range(table::METHOD_DEF, self.index, 5, table::PARAM);
        (first..last).map(move |index| Param {
            assembly: self.assembly,
            index,
        })
    }

    /// Whether a parameter carries `System.ParamArrayAttribute` (II.23.2) -- a C#
    /// `params` array, so the method is callable with a variable number of trailing
    /// arguments. Only the last parameter may, but any is checked.
    #[must_use]
    pub fn has_param_array(&self) -> bool {
        self.params().any(|param| {
            self.assembly
                .custom_attributes(param.token())
                .any(|attribute| {
                    self.assembly
                        .resolve_method(attribute.constructor)
                        .and_then(|target| target.declaring_type)
                        .is_some_and(|name| {
                            name.namespace == "System" && name.name == "ParamArrayAttribute"
                        })
                })
        })
    }
}

/// A navigable parameter definition (II.22.33).
#[derive(Debug, Clone, Copy)]
pub struct Param<'a> {
    assembly: Assembly<'a>,
    index: u32,
}

impl<'a> Param<'a> {
    /// The parameter's name, if recorded.
    #[must_use]
    pub fn name(&self) -> Option<&'a str> {
        let row = self.assembly.tables.row(table::PARAM, self.index)?;
        self.assembly.strings().get(row.raw(2)).ok()
    }

    /// The parameter attribute flags (II.23.1.13).
    #[must_use]
    pub fn flags(&self) -> u32 {
        self.assembly
            .tables
            .row(table::PARAM, self.index)
            .map_or(0, |row| row.raw(0))
    }

    /// The 1-based parameter position; 0 marks the return value's row.
    #[must_use]
    pub fn sequence(&self) -> u32 {
        self.assembly
            .tables
            .row(table::PARAM, self.index)
            .map_or(0, |row| row.raw(1))
    }

    /// This parameter's metadata token (for, e.g., its custom attributes).
    #[must_use]
    pub fn token(&self) -> Token {
        Token::new(table::PARAM, self.index)
    }

    /// The parameter's DEFAULT ARGUMENT, if it has one -- the `Constant` row (II.22.9) an
    /// optional parameter carries, which is how a default crosses an assembly boundary.
    ///
    /// `Constant` is keyed on a `HasConstant` coded index, so a `Param` parent reads exactly as a
    /// `Field` parent does; only the token's table differs.
    #[must_use]
    pub fn constant(&self) -> Option<ConstantValue> {
        self.assembly.constant(Token::new(table::PARAM, self.index))
    }
}

/// A navigable referenced-type row (II.22.38).
#[derive(Debug, Clone, Copy)]
pub struct TypeRef<'a> {
    assembly: Assembly<'a>,
    index: u32,
}

impl<'a> TypeRef<'a> {
    /// The referenced type's namespace and name.
    #[must_use]
    pub fn name(&self) -> Option<TypeName<'a>> {
        let row = self.assembly.tables.row(table::TYPE_REF, self.index)?;
        let name = self.assembly.strings().get(row.raw(1)).ok()?;
        let namespace = self.assembly.strings().get(row.raw(2)).ok()?;
        Some(TypeName { namespace, name })
    }

    /// The resolution scope token: where the type is defined (an `AssemblyRef`,
    /// `ModuleRef`, `Module`, or enclosing `TypeRef`).
    #[must_use]
    pub fn resolution_scope(&self) -> Token {
        self.assembly
            .tables
            .row(table::TYPE_REF, self.index)
            .map_or(Token::new(0, 0), |row| row.token(0))
    }

    /// This referenced type's `TypeRef` metadata token.
    #[must_use]
    pub fn token(&self) -> Token {
        Token::new(table::TYPE_REF, self.index)
    }
}

/// A navigable referenced-assembly row (II.22.5).
#[derive(Debug, Clone, Copy)]
pub struct AssemblyRef<'a> {
    assembly: Assembly<'a>,
    index: u32,
}

impl<'a> AssemblyRef<'a> {
    /// The referenced assembly's simple name.
    #[must_use]
    pub fn name(&self) -> Option<&'a str> {
        let row = self.assembly.tables.row(table::ASSEMBLY_REF, self.index)?;
        self.assembly.strings().get(row.raw(6)).ok()
    }

    /// The referenced assembly's version `(major, minor, build, revision)`.
    #[must_use]
    pub fn version(&self) -> (u16, u16, u16, u16) {
        self.assembly
            .tables
            .row(table::ASSEMBLY_REF, self.index)
            .map_or((0, 0, 0, 0), |row| {
                (
                    row.raw(0) as u16,
                    row.raw(1) as u16,
                    row.raw(2) as u16,
                    row.raw(3) as u16,
                )
            })
    }

    /// The reference's flags (II.23.1.2). The `afPublicKey` bit (0x0001) says
    /// [`AssemblyRef::public_key_or_token`] holds the full public key rather than its 8-byte token.
    #[must_use]
    pub fn flags(&self) -> u32 {
        self.assembly
            .tables
            .row(table::ASSEMBLY_REF, self.index)
            .map_or(0, |row| row.raw(4))
    }

    /// The reference's public key or token blob (II.22.5); empty for a tokenless name-only
    /// reference. [`AssemblyRef::flags`] `afPublicKey` distinguishes a full key from a token.
    #[must_use]
    pub fn public_key_or_token(&self) -> &'a [u8] {
        self.assembly
            .tables
            .row(table::ASSEMBLY_REF, self.index)
            .and_then(|row| self.assembly.image.blob().get(row.raw(5)).ok())
            .unwrap_or(&[])
    }
}

/// A navigable member reference (II.22.25): a method or field referenced through
/// its parent type, by name and signature. The runtime resolves a `call`/`ldfld`
/// token to this to find the target.
#[derive(Debug, Clone, Copy)]
pub struct MemberRef<'a> {
    assembly: Assembly<'a>,
    index: u32,
}

impl<'a> MemberRef<'a> {
    /// The token of the member's parent (a `TypeRef`, `TypeDef`, `ModuleRef`,
    /// `MethodDef`, or `TypeSpec`).
    #[must_use]
    pub fn parent(&self) -> Token {
        self.assembly
            .tables
            .row(table::MEMBER_REF, self.index)
            .map_or(Token::new(0, 0), |row| row.token(0))
    }

    /// The referenced member's name.
    #[must_use]
    pub fn name(&self) -> Option<&'a str> {
        let row = self.assembly.tables.row(table::MEMBER_REF, self.index)?;
        self.assembly.strings().get(row.raw(1)).ok()
    }

    /// The raw signature blob.
    #[must_use]
    pub fn signature_blob(&self) -> &'a [u8] {
        self.assembly
            .tables
            .row(table::MEMBER_REF, self.index)
            .and_then(|row| self.assembly.image.blob().get(row.raw(2)).ok())
            .unwrap_or(&[])
    }

    /// Whether the reference is to a field (its signature starts with the FIELD
    /// calling convention) rather than a method.
    #[must_use]
    pub fn is_field(&self) -> bool {
        self.signature_blob().first() == Some(&calling::FIELD)
    }

    /// The referenced method's signature, if this is a method reference.
    #[must_use]
    pub fn method_signature(&self) -> Option<MethodSig> {
        if self.is_field() {
            return None;
        }
        parse_method(self.signature_blob()).ok()
    }

    /// The referenced field's type, if this is a field reference.
    #[must_use]
    pub fn field_type(&self) -> Option<SigType> {
        if self.is_field() {
            parse_field(self.signature_blob()).ok()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::MetadataImage;
    use alloc::vec::Vec;

    const METADATA_SIGNATURE: u32 = 0x424A_5342;

    #[test]
    fn decodes_positional_int_and_string_arguments() {
        let blob = [
            0x01, 0x00, 0x2a, 0x00, 0x00, 0x00, 0x06, b'a', b'n', b's', b'w', b'e', b'r', 0x00,
            0x00,
        ];
        let decoded =
            decode_custom_attribute(&blob, &[SigType::I4, SigType::String], &|_| element::I4)
                .unwrap();
        assert_eq!(decoded.fixed, [AttrArg::Int(42), AttrArg::Str("answer")]);
        assert!(decoded.named.is_empty());
    }

    #[test]
    fn decodes_named_string_and_enum_arguments() {
        let blob = [
            0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x00, 0x53, 0x0e, 0x04, b'N', b'o', b't',
            b'e', 0x02, b'h', b'i', 0x53, 0x55, 0x05, b'C', b'o', b'l', b'o', b'r', 0x05, b'S',
            b'h', b'a', b'd', b'e', 0x28, 0x00, 0x00, 0x00,
        ];
        let decoded = decode_custom_attribute(&blob, &[SigType::I4], &|name| {
            assert_eq!(name, "Color");
            element::I4
        })
        .unwrap();
        assert_eq!(decoded.fixed, [AttrArg::Int(2)]);
        assert_eq!(
            decoded.named,
            [
                AttrNamed {
                    is_field: true,
                    name: "Note",
                    value: AttrArg::Str("hi"),
                },
                AttrNamed {
                    is_field: true,
                    name: "Shade",
                    value: AttrArg::Int(40),
                },
            ]
        );
    }

    #[test]
    fn decodes_named_typeof_argument() {
        let blob = [
            0x01, 0x00, 0x01, 0x00, 0x53, 0x50, 0x04, b'K', b'i', b'n', b'd', 0x07, b'P', b'r',
            b'o', b'g', b'r', b'a', b'm',
        ];
        let decoded = decode_custom_attribute(&blob, &[], &|_| element::I4).unwrap();
        assert!(decoded.fixed.is_empty());
        assert_eq!(
            decoded.named,
            [AttrNamed {
                is_field: true,
                name: "Kind",
                value: AttrArg::Type("Program"),
            }]
        );
    }

    #[test]
    fn a_wrong_prolog_is_rejected() {
        assert!(decode_custom_attribute(&[0x02, 0x00], &[], &|_| element::I4).is_none());
    }

    /// Builds a metadata root with a `#Strings` heap and a `#~` stream holding one
    /// Module row and two TypeDef rows (`<Module>` and `Foo.Bar`).
    fn synthetic_assembly() -> Vec<u8> {
        let mut strings = Vec::new();
        strings.push(0);
        strings.extend_from_slice(b"MyModule\0");
        strings.extend_from_slice(b"<Module>\0");
        strings.extend_from_slice(b"Bar\0");
        strings.extend_from_slice(b"Foo\0");
        strings.extend_from_slice(b"Object\0");
        strings.extend_from_slice(b"System\0");

        let mut tables = Vec::new();
        tables.extend_from_slice(&0u32.to_le_bytes());
        tables.extend_from_slice(&[2, 0, 0, 0]);
        let valid = (1u64 << table::MODULE) | (1u64 << table::TYPE_REF) | (1u64 << table::TYPE_DEF);
        tables.extend_from_slice(&valid.to_le_bytes());
        tables.extend_from_slice(&0u64.to_le_bytes());
        tables.extend_from_slice(&1u32.to_le_bytes());
        tables.extend_from_slice(&1u32.to_le_bytes());
        tables.extend_from_slice(&2u32.to_le_bytes());
        tables.extend_from_slice(&[0, 0]);
        tables.extend_from_slice(&1u16.to_le_bytes());
        tables.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        tables.extend_from_slice(&0u16.to_le_bytes());
        tables.extend_from_slice(&27u16.to_le_bytes());
        tables.extend_from_slice(&34u16.to_le_bytes());
        tables.extend_from_slice(&0u32.to_le_bytes());
        tables.extend_from_slice(&10u16.to_le_bytes());
        tables.extend_from_slice(&0u16.to_le_bytes());
        tables.extend_from_slice(&[0, 0, 1, 0, 1, 0]);
        tables.extend_from_slice(&0u32.to_le_bytes());
        tables.extend_from_slice(&19u16.to_le_bytes());
        tables.extend_from_slice(&23u16.to_le_bytes());
        tables.extend_from_slice(&[0, 0, 1, 0, 1, 0]);

        let mut root = Vec::new();
        root.extend_from_slice(&METADATA_SIGNATURE.to_le_bytes());
        root.extend_from_slice(&[1, 0, 1, 0]);
        root.extend_from_slice(&0u32.to_le_bytes());
        root.extend_from_slice(&4u32.to_le_bytes());
        root.extend_from_slice(b"v1\0\0");
        root.extend_from_slice(&0u16.to_le_bytes());
        root.extend_from_slice(&2u16.to_le_bytes());
        let headers_len = 24 + (8 + 12) + (8 + 4);
        let strings_offset = headers_len;
        let tables_offset = headers_len + strings.len();
        root.extend_from_slice(&(strings_offset as u32).to_le_bytes());
        root.extend_from_slice(&(strings.len() as u32).to_le_bytes());
        root.extend_from_slice(b"#Strings\0\0\0\0");
        root.extend_from_slice(&(tables_offset as u32).to_le_bytes());
        root.extend_from_slice(&(tables.len() as u32).to_le_bytes());
        root.extend_from_slice(b"#~\0\0");
        assert_eq!(root.len(), headers_len);
        root.extend_from_slice(&strings);
        root.extend_from_slice(&tables);
        root
    }

    #[test]
    fn reads_type_refs_with_scope_and_name() {
        let root = synthetic_assembly();
        let image = MetadataImage::parse_metadata_root(&root).unwrap();
        let assembly = Assembly::from_image(image).unwrap();
        let object = assembly.type_ref(1).unwrap();
        assert_eq!(
            object.name(),
            Some(TypeName {
                namespace: "System",
                name: "Object"
            })
        );
        assert!(object.resolution_scope().is_nil());
        assert_eq!(assembly.type_count(), 2);
    }

    #[test]
    fn type_def_views_expose_name_flags_and_empty_member_ranges() {
        let root = synthetic_assembly();
        let image = MetadataImage::parse_metadata_root(&root).unwrap();
        let assembly = Assembly::from_image(image).unwrap();
        let foo_bar = assembly.type_def(2).unwrap();
        assert_eq!(
            foo_bar.name(),
            Some(TypeName {
                namespace: "Foo",
                name: "Bar"
            })
        );
        assert_eq!(foo_bar.flags(), 0);
        assert!(foo_bar.extends().is_nil());
        assert_eq!(foo_bar.fields().count(), 0);
        assert_eq!(foo_bar.methods().count(), 0);
        assert_eq!(foo_bar.interfaces().count(), 0);
        assert_eq!(foo_bar.custom_attributes().count(), 0);
        assert_eq!(foo_bar.properties().count(), 0);
        assert_eq!(foo_bar.events().count(), 0);
        assert!(foo_bar.enclosing_type().is_none());
        assert_eq!(foo_bar.nested_types().count(), 0);
        assert_eq!(assembly.member_refs().count(), 0);
        assert!(assembly.member_ref(1).is_none());
        assert!(assembly.type_def(0).is_none());
        assert!(assembly.type_def(3).is_none());
    }

    #[test]
    fn enumerates_the_module_and_types() {
        let root = synthetic_assembly();
        let image = MetadataImage::parse_metadata_root(&root).unwrap();
        let assembly = Assembly::from_image(image).unwrap();
        assert_eq!(assembly.module_name(), Some("MyModule"));
        assert_eq!(assembly.type_count(), 2);
        assert!(assembly.find_type("Foo", "Bar").is_some());
        assert!(assembly.find_type("", "Nope").is_none());
        let names: Vec<_> = assembly.types().collect();
        assert_eq!(
            names,
            [
                TypeName {
                    namespace: "",
                    name: "<Module>"
                },
                TypeName {
                    namespace: "Foo",
                    name: "Bar"
                },
            ]
        );
    }
}
