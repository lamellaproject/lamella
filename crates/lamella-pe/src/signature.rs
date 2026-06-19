//! Encoding the signature blobs that metadata rows reference (II.23.2).

use crate::heap::compress_u32;
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use lamella_metadata::signature::{calling, element};

/// The leading byte of a `DEFAULT` (non-instance) method signature (II.23.2.1).
const DEFAULT: u8 = 0x00;
/// The leading byte of a local-variable signature (II.23.2.6).
const LOCAL_SIG: u8 = 0x07;

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
    /// A reference type, carrying its `TypeDefOrRef` coded index.
    Class(u32),
    /// A value type, carrying its `TypeDefOrRef` coded index.
    ValueType(u32),
    /// A single-dimension zero-based array of the element type.
    SzArray(Box<TypeSig>),
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
        TypeSig::Class(coded) => {
            out.push(element::CLASS);
            compress_u32(*coded, out);
        }
        TypeSig::ValueType(coded) => {
            out.push(element::VALUETYPE);
            compress_u32(*coded, out);
        }
        TypeSig::SzArray(elem) => {
            out.push(element::SZARRAY);
            encode_type(elem, out);
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

/// Encodes a field signature (II.23.2.4).
#[must_use]
pub fn field_signature(field_type: &TypeSig) -> Vec<u8> {
    let mut out = vec![calling::FIELD];
    encode_type(field_type, &mut out);
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
        assert_eq!(type_signature(&TypeSig::Class(0x49)), [0x12, 0x49]);
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
}
