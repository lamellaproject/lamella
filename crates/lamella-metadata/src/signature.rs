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
    /// The vararg-sentinel element type, separating fixed from vararg parameters.
    pub const SENTINEL: u8 = 0x41;
    /// The leading byte of a local-variable signature (II.23.2.6).
    pub const LOCAL_SIG: u8 = 0x07;
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
    /// A general (multi-dimensional) array; followed by element type and shape.
    pub const ARRAY: u8 = 0x14;
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
    /// A required custom modifier; followed by a `TypeDefOrRef` token.
    pub const CMOD_REQD: u8 = 0x1F;
    /// An optional custom modifier; followed by a `TypeDefOrRef` token.
    pub const CMOD_OPT: u8 = 0x20;
    /// A pinned local-variable constraint, preceding the local's type (II.23.2.6).
    pub const PINNED: u8 = 0x45;
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

/// Reads one type signature from `reader`.
pub fn read_type(reader: &mut Reader) -> Result<SigType, SigError> {
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
            element::CMOD_REQD | element::CMOD_OPT => {
                read_type_def_or_ref(reader)?;
                continue;
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
    let mut reader = Reader::new(blob);
    let convention = reader.read_u8()?;
    if convention != calling::FIELD {
        return Err(SigError::BadCallingConvention(convention));
    }
    read_type(&mut reader)
}

/// Decodes a method-signature blob (II.23.2.1): the calling convention, the
/// parameter count, the return type, then the parameter types. A vararg sentinel
/// between fixed and vararg parameters is skipped.
pub fn parse_method(blob: &[u8]) -> Result<MethodSig, SigError> {
    let mut reader = Reader::new(blob);
    let convention = reader.read_u8()?;
    let has_this = convention & calling::HAS_THIS != 0;
    let explicit_this = convention & calling::EXPLICIT_THIS != 0;
    let param_count = reader.read_compressed_u32()?;
    let return_type = read_type(&mut reader)?;
    let mut parameters = Vec::new();
    while (parameters.len() as u32) < param_count {
        if reader.peek_u8()? == calling::SENTINEL {
            reader.read_u8()?;
        }
        parameters.push(read_type(&mut reader)?);
    }
    Ok(MethodSig {
        has_this,
        explicit_this,
        return_type,
        parameters,
    })
}

/// Decodes a local-variable signature blob (II.23.2.6): the LOCAL_SIG byte, the
/// count, then each local's type. A `pinned` constraint before a local is skipped;
/// a by-ref local decodes through [`read_type`]'s `BYREF` handling.
pub fn parse_local_var_sig(blob: &[u8]) -> Result<Vec<SigType>, SigError> {
    let mut reader = Reader::new(blob);
    let convention = reader.read_u8()?;
    if convention != calling::LOCAL_SIG {
        return Err(SigError::BadCallingConvention(convention));
    }
    let count = reader.read_compressed_u32()?;
    let mut locals = Vec::with_capacity(count as usize);
    while (locals.len() as u32) < count {
        while reader.peek_u8()? == element::PINNED {
            reader.read_u8()?;
        }
        locals.push(read_type(&mut reader)?);
    }
    Ok(locals)
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

    #[test]
    fn local_var_sig_skips_pinned_and_reads_byref() {
        let blob = [
            calling::LOCAL_SIG,
            0x02,
            element::PINNED,
            element::I4,
            element::BYREF,
            element::R8,
        ];
        assert_eq!(
            parse_local_var_sig(&blob),
            Ok(alloc::vec![
                SigType::I4,
                SigType::ByRef(alloc::boxed::Box::new(SigType::R8))
            ])
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

    #[test]
    fn an_unknown_element_type_errors() {
        assert_eq!(parse_type(&[0x77]), Err(SigError::BadElementType(0x77)));
        assert_eq!(parse_type(&[]), Err(SigError::Truncated));
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
