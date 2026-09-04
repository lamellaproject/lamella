//! Type signatures (ECMA-335 1st ed, II.23.2).

use crate::bytes::{ReadError, Reader};
use alloc::boxed::Box;
use alloc::vec::Vec;
use lamella_token::Token;

/// Calling-convention bits in a signature's leading byte (II.23.2.3).
pub mod calling {
    /// The leading byte of a field signature.
    pub const FIELD: u8 = 0x06;
    /// The instance flag: the method has a `this` parameter.
    pub const HAS_THIS: u8 = 0x20;
    /// The explicit-`this` flag: `this` is the first declared parameter.
    pub const EXPLICIT_THIS: u8 = 0x40;
    /// The `vararg` calling convention (the low nibble of the leading byte): a method taking a
    /// variable argument list (`__arglist`). Masked with [`CONVENTION_MASK`].
    pub const VARARG: u8 = 0x05;
    /// The mask for the calling-convention nibble (the low 4 bits of the leading byte).
    pub const CONVENTION_MASK: u8 = 0x0F;
    /// The GENERIC flag: the method declares type parameters, so its signature carries a
    /// `GenParamCount` BEFORE `ParamCount` (II.23.2.1).
    ///
    /// IT IS ABOVE [`CONVENTION_MASK`], WHICH IS EXACTLY WHY IT HAS TO BE TESTED SEPARATELY. A
    /// generic method's leading byte masks to `0x00` -- indistinguishable from DEFAULT -- so a
    /// reader that only looks at the nibble reads `GenParamCount` as the parameter count and every
    /// later read shifts by one. That produces a WRONG signature and no error.
    pub const GENERIC: u8 = 0x10;
    /// The vararg-sentinel element type, separating fixed from vararg parameters.
    pub const SENTINEL: u8 = 0x41;
    /// The leading byte of a local-variable signature (II.23.2.6).
    pub const LOCAL_SIG: u8 = 0x07;
    /// The leading byte of a `MethodSpec` instantiation signature (II.23.2.15):
    /// `IMAGE_CEE_CS_CALLCONV_GENERICINST`.
    ///
    /// **`0x0A`, NOT `0x15`, AND THE TWO ARE BOTH CALLED `GENERICINST`.** This one is a CALLING
    /// CONVENTION and leads a whole blob; [`super::element::GENERICINST`] (`0x15`) is an ELEMENT
    /// TYPE that appears INSIDE a type signature. Same name, different namespaces, different
    /// positions -- and `0x0A` is also [`super::element::I8`], so a reader that takes this byte for
    /// an element type decodes a `long` and reports no error.
    pub const GENERICINST: u8 = 0x0A;
}

/// The element-type bytes a signature begins with (II.23.1.16).
pub mod element {
    /// `void`.
    pub const VOID: u8 = 0x01;
    /// `bool`.
    pub const BOOLEAN: u8 = 0x02;
    /// `char`.
    pub const CHAR: u8 = 0x03;
    /// `sbyte`.
    pub const I1: u8 = 0x04;
    /// `byte`.
    pub const U1: u8 = 0x05;
    /// `short`.
    pub const I2: u8 = 0x06;
    /// `ushort`.
    pub const U2: u8 = 0x07;
    /// `int`.
    pub const I4: u8 = 0x08;
    /// `uint`.
    pub const U4: u8 = 0x09;
    /// `long`.
    pub const I8: u8 = 0x0A;
    /// `ulong`.
    pub const U8: u8 = 0x0B;
    /// `float`.
    pub const R4: u8 = 0x0C;
    /// `double`.
    pub const R8: u8 = 0x0D;
    /// `string`.
    pub const STRING: u8 = 0x0E;
    /// An unmanaged pointer; followed by the pointee type.
    pub const PTR: u8 = 0x0F;
    /// A managed reference; followed by the referent type.
    pub const BYREF: u8 = 0x10;
    /// A value type; followed by a `TypeDefOrRef` token.
    pub const VALUETYPE: u8 = 0x11;
    /// A reference type; followed by a `TypeDefOrRef` token.
    pub const CLASS: u8 = 0x12;
    /// A generic TYPE parameter in a generic type definition (`!0`), II.23.1.16: "represented as
    /// number (compressed unsigned integer)".
    ///
    /// Decodes into [`super::SigType::Var`]. It is folded into the AOT's
    /// interface-method tag (`lamella_aot::resolver::interface_method_tag`), which is FROZEN
    /// cross-assembly ABI -- **the byte alone is not the contribution.** A parameter number
    /// follows it, because `!0` and `!1` are different types and one byte cannot say which.
    pub const VAR: u8 = 0x13;
    /// A general (multi-dimensional) array; followed by element type and shape.
    pub const ARRAY: u8 = 0x14;
    /// A generic type INSTANTIATION (`List<int>`), II.23.1.16: "Followed by type type-arg-count
    /// type-1 ... type-n". Decoded into [`super::SigType::GenericInst`].
    ///
    /// **THIS BYTE IS THE ONE THAT CANNOT CARRY ITS OWN IDENTITY**, and the AOT's interface-method
    /// tag folds the instantiation's CANONICAL SPELLING after it rather than the byte alone: the
    /// byte says "an instantiation" and cannot say WHICH, so `List<int>` and `HashSet<int>` would be
    /// one dispatch key. A definition token cannot stand in for the name either -- a token is
    /// meaningful only inside its own assembly, and that tag is cross-assembly ABI.
    pub const GENERICINST: u8 = 0x15;
    /// `System.TypedReference`.
    pub const TYPEDBYREF: u8 = 0x16;
    /// `native int`.
    pub const I: u8 = 0x18;
    /// `native uint`.
    pub const U: u8 = 0x19;
    /// A function pointer; followed by a method signature.
    pub const FNPTR: u8 = 0x1B;
    /// `object`.
    pub const OBJECT: u8 = 0x1C;
    /// A single-dimensional zero-based array; followed by element type.
    pub const SZARRAY: u8 = 0x1D;
    /// A generic METHOD parameter in a generic method definition (`!!0`), II.23.1.16: also
    /// "represented as number (compressed unsigned integer)". Decoded into [`super::SigType::MVar`].
    pub const MVAR: u8 = 0x1E;
    /// A required custom modifier; followed by a `TypeDefOrRef` token.
    pub const CMOD_REQD: u8 = 0x1F;
    /// An optional custom modifier; followed by a `TypeDefOrRef` token.
    pub const CMOD_OPT: u8 = 0x20;
    /// A pinned local-variable constraint, preceding the local's type (II.23.2.6).
    pub const PINNED: u8 = 0x45;
}

/// The ECMA-335 element-type byte a [`SigType`](super::SigType) is spelled with -- the ENCODE
/// direction of the table [`element`] declares and [`parse_type`](super::parse_type) decodes.
///
/// # Why it lives here and not in a consumer
///
/// It maps a `lamella-metadata` type onto `lamella-metadata` constants, so a copy anywhere else is a
/// SECOND table over one byte space. A separate copy missing an arm for `Pointer`/`ByRef` tags
/// `IFoo.Bar(int*)`, `IFoo.Bar(byte*)` and `IFoo.Bar(ref int)` alike. One table, beside the decoder it must
/// agree with.
///
/// **EVERY BYTE IS FROZEN.** The AOT folds these into interface-method tags, which are baked
/// into emitted code and into itable entries in type descriptors, and a program object links against
/// library objects compiled at other times -- so changing a byte silently mis-dispatches unless every
/// artifact is rebuilt. There is deliberately NO fallback arm: a `SigType` this does not name is a
/// COMPILE ERROR here, which is what forces the value to be chosen on purpose.
///
/// **THE THREE GENERIC BYTES ARE CORRECT AS BYTES AND INSUFFICIENT AS IDENTITIES.**
/// `GENERICINST` says "an instantiation" and cannot say WHICH; `VAR` says "a type parameter" and
/// cannot say which NUMBER. **A caller that wants a single byte must ask whether one is enough for
/// its own question** -- the AOT's `fold_tag_element` adds what the byte cannot carry, and its
/// `unbox_normal_form` decided one byte is not enough and refuses instead.
#[must_use]
pub fn element_byte(ty: &super::SigType) -> u8 {
    use super::SigType;
    match ty {
        SigType::Void => element::VOID,
        SigType::Boolean => element::BOOLEAN,
        SigType::Char => element::CHAR,
        SigType::I1 => element::I1,
        SigType::U1 => element::U1,
        SigType::I2 => element::I2,
        SigType::U2 => element::U2,
        SigType::I4 => element::I4,
        SigType::U4 => element::U4,
        SigType::I8 => element::I8,
        SigType::U8 => element::U8,
        SigType::R4 => element::R4,
        SigType::R8 => element::R8,
        SigType::String => element::STRING,
        SigType::Pointer(_) => element::PTR,
        SigType::ByRef(_) => element::BYREF,
        SigType::ValueType(_) => element::VALUETYPE,
        SigType::Class(_) => element::CLASS,
        SigType::Array { .. } => element::ARRAY,
        SigType::TypedByRef => element::TYPEDBYREF,
        SigType::IntPtr => element::I,
        SigType::UIntPtr => element::U,
        SigType::Object => element::OBJECT,
        SigType::SzArray(_) => element::SZARRAY,
        SigType::Var(_) => element::VAR,
        SigType::MVar(_) => element::MVAR,
        SigType::GenericInst { .. } => element::GENERICINST,
    }
}

/// [`element_byte`] read backwards, for the PAYLOAD-FREE bytes only.
///
/// # Why this is partial, and why the partiality is the point
///
/// `element_byte` is total because every `SigType` has a byte. The inverse is NOT: a byte says which
/// KIND of type, and for nine of them the kind is all it says. `CLASS` and `VALUETYPE` are followed
/// by a `TypeDefOrRef`, `SZARRAY` / `PTR` / `BYREF` / `ARRAY` by an element type, `GENERICINST` by a
/// definition and its arguments, and `VAR` / `MVAR` by a number. **Answering `Some` for any of those
/// would mean inventing the part the byte does not carry**, which is the same objection
/// `element_byte`'s own documentation raises against callers that want one byte to be an identity.
///
/// So this answers only where the byte IS the whole type, and `None` everywhere else. A caller that
/// needs a named or constructed type has to get it from a signature blob, which is the only place
/// the rest of the information exists.
#[must_use]
pub fn payload_free_sig(byte: u8) -> Option<super::SigType> {
    use super::SigType;
    Some(match byte {
        element::VOID => SigType::Void,
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
        element::TYPEDBYREF => SigType::TypedByRef,
        element::I => SigType::IntPtr,
        element::U => SigType::UIntPtr,
        element::OBJECT => SigType::Object,
        _ => return None,
    })
}

/// An error decoding a signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigError {
    /// A read ran past the end of the blob.
    Truncated,
    /// An element-type byte was not recognized.
    BadElementType(u8),
    /// A field signature did not begin with the FIELD calling convention.
    BadCallingConvention(u8),
    /// A method signature declares type parameters (the GENERIC convention, II.23.2.1).
    ///
    /// **NOT RETURNED** -- [`super::parse_method`] decodes the GENERIC convention. The variant is
    /// public, so removing it would break anyone matching on it, and the reason it names is worth
    /// keeping: that layout carries an extra leading `GenParamCount`, so reading straight past it
    /// yields a plausible and wrong signature rather than an error.
    GenericSignature,
}

/// A decoded method signature (II.23.2.1): the `this` flags, the return type, and
/// the parameter types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodSig {
    /// Whether the method has an implicit `this` parameter.
    pub has_this: bool,
    /// Whether `this` is given explicitly as the first parameter.
    pub explicit_this: bool,
    /// The return type.
    pub return_type: SigType,
    /// The parameter types, in order.
    pub parameters: Vec<SigType>,
    /// Whether the calling convention is `vararg` (II.23.2.3) -- a method taking a variable
    /// argument list (`__arglist`). A vararg `MethodDef` carries only the fixed parameters; a
    /// vararg call-site `MemberRef` carries the fixed parameters, then a sentinel, then the
    /// variable-argument types.
    pub is_vararg: bool,
    /// The REQUIRED custom modifiers on the RETURN type, as `TypeDefOrRef` tokens, in the order
    /// the blob wrote them. Empty for almost every method.
    ///
    /// **THE ONE THING THAT DISTINGUISHES AN INIT-ONLY SETTER FROM AN ORDINARY ONE.** csc emits
    /// `void modreq(System.Runtime.CompilerServices.IsExternalInit)` as an `init` accessor's
    /// return type, and the signature is an ordinary `void set_P(int)` in every other respect --
    /// so a decoder that drops the modifier reports the two as the same method, and a consumer
    /// assigns an init-only property wherever it likes: `b.P = 1` on an imported init-only property
    /// compiles clean without the modifier and is CS8852 under csc.
    ///
    /// Only the return position is captured, because that is where the one modifier this compiler
    /// acts on appears; see [`read_type_collecting_required`].
    pub return_type_required_modifiers: Vec<Token>,
    /// EVERY required custom modifier in this signature, in any position -- the return type and all
    /// parameters -- in the order the decoder met them.
    ///
    /// **This answers a different question from the field above and deliberately overlaps it.**
    /// `return_type_required_modifiers` is POSITIONAL: it exists because `modreq(IsExternalInit)` on
    /// the return type is what makes a setter init-only, and where it sits is the whole meaning. This
    /// one is not positional at all -- it exists because II.7.1.1 says a `modreq` a consumer does not
    /// understand makes the item UNUSABLE, and that rule does not care which position carried it.
    ///
    /// A consumer asking "may I use this member" must read THIS list. Reading the positional one
    /// instead answers for the return type and silently accepts a `modreq` on a parameter, which is
    /// the same shape as checking two of three call sites.
    pub required_modifiers: Vec<Token>,
    /// How many type parameters the method itself declares (the `1` of `T Identity<T>(T)`), or 0.
    ///
    /// **BINDING-SIGNIFICANT, NOT BOOKKEEPING** (II.23.2.1): the runtime overloads generic methods
    /// by their type-parameter count, so `M<T>()` and `M<T,U>()` are different methods and this is
    /// the difference. A reader that skipped it could not tell them apart.
    pub generic_param_count: u32,
    /// For a vararg call-site signature, the index in `parameters` at which the sentinel appeared --
    /// the count of FIXED parameters (the remainder are the variable arguments). `None` if no
    /// sentinel was present (a vararg `MethodDef`, or any non-vararg signature).
    pub sentinel_index: Option<usize>,
}

impl From<ReadError> for SigError {
    fn from(_: ReadError) -> SigError {
        SigError::Truncated
    }
}

/// A decoded type signature (II.23.2.12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigType {
    /// `void`.
    Void,
    /// `bool`.
    Boolean,
    /// `char`.
    Char,
    /// `sbyte`.
    I1,
    /// `byte`.
    U1,
    /// `short`.
    I2,
    /// `ushort`.
    U2,
    /// `int`.
    I4,
    /// `uint`.
    U4,
    /// `long`.
    I8,
    /// `ulong`.
    U8,
    /// `float`.
    R4,
    /// `double`.
    R8,
    /// `string`.
    String,
    /// `object`.
    Object,
    /// `native int`.
    IntPtr,
    /// `native uint`.
    UIntPtr,
    /// `System.TypedReference`.
    TypedByRef,
    /// A reference type named by a token.
    Class(Token),
    /// A value type named by a token.
    ValueType(Token),
    /// A single-dimensional zero-based array of the element type.
    SzArray(Box<SigType>),
    /// A multi-dimensional array of the element type, with its rank.
    Array {
        /// The element type.
        element: Box<SigType>,
        /// The number of dimensions.
        rank: u32,
    },
    /// An unmanaged pointer to the pointee type.
    Pointer(Box<SigType>),
    /// A managed reference to the referent type.
    ByRef(Box<SigType>),
    /// A generic parameter of the enclosing TYPE, by its zero-based number: the `T` of `Box<T>`
    /// seen from inside it (`ELEMENT_TYPE_VAR`, II.23.1.16).
    Var(u32),
    /// A generic parameter of the METHOD itself (`ELEMENT_TYPE_MVAR`).
    ///
    /// **Not interchangeable with [`SigType::Var`].** A generic method inside a generic type has
    /// both numbering spaces live at once, so `!0` and `!!0` are different types there.
    MVar(u32),
    /// An instantiation of a generic type: `List<int>`, or `Box<!0>` inside another generic
    /// (`ELEMENT_TYPE_GENERICINST`).
    GenericInst {
        /// The generic definition, a [`SigType::Class`] or [`SigType::ValueType`] naming `C\`n`.
        definition: Box<SigType>,
        /// The type arguments, in order.
        arguments: Vec<SigType>,
    },
}

/// Reads a `TypeDefOrRef` token compressed into a signature (II.23.2.8): a
/// compressed integer whose low two bits are the tag (TypeDef/TypeRef/TypeSpec).
fn read_type_def_or_ref(reader: &mut Reader) -> Result<Token, SigError> {
    use crate::tables::table;
    let coded = reader.read_compressed_u32()?;
    let table = match coded & 0x03 {
        0 => table::TYPE_DEF,
        1 => table::TYPE_REF,
        _ => table::TYPE_SPEC,
    };
    Ok(Token::new(table, coded >> 2))
}

/// Reads one type signature from `reader`, collecting any REQUIRED custom modifiers into
/// `required` rather than dropping them.
///
/// **THE MODIFIERS ARE COLLECTED, NOT MODELLED, AND THAT IS DELIBERATE.** A `SigType::Modified`
/// variant would be the faithful shape and would break 923 match sites across the compiler, the
/// loader and the AOT lane -- nearly all of which want the type UNDER the modifier and would
/// unwrap it again. A caller that cares about a modifier asks for it here; every other caller uses
/// [`read_type`] and is unaffected.
///
/// The list is FLAT and unpositioned: a modifier nested inside an array element or a generic
/// argument is appended to the same list with nothing to say where it came from, so this answers
/// "which required modifiers appear anywhere in this type" and not "where".
fn read_type_collecting_required(
    reader: &mut Reader,
    required: &mut Vec<Token>,
) -> Result<SigType, SigError> {
    read_type_inner(reader, Some(required))
}

/// Reads one type signature from `reader`.
pub fn read_type(reader: &mut Reader) -> Result<SigType, SigError> {
    read_type_inner(reader, None)
}

fn read_type_inner(
    reader: &mut Reader,
    mut required: Option<&mut Vec<Token>>,
) -> Result<SigType, SigError> {
    loop {
        let element = reader.read_u8()?;
        return Ok(match element {
            element::VOID => SigType::Void,
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
            element::I => SigType::IntPtr,
            element::U => SigType::UIntPtr,
            element::TYPEDBYREF => SigType::TypedByRef,
            element::CLASS => SigType::Class(read_type_def_or_ref(reader)?),
            element::VALUETYPE => SigType::ValueType(read_type_def_or_ref(reader)?),
            element::SZARRAY => SigType::SzArray(Box::new(read_type(reader)?)),
            element::PTR => SigType::Pointer(Box::new(read_type(reader)?)),
            element::BYREF => SigType::ByRef(Box::new(read_type(reader)?)),
            element::ARRAY => {
                let inner = read_type(reader)?;
                let rank = reader.read_compressed_u32()?;
                let sizes = reader.read_compressed_u32()?;
                for _ in 0..sizes {
                    reader.read_compressed_u32()?;
                }
                let bounds = reader.read_compressed_u32()?;
                for _ in 0..bounds {
                    reader.read_compressed_u32()?;
                }
                SigType::Array {
                    element: Box::new(inner),
                    rank,
                }
            }
            element::CMOD_REQD => {
                let modifier = read_type_def_or_ref(reader)?;
                if let Some(collected) = required.as_deref_mut() {
                    collected.push(modifier);
                }
                continue;
            }
            element::CMOD_OPT => {
                read_type_def_or_ref(reader)?;
                continue;
            }
            element::VAR => SigType::Var(reader.read_compressed_u32()?),
            element::MVAR => SigType::MVar(reader.read_compressed_u32()?),
            element::GENERICINST => {
                let definition = Box::new(read_type(reader)?);
                let count = reader.read_compressed_u32()?;
                let mut arguments = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    arguments.push(read_type(reader)?);
                }
                SigType::GenericInst {
                    definition,
                    arguments,
                }
            }
            other => return Err(SigError::BadElementType(other)),
        });
    }
}

/// Decodes a standalone type-signature blob.
pub fn parse_type(blob: &[u8]) -> Result<SigType, SigError> {
    read_type(&mut Reader::new(blob))
}

/// Decodes a field-signature blob (II.23.2.4): the FIELD byte then the type.
pub fn parse_field(blob: &[u8]) -> Result<SigType, SigError> {
    Ok(parse_field_with_modifiers(blob)?.0)
}

/// [`parse_field`], and the field's REQUIRED custom modifiers beside the type.
///
/// A field carries them too -- `modreq(System.Runtime.CompilerServices.IsVolatile)` is how a
/// `volatile` field is spelled -- so a consumer deciding whether it may use a member has to ask a
/// field the same question it asks a method, from the same decoder rather than a second one.
pub fn parse_field_with_modifiers(blob: &[u8]) -> Result<(SigType, Vec<Token>), SigError> {
    let mut reader = Reader::new(blob);
    let convention = reader.read_u8()?;
    if convention != calling::FIELD {
        return Err(SigError::BadCallingConvention(convention));
    }
    let mut required = Vec::new();
    let sig = read_type_collecting_required(&mut reader, &mut required)?;
    Ok((sig, required))
}

/// Decodes a method-signature blob (II.23.2.1): the calling convention, the
/// parameter count, the return type, then the parameter types. A vararg sentinel
/// between fixed and vararg parameters is skipped.
pub fn parse_method(blob: &[u8]) -> Result<MethodSig, SigError> {
    let mut reader = Reader::new(blob);
    let convention = reader.read_u8()?;
    let has_this = convention & calling::HAS_THIS != 0;
    let explicit_this = convention & calling::EXPLICIT_THIS != 0;
    let is_vararg = convention & calling::CONVENTION_MASK == calling::VARARG;
    let generic_param_count = if convention & calling::GENERIC != 0 {
        reader.read_compressed_u32()?
    } else {
        0
    };
    let param_count = reader.read_compressed_u32()?;
    let mut return_type_required_modifiers = Vec::new();
    let return_type =
        read_type_collecting_required(&mut reader, &mut return_type_required_modifiers)?;
    let mut required_modifiers = return_type_required_modifiers.clone();
    let mut parameters = Vec::new();
    let mut sentinel_index = None;
    while (parameters.len() as u32) < param_count {
        if reader.peek_u8()? == calling::SENTINEL {
            reader.read_u8()?;
            sentinel_index = Some(parameters.len());
        }
        parameters.push(read_type_collecting_required(
            &mut reader,
            &mut required_modifiers,
        )?);
    }
    Ok(MethodSig {
        has_this,
        explicit_this,
        return_type,
        parameters,
        is_vararg,
        sentinel_index,
        generic_param_count,
        return_type_required_modifiers,
        required_modifiers,
    })
}

/// Decodes a `MethodSpec` instantiation blob (II.23.2.15): the `GENERICINST` calling convention,
/// the argument count, then that many type signatures -- the type arguments of one generic-method
/// call site, in declaration order.
///
/// This is the last signature shape in the format that had no decoder here, which left its only
/// consumer walking the blob itself and finding each argument's boundary by the shortest prefix
/// that parses. That works and is a workaround; one decoder per format is the rule.
///
/// The leading byte is [`calling::GENERICINST`] (`0x0A`) and NOT `element::GENERICINST` (`0x15`).
/// A reader that checks the wrong one rejects every real blob; one that checks NEITHER and reads
/// straight on takes `0x0A` for `ELEMENT_TYPE_I8` and decodes a `long`, with no error.
pub fn parse_method_spec(blob: &[u8]) -> Result<Vec<SigType>, SigError> {
    let mut reader = Reader::new(blob);
    let convention = reader.read_u8()?;
    if convention != calling::GENERICINST {
        return Err(SigError::BadCallingConvention(convention));
    }
    let count = reader.read_compressed_u32()?;
    let mut arguments = Vec::with_capacity(count as usize);
    while (arguments.len() as u32) < count {
        arguments.push(read_type(&mut reader)?);
    }
    Ok(arguments)
}

/// One local-variable slot as its signature declares it (II.23.2.6): the slot's type, and
/// whether the slot carries the `pinned` constraint.
///
/// `pinned` is not a property of the TYPE -- it is a promise the slot makes to the garbage
/// collector: *while this slot is live, do not move the object it references*. C#
/// `fixed (int* p = arr)` compiles to exactly this, because an unmanaged pointer
/// (`ELEMENT_TYPE_PTR`) is NOT reported to the collector -- so the only thing that can keep
/// `p` valid across a collection is the array not moving. A consumer that hands raw addresses
/// to a MOVING collector and drops this flag produces a dangling interior pointer with no
/// error anywhere; a non-moving collector is unaffected, which is what makes the omission easy
/// to miss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalVar {
    /// The slot's type.
    pub ty: SigType,
    /// Whether the slot is `pinned` (`ELEMENT_TYPE_PINNED`, 0x45): the collector must not
    /// relocate the object this slot references for as long as the slot is live.
    pub pinned: bool,
}

/// Decodes a local-variable signature blob (II.23.2.6) COMPLETE: the LOCAL_SIG byte, the
/// count, then each local's `pinned` constraint (when present) and its type. A by-ref local
/// decodes through [`read_type`]'s `BYREF` handling.
///
/// This is the full form; [`parse_local_var_sig`] is the types-only view over it. Use this one
/// wherever the answer reaches a collector.
pub fn parse_local_vars(blob: &[u8]) -> Result<Vec<LocalVar>, SigError> {
    let mut reader = Reader::new(blob);
    let convention = reader.read_u8()?;
    if convention != calling::LOCAL_SIG {
        return Err(SigError::BadCallingConvention(convention));
    }
    let count = reader.read_compressed_u32()?;
    let mut locals = Vec::with_capacity(count as usize);
    while (locals.len() as u32) < count {
        let mut pinned = false;
        while reader.peek_u8()? == element::PINNED {
            reader.read_u8()?;
            pinned = true;
        }
        locals.push(LocalVar {
            ty: read_type(&mut reader)?,
            pinned,
        });
    }
    Ok(locals)
}

/// The local-variable TYPES only, by slot index -- [`parse_local_vars`] with the `pinned`
/// constraint dropped.
///
/// Dropping it is right for a consumer that only types its slots (a `pinned int32[]` slot holds
/// an `int32[]`), and it is enough for an interpreter whose managed pointers carry their base
/// object -- such a pointer relocates with the object, so nothing has to be held still. It is
/// NOT enough for anything that derives a raw address and reports roots to a moving collector:
/// see [`LocalVar::pinned`] and read the signature through [`parse_local_vars`] there.
pub fn parse_local_var_sig(blob: &[u8]) -> Result<Vec<SigType>, SigError> {
    Ok(parse_local_vars(blob)?
        .into_iter()
        .map(|local| local.ty)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::table;

    #[test]
    fn primitive_types() {
        assert_eq!(parse_type(&[element::I4]), Ok(SigType::I4));
        assert_eq!(parse_type(&[element::STRING]), Ok(SigType::String));
        assert_eq!(parse_type(&[element::OBJECT]), Ok(SigType::Object));
        assert_eq!(parse_type(&[element::BOOLEAN]), Ok(SigType::Boolean));
    }

    #[test]
    fn local_var_sig_decodes_each_local() {
        let blob = [
            calling::LOCAL_SIG,
            0x03,
            element::I4,
            element::R8,
            element::STRING,
        ];
        assert_eq!(
            parse_local_var_sig(&blob),
            Ok(alloc::vec![SigType::I4, SigType::R8, SigType::String])
        );
    }

    /// The blob a C# `fixed (int* p = arr)` produces for its holder slot: `pinned int32[]`,
    /// beside an ordinary local.
    const PINNED_ARRAY_LOCALS: &[u8] = &[
        calling::LOCAL_SIG,
        0x03,
        element::PINNED,
        element::SZARRAY,
        element::I4,
        element::I4,
        element::BYREF,
        element::R8,
    ];

    /// The types-only view drops the constraint, so a `pinned int32[]` slot reads as an
    /// `int32[]` -- correct for typing a slot, and the reason this omission was invisible.
    #[test]
    fn local_var_sig_reads_the_types_and_drops_the_pin() {
        assert_eq!(
            parse_local_var_sig(PINNED_ARRAY_LOCALS),
            Ok(alloc::vec![
                SigType::SzArray(alloc::boxed::Box::new(SigType::I4)),
                SigType::I4,
                SigType::ByRef(alloc::boxed::Box::new(SigType::R8))
            ])
        );
    }

    /// **The defect is stated here, not described elsewhere.** Slot 0 is the `fixed` holder and
    /// its `pinned` constraint must REACH the caller; the other two must not acquire one. The
    /// first assertion is what the old reader could not satisfy -- it consumed the 0x45 byte and
    /// reported nothing -- and the last two are what stops a fix from pinning everything.
    #[test]
    fn a_pinned_local_reports_its_constraint_and_its_neighbours_do_not() {
        let locals = parse_local_vars(PINNED_ARRAY_LOCALS).expect("the blob decodes");
        assert_eq!(locals.len(), 3);
        assert!(locals[0].pinned, "the `fixed` holder slot is pinned");
        assert_eq!(
            locals[0].ty,
            SigType::SzArray(alloc::boxed::Box::new(SigType::I4))
        );
        assert!(!locals[1].pinned, "a plain local is not pinned");
        assert!(!locals[2].pinned, "a by-ref local is not pinned");
        assert_eq!(
            parse_local_var_sig(PINNED_ARRAY_LOCALS).unwrap(),
            locals.iter().map(|l| l.ty.clone()).collect::<alloc::vec::Vec<_>>()
        );
    }

    /// A slot may be pinned AND by-ref (`pinned int32&`, what pinning a struct field's address
    /// looks like), and the constraint must survive the by-ref decode rather than be eaten by it.
    #[test]
    fn a_pinned_byref_local_keeps_both_facts() {
        let blob = [
            calling::LOCAL_SIG,
            0x01,
            element::PINNED,
            element::BYREF,
            element::I4,
        ];
        let locals = parse_local_vars(&blob).expect("the blob decodes");
        assert!(locals[0].pinned);
        assert_eq!(
            locals[0].ty,
            SigType::ByRef(alloc::boxed::Box::new(SigType::I4))
        );
    }

    #[test]
    fn local_var_sig_rejects_a_wrong_convention() {
        assert!(parse_local_var_sig(&[calling::FIELD, element::I4]).is_err());
    }

    #[test]
    fn arrays_pointers_and_byref_nest() {
        assert_eq!(
            parse_type(&[element::SZARRAY, element::I4]),
            Ok(SigType::SzArray(Box::new(SigType::I4)))
        );
        assert_eq!(
            parse_type(&[element::BYREF, element::STRING]),
            Ok(SigType::ByRef(Box::new(SigType::String)))
        );
        assert_eq!(
            parse_type(&[element::SZARRAY, element::SZARRAY, element::I4]),
            Ok(SigType::SzArray(Box::new(SigType::SzArray(Box::new(
                SigType::I4
            )))))
        );
    }

    #[test]
    fn class_and_value_type_carry_a_token() {
        let sig = parse_type(&[element::CLASS, 0x0D]).unwrap();
        let SigType::Class(token) = sig else {
            panic!("expected a class type");
        };
        assert_eq!(token.table(), table::TYPE_REF);
        assert_eq!(token.row(), 3);
    }

    #[test]
    fn multidim_array_keeps_its_rank() {
        let sig = parse_type(&[element::ARRAY, element::I4, 0x02, 0x00, 0x00]).unwrap();
        assert_eq!(
            sig,
            SigType::Array {
                element: Box::new(SigType::I4),
                rank: 2
            }
        );
    }

    /// **THE ENCODER AND ITS INVERSE MUST AGREE ON EVERY BYTE THEY BOTH CLAIM**, and nothing in
    /// the type system connects them -- they are two `match`es over the same alphabet, written apart.
    ///
    /// The round trip is asserted in both directions on purpose. `payload_free_sig(element_byte(t))
    /// == t` catches a byte the inverse maps to the WRONG type; the second half catches a byte the
    /// inverse claims and should not, which is the dangerous direction: answering `Some` for
    /// `CLASS` or `GENERICINST` would hand a caller a type with the token or the arguments simply
    /// missing, and that type would then be laid out and dispatched as though it were complete.
    #[test]
    fn the_payload_free_inverse_round_trips_and_claims_nothing_more() {
        let payload_free = [
            SigType::Void,
            SigType::Boolean,
            SigType::Char,
            SigType::I1,
            SigType::U1,
            SigType::I2,
            SigType::U2,
            SigType::I4,
            SigType::U4,
            SigType::I8,
            SigType::U8,
            SigType::R4,
            SigType::R8,
            SigType::String,
            SigType::TypedByRef,
            SigType::IntPtr,
            SigType::UIntPtr,
            SigType::Object,
        ];
        for ty in &payload_free {
            assert_eq!(
                payload_free_sig(element_byte(ty)).as_ref(),
                Some(ty),
                "{ty:?} must survive element_byte -> payload_free_sig unchanged"
            );
        }

        let carries_more = [
            SigType::Class(Token::new(0x02, 1)),
            SigType::ValueType(Token::new(0x02, 1)),
            SigType::SzArray(Box::new(SigType::I4)),
            SigType::Pointer(Box::new(SigType::I4)),
            SigType::ByRef(Box::new(SigType::I4)),
            SigType::Array {
                element: Box::new(SigType::I4),
                rank: 2,
            },
            SigType::Var(0),
            SigType::MVar(0),
            SigType::GenericInst {
                definition: Box::new(SigType::Class(Token::new(0x02, 1))),
                arguments: alloc::vec![SigType::I4],
            },
        ];
        for ty in &carries_more {
            assert_eq!(
                payload_free_sig(element_byte(ty)),
                None,
                "{ty:?} carries more than its byte, so the inverse must refuse it rather than \
                 answer with the payload missing"
            );
        }
    }

    #[test]
    fn an_unknown_element_type_errors() {
        assert_eq!(parse_type(&[0x77]), Err(SigError::BadElementType(0x77)));
        assert_eq!(parse_type(&[]), Err(SigError::Truncated));
    }

    #[test]
    fn a_generic_method_signature_is_decoded_not_misread() {
        let generic = [
            calling::GENERIC | calling::HAS_THIS,
            0x01,
            0x02,
            element::I4,
            element::I4,
            element::I4,
        ];
        let decoded = parse_method(&generic).expect("a generic signature decodes");
        assert_eq!(decoded.generic_param_count, 1);
        assert_eq!(decoded.parameters.len(), 2, "GenParamCount must not be read as ParamCount");
        assert_eq!(decoded.return_type, SigType::I4);
        assert!(decoded.has_this);

        let mut as_default = generic;
        as_default[0] &= !calling::GENERIC;
        let misread = parse_method(&as_default).expect("the shifted read succeeds -- that is why");
        assert_eq!(misread.parameters.len(), 1, "truth is 2");
        assert_ne!(misread.return_type, SigType::I4, "truth is I4");

        let nibble_default =
            parse_method(&[calling::GENERIC, 0x01, 0x00, element::I4]).expect("decodes");
        assert_eq!(nibble_default.generic_param_count, 1);
        assert_eq!(nibble_default.parameters.len(), 0);
        assert_eq!(nibble_default.return_type, SigType::I4);

        let ordinary = parse_method(&[calling::HAS_THIS, 0x00, element::VOID]).expect("decodes");
        assert_eq!(ordinary.generic_param_count, 0);
        assert!(parse_method(&[calling::HAS_THIS, 0x00, element::VOID]).is_ok());
    }

    #[test]
    fn a_method_spec_instantiation_decodes_its_arguments() {
        assert_eq!(
            parse_method_spec(&[calling::GENERICINST, 0x02, element::I4, element::STRING]),
            Ok(alloc::vec![SigType::I4, SigType::String])
        );

        let decoded = parse_method_spec(&[
            calling::GENERICINST,
            0x01,
            element::GENERICINST,
            element::CLASS,
            0x0D,
            0x01,
            element::U1,
        ])
        .expect("a nested instantiation decodes");
        let [SigType::GenericInst {
            definition,
            arguments,
        }] = &decoded[..]
        else {
            panic!("expected one instantiation argument, got {decoded:?}");
        };
        assert_eq!(**definition, SigType::Class(Token::new(table::TYPE_REF, 3)));
        assert_eq!(arguments[..], [SigType::U1]);

        assert_eq!(
            parse_method_spec(&[element::GENERICINST, 0x01, element::I4]),
            Err(SigError::BadCallingConvention(element::GENERICINST))
        );
        assert_eq!(
            parse_method_spec(&[element::I4, 0x01, element::I4]),
            Err(SigError::BadCallingConvention(element::I4))
        );
        assert_eq!(parse_method_spec(&[]), Err(SigError::Truncated));
    }

    #[test]
    fn field_signature() {
        assert_eq!(parse_field(&[calling::FIELD, element::I4]), Ok(SigType::I4));
        assert_eq!(
            parse_field(&[0x00, element::I4]),
            Err(SigError::BadCallingConvention(0x00))
        );
    }

    #[test]
    fn instance_method_signature() {
        let sig = parse_method(&[
            calling::HAS_THIS,
            0x02,
            element::I4,
            element::STRING,
            element::BOOLEAN,
        ])
        .unwrap();
        assert!(sig.has_this);
        assert_eq!(sig.return_type, SigType::I4);
        assert_eq!(sig.parameters, [SigType::String, SigType::Boolean]);
    }

    #[test]
    fn static_void_no_arg_signature() {
        let sig = parse_method(&[0x00, 0x00, element::VOID]).unwrap();
        assert!(!sig.has_this);
        assert_eq!(sig.return_type, SigType::Void);
        assert!(sig.parameters.is_empty());
    }
}
