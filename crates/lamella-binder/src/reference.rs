//! Loading a reference assembly's types into the binder's [`Model`].

use crate::bound::integer_literal;
use crate::special::SpecialType;
use crate::symbols::{
    TypeParameterConstraints,
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
    let constraints = assembly.generic_param_constraints();
    let required_members = assembly.tokens_with_attribute(
        REQUIRED_MEMBER_ATTRIBUTE_NAMESPACE,
        REQUIRED_MEMBER_ATTRIBUTE_NAME,
    );
    let sets_required = assembly.tokens_with_attribute(
        "System.Diagnostics.CodeAnalysis",
        "SetsRequiredMembersAttribute",
    );
    for type_def in assembly.type_defs() {
        let own_parameters: &[&str] = type_parameters
            .get(&type_def.token().row())
            .map_or(&[], Vec::as_slice);
        if let Some(info) = type_info(
            assembly,
            &type_def,
            &param_array,
            &conditional,
            &method_type_parameters,
            own_parameters,
            &required_members,
            &sets_required,
        ) {
            let mut info = info;
            if let Some(names) = type_parameters.get(&type_def.token().row()) {
                info.type_parameters = names.iter().map(|&name| Box::from(name)).collect();
                let owner = type_def.token().row() << 1;
                info.type_parameter_constraints = (0..info.type_parameters.len())
                    .map(|number| {
                        decode_constraints(assembly, constraints.get(&(owner, number as u32)))
                    })
                    .collect();
            }
            model.insert(info);
        }
    }
}

/// Turns one imported parameter's `(flags, constraint tokens)` into the binder's model.
///
/// **`struct` IS READ FROM THE FLAG, NOT FROM THE `System.ValueType` ROW.** csc emits BOTH for
/// `where T : struct` -- the `0x0008` bit and a `GenericParamConstraint` naming `System.ValueType`.
/// Taking the row as a named constraint too would make every `struct`-constrained parameter also
/// require convertibility to `System.ValueType`, which is true but reports as a second, confusing
/// diagnostic when it fails. The row is dropped for that reason and the flag is authoritative.
///
/// **AN UNREADABLE CONSTRAINT BECOMES ABSENCE, WHICH UNDER-REPORTS.** A token whose type this
/// binder cannot name yields nothing rather than a guess, so a call that violates it is a MISSED
/// diagnostic and never a false one -- the same direction `is_sealed` takes for an imported type.
fn decode_constraints(
    assembly: &Assembly,
    entry: Option<&(u32, Vec<lamella_token::Token>)>,
) -> TypeParameterConstraints {
    let mut result = TypeParameterConstraints::default();
    let Some((flags, tokens)) = entry else {
        return result;
    };
    result.reference_type = flags & 0x0004 != 0;
    result.value_type = flags & 0x0008 != 0;
    result.default_constructor = flags & 0x0010 != 0 && !result.value_type;
    for &token in tokens {
        let symbol = token_type_symbol(assembly, token);
        if symbol.is_error() {
            continue;
        }
        if result.value_type && matches!(&symbol, TypeSymbol::Named(parts)
            if parts.len() == 2 && &*parts[0] == "System" && &*parts[1] == "ValueType")
        {
            continue;
        }
        result.types.push(symbol);
    }
    result
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
    own_parameters: &[&str],
    required_members: &BTreeSet<lamella_token::Token>,
    sets_required: &BTreeSet<lamella_token::Token>,
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
        .then(|| base_type_symbol(assembly, extends, own_parameters))
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
        let symbol = base_type_symbol(assembly, interface, own_parameters);
        if !symbol.is_error() {
            info.bases.push(symbol);
        }
    }
    for field in type_def.fields() {
        if let (Some(field_name), Some(signature)) = (field.name(), field.signature()) {
            let constant = field.constant().and_then(constant_to_literal);
            info.fields.push(FieldSymbol {
                name: field_name.into(),
                ty: sigtype_to_symbol(assembly, &signature, &[], own_parameters),
                is_static: field.flags() & 0x0010 != 0
                    && (field.flags() & 0x0040 == 0 || constant.is_some()),
                is_readonly: field.flags() & 0x0020 != 0,
                is_volatile: false,
                accessibility: member_accessibility(field.flags()),
                constant,
                is_required: required_members.contains(&field.token()),
            });
        }
    }
    let required_properties: Vec<&str> = type_def
        .properties()
        .filter(|property| required_members.contains(&property.token()))
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
        let method_own_parameters: &[&str] = method_type_parameters
            .get(&method.rid())
            .map_or(&[], Vec::as_slice);
        let symbol = MethodSymbol {
            explicit_interface: None,
            name: method_name.into(),
            return_type: sigtype_to_symbol(
                assembly,
                &signature.return_type,
                method_own_parameters,
                own_parameters,
            ),
            parameters: signature
                .parameters
                .iter()
                .map(|parameter| {
                    sigtype_to_symbol(assembly, parameter, method_own_parameters, own_parameters)
                })
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
            type_parameter_constraints: Vec::new(),
            conditional: conditional.get(&method.rid()).cloned().unwrap_or_default(),
            sets_required_members: sets_required.contains(&method.token()),
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
///
/// `method_parameters` names the enclosing METHOD's `!!n`; `declaring_parameters` names the
/// DECLARING TYPE's `!n`. **They are separate numbering spaces and a signature may mention both**,
/// so neither list may stand in for the other -- `Dictionary<TKey,TValue>.TryGetValue(TKey, out
/// TValue)` is `!0` and `!1` while a `TryGetValue<T>` beside it would be `!!0`.
fn sigtype_to_symbol(
    assembly: &Assembly,
    sig: &SigType,
    method_parameters: &[&str],
    declaring_parameters: &[&str],
) -> TypeSymbol {
    if let Some(special) = primitive_symbol(sig) {
        return special;
    }
    let recur = |inner: &SigType| {
        sigtype_to_symbol(assembly, inner, method_parameters, declaring_parameters)
    };
    match sig {
        SigType::IntPtr => named_symbol("System", "IntPtr"),
        SigType::UIntPtr => named_symbol("System", "UIntPtr"),
        SigType::TypedByRef => named_symbol("System", "TypedReference"),
        SigType::Class(token) | SigType::ValueType(token) => token_type_symbol(assembly, *token),
        SigType::SzArray(element) => recur(element).into_array(1),
        SigType::Array { element, rank } => recur(element).into_array(*rank as u8),
        SigType::ByRef(referent) => TypeSymbol::ByRef(Box::new(recur(referent))),
        SigType::Pointer(referent) => TypeSymbol::Pointer(Box::new(recur(referent))),
        SigType::MVar(index) => match method_parameters.get(*index as usize) {
            Some(name) => TypeSymbol::Named([Box::from(*name)].into()),
            None => TypeSymbol::Error,
        },
        SigType::Var(index) => match declaring_parameters.get(*index as usize) {
            Some(name) => TypeSymbol::Named([Box::from(*name)].into()),
            None => TypeSymbol::Error,
        },
        SigType::GenericInst {
            definition,
            arguments,
        } => {
            let named = match definition.as_ref() {
                SigType::Class(token) | SigType::ValueType(token) => {
                    token_type_symbol(assembly, *token)
                }
                _ => return TypeSymbol::Error,
            };
            let TypeSymbol::Named(parts) = named else {
                return TypeSymbol::Error;
            };
            let mut parts = parts.into_vec();
            let Some(last) = parts.last_mut() else {
                return TypeSymbol::Error;
            };
            *last = crate::symbols::unmangled_type_name(&last[..]);
            let decoded: Vec<TypeSymbol> = arguments.iter().map(|a| recur(a)).collect();
            if decoded.is_empty() || decoded.iter().any(|a| a.is_error()) {
                return TypeSymbol::Error;
            }
            TypeSymbol::Instantiation {
                definition: parts.into(),
                arguments: decoded.into(),
            }
        }
        SigType::Void
        | SigType::Boolean
        | SigType::Char
        | SigType::I1
        | SigType::U1
        | SigType::I2
        | SigType::U2
        | SigType::I4
        | SigType::U4
        | SigType::I8
        | SigType::U8
        | SigType::R4
        | SigType::R8
        | SigType::String
        | SigType::Object => primitive_symbol(sig).unwrap_or(TypeSymbol::Error),
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

/// Resolves a token in a type's BASE LIST -- `extends` (II.22.37) or an `InterfaceImpl`
/// (II.22.23) -- to a symbol, **following a `TypeSpec` instead of dropping it**.
///
/// [`token_type_symbol`] answers the error type for a `TypeSpec` by design: it maps a NAME, and a
/// spec row has none. **A constructed generic base can only be spelled as a spec**, so a base list
/// read through names alone silently loses every generic base class and every generic interface --
/// `` List`1 `` implements `` IList`1<!0> `` through a spec, so `IList<int> il = someList;` was
/// CS0266 on a correct program while every non-generic interface on the same type resolved.
///
/// **THIS IS THE SAME MISSING CAPABILITY THAT BREAKS THE AOT TIER'S INTERFACE TAG AND A `catch`
/// ON A GENERIC TYPE** -- three consumers of "name a `TypeSpec`", each found separately, each read
/// as its own feature. The repair is the same in all of them: decode the spec's signature rather
/// than asking for a name that does not exist.
///
/// `own_parameters` names the DECLARING type's `!n`, which a base's arguments mention.
fn base_type_symbol(assembly: &Assembly, token: Token, own_parameters: &[&str]) -> TypeSymbol {
    if token.table() == table::TYPE_SPEC {
        return match assembly.type_spec_signature(token) {
            Some(signature) => sigtype_to_symbol(assembly, &signature, &[], own_parameters),
            None => TypeSymbol::Error,
        };
    }
    token_type_symbol(assembly, token)
}

/// Resolves a `TypeDef`/`TypeRef` token to a named type symbol (the error type for
/// a `TypeSpec` or an unresolved token).
///
/// **A NESTED TYPE IS NAMED BY ITS ENCLOSING TYPE, NOT BY ITS OWN NAMESPACE, WHICH IS EMPTY**
/// (II.22.37), so the name comes from [`Assembly::type_token_full_name`] rather than off the row.
/// Reading the row directly yields a bare `Enumerator`, and a bare name is then resolved by a
/// unique-simple-name search across every referenced assembly -- which answered
/// `List<T>.Enumerator` with `System.Diagnostics.Activity.Enumerator`1` out of a diagnostics
/// package, because that one happened to be found first.
///
/// **The walk is the shared one on purpose.** [`type_info`] applies the same rule when it
/// REGISTERS a nested type, and `lamella-load` applies it again when it INDEXES one; the three
/// have to agree or a signature naming a nested type resolves to something the model never
/// registered. It is one function in `lamella-metadata` for that reason, not three walks that
/// happen to match today.
fn token_type_symbol(assembly: &Assembly, token: Token) -> TypeSymbol {
    let Some((namespace, name)) = assembly.type_token_full_name(token) else {
        return TypeSymbol::Error;
    };
    match special_for_named(&namespace, &name) {
        Some(special) => TypeSymbol::Special(special),
        None => named_symbol(&namespace, &name),
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
