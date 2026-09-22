//! Encoding the marshalling descriptor blob a `FieldMarshal` row points at (II.23.4).

use crate::heap::compress_u32;
use alloc::boxed::Box;
use alloc::vec::Vec;

/// `NATIVE_TYPE_FIXEDSYSSTRING` (II.23.4) -- `UnmanagedType.ByValTStr`.
const FIXED_SYS_STRING: u8 = 0x17;
/// `NATIVE_TYPE_SAFEARRAY` -- `UnmanagedType.SafeArray`.
const SAFE_ARRAY: u8 = 0x1D;
/// `NATIVE_TYPE_FIXEDARRAY` -- `UnmanagedType.ByValArray`.
const FIXED_ARRAY: u8 = 0x1E;
/// `NATIVE_TYPE_ARRAY` -- `UnmanagedType.LPArray`.
const ARRAY: u8 = 0x2A;
/// `NATIVE_TYPE_CUSTOMMARSHALER` -- `UnmanagedType.CustomMarshaler`.
const CUSTOM_MARSHALER: u8 = 0x2C;

/// `NATIVE_TYPE_MAX` -- the "unspecified" element type an `ARRAY` descriptor carries when the
/// source named no `ArraySubType`. It is a real byte in the blob rather than an omission: csc
/// writes `2A 50` for a bare `[MarshalAs(UnmanagedType.LPArray)]`, so an `ARRAY` always has an
/// element type even when the program did not choose one.
pub const NATIVE_TYPE_MAX: u8 = 0x50;

/// A marshalling descriptor (II.23.4), in the shapes C# can ask for.
///
/// The variants are the grammar's productions, not `UnmanagedType`'s members: every member that
/// carries no extra data is one [`MarshalSpec::Simple`] holding its own byte, because
/// `UnmanagedType`'s values ARE the `NATIVE_TYPE_*` values (verified across 17 members against the
/// pinned csc). The four that carry data get a variant each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarshalSpec {
    /// A descriptor that is one `NATIVE_TYPE_*` byte and nothing else -- `Bool` (`0x02`),
    /// `LPStr` (`0x14`), `I4` (`0x07`), `Interface` (`0x1C`) and every other member with no
    /// operand.
    Simple(u8),
    /// `FIXEDSYSSTRING <Size>` -- `[MarshalAs(UnmanagedType.ByValTStr, SizeConst = n)]`, a string
    /// stored inline in the structure rather than as a pointer to one.
    FixedSysString {
        /// The `SizeConst`, in characters.
        size: u32,
    },
    /// `FIXEDARRAY <NumElem> [<ArrayElemType>]` -- `[MarshalAs(UnmanagedType.ByValArray, ...)]`.
    ///
    /// The element type is genuinely optional here, unlike [`MarshalSpec::Array`]'s, which always
    /// occupies its byte.
    FixedArray {
        /// The `SizeConst`: the number of elements stored inline.
        size: u32,
        /// The `ArraySubType`, when the source named one.
        element: Option<u8>,
    },
    /// `ARRAY <ArrayElemType> [<ParamNum> [<NumElem> <ElemMult>]]` --
    /// `[MarshalAs(UnmanagedType.LPArray, ...)]`, a pointer to elements whose count is found at
    /// run time.
    ///
    /// The tail is positional, so a later field cannot be written without the earlier ones; the
    /// caller decides which are present (see the module doc).
    Array {
        /// The `ArraySubType`, or [`NATIVE_TYPE_MAX`] when the source named none.
        element: u8,
        /// The `ParamNum`: which parameter holds the element count. `None` writes no tail at all.
        param_num: Option<u32>,
        /// The `NumElem` and `ElemMult`, written only when `param_num` is present.
        size: Option<(u32, u32)>,
    },
    /// `SAFEARRAY [<VariantType>]` -- `[MarshalAs(UnmanagedType.SafeArray, ...)]`.
    SafeArray {
        /// The `SafeArraySubType` (a `VarEnum`), when the source named one.
        variant: Option<u32>,
    },
    /// `CUSTOMMARSHALER <Guid> <UnmanagedType> <MarshalType> <MarshalCookie>` --
    /// `[MarshalAs(UnmanagedType.CustomMarshaler, MarshalType = "...", MarshalCookie = "...")]`.
    ///
    /// The first two strings are vestigial and csc writes both EMPTY (measured: `2C 00 00 ...`),
    /// so they are not fields here -- naming them would invite a caller to fill them with
    /// something the runtime ignores.
    CustomMarshaler {
        /// The `MarshalType`: the assembly-qualified name of the `ICustomMarshaler`.
        marshal_type: Box<str>,
        /// The `MarshalCookie`: the string handed to that marshaler.
        cookie: Box<str>,
    },
}

impl MarshalSpec {
    /// The descriptor's bytes, as the `#Blob` entry a `FieldMarshal` row names.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut blob = Vec::new();
        match self {
            MarshalSpec::Simple(native) => blob.push(*native),
            MarshalSpec::FixedSysString { size } => {
                blob.push(FIXED_SYS_STRING);
                compress_u32(*size, &mut blob);
            }
            MarshalSpec::FixedArray { size, element } => {
                blob.push(FIXED_ARRAY);
                compress_u32(*size, &mut blob);
                if let Some(element) = element {
                    blob.push(*element);
                }
            }
            MarshalSpec::Array {
                element,
                param_num,
                size,
            } => {
                blob.push(ARRAY);
                blob.push(*element);
                if let Some(param_num) = param_num {
                    compress_u32(*param_num, &mut blob);
                    if let Some((num_elem, elem_mult)) = size {
                        compress_u32(*num_elem, &mut blob);
                        compress_u32(*elem_mult, &mut blob);
                    }
                }
            }
            MarshalSpec::SafeArray { variant } => {
                blob.push(SAFE_ARRAY);
                if let Some(variant) = variant {
                    compress_u32(*variant, &mut blob);
                }
            }
            MarshalSpec::CustomMarshaler {
                marshal_type,
                cookie,
            } => {
                blob.push(CUSTOM_MARSHALER);
                blob.push(0x00);
                blob.push(0x00);
                push_ser_string(marshal_type, &mut blob);
                push_ser_string(cookie, &mut blob);
            }
        }
        blob
    }
}

/// Appends a `SerString` (II.23.3): a compressed byte count then the UTF-8 bytes.
fn push_ser_string(text: &str, out: &mut Vec<u8>) {
    compress_u32(text.len() as u32, out);
    out.extend_from_slice(text.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;


    #[test]
    fn a_simple_descriptor_is_its_own_native_type_byte() {
        assert_eq!(MarshalSpec::Simple(0x02).encode(), vec![0x02]);
        assert_eq!(MarshalSpec::Simple(0x14).encode(), vec![0x14]);
        assert_eq!(MarshalSpec::Simple(0x15).encode(), vec![0x15]);
        assert_eq!(MarshalSpec::Simple(0x08).encode(), vec![0x08]);
        assert_eq!(MarshalSpec::Simple(0x03).encode(), vec![0x03]);
        assert_eq!(MarshalSpec::Simple(0x1F).encode(), vec![0x1F]);
        assert_eq!(MarshalSpec::Simple(0x1C).encode(), vec![0x1C]);
    }

    #[test]
    fn a_by_val_tstr_carries_its_size_compressed() {
        assert_eq!(
            MarshalSpec::FixedSysString { size: 4 }.encode(),
            vec![0x17, 0x04]
        );
        assert_eq!(
            MarshalSpec::FixedSysString { size: 128 }.encode(),
            vec![0x17, 0x80, 0x80]
        );
        assert_eq!(
            MarshalSpec::FixedSysString { size: 16384 }.encode(),
            vec![0x17, 0xC0, 0x00, 0x40, 0x00]
        );
    }

    #[test]
    fn a_by_val_array_omits_the_element_type_the_source_did_not_name() {
        assert_eq!(
            MarshalSpec::FixedArray {
                size: 3,
                element: Some(0x07)
            }
            .encode(),
            vec![0x1E, 0x03, 0x07]
        );
        assert_eq!(
            MarshalSpec::FixedArray {
                size: 300,
                element: None
            }
            .encode(),
            vec![0x1E, 0x81, 0x2C]
        );
        assert_eq!(
            MarshalSpec::FixedArray {
                size: 0,
                element: None
            }
            .encode(),
            vec![0x1E, 0x00]
        );
    }

    #[test]
    fn an_lp_array_writes_a_positional_tail() {
        assert_eq!(
            MarshalSpec::Array {
                element: NATIVE_TYPE_MAX,
                param_num: None,
                size: None
            }
            .encode(),
            vec![0x2A, 0x50]
        );
        assert_eq!(
            MarshalSpec::Array {
                element: 0x07,
                param_num: Some(1),
                size: None
            }
            .encode(),
            vec![0x2A, 0x07, 0x01]
        );
        assert_eq!(
            MarshalSpec::Array {
                element: 0x07,
                param_num: Some(2),
                size: Some((7, 1))
            }
            .encode(),
            vec![0x2A, 0x07, 0x02, 0x07, 0x01]
        );
        assert_eq!(
            MarshalSpec::Array {
                element: NATIVE_TYPE_MAX,
                param_num: Some(0),
                size: Some((5, 0))
            }
            .encode(),
            vec![0x2A, 0x50, 0x00, 0x05, 0x00]
        );
    }

    #[test]
    fn a_safe_array_carries_its_variant_type_only_when_named() {
        assert_eq!(
            MarshalSpec::SafeArray { variant: None }.encode(),
            vec![0x1D]
        );
        assert_eq!(
            MarshalSpec::SafeArray { variant: Some(3) }.encode(),
            vec![0x1D, 0x03]
        );
    }

    #[test]
    fn a_custom_marshaler_writes_two_empty_strings_before_its_own() {
        assert_eq!(
            MarshalSpec::CustomMarshaler {
                marshal_type: "N.C".into(),
                cookie: "ck".into()
            }
            .encode(),
            vec![0x2C, 0x00, 0x00, 0x03, b'N', b'.', b'C', 0x02, b'c', b'k']
        );
    }
}
