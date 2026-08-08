//! Loading a reference assembly's types into the binder's [`Model`].

use crate::bound::integer_literal;
use crate::special::SpecialType;
use crate::symbols::{
    Accessibility, EventSymbol, FieldSymbol, MethodSymbol, Model, PropertySymbol, TypeInfo,
    TypeKind,
};
use crate::types::TypeSymbol;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use lamella_metadata::flags::{
    method_is_abstract, method_is_final, method_is_newslot, method_is_virtual,
};
use lamella_metadata::tables::table;
use lamella_metadata::{Assembly, ConstantValue, SigType, TypeName};
use lamella_syntax::ast::Literal;

/// The attribute that carries `required` (C# 11) across an assembly boundary. There is no
/// FieldAttributes or PropertyAttributes bit for `required` -- this attribute IS the encoding, so a
/// reader that does not decode it answers "not required" for every imported member, which is an
/// answer that looks exactly like a correct one.
const REQUIRED_MEMBER_ATTRIBUTE_NAMESPACE: &str = "System.Runtime.CompilerServices";
const REQUIRED_MEMBER_ATTRIBUTE_NAME: &str = "RequiredMemberAttribute";
use lamella_syntax::token::RealSuffix;
use lamella_token::Token;

/// Adds every type defined in `assembly` to `model`.
pub fn load_assembly(model: &mut Model, assembly: &Assembly) {
    let param_array = assembly.param_array_params();
    let conditional = assembly.conditional_symbols();
    let type_parameters = assembly.type_parameter_names();
    let method_type_parameters = assembly.method_type_parameter_names();
    for type_def in assembly.type_defs() {
        if let Some(info) = type_info(
            assembly,
            &type_def,
            &param_array,
            &conditional,
            &method_type_parameters,
        ) {
            let mut info = info;
            if let Some(names) = type_parameters.get(&type_def.token().row()) {
                info.type_parameters = names.iter().map(|&name| Box::from(name)).collect();
            }
            model.insert(info);
        }
    }
}

/// The full dotted name of the type `type_def` is nested in, walking outward through the
/// `NestedClass` chain (II.22.32) so a doubly-nested `A.B.C` reports `A.B`. `None` when the type is
/// not nested, or when the chain does not resolve.
fn enclosing_full_name(type_def: &lamella_metadata::TypeDef) -> Option<Box<str>> {
    let outer = type_def.enclosing_type()?;
    let TypeName { namespace, name } = outer.name()?;
    Some(match enclosing_full_name(&outer) {
        Some(grandparent) => alloc::format!("{grandparent}.{name}").into(),
        None if namespace.is_empty() => name.into(),
        None => alloc::format!("{namespace}.{name}").into(),
    })
}

fn type_info(
    assembly: &Assembly,
    type_def: &lamella_metadata::TypeDef,
    param_array: &BTreeSet<u32>,
    conditional: &BTreeMap<u32, Vec<Box<str>>>,
    method_type_parameters: &BTreeMap<u32, Vec<&str>>,
) -> Option<TypeInfo> {
    let TypeName { namespace, name } = type_def.name()?;
    if name == "<Module>" {
        return None;
    }
    let (namespace, enclosing) = if type_def.is_nested() {
        match enclosing_full_name(type_def) {
            None => return None,
            Some(outer) => (outer.clone(), Some(outer)),
        }
    } else {
        (Box::from(namespace), None)
    };
    let namespace: &str = &namespace;
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
    } else if is_base(&base, "System", "MulticastDelegate") || is_base(&base, "System", "Delegate") {
        TypeKind::Delegate
    } else {
        TypeKind::Class
    };

    let mut info = TypeInfo::new(namespace, name, kind);
    info.is_external = true;
    info.enclosing = enclosing;
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
            let constant = field.constant().and_then(constant_to_literal);
            info.fields.push(FieldSymbol {
                name: field_name.into(),
                ty: sigtype_to_symbol(assembly, &signature, &[]),
                is_static: field.flags() & 0x0010 != 0
                    && (field.flags() & 0x0040 == 0 || constant.is_some()),
                is_readonly: field.flags() & 0x0020 != 0,
                is_volatile: false,
                accessibility: member_accessibility(field.flags()),
                constant,
                is_required: assembly.has_attribute(
                    field.token(),
                    REQUIRED_MEMBER_ATTRIBUTE_NAMESPACE,
                    REQUIRED_MEMBER_ATTRIBUTE_NAME,
                ),
            });
        }
    }
    let required_properties: Vec<&str> = type_def
        .properties()
        .filter(|property| {
            assembly.has_attribute(
                property.token(),
                REQUIRED_MEMBER_ATTRIBUTE_NAMESPACE,
                REQUIRED_MEMBER_ATTRIBUTE_NAME,
            )
        })
        .filter_map(|property| property.name())
        .collect();
    for method in type_def.methods() {
        let Some(method_name) = method.name() else {
            continue;
        };
        let Some(signature) = method.signature() else {
            info.undecodable_members.push(method_name.into());
            continue;
        };
        let own_parameters: &[&str] = method_type_parameters
            .get(&method.rid())
            .map_or(&[], Vec::as_slice);
        let symbol = MethodSymbol {
            name: method_name.into(),
            return_type: sigtype_to_symbol(assembly, &signature.return_type, own_parameters),
            parameters: signature
                .parameters
                .iter()
                .map(|parameter| sigtype_to_symbol(assembly, parameter, own_parameters))
                .collect(),
            parameter_info: imported_parameter_info(&method, signature.parameters.len()),
            is_static: !signature.has_this,
            is_params: method
                .params()
                .any(|parameter| param_array.contains(&parameter.token().row())),
            is_vararg: signature.is_vararg,
            is_virtual: method_is_virtual(method.flags()),
            is_abstract: method_is_abstract(method.flags()),
            is_override: method_is_virtual(method.flags()) && !method_is_newslot(method.flags()),
            is_sealed: method_is_final(method.flags()),
            accessibility: member_accessibility(method.flags()),
            type_parameters: method_type_parameters
                .get(&method.rid())
                .map(|names| names.iter().map(|&name| Box::from(name)).collect())
                .unwrap_or_default(),
            conditional: conditional.get(&method.rid()).cloned().unwrap_or_default(),
            sets_required_members: assembly.has_attribute(
                method.token(),
                "System.Diagnostics.CodeAnalysis",
                "SetsRequiredMembersAttribute",
            ),
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
                    is_virtual: symbol.is_virtual,
                    is_abstract: symbol.is_abstract,
                    is_override: symbol.is_override,
                    is_sealed: symbol.is_sealed,
                    has_getter: method_name.starts_with("get_"),
                    has_setter: method_name.starts_with("set_"),
                    is_required: required_properties.contains(&property_name),
                });
            }
        }
        let event = method_name
            .strip_prefix("add_")
            .or_else(|| method_name.strip_prefix("remove_"))
            .filter(|_| symbol.parameters.len() == 1)
            .map(|name| (name, symbol.parameters[0].clone()));
        if let Some((event_name, ty)) = event {
            if info.find_event(event_name).is_none() {
                info.events.push(EventSymbol {
                    name: event_name.into(),
                    ty,
                    is_static: symbol.is_static,
                    accessibility: symbol.accessibility,
                    is_abstract: symbol.is_abstract,
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

/// Maps a referenced member's access flags (the low 3 bits) to an accessibility, over the full
/// `MemberAccess` mask (II.23.1.5 / II.23.1.10). `FamANDAssem` maps to the most restrictive
/// answer, since it is inaccessible from another assembly whatever the reference says.
/// The declared names and `ref`/`out` modes of an imported method's parameters, read from the
/// `Param` table (II.22.33).
///
/// TWO DETAILS THAT ARE NOT OPTIONAL. **`Sequence` 0 is the RETURN value**, not the first
/// parameter, and parameters are numbered from 1 -- so the rows are placed BY SEQUENCE rather than
/// by iteration order, or every name would be off by one for any method whose return value carries
/// a row. And **`ref` and `out` are the same type in a signature** (`T&`); only the `Out` flag
/// (0x0002) separates them, which is exactly the fact CS1620 exists to report.
///
/// A row may be absent or unnamed -- a reference assembly is not obliged to carry names -- so any
/// slot it does not fill stays empty, and [`MethodSymbol::parameter_name`] reports that as "not
/// known" rather than as a name. Returns EMPTY when no row supplied anything, which keeps the
/// vector's invariant honest: absent, not fabricated.
fn imported_parameter_info(
    method: &lamella_metadata::Method,
    count: usize,
) -> Vec<crate::symbols::ParameterInfo> {
    use crate::symbols::{ParameterInfo, ParameterMode};
    const PARAM_OUT: u32 = 0x0002;
    let mut info = alloc::vec![
        ParameterInfo {
            name: "".into(),
            mode: ParameterMode::Value,
        };
        count
    ];
    let mut any = false;
    for param in method.params() {
        let sequence = param.sequence();
        if sequence == 0 {
            continue;
        }
        let Some(slot) = info.get_mut(sequence as usize - 1) else {
            continue;
        };
        if let Some(name) = param.name() {
            slot.name = name.into();
            any = true;
        }
        if param.flags() & PARAM_OUT != 0 {
            slot.mode = ParameterMode::Out;
            any = true;
        }
    }
    if any { info } else { Vec::new() }
}

fn member_accessibility(flags: u32) -> Accessibility {
    match flags & 0x0007 {
        0x0000 => Accessibility::Private,
        0x0001 => Accessibility::Private,
        0x0002 => Accessibility::Private,
        0x0003 => Accessibility::Internal,
        0x0004 => Accessibility::Protected,
        0x0005 => Accessibility::ProtectedInternal,
        _ => Accessibility::Public,
    }
}

/// Maps a metadata constant to the literal the binder folds a `const` field or enum member
/// to at its use site (instead of an `ldsfld` on a storageless slot): the integral, char, and
/// bool values, and now the float (`Single.MaxValue`, `Double.NaN`) and string ones. A null
/// constant has no fold -- it is already the null literal at the use site.
fn constant_to_literal(value: ConstantValue) -> Option<Literal> {
    Some(match value {
        ConstantValue::Bool(b) => Literal::Boolean(b),
        ConstantValue::Char(c) => Literal::Character(c),
        ConstantValue::I1(n) => integer_literal(i64::from(n)),
        ConstantValue::U1(n) => integer_literal(i64::from(n)),
        ConstantValue::I2(n) => integer_literal(i64::from(n)),
        ConstantValue::U2(n) => integer_literal(i64::from(n)),
        ConstantValue::I4(n) => integer_literal(i64::from(n)),
        ConstantValue::U4(n) => integer_literal(i64::from(n)),
        ConstantValue::I8(n) => integer_literal(n),
        ConstantValue::U8(n) => integer_literal(n as i64),
        ConstantValue::R4(f) => Literal::Real {
            bits: f64::from(f).to_bits(),
            suffix: RealSuffix::Float,
        },
        ConstantValue::R8(f) => Literal::Real {
            bits: f.to_bits(),
            suffix: RealSuffix::Double,
        },
        ConstantValue::String(units) => Literal::String(units.into_boxed_slice()),
        ConstantValue::Null => return None,
    })
}

/// Maps a metadata signature element to a [`TypeSymbol`].
fn sigtype_to_symbol(assembly: &Assembly, sig: &SigType, method_parameters: &[&str]) -> TypeSymbol {
    if let Some(special) = primitive_symbol(sig) {
        return special;
    }
    match sig {
        SigType::IntPtr => named_symbol("System", "IntPtr"),
        SigType::UIntPtr => named_symbol("System", "UIntPtr"),
        SigType::TypedByRef => named_symbol("System", "TypedReference"),
        SigType::Class(token) | SigType::ValueType(token) => token_type_symbol(assembly, *token),
        SigType::SzArray(element) => sigtype_to_symbol(assembly, element, method_parameters).into_array(1),
        SigType::Array { element, rank } => {
            sigtype_to_symbol(assembly, element, method_parameters).into_array(*rank as u8)
        }
        SigType::ByRef(referent) => {
            TypeSymbol::ByRef(Box::new(sigtype_to_symbol(assembly, referent, method_parameters)))
        }
        SigType::Pointer(referent) => {
            TypeSymbol::Pointer(Box::new(sigtype_to_symbol(assembly, referent, method_parameters)))
        }
        SigType::MVar(index) => match method_parameters.get(*index as usize) {
            Some(name) => TypeSymbol::Named([Box::from(*name)].into()),
            None => TypeSymbol::Error,
        },
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
pub(crate) fn special_for_named(namespace: &str, name: &str) -> Option<SpecialType> {
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
        "Decimal" => SpecialType::Decimal,
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
