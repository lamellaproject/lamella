//! Loading a reference assembly's types into the binder's [`Model`].

use crate::special::SpecialType;
use crate::symbols::{
    Accessibility, FieldSymbol, MethodSymbol, Model, PropertySymbol, TypeInfo, TypeKind,
};
use crate::types::TypeSymbol;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use lamella_metadata::tables::table;
use lamella_metadata::{Assembly, ConstantValue, SigType, TypeName};
use lamella_token::Token;

/// Adds every type defined in `assembly` to `model`.
pub fn load_assembly(model: &mut Model, assembly: &Assembly) {
    let param_array = assembly.param_array_params();
    let conditional = assembly.conditional_symbols();
    for type_def in assembly.type_defs() {
        if let Some(info) = type_info(assembly, &type_def, &param_array, &conditional) {
            model.insert(info);
        }
    }
}

fn type_info(
    assembly: &Assembly,
    type_def: &lamella_metadata::TypeDef,
    param_array: &BTreeSet<u32>,
    conditional: &BTreeMap<u32, Vec<Box<str>>>,
) -> Option<TypeInfo> {
    let TypeName { namespace, name } = type_def.name()?;
    if name == "<Module>" {
        return None;
    }
    let extends = type_def.extends();
    let base = (!extends.is_nil())
        .then(|| token_type_symbol(assembly, extends))
        .filter(|symbol| !symbol.is_error());
    let kind = if type_def.is_interface() {
        TypeKind::Interface
    } else if is_base(&base, "System", "Enum") {
        TypeKind::Enum
    } else if is_base(&base, "System", "ValueType") {
        TypeKind::Struct
    } else {
        TypeKind::Class
    };

    let mut info = TypeInfo::new(namespace, name, kind);
    info.is_external = true;
    info.assembly = assembly.assembly_name().map(Box::from);
    if let Some(base) = base {
        info.bases.push(base.clone());
        info.base = Some(base);
    }
    for interface in type_def.interfaces() {
        let symbol = token_type_symbol(assembly, interface);
        if !symbol.is_error() {
            info.bases.push(symbol);
        }
    }
    for field in type_def.fields() {
        if let (Some(field_name), Some(signature)) = (field.name(), field.signature()) {
            let constant = field.constant().and_then(constant_to_i64);
            info.fields.push(FieldSymbol {
                name: field_name.into(),
                ty: sigtype_to_symbol(assembly, &signature),
                is_static: field.flags() & 0x0010 != 0
                    && (field.flags() & 0x0040 == 0 || constant.is_some()),
                is_readonly: false,
                accessibility: member_accessibility(field.flags()),
                constant,
            });
        }
    }
    for method in type_def.methods() {
        let (Some(method_name), Some(signature)) = (method.name(), method.signature()) else {
            continue;
        };
        let symbol = MethodSymbol {
            name: method_name.into(),
            return_type: sigtype_to_symbol(assembly, &signature.return_type),
            parameters: signature
                .parameters
                .iter()
                .map(|parameter| sigtype_to_symbol(assembly, parameter))
                .collect(),
            is_static: !signature.has_this,
            is_params: method
                .params()
                .any(|parameter| param_array.contains(&parameter.token().row())),
            accessibility: member_accessibility(method.flags()),
            conditional: conditional.get(&method.rid()).cloned().unwrap_or_default(),
        };
        let property = method_name
            .strip_prefix("get_")
            .filter(|_| signature.parameters.is_empty())
            .map(|name| (name, symbol.return_type.clone()))
            .or_else(|| {
                method_name
                    .strip_prefix("set_")
                    .filter(|_| symbol.parameters.len() == 1)
                    .map(|name| (name, symbol.parameters[0].clone()))
            });
        if let Some((property_name, ty)) = property {
            if info.find_property(property_name).is_none() {
                info.properties.push(PropertySymbol {
                    name: property_name.into(),
                    ty,
                    is_static: symbol.is_static,
                    accessibility: Accessibility::Public,
                });
            }
        }
        if method_name == ".ctor" {
            info.constructors.push(symbol);
        } else {
            info.methods.push(symbol);
        }
    }
    Some(info)
}

/// Maps a referenced member's access flags (the low 3 bits, II.23.1.5 / II.23.1.10) to an
/// accessibility. Only `internal` (Assembly / FamANDAssem) is read -- it is enforced across
/// assemblies as CS0122; every other level stays Public, so existing public/protected BCL
/// access is unchanged (we model neither cross-assembly protected nor InternalsVisibleTo).
fn member_accessibility(flags: u32) -> Accessibility {
    match flags & 0x0007 {
        0x0002 | 0x0003 => Accessibility::Internal,
        _ => Accessibility::Public,
    }
}

/// Narrows a metadata constant to the binder's integral fold (`Option<i64>`): the integer,
/// char, and bool literals fold (enum members, `int.MaxValue`, `SeekOrigin.Begin`); a string,
/// float, or null constant does not -- the binder folds only integral constants.
fn constant_to_i64(value: ConstantValue) -> Option<i64> {
    match value {
        ConstantValue::Bool(b) => Some(i64::from(b)),
        ConstantValue::Char(c) => Some(i64::from(c)),
        ConstantValue::I1(n) => Some(i64::from(n)),
        ConstantValue::U1(n) => Some(i64::from(n)),
        ConstantValue::I2(n) => Some(i64::from(n)),
        ConstantValue::U2(n) => Some(i64::from(n)),
        ConstantValue::I4(n) => Some(i64::from(n)),
        ConstantValue::U4(n) => Some(i64::from(n)),
        ConstantValue::I8(n) => Some(n),
        ConstantValue::U8(n) => Some(n as i64),
        ConstantValue::R4(_) | ConstantValue::R8(_) | ConstantValue::String(_) | ConstantValue::Null => {
            None
        }
    }
}

/// Maps a metadata signature element to a [`TypeSymbol`].
fn sigtype_to_symbol(assembly: &Assembly, sig: &SigType) -> TypeSymbol {
    if let Some(special) = primitive_symbol(sig) {
        return special;
    }
    match sig {
        SigType::IntPtr => named_symbol("System", "IntPtr"),
        SigType::UIntPtr => named_symbol("System", "UIntPtr"),
        SigType::TypedByRef => named_symbol("System", "TypedReference"),
        SigType::Class(token) | SigType::ValueType(token) => token_type_symbol(assembly, *token),
        SigType::SzArray(element) => sigtype_to_symbol(assembly, element).into_array(1),
        SigType::Array { element, rank } => {
            sigtype_to_symbol(assembly, element).into_array(*rank as u8)
        }
        SigType::ByRef(referent) => sigtype_to_symbol(assembly, referent),
        SigType::Pointer(_) => TypeSymbol::Error,
        _ => TypeSymbol::Error,
    }
}

/// The [`TypeSymbol`] for a primitive signature element, or `None` for the
/// composite ones (those need the assembly to resolve).
fn primitive_symbol(sig: &SigType) -> Option<TypeSymbol> {
    let special = match sig {
        SigType::Void => SpecialType::Void,
        SigType::Boolean => SpecialType::Boolean,
        SigType::Char => SpecialType::Char,
        SigType::I1 => SpecialType::SByte,
        SigType::U1 => SpecialType::Byte,
        SigType::I2 => SpecialType::Int16,
        SigType::U2 => SpecialType::UInt16,
        SigType::I4 => SpecialType::Int32,
        SigType::U4 => SpecialType::UInt32,
        SigType::I8 => SpecialType::Int64,
        SigType::U8 => SpecialType::UInt64,
        SigType::R4 => SpecialType::Single,
        SigType::R8 => SpecialType::Double,
        SigType::String => SpecialType::String,
        SigType::Object => SpecialType::Object,
        _ => return None,
    };
    Some(TypeSymbol::Special(special))
}

/// Resolves a `TypeDef`/`TypeRef` token to a named type symbol (the error type for
/// a `TypeSpec` or an unresolved token).
fn token_type_symbol(assembly: &Assembly, token: Token) -> TypeSymbol {
    let name = match token.table() {
        table::TYPE_DEF => assembly
            .type_def(token.row())
            .and_then(|type_def| type_def.name()),
        table::TYPE_REF => assembly
            .type_ref(token.row())
            .and_then(|type_ref| type_ref.name()),
        _ => None,
    };
    match name {
        Some(TypeName { namespace, name }) => match special_for_named(namespace, name) {
            Some(special) => TypeSymbol::Special(special),
            None => named_symbol(namespace, name),
        },
        None => TypeSymbol::Error,
    }
}

/// The [`SpecialType`] of a core BCL type named `System.<name>` (`Object`, `String`,
/// or a numeric/`bool`/`char` primitive), or `None` for any other named type.
fn special_for_named(namespace: &str, name: &str) -> Option<SpecialType> {
    if namespace != "System" {
        return None;
    }
    Some(match name {
        "Object" => SpecialType::Object,
        "String" => SpecialType::String,
        "Boolean" => SpecialType::Boolean,
        "Char" => SpecialType::Char,
        "SByte" => SpecialType::SByte,
        "Byte" => SpecialType::Byte,
        "Int16" => SpecialType::Int16,
        "UInt16" => SpecialType::UInt16,
        "Int32" => SpecialType::Int32,
        "UInt32" => SpecialType::UInt32,
        "Int64" => SpecialType::Int64,
        "UInt64" => SpecialType::UInt64,
        "Single" => SpecialType::Single,
        "Double" => SpecialType::Double,
        _ => return None,
    })
}

/// A named-type symbol from a namespace (empty or dotted) and a simple name.
fn named_symbol(namespace: &str, name: &str) -> TypeSymbol {
    let mut parts: Vec<Box<str>> = Vec::new();
    if !namespace.is_empty() {
        for part in namespace.split('.') {
            parts.push(part.into());
        }
    }
    parts.push(name.into());
    TypeSymbol::Named(parts.into_boxed_slice())
}

/// Whether `base` is the named type `namespace.name`.
fn is_base(base: &Option<TypeSymbol>, namespace: &str, name: &str) -> bool {
    matches!(base, Some(symbol) if *symbol == named_symbol(namespace, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitive_signature_elements_map_to_special_types() {
        assert_eq!(
            primitive_symbol(&SigType::I4),
            Some(TypeSymbol::Special(SpecialType::Int32))
        );
        assert_eq!(
            primitive_symbol(&SigType::String),
            Some(TypeSymbol::Special(SpecialType::String))
        );
        assert_eq!(
            primitive_symbol(&SigType::Void),
            Some(TypeSymbol::Special(SpecialType::Void))
        );
        assert_eq!(
            primitive_symbol(&SigType::R8),
            Some(TypeSymbol::Special(SpecialType::Double))
        );
        assert_eq!(
            primitive_symbol(&SigType::Object),
            Some(TypeSymbol::Special(SpecialType::Object))
        );
        assert_eq!(primitive_symbol(&SigType::IntPtr), None);
        assert_eq!(
            primitive_symbol(&SigType::SzArray(Box::new(SigType::I4))),
            None
        );
    }

    #[test]
    fn named_symbol_joins_namespace_and_name() {
        assert_eq!(
            named_symbol("System", "String").to_string(),
            "System.String"
        );
        assert_eq!(
            named_symbol("System.IO", "Stream").to_string(),
            "System.IO.Stream"
        );
        assert_eq!(named_symbol("", "Widget").to_string(), "Widget");
    }
}
