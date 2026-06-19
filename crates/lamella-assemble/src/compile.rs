//! Compiling a bound program to a managed PE: the bridge over the whole back end.

use crate::method::{emit_body, max_stack};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use lamella_binder::{
    Binder, Diagnostic, SpecialType, TypeSymbol, bind_compilation_unit, bind_type, collect_model,
};
use lamella_cil::{MethodBodyImage, write_method_body};
use lamella_pe::{ImageBuilder, TypeSig, local_signature, method_signature};
use lamella_syntax::ast::{
    CompilationUnit, Member, Modifier, NamespaceMember, Parameter, QualifiedName, TypeDecl,
};
use lamella_token::Token;

const PUBLIC_CLASS: u32 = 0x0000_0001;
const METHOD_PUBLIC: u16 = 0x0006;
const METHOD_STATIC: u16 = 0x0010;
const IL_MANAGED: u16 = 0x0000;

/// The outcome of compiling a unit: its diagnostics and, when they are clean and
/// emission succeeds, the assembled image.
pub struct Compilation {
    /// The semantic diagnostics from binding.
    pub diagnostics: Vec<Diagnostic>,
    /// The assembled PE image, or `None` when binding failed or a construct is not
    /// lowered yet.
    pub image: Option<Vec<u8>>,
}

/// Binds and assembles `unit` into a managed library named `assembly_name`.
#[must_use]
pub fn compile_unit(unit: &CompilationUnit, module_name: &str, assembly_name: &str) -> Compilation {
    let diagnostics = bind_compilation_unit(unit);
    if !diagnostics.is_empty() {
        return Compilation {
            diagnostics,
            image: None,
        };
    }
    let image = build_image(unit, module_name, assembly_name).ok();
    Compilation { diagnostics, image }
}

fn build_image(
    unit: &CompilationUnit,
    module_name: &str,
    assembly_name: &str,
) -> Result<Vec<u8>, crate::EmitError> {
    let mut binder = Binder::with_model(collect_model(unit));
    let mut image = ImageBuilder::new(module_name, assembly_name);
    let object = image.object_type();
    let mut entry_point = None;
    emit_namespace(
        &mut image,
        &mut binder,
        object,
        &mut entry_point,
        &unit.members,
        "",
    )?;
    let is_dll = entry_point.is_none();
    Ok(image.finish(entry_point.unwrap_or(Token::new(0, 0)), is_dll))
}

fn emit_namespace(
    image: &mut ImageBuilder,
    binder: &mut Binder,
    object: Token,
    entry_point: &mut Option<Token>,
    members: &[NamespaceMember],
    namespace: &str,
) -> Result<(), crate::EmitError> {
    for member in members {
        match member {
            NamespaceMember::Type(declaration) => {
                emit_type(image, binder, object, entry_point, namespace, declaration)?;
            }
            NamespaceMember::Namespace(declaration) => {
                let inner = join_namespace(namespace, &declaration.name);
                emit_namespace(
                    image,
                    binder,
                    object,
                    entry_point,
                    &declaration.members,
                    &inner,
                )?;
            }
            NamespaceMember::Enum(_) | NamespaceMember::Delegate(_) => {}
        }
    }
    Ok(())
}

fn emit_type(
    image: &mut ImageBuilder,
    binder: &mut Binder,
    object: Token,
    entry_point: &mut Option<Token>,
    namespace: &str,
    declaration: &TypeDecl,
) -> Result<(), crate::EmitError> {
    image.add_type(namespace, &declaration.name, object, PUBLIC_CLASS);
    let enclosing = named_symbol(namespace, &declaration.name);
    for member in &declaration.members {
        if let Member::Method {
            modifiers,
            return_type,
            name,
            parameters,
            body: Some(body),
            ..
        } = member
        {
            let token = emit_one_method(
                image,
                binder,
                &enclosing,
                modifiers,
                name,
                return_type,
                parameters,
                body,
            )?;
            if entry_point.is_none() && &**name == "Main" && modifiers.contains(&Modifier::Static) {
                *entry_point = Some(token);
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_one_method(
    image: &mut ImageBuilder,
    binder: &mut Binder,
    enclosing: &TypeSymbol,
    modifiers: &[Modifier],
    name: &str,
    return_type: &lamella_syntax::ast::TypeRef,
    parameters: &[Parameter],
    body: &lamella_syntax::ast::Stmt,
) -> Result<Token, crate::EmitError> {
    let return_symbol = bind_type(return_type);
    let params: Vec<(Box<str>, TypeSymbol)> = parameters
        .iter()
        .map(|parameter| (parameter.name.clone(), bind_type(&parameter.ty)))
        .collect();

    let bound = binder.bind_method(
        Some(enclosing.clone()),
        name,
        return_symbol.clone(),
        &params,
        body,
    );

    let parameter_names: Vec<Box<str>> = params.iter().map(|(name, _)| name.clone()).collect();
    let (code, local_types) = emit_body(&parameter_names, &bound)?;
    let local_var_sig = if local_types.is_empty() {
        None
    } else {
        let locals: Vec<TypeSig> = local_types.iter().map(type_sig).collect::<Result<_, _>>()?;
        Some(image.add_standalone_sig(&local_signature(&locals)))
    };
    let body_image = MethodBodyImage {
        max_stack: max_stack(&code),
        init_locals: local_var_sig.is_some(),
        local_var_sig,
        code: code.into_boxed_slice(),
        handlers: Box::new([]),
    };
    let body_bytes = write_method_body(&body_image)
        .map_err(|_| crate::EmitError::Unsupported("method body could not be written"))?;

    let is_static = modifiers.contains(&Modifier::Static);
    let parameter_sigs: Vec<TypeSig> = params
        .iter()
        .map(|(_, ty)| type_sig(ty))
        .collect::<Result<_, _>>()?;
    let signature = method_signature(!is_static, &parameter_sigs, &type_sig(&return_symbol)?);

    let flags = METHOD_PUBLIC | if is_static { METHOD_STATIC } else { 0 };
    Ok(image.add_method(name, &signature, &body_bytes, flags, IL_MANAGED))
}

/// Maps a bound type to its signature form. Named and array types come later.
fn type_sig(ty: &TypeSymbol) -> Result<TypeSig, crate::EmitError> {
    let TypeSymbol::Special(special) = ty else {
        return Err(crate::EmitError::Unsupported(
            "named and array types in signatures come later",
        ));
    };
    Ok(match special {
        SpecialType::Void => TypeSig::Void,
        SpecialType::Boolean => TypeSig::Boolean,
        SpecialType::Char => TypeSig::Char,
        SpecialType::SByte => TypeSig::SByte,
        SpecialType::Byte => TypeSig::Byte,
        SpecialType::Int16 => TypeSig::Int16,
        SpecialType::UInt16 => TypeSig::UInt16,
        SpecialType::Int32 => TypeSig::Int32,
        SpecialType::UInt32 => TypeSig::UInt32,
        SpecialType::Int64 => TypeSig::Int64,
        SpecialType::UInt64 => TypeSig::UInt64,
        SpecialType::Single => TypeSig::Single,
        SpecialType::Double => TypeSig::Double,
        SpecialType::String => TypeSig::String,
        SpecialType::Object => TypeSig::Object,
        _ => {
            return Err(crate::EmitError::Unsupported(
                "this primitive type has no signature mapping yet",
            ));
        }
    })
}

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

fn join_namespace(outer: &str, name: &QualifiedName) -> String {
    let mut joined = String::from(outer);
    for part in &name.parts {
        if !joined.is_empty() {
            joined.push('.');
        }
        joined.push_str(part);
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_syntax::parser::parse_compilation_unit;

    #[test]
    fn compiles_a_method_to_a_round_trippable_dll() {
        let unit = parse_compilation_unit(
            "namespace App { public class Program { \
                public static int Answer() { return 42; } \
                public static int Add(int a, int b) { return a + b; } \
                public static int Square(int n) { int r = n * n; return r; } \
             } }",
        )
        .unit;

        let result = compile_unit(&unit, "app.dll", "app");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let image = result.image.expect("an image");

        let pe = lamella_metadata::pe::PeImage::parse(&image).expect("valid PE");
        assert_eq!(pe.cli_header_rva(), lamella_pe::pe::TEXT_RVA);
        assert!(lamella_metadata::image::MetadataImage::read(&image).is_ok());
    }

    #[test]
    fn binding_errors_block_emission() {
        let unit = parse_compilation_unit("class C { int M() { return \"s\"; } }").unit;
        let result = compile_unit(&unit, "c.dll", "c");
        assert!(!result.diagnostics.is_empty());
        assert!(result.image.is_none());
    }
}
