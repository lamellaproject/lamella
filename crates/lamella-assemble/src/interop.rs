//! Reading the three interop-layout attributes into the metadata they become: `[StructLayout]`
//! into `TypeAttributes` bits plus a `ClassLayout` row (II.22.8), `[FieldOffset]` into a
//! `FieldLayout` row (II.22.16), and `[MarshalAs]` into a `FieldMarshal` row (II.22.17).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use lamella_binder::{Binder, TypeSymbol};
use lamella_pe::marshal::{MarshalSpec, NATIVE_TYPE_MAX};
use lamella_syntax::ast::{Attribute, AttributeArgument, AttributeSection, Expr, Literal};

/// The `TypeAttributes` layout field (II.23.1.15), `LayoutMask` in the spec.
pub(crate) const LAYOUT_MASK: u32 = 0x0000_0018;
/// `TypeAttributes.SequentialLayout` -- fields laid out in declaration order.
const SEQUENTIAL_LAYOUT: u32 = 0x0000_0008;
/// `TypeAttributes.ExplicitLayout` -- fields laid out at the offsets `FieldLayout` gives.
const EXPLICIT_LAYOUT: u32 = 0x0000_0010;
/// The `TypeAttributes` string-format field (II.23.1.15), `StringFormatMask` in the spec.
pub(crate) const STRING_FORMAT_MASK: u32 = 0x0003_0000;
/// `TypeAttributes.UnicodeClass`.
const UNICODE_CLASS: u32 = 0x0001_0000;
/// `TypeAttributes.AutoClass`.
const AUTO_CLASS: u32 = 0x0002_0000;

/// What a `[StructLayout]` asks for: the `TypeAttributes` bits, and the `ClassLayout` row -- if
/// the attribute asked for one at all.
pub(crate) struct StructLayout {
    /// The layout and string-format bits to install on the `TypeDef`, already masked so a caller
    /// can clear [`LAYOUT_MASK`] and [`STRING_FORMAT_MASK`] and OR these in.
    pub(crate) type_flags: u32,
    /// The `Pack` and `Size` of the `ClassLayout` row, or `None` when both are zero or absent.
    ///
    pub(crate) class_layout: Option<(u16, u32)>,
}

/// Reads the `[StructLayout(...)]` among `attributes`, or `None` if there is none.
///
/// The positional argument is the `LayoutKind`; `CharSet`, `Pack` and `Size` are named. An
/// attribute whose `LayoutKind` does not resolve yields `None` -- the whole attribute is ignored
/// rather than half-applied, because a `Pack` installed without the layout kind that motivated it
/// is a type laid out one way and packed for another.
pub(crate) fn struct_layout(
    binder: &mut Binder,
    enclosing: &TypeSymbol,
    attributes: &[AttributeSection],
) -> Option<StructLayout> {
    let attribute = find_attribute(attributes, "StructLayout")?;
    let kind = positional(attribute, 0).and_then(|expr| constant(binder, enclosing, expr))?;
    let layout = match kind {
        0 => SEQUENTIAL_LAYOUT,
        2 => EXPLICIT_LAYOUT,
        3 => 0,
        _ => return None,
    };
    let charset = match named(binder, enclosing, attribute, "CharSet") {
        Some(3) => UNICODE_CLASS,
        Some(4) => AUTO_CLASS,
        _ => 0,
    };
    let pack = named(binder, enclosing, attribute, "Pack").unwrap_or(0);
    let size = named(binder, enclosing, attribute, "Size").unwrap_or(0);
    let pack = u16::try_from(pack).unwrap_or(0);
    let size = u32::try_from(size).unwrap_or(0);
    Some(StructLayout {
        type_flags: layout | charset,
        class_layout: (pack != 0 || size != 0).then_some((pack, size)),
    })
}

/// Reads the byte offset a `[FieldOffset(n)]` among `attributes` puts a field at, or `None` if
/// there is none.
pub(crate) fn field_offset(
    binder: &mut Binder,
    enclosing: &TypeSymbol,
    attributes: &[AttributeSection],
) -> Option<u32> {
    let attribute = find_attribute(attributes, "FieldOffset")?;
    let offset = positional(attribute, 0).and_then(|expr| constant(binder, enclosing, expr))?;
    u32::try_from(offset).ok()
}

/// Reads the marshalling descriptor a `[MarshalAs(...)]` among `attributes` asks for, or `None`
/// if there is none (or if its `UnmanagedType` does not resolve).
///
pub(crate) fn marshal_spec(
    binder: &mut Binder,
    enclosing: &TypeSymbol,
    attributes: &[AttributeSection],
) -> Option<MarshalSpec> {
    let attribute = find_attribute(attributes, "MarshalAs")?;
    let native = positional(attribute, 0).and_then(|expr| constant(binder, enclosing, expr))?;
    let native = u8::try_from(native).ok()?;
    let array_sub_type =
        named(binder, enclosing, attribute, "ArraySubType").and_then(|v| u8::try_from(v).ok());
    let size_const =
        named(binder, enclosing, attribute, "SizeConst").and_then(|v| u32::try_from(v).ok());
    let size_param_index =
        named(binder, enclosing, attribute, "SizeParamIndex").and_then(|v| u32::try_from(v).ok());
    Some(match native {
        0x17 => MarshalSpec::FixedSysString {
            size: size_const.unwrap_or(0),
        },
        0x1D => MarshalSpec::SafeArray {
            variant: named(binder, enclosing, attribute, "SafeArraySubType")
                .and_then(|v| u32::try_from(v).ok()),
        },
        0x1E => MarshalSpec::FixedArray {
            size: size_const.unwrap_or(0),
            element: array_sub_type,
        },
        0x2A => MarshalSpec::Array {
            element: array_sub_type.unwrap_or(NATIVE_TYPE_MAX),
            param_num: (size_param_index.is_some() || size_const.is_some())
                .then(|| size_param_index.unwrap_or(0)),
            size: size_const.map(|size| (size, u32::from(size_param_index.is_some()))),
        },
        0x2C => MarshalSpec::CustomMarshaler {
            marshal_type: named_string(binder, enclosing, attribute, "MarshalType")?,
            cookie: named_string(binder, enclosing, attribute, "MarshalCookie")
                .unwrap_or_else(|| "".into()),
        },
        _ => MarshalSpec::Simple(native),
    })
}

/// Whether a TYPE attribute is pseudo-custom -- consumed into `TypeAttributes` bits and a
/// `ClassLayout` row rather than kept as a `CustomAttribute` row (II.21.2.1). See the module doc
/// for the measurement.
pub(crate) fn is_pseudo_custom_type_attribute(name: Option<&str>) -> bool {
    matches!(name, Some("StructLayout" | "StructLayoutAttribute"))
}

/// Whether a FIELD attribute is pseudo-custom -- consumed into a `FieldLayout` or `FieldMarshal`
/// row rather than kept as a `CustomAttribute` row (II.21.2.1).
///
/// `FieldOffset` belongs here as much as `MarshalAs` does: it becomes a row and nothing else.
pub(crate) fn is_pseudo_custom_field_attribute(name: Option<&str>) -> bool {
    matches!(
        name,
        Some(
            "FieldOffset"
                | "FieldOffsetAttribute"
                | "MarshalAs"
                | "MarshalAsAttribute"
        )
    )
}

/// Drops the pseudo-custom attributes from `sections`, leaving the user attributes that still earn
/// a `CustomAttribute` row. Returns an empty vector when nothing is left.
///
/// **FILTERED PER ATTRIBUTE RATHER THAN PER SECTION**, because one section can hold both kinds:
/// `[Mark("x"), FieldOffset(4)]` is a single section whose first attribute is a row and whose
/// second is a layout. A TARGETED section (`[return: ...]`) is about something else and is left
/// exactly as it came.
pub(crate) fn without_pseudo_custom(
    sections: &[AttributeSection],
    is_pseudo: fn(Option<&str>) -> bool,
) -> Vec<AttributeSection> {
    sections
        .iter()
        .map(|section| AttributeSection {
            target: section.target.clone(),
            span: section.span,
            attributes: section
                .attributes
                .iter()
                .filter(|attribute| {
                    section.target.is_some()
                        || !is_pseudo(attribute.name.parts.last().map(|part| &**part))
                })
                .cloned()
                .collect(),
        })
        .filter(|section| !section.attributes.is_empty())
        .collect()
}

/// The first attribute in `sections` whose last name part is `simple_name` (with or without the
/// `Attribute` suffix), skipping TARGETED sections -- a `[return: MarshalAs]` belongs to the return
/// parameter and not to the member the section is written on.
///
/// Matched on the LAST name part, so `MarshalAs`, `MarshalAsAttribute` and a namespace-qualified
/// `System.Runtime.InteropServices.MarshalAs` all answer alike -- the rule every other attribute
/// reader in this compiler uses.
fn find_attribute<'a>(sections: &'a [AttributeSection], simple_name: &str) -> Option<&'a Attribute> {
    sections
        .iter()
        .filter(|section| section.target.is_none())
        .flat_map(|section| section.attributes.iter())
        .find(|attribute| matches_name(attribute, simple_name))
}

/// The marshalling descriptor of one already-found `[MarshalAs]`, for a caller that located the
/// attribute itself (a `[return:]` section, a parameter's own list).
pub(crate) fn marshal_spec_of(
    binder: &mut Binder,
    enclosing: &TypeSymbol,
    attribute: &Attribute,
) -> Option<MarshalSpec> {
    let section = AttributeSection {
        target: None,
        span: attribute.span,
        attributes: alloc::vec![attribute.clone()],
    };
    marshal_spec(binder, enclosing, &[section])
}

fn matches_name(attribute: &Attribute, simple_name: &str) -> bool {
    match attribute.name.parts.last() {
        Some(last) => {
            &**last == simple_name
                || (last.len() == simple_name.len() + 9
                    && last.starts_with(simple_name)
                    && last.ends_with("Attribute"))
        }
        None => false,
    }
}

/// The `index`-th positional argument of `attribute`.
fn positional(attribute: &Attribute, index: usize) -> Option<&Expr> {
    attribute
        .arguments
        .iter()
        .filter_map(|argument| match argument {
            AttributeArgument::Positional(expr) => Some(expr),
            AttributeArgument::Named { .. } => None,
        })
        .nth(index)
}

/// The expression `attribute`'s named argument `name` is given.
fn named_value<'a>(attribute: &'a Attribute, name: &str) -> Option<&'a Expr> {
    attribute.arguments.iter().find_map(|argument| match argument {
        AttributeArgument::Named { name: n, value } if &**n == name => Some(value),
        _ => None,
    })
}

/// The integer value of `attribute`'s named argument `name`, or `None` when it is absent or is
/// no integral constant.
fn named(
    binder: &mut Binder,
    enclosing: &TypeSymbol,
    attribute: &Attribute,
    name: &str,
) -> Option<i64> {
    constant(binder, enclosing, named_value(attribute, name)?)
}

/// The string value of `attribute`'s named argument `name`, or `None` when it is absent or is no
/// string constant.
fn named_string(
    binder: &mut Binder,
    enclosing: &TypeSymbol,
    attribute: &Attribute,
    name: &str,
) -> Option<Box<str>> {
    match binder.attribute_argument_constant(Some(enclosing), named_value(attribute, name)?)? {
        Literal::String(units) => Some(String::from_utf16_lossy(&units).into_boxed_str()),
        _ => None,
    }
}

/// The integral value of a layout-attribute argument, bound in `enclosing`'s scope and folded: an
/// enum member's is its underlying integer.
fn constant(binder: &mut Binder, enclosing: &TypeSymbol, expr: &Expr) -> Option<i64> {
    let literal = binder.attribute_argument_constant(Some(enclosing), expr)?;
    lamella_binder::literal_int_value(&literal)
}
