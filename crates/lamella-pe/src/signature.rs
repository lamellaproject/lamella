//! Encoding the signature blobs that metadata rows reference (II.23.2).

use crate::heap::compress_u32;
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use lamella_metadata::CodedIndex;
use lamella_metadata::signature::{calling, element};
use lamella_token::Token;

/// The leading byte of a `DEFAULT` (non-instance) method signature (II.23.2.1).
const DEFAULT: u8 = 0x00;
/// The leading byte of a local-variable signature (II.23.2.6).
const LOCAL_SIG: u8 = 0x07;
/// The leading byte of a property signature (II.23.2.5).
const PROPERTY: u8 = 0x08;

/// A type as it appears in a signature blob (II.23.2.12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeSig {
    /// `void` (only valid as a method return).
    Void,
    /// `bool`.
    Boolean,
    /// `char`.
    Char,
    /// `sbyte` / `byte`.
    SByte,
    /// `byte`.
    Byte,
    /// `short` / `ushort`.
    Int16,
    /// `ushort`.
    UInt16,
    /// `int`.
    Int32,
    /// `uint`.
    UInt32,
    /// `long`.
    Int64,
    /// `ulong`.
    UInt64,
    /// `float`.
    Single,
    /// `double`.
    Double,
    /// `string`.
    String,
    /// `object`.
    Object,
    /// `native int` (e.g. the method pointer in a delegate's constructor, or `System.IntPtr`).
    NativeInt,
    /// `native uint` (`System.UIntPtr`).
    NativeUInt,
    /// A reference type, carrying the token of its `TypeDef`/`TypeRef`.
    Class(Token),
    /// A value type, carrying the token of its `TypeDef`/`TypeRef`.
    ValueType(Token),
    /// A single-dimension zero-based array of the element type.
    SzArray(Box<TypeSig>),
    /// A managed reference (`&`) to the referent type -- a `ref`/`out` parameter.
    ByRef(Box<TypeSig>),
    /// `System.TypedReference` (II.23.1.16): the special typed-reference element, carried
    /// inline with no token -- the type a `__makeref` result / a typed-reference local takes.
    TypedByRef,
    /// An unmanaged pointer (`*`) to the pointee type -- an unsafe `T*` (II.23.2.12).
    Pointer(Box<TypeSig>),
    /// A `pinned` local (II.23.2.9): the GC must not move its referent. Only valid as a
    /// local-variable type (a `fixed` array holder).
    Pinned(Box<TypeSig>),
    /// A multi-dimensional (rectangular) array of the element type with the given
    /// rank, zero-based with no fixed sizes -- the form a `T[,]` TypeSpec takes.
    Array {
        /// The element type.
        element: Box<TypeSig>,
        /// The number of dimensions (>= 2 for a rectangular array).
        rank: u32,
    },
    /// A generic parameter of the enclosing TYPE, by its zero-based number: `!0` is the `T` of
    /// `Box<T>` (ECMA-335 4th ed, II.23.1.16 `ELEMENT_TYPE_VAR`).
    Var(u32),
    /// A generic parameter of the METHOD itself, by its zero-based number: `!!0` is the `T` of
    /// `T Identity<T>(T)` (`ELEMENT_TYPE_MVAR`).
    ///
    /// **Distinct from [`TypeSig::Var`] and NOT interchangeable with it.** A generic method inside
    /// a generic type has both numbering spaces live at once, and `!0` and `!!0` are different
    /// types there. Encoding one as the other produces a signature that decodes without error and
    /// means something else.
    MVar(u32),
    /// A type carrying a REQUIRED custom modifier -- `modreq(M) T` (II.23.2.7).
    ///
    /// **THE MODIFIER PRECEDES THE TYPE IT MODIFIES IN THE BLOB**, which is why this wraps rather
    /// than decorates: `void modreq(IsExternalInit)` encodes as `CMOD_REQD <M> VOID`, and a reader
    /// takes the modifier first and then reads a type.
    ///
    /// **REQUIRED, NOT OPTIONAL, AND THE DIFFERENCE IS THE WHOLE POINT OF THE ONE USE THIS HAS.** A
    /// `modopt` may be ignored by a consumer; a `modreq` may not, so a compiler that does not
    /// understand `IsExternalInit` must refuse to bind the accessor rather than treat it as an
    /// ordinary setter. That is what makes an init-only setter safe to expose to a compiler
    /// predating C# 9 -- and what makes DROPPING one on import an accepts-invalid rather than a
    /// cosmetic loss.
    Modified {
        /// The `TypeDef`/`TypeRef` token of the modifier type.
        modifier: Token,
        /// The type being modified.
        inner: Box<TypeSig>,
    },
    /// An instantiation of a generic type: `Box<int>`, or `Box<!0>` inside another generic
    /// (`ELEMENT_TYPE_GENERICINST`).
    GenericInst {
        /// The generic type definition being instantiated -- a [`TypeSig::Class`] or
        /// [`TypeSig::ValueType`] naming the `C\`n` TypeDef/TypeRef.
        definition: Box<TypeSig>,
        /// The type arguments, in order. Its length must equal the definition's arity: the count is
        /// written into the blob, and a consumer reads that many types back.
        arguments: Vec<TypeSig>,
    },
}

fn encode_type(sig: &TypeSig, out: &mut Vec<u8>) {
    match sig {
        TypeSig::Void => out.push(element::VOID),
        TypeSig::Boolean => out.push(element::BOOLEAN),
        TypeSig::Char => out.push(element::CHAR),
        TypeSig::SByte => out.push(element::I1),
        TypeSig::Byte => out.push(element::U1),
        TypeSig::Int16 => out.push(element::I2),
        TypeSig::UInt16 => out.push(element::U2),
        TypeSig::Int32 => out.push(element::I4),
        TypeSig::UInt32 => out.push(element::U4),
        TypeSig::Int64 => out.push(element::I8),
        TypeSig::UInt64 => out.push(element::U8),
        TypeSig::Single => out.push(element::R4),
        TypeSig::Double => out.push(element::R8),
        TypeSig::String => out.push(element::STRING),
        TypeSig::Object => out.push(element::OBJECT),
        TypeSig::NativeInt => out.push(element::I),
        TypeSig::NativeUInt => out.push(element::U),
        TypeSig::Class(token) => {
            out.push(element::CLASS);
            compress_u32(CodedIndex::TypeDefOrRef.encode(*token), out);
        }
        TypeSig::ValueType(token) => {
            out.push(element::VALUETYPE);
            compress_u32(CodedIndex::TypeDefOrRef.encode(*token), out);
        }
        TypeSig::SzArray(elem) => {
            out.push(element::SZARRAY);
            encode_type(elem, out);
        }
        TypeSig::ByRef(referent) => {
            out.push(element::BYREF);
            encode_type(referent, out);
        }
        TypeSig::TypedByRef => out.push(element::TYPEDBYREF),
        TypeSig::Pointer(pointee) => {
            out.push(element::PTR);
            encode_type(pointee, out);
        }
        TypeSig::Pinned(referent) => {
            out.push(element::PINNED);
            encode_type(referent, out);
        }
        TypeSig::Array { element: elem, rank } => {
            out.push(element::ARRAY);
            encode_type(elem, out);
            compress_u32(*rank, out);
            compress_u32(0, out);
            compress_u32(0, out);
        }
        TypeSig::Var(number) => {
            out.push(element::VAR);
            compress_u32(*number, out);
        }
        TypeSig::MVar(number) => {
            out.push(element::MVAR);
            compress_u32(*number, out);
        }
        TypeSig::Modified { modifier, inner } => {
            out.push(element::CMOD_REQD);
            compress_u32(CodedIndex::TypeDefOrRef.encode(*modifier), out);
            encode_type(inner, out);
        }
        TypeSig::GenericInst {
            definition,
            arguments,
        } => {
            out.push(element::GENERICINST);
            encode_type(definition, out);
            compress_u32(arguments.len() as u32, out);
            for argument in arguments {
                encode_type(argument, out);
            }
        }
    }
}

/// Encodes a standalone type signature.
#[must_use]
pub fn type_signature(sig: &TypeSig) -> Vec<u8> {
    let mut out = Vec::new();
    encode_type(sig, &mut out);
    out
}

/// Encodes a method signature: convention, parameter count, return type, then the
/// parameter types (II.23.2.1).
#[must_use]
pub fn method_signature(has_this: bool, parameters: &[TypeSig], return_type: &TypeSig) -> Vec<u8> {
    let mut out = vec![if has_this {
        DEFAULT | calling::HAS_THIS
    } else {
        DEFAULT
    }];
    compress_u32(parameters.len() as u32, &mut out);
    encode_type(return_type, &mut out);
    for parameter in parameters {
        encode_type(parameter, &mut out);
    }
    out
}

/// Encodes a GENERIC method DEF signature (II.23.2.1): the `GENERIC` convention bit, then
/// `GenParamCount` BEFORE `ParamCount`, then the return and parameter types.
///
/// `generic_parameters` is how many type parameters the METHOD declares -- the `1` of
/// `T Identity<T>(T)`. Its types are referred to as [`TypeSig::MVar`], numbered from zero.
///
/// **`GenParamCount` IS BINDING-SIGNIFICANT, NOT BOOKKEEPING.** II.23.2.1 says both `MethodDef`
/// and `MemberRef` shall carry the GENERIC convention together with the count, because it is what
/// lets the runtime **overload generic methods by their number of type parameters** -- `M<T>()` and
/// `M<T,U>()` are different methods and this field is the difference.
///
/// **AND THE `GenericParam` ROW DOES NOT MAKE A METHOD GENERIC -- THIS CONVENTION DOES. MEASURED,
/// NOT INFERRED FROM THE PAGE:** emitting the row while writing an ordinary signature makes csc
/// answer **CS0308, "the non-generic method cannot be used with type arguments"**, at a call site
/// that otherwise compiles. The two must agree and the SIGNATURE wins, so a writer that adds the row
/// and forgets the convention produces metadata that contradicts itself.
#[must_use]
pub fn generic_method_signature(
    has_this: bool,
    generic_parameters: u32,
    parameters: &[TypeSig],
    return_type: &TypeSig,
) -> Vec<u8> {
    let mut out = vec![if has_this {
        DEFAULT | calling::GENERIC | calling::HAS_THIS
    } else {
        DEFAULT | calling::GENERIC
    }];
    compress_u32(generic_parameters, &mut out);
    compress_u32(parameters.len() as u32, &mut out);
    encode_type(return_type, &mut out);
    for parameter in parameters {
        encode_type(parameter, &mut out);
    }
    out
}

/// Encodes a `MethodSpec` instantiation blob (II.23.2.15): the generic ARGUMENTS at one call site
/// to a generic method, as `GENERICINST GenArgCount Type*`.
///
/// **THE LEADING BYTE IS `0x0A`, NOT `ELEMENT_TYPE_GENERICINST` (0x15), AND THE TWO ARE EASY TO
/// CONFUSE BECAUSE THEY SHARE A NAME.** II.23.2.15's `GENERICINST` is a CALLING CONVENTION value
/// (`IMAGE_CEE_CS_CALLCONV_GENERICINST`); the 0x15 of the same name is an ELEMENT TYPE used inside a
/// type signature. They live in different namespaces and appear in different positions. Writing 0x15
/// here produces a blob a reader will not recognize as an instantiation at all.
#[must_use]
pub fn method_spec_signature(arguments: &[TypeSig]) -> Vec<u8> {
    let mut out = vec![calling::GENERICINST];
    compress_u32(arguments.len() as u32, &mut out);
    for argument in arguments {
        encode_type(argument, &mut out);
    }
    out
}

/// Encodes a vararg method DEF signature (II.23.2.1, `VARARG` convention): only the
/// FIXED parameters appear (no sentinel); the variable arguments live in each call
/// site's signature. csc emits `25 00 01` for `public T(__arglist)`.
#[must_use]
pub fn vararg_method_signature(
    has_this: bool,
    parameters: &[TypeSig],
    return_type: &TypeSig,
) -> Vec<u8> {
    let mut out = vec![if has_this {
        calling::VARARG | calling::HAS_THIS
    } else {
        calling::VARARG
    }];
    compress_u32(parameters.len() as u32, &mut out);
    encode_type(return_type, &mut out);
    for parameter in parameters {
        encode_type(parameter, &mut out);
    }
    out
}

/// Encodes a vararg CALL-SITE signature (II.23.2.1): the `VARARG` convention, the
/// TOTAL parameter count, the return type, the fixed parameter types, a `SENTINEL`,
/// then each variable argument's type. csc emits `25 04 01 41 08 0E 1C 0D` for
/// `new T(__arglist(2, "s", null, 2.2))` (null rides as `object`).
#[must_use]
pub fn vararg_call_site_signature(
    has_this: bool,
    fixed: &[TypeSig],
    variable: &[TypeSig],
    return_type: &TypeSig,
) -> Vec<u8> {
    let mut out = vec![if has_this {
        calling::VARARG | calling::HAS_THIS
    } else {
        calling::VARARG
    }];
    compress_u32((fixed.len() + variable.len()) as u32, &mut out);
    encode_type(return_type, &mut out);
    for parameter in fixed {
        encode_type(parameter, &mut out);
    }
    out.push(calling::SENTINEL);
    for argument in variable {
        encode_type(argument, &mut out);
    }
    out
}

/// Encodes a field signature (II.23.2.4).
#[must_use]
pub fn field_signature(field_type: &TypeSig) -> Vec<u8> {
    let mut out = vec![calling::FIELD];
    encode_type(field_type, &mut out);
    out
}

/// Encodes a property signature (II.23.2.5): the `PROPERTY` convention (with the
/// instance flag), the count of index parameters, the property type, then the index
/// parameter types. An ordinary property passes no index parameters; an indexer (17.8)
/// passes the types of its index list.
#[must_use]
pub fn property_signature(
    has_this: bool,
    index_params: &[TypeSig],
    property_type: &TypeSig,
) -> Vec<u8> {
    let mut out = vec![if has_this {
        PROPERTY | calling::HAS_THIS
    } else {
        PROPERTY
    }];
    compress_u32(index_params.len() as u32, &mut out);
    encode_type(property_type, &mut out);
    for parameter in index_params {
        encode_type(parameter, &mut out);
    }
    out
}

/// Encodes a local-variable signature: the locals of a method body, in slot order
/// (II.23.2.6).
#[must_use]
pub fn local_signature(locals: &[TypeSig]) -> Vec<u8> {
    let mut out = vec![LOCAL_SIG];
    compress_u32(locals.len() as u32, &mut out);
    for local in locals {
        encode_type(local, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitive_and_array_types_encode_to_their_element_bytes() {
        assert_eq!(type_signature(&TypeSig::Int32), [0x08]);
        assert_eq!(type_signature(&TypeSig::String), [0x0E]);
        assert_eq!(type_signature(&TypeSig::Object), [0x1C]);
        assert_eq!(
            type_signature(&TypeSig::SzArray(Box::new(TypeSig::Int32))),
            [0x1D, 0x08]
        );
        assert_eq!(
            type_signature(&TypeSig::Class(Token::new(0x02, 18))),
            [0x12, 0x48]
        );
    }

    /// An init-only setter is `void modreq(IsExternalInit) set_P(int)`, and its whole distinction
    /// from an ordinary setter is that modifier -- the rest of the signature is identical.
    ///
    /// **ENCODED AND DECODED IN ONE TEST, BECAUSE EITHER HALF ALONE PROVES NOTHING USEFUL.** A
    /// writer that emits the bytes and a reader that drops them agree that nothing is wrong, and
    /// the two compilers only disagree later, over an assignment nobody in this crate can see. The
    /// round trip is what ties them together.
    #[test]
    fn a_required_modifier_survives_the_round_trip() {
        let is_external_init = Token::new(0x01, 7);
        let sig = method_signature(
            true,
            &[TypeSig::Int32],
            &TypeSig::Modified {
                modifier: is_external_init,
                inner: Box::new(TypeSig::Void),
            },
        );
        assert_eq!(sig, [0x20, 0x01, 0x1F, 0x1D, 0x01, 0x08]);
        let decoded = lamella_metadata::parse_method(&sig).expect("a well-formed signature");
        assert_eq!(decoded.return_type, lamella_metadata::SigType::Void);
        assert_eq!(decoded.parameters, [lamella_metadata::SigType::I4]);
        assert_eq!(decoded.return_type_required_modifiers, [is_external_init]);
        let plain = method_signature(true, &[TypeSig::Int32], &TypeSig::Void);
        let decoded_plain = lamella_metadata::parse_method(&plain).expect("a well-formed signature");
        assert_eq!(decoded_plain.return_type, lamella_metadata::SigType::Void);
        assert!(decoded_plain.return_type_required_modifiers.is_empty());
    }

    #[test]
    fn method_signatures_carry_convention_count_and_types() {
        assert_eq!(
            method_signature(false, &[TypeSig::Int32], &TypeSig::Void),
            [0x00, 0x01, 0x01, 0x08]
        );
        assert_eq!(
            method_signature(true, &[], &TypeSig::Int32),
            [0x20, 0x00, 0x08]
        );
    }

    #[test]
    fn field_and_local_signatures() {
        assert_eq!(field_signature(&TypeSig::String), [0x06, 0x0E]);
        assert_eq!(
            local_signature(&[TypeSig::Int32, TypeSig::Boolean]),
            [0x07, 0x02, 0x08, 0x02]
        );
    }

    #[test]
    fn vararg_signatures_match_the_csc_oracle() {
        assert_eq!(
            vararg_method_signature(true, &[], &TypeSig::Void),
            [0x25, 0x00, 0x01]
        );
        assert_eq!(
            vararg_method_signature(false, &[TypeSig::Int32], &TypeSig::Int32),
            [0x05, 0x01, 0x08, 0x08]
        );
        assert_eq!(
            vararg_call_site_signature(
                true,
                &[],
                &[
                    TypeSig::Int32,
                    TypeSig::String,
                    TypeSig::Object,
                    TypeSig::Double
                ],
                &TypeSig::Void
            ),
            [0x25, 0x04, 0x01, 0x41, 0x08, 0x0E, 0x1C, 0x0D]
        );
    }
}
