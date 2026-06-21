//! Compiling a bound program to a managed PE: the bridge over the whole back end.

use crate::debug::LineMap;
use crate::expr::is_value_type;
use crate::method::{ConstructorPrologue, EmittedBody, emit_body, max_stack};
use crate::tokens::Tokens;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use lamella_binder::{
    Binder, BoundExpr, BoundExprKind, BoundStmt, BoundStmtKind, ConversionKind,
    Diagnostic as BinderDiagnostic, Model, SpecialType, TypeSymbol,
    bind_compilation_unit_with_references, bind_type, collect_into, load_assembly,
};
use lamella_cil::{Instruction, MethodBodyImage, encode_with_offsets, write_method_body};
use lamella_metadata::Assembly;
use lamella_metadata::signature::element;
use lamella_pe::{
    ImageBuilder, LocalVariable, MethodDebug, SequencePoint, TypeSig, field_signature,
    local_signature, method_signature, property_signature, type_signature,
};
use lamella_syntax::ast::{
    AssignmentOperator, CompilationUnit, ConstructorInitializer, ConstructorInitializerKind,
    DelegateDecl, EnumDecl, Expr, ExprKind, Literal, Member, Modifier, NamespaceMember, Parameter,
    ParameterModifier, QualifiedName, Stmt, StmtKind, TypeDecl, TypeKind, TypeRef, UsingDirective,
    UsingKind, VariableDeclarator, explicit_interface_member_name,
};
use lamella_syntax::diagnostic::{Diagnostic as SyntaxDiagnostic, Severity};
use lamella_syntax::parser::parse_compilation_unit;
use lamella_syntax::span::Span;
use lamella_token::Token;

const TYPE_DEF: u8 = 0x02;
const FIELD: u8 = 0x04;
const METHOD_DEF: u8 = 0x06;
const PUBLIC_CLASS: u32 = 0x0000_0001;
const PUBLIC_STRUCT: u32 = 0x0000_0001 | 0x0000_0008 | 0x0000_0100;
const METHOD_PUBLIC: u16 = 0x0006;
const METHOD_PRIVATE: u16 = 0x0001;
const METHOD_STATIC: u16 = 0x0010;
const METHOD_VIRTUAL: u16 = 0x0040;
const METHOD_HIDEBYSIG: u16 = 0x0080;
const METHOD_NEWSLOT: u16 = 0x0100;
const METHOD_FINAL: u16 = 0x0020;
const METHOD_ABSTRACT: u16 = 0x0400;
const INTERFACE_FLAGS: u32 = 0x0000_0001 | 0x0000_0020 | 0x0000_0080;
const IFACE_METHOD_FLAGS: u16 =
    METHOD_PUBLIC | METHOD_VIRTUAL | METHOD_ABSTRACT | METHOD_NEWSLOT | METHOD_HIDEBYSIG;
const DELEGATE_TYPE_FLAGS: u32 = 0x0000_0001 | 0x0000_0100;
const DELEGATE_CTOR_FLAGS: u16 = METHOD_PUBLIC | METHOD_HIDEBYSIG | 0x0800 | 0x1000;
const DELEGATE_INVOKE_FLAGS: u16 =
    METHOD_PUBLIC | METHOD_HIDEBYSIG | METHOD_VIRTUAL | METHOD_NEWSLOT;
const FIELD_PUBLIC: u16 = 0x0006;
const FIELD_STATIC: u16 = 0x0010;
const CTOR_FLAGS: u16 = 0x0006 | 0x0800 | 0x1000;
const CCTOR_FLAGS: u16 = 0x0001 | METHOD_STATIC | METHOD_HIDEBYSIG | 0x0800 | 0x1000;
const SPECIAL_NAME: u16 = 0x0800;
const IL_MANAGED: u16 = 0x0000;
const FINALIZE_FLAGS: u16 = 0x0004 | METHOD_VIRTUAL | METHOD_HIDEBYSIG;
const ENUM_TYPE_FLAGS: u32 = 0x0000_0001 | 0x0000_0100;
const ENUM_VALUE_FIELD_FLAGS: u16 = FIELD_PUBLIC | 0x0200 | 0x0400;
const ENUM_MEMBER_FIELD_FLAGS: u16 = FIELD_PUBLIC | FIELD_STATIC | 0x0040 | 0x8000;

/// A diagnostic from any stage of compilation -- parsing or binding -- reduced to
/// what a driver reports: the `CSxxxx` code, the rendered message, and the span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// The C# compiler code (`CSxxxx`).
    pub code: u16,
    /// Whether it stops compilation (an error) or not (a warning).
    pub severity: Severity,
    /// The rendered message.
    pub message: String,
    /// The source location.
    pub span: Span,
}

impl Diagnostic {
    fn from_syntax(diagnostic: &SyntaxDiagnostic) -> Diagnostic {
        Diagnostic {
            code: diagnostic.code(),
            severity: diagnostic.severity(),
            message: format!("{}", diagnostic.kind),
            span: diagnostic.span,
        }
    }

    fn from_binder(diagnostic: &BinderDiagnostic) -> Diagnostic {
        Diagnostic {
            code: diagnostic.code(),
            severity: diagnostic.severity(),
            message: format!("{}", diagnostic.kind),
            span: diagnostic.span,
        }
    }

    /// Whether this diagnostic is an error (and so blocks emission).
    #[must_use]
    pub fn is_error(&self) -> bool {
        matches!(self.severity, Severity::Error)
    }
}

/// The outcome of compiling a unit: its diagnostics and, when they are clean and
/// emission succeeds, the assembled image.
pub struct Compilation {
    /// The diagnostics from parsing and binding, in that order.
    pub diagnostics: Vec<Diagnostic>,
    /// The assembled PE image, or `None` when binding failed or a construct is not
    /// lowered yet.
    pub image: Option<Vec<u8>>,
    /// The standalone Portable PDB, when debug info was requested and emitted.
    pub pdb: Option<Vec<u8>>,
    /// Why emission produced no image, when binding was clean but a construct is
    /// not lowered yet.
    pub emit_error: Option<crate::EmitError>,
}

/// Binds and assembles `unit` into a managed library named `assembly_name`.
#[must_use]
pub fn compile_unit(unit: &CompilationUnit, module_name: &str, assembly_name: &str) -> Compilation {
    compile_unit_with_references(unit, module_name, assembly_name, &[])
}

/// Binds and assembles `unit` against `references` (the BCL), so it can call into
/// and name types from those assemblies.
#[must_use]
pub fn compile_unit_with_references(
    unit: &CompilationUnit,
    module_name: &str,
    assembly_name: &str,
    references: &[Assembly],
) -> Compilation {
    compile(unit, module_name, assembly_name, references, None)
}

/// Like [`compile_unit_with_references`], but also emits a standalone Portable PDB
/// attributing the code to `source_path` (with `source` as the document text for
/// line/column mapping). The PDB lands in [`Compilation::pdb`].
#[must_use]
pub fn compile_unit_with_debug(
    unit: &CompilationUnit,
    module_name: &str,
    assembly_name: &str,
    references: &[Assembly],
    source: &str,
    source_path: &str,
) -> Compilation {
    compile(
        unit,
        module_name,
        assembly_name,
        references,
        Some((source, source_path)),
    )
}

/// Parses, binds, and assembles `source` end to end -- the driver entry. Parse
/// diagnostics and binder diagnostics both reach [`Compilation::diagnostics`]. A
/// syntax error blocks binding (so a broken tree cannot spray cascading binder
/// diagnostics) and emission. `source_path` names the source for the PDB, emitted
/// when `emit_debug` is set.
#[must_use]
pub fn compile_source(
    source: &str,
    source_path: &str,
    module_name: &str,
    assembly_name: &str,
    references: &[Assembly],
    emit_debug: bool,
) -> Compilation {
    let parsed = parse_compilation_unit(source);
    let parse_diagnostics: Vec<Diagnostic> = parsed
        .diagnostics
        .iter()
        .map(Diagnostic::from_syntax)
        .collect();
    if parse_diagnostics.iter().any(Diagnostic::is_error) {
        return Compilation {
            diagnostics: parse_diagnostics,
            image: None,
            pdb: None,
            emit_error: None,
        };
    }
    let debug = emit_debug.then_some((source, source_path));
    let mut compiled = compile(&parsed.unit, module_name, assembly_name, references, debug);
    if !parse_diagnostics.is_empty() {
        let mut diagnostics = parse_diagnostics;
        diagnostics.append(&mut compiled.diagnostics);
        compiled.diagnostics = diagnostics;
    }
    compiled
}

fn compile(
    unit: &CompilationUnit,
    module_name: &str,
    assembly_name: &str,
    references: &[Assembly],
    debug: Option<(&str, &str)>,
) -> Compilation {
    let diagnostics: Vec<Diagnostic> = bind_compilation_unit_with_references(unit, references)
        .iter()
        .map(Diagnostic::from_binder)
        .collect();
    if diagnostics.iter().any(Diagnostic::is_error) {
        return Compilation {
            diagnostics,
            image: None,
            pdb: None,
            emit_error: None,
        };
    }
    match build_image(unit, module_name, assembly_name, references, debug) {
        Ok((image, pdb)) => Compilation {
            diagnostics,
            image: Some(image),
            pdb,
            emit_error: None,
        },
        Err(error) => Compilation {
            diagnostics,
            image: None,
            pdb: None,
            emit_error: Some(error),
        },
    }
}

/// The binder model for `unit` over its references: the reference types first,
/// then the unit's own, with the base chain linked across both.
fn reference_model(unit: &CompilationUnit, references: &[Assembly]) -> Model {
    let mut model = Model::new();
    for reference in references {
        load_assembly(&mut model, reference);
    }
    collect_into(&mut model, unit);
    model.link_bases();
    model
}

fn build_image(
    unit: &CompilationUnit,
    module_name: &str,
    assembly_name: &str,
    references: &[Assembly],
    debug: Option<(&str, &str)>,
) -> Result<(Vec<u8>, Option<Vec<u8>>), crate::EmitError> {
    let mut tokens = assign_tokens(unit);
    let mut binder = Binder::with_model(reference_model(unit, references));
    let mut image = ImageBuilder::new(module_name, assembly_name);
    let object = image.object_type();
    let mut entry_point = None;
    let context = debug.map(|(source, _)| DebugContext {
        source,
        lines: LineMap::new(source),
    });
    emit_namespace(
        &mut image,
        &mut binder,
        object,
        &mut tokens,
        &mut entry_point,
        &unit.usings,
        &unit.members,
        "",
        context.as_ref(),
    )?;
    let is_dll = entry_point.is_none();
    let entry = entry_point.unwrap_or(Token::new(0, 0));
    let pdb = debug.map(|(_, path)| image.build_pdb(path, entry));
    let image = match debug {
        Some(_) => image.finish_with_debug(entry, is_dll, &pdb_file_name(module_name)),
        None => image.finish(entry, is_dll),
    };
    Ok((image, pdb))
}

/// The PDB file name beside an assembly: the module name with a `.pdb` extension.
fn pdb_file_name(module_name: &str) -> String {
    let stem = module_name
        .rsplit_once('.')
        .map_or(module_name, |(stem, _)| stem);
    let mut name = String::from(stem);
    name.push_str(".pdb");
    name
}

/// Source context for resolving a statement's span to line/column while emitting.
struct DebugContext<'a> {
    source: &'a str,
    lines: LineMap,
}

#[allow(clippy::too_many_arguments)]
fn emit_namespace(
    image: &mut ImageBuilder,
    binder: &mut Binder,
    object: Token,
    tokens: &mut Tokens,
    entry_point: &mut Option<Token>,
    usings: &[UsingDirective],
    members: &[NamespaceMember],
    namespace: &str,
    debug: Option<&DebugContext>,
) -> Result<(), crate::EmitError> {
    let scope = binder.import_scope();
    for using in usings {
        if let UsingKind::Namespace(name) = &using.kind {
            binder.import_namespace(&join_namespace("", name));
        }
    }
    for member in members {
        match member {
            NamespaceMember::Type(declaration) => {
                emit_type(
                    image,
                    binder,
                    object,
                    tokens,
                    entry_point,
                    namespace,
                    declaration,
                    debug,
                )?;
                let enclosing_full = qualified_dotted(namespace, &declaration.name);
                for member in &declaration.members {
                    if let Member::NestedType(nested) = member {
                        if matches!(
                            nested.as_ref(),
                            NamespaceMember::Type(_)
                                | NamespaceMember::Enum(_)
                                | NamespaceMember::Delegate(_)
                        ) {
                            emit_namespace(
                                image,
                                binder,
                                object,
                                tokens,
                                entry_point,
                                &[],
                                core::slice::from_ref(nested.as_ref()),
                                &enclosing_full,
                                debug,
                            )?;
                        }
                    }
                }
            }
            NamespaceMember::Namespace(declaration) => {
                let inner = join_namespace(namespace, &declaration.name);
                emit_namespace(
                    image,
                    binder,
                    object,
                    tokens,
                    entry_point,
                    &declaration.usings,
                    &declaration.members,
                    &inner,
                    debug,
                )?;
            }
            NamespaceMember::Delegate(declaration) => {
                emit_delegate(image, tokens, namespace, declaration)?;
            }
            NamespaceMember::Enum(declaration) => {
                emit_enum(image, binder, tokens, namespace, declaration)?;
            }
        }
    }
    binder.restore_import_scope(scope);
    Ok(())
}

/// Emits an interface as a `TypeDef` with no base, no constructor, and abstract
/// methods (II.22.37 semantics). Implementing classes get an `InterfaceImpl` row.
fn emit_interface(
    image: &mut ImageBuilder,
    tokens: &Tokens,
    namespace: &str,
    declaration: &TypeDecl,
) -> Result<(), crate::EmitError> {
    let nil = Token::new(TYPE_DEF, 0);
    image.add_type(namespace, &declaration.name, nil, INTERFACE_FLAGS);
    for member in &declaration.members {
        if let Member::Method {
            return_type,
            name,
            parameters,
            ..
        } = member
        {
            let parameter_sigs: Vec<TypeSig> = parameters
                .iter()
                .map(|parameter| type_sig(tokens, &bind_type(&parameter.ty)))
                .collect::<Result<_, _>>()?;
            let signature = method_signature(
                true,
                &parameter_sigs,
                &type_sig(tokens, &bind_type(return_type))?,
            );
            image.add_abstract_method(name, &signature, IFACE_METHOD_FLAGS);
        }
    }
    Ok(())
}

/// Emits a delegate as a sealed class extending `System.MulticastDelegate`, with its
/// runtime-implemented `.ctor(object, native int)` and `Invoke(params) -> ret`. The
/// runtime supplies both bodies; `new D(method)` is `ldftn` + `newobj .ctor`, and
/// `d(args)` is `callvirt Invoke`.
fn emit_delegate(
    image: &mut ImageBuilder,
    tokens: &Tokens,
    namespace: &str,
    declaration: &DelegateDecl,
) -> Result<(), crate::EmitError> {
    let base = image.type_ref("System", "MulticastDelegate");
    image.add_type(namespace, &declaration.name, base, DELEGATE_TYPE_FLAGS);
    let ctor_signature =
        method_signature(true, &[TypeSig::Object, TypeSig::NativeInt], &TypeSig::Void);
    image.add_runtime_method(".ctor", &ctor_signature, DELEGATE_CTOR_FLAGS);
    let return_sig = type_sig(tokens, &bind_type(&declaration.return_type))?;
    let parameter_sigs: Vec<TypeSig> = declaration
        .parameters
        .iter()
        .map(|parameter| type_sig(tokens, &bind_type(&parameter.ty)))
        .collect::<Result<_, _>>()?;
    let invoke_signature = method_signature(true, &parameter_sigs, &return_sig);
    image.add_runtime_method("Invoke", &invoke_signature, DELEGATE_INVOKE_FLAGS);
    Ok(())
}

/// Emits an enum as a `TypeDef` extending `System.Enum`: a `value__` instance field
/// of the underlying integral type, then one `static literal` field per member
/// carrying its `Constant` value (II.14.3). Member reads fold to integer constants,
/// so these fields exist for reflection -- `typeof`, `Enum.Parse`/`ToString`, and
/// boxing (the box names the enum type). The `TypeDef` token and the Field rows were
/// reserved by the token pre-pass, so later types stay aligned.
fn emit_enum(
    image: &mut ImageBuilder,
    binder: &Binder,
    tokens: &Tokens,
    namespace: &str,
    declaration: &EnumDecl,
) -> Result<(), crate::EmitError> {
    let enum_ty = named_symbol(namespace, &declaration.name);
    let enum_token = tokens
        .type_token(&enum_ty)
        .ok_or(crate::EmitError::Unsupported("an enum with no reserved token"))?;
    let underlying = declaration
        .base
        .as_ref()
        .map(bind_type)
        .unwrap_or(TypeSymbol::Special(SpecialType::Int32));
    let (constant_element, constant_width) = enum_constant_encoding(&underlying)?;

    let base = image.type_ref("System", "Enum");
    let enclosing = binder
        .model()
        .get_by_symbol(&enum_ty)
        .and_then(|info| info.enclosing.clone());
    let (metadata_namespace, flags) = match &enclosing {
        Some(_) => ("", (ENUM_TYPE_FLAGS & !0x0000_0007) | 0x0000_0002),
        None => (namespace, ENUM_TYPE_FLAGS),
    };
    let enum_type_token = image.add_type(metadata_namespace, &declaration.name, base, flags);
    if let Some(enclosing_full) = &enclosing {
        if let Some(enclosing_token) = tokens.type_token(&type_symbol_from_dotted(enclosing_full)) {
            image.add_nested_class(enum_type_token, enclosing_token);
        }
    }
    let value_field_sig = field_signature(&type_sig(tokens, &underlying)?);
    image.add_field("value__", &value_field_sig, ENUM_VALUE_FIELD_FLAGS);
    let member_field_sig = field_signature(&TypeSig::ValueType(enum_token));
    let members: Vec<(Box<str>, i64)> = binder
        .model()
        .get_by_symbol(&enum_ty)
        .map(|info| {
            info.fields
                .iter()
                .map(|field| (field.name.clone(), field.constant.unwrap_or(0)))
                .collect()
        })
        .unwrap_or_default();
    for (name, value) in members {
        let field = image.add_field(&name, &member_field_sig, ENUM_MEMBER_FIELD_FLAGS);
        image.add_constant(field, constant_element, &value.to_le_bytes()[..constant_width]);
    }
    Ok(())
}

/// The `Constant`-table element-type byte and little-endian byte width for an enum's
/// underlying integral type (II.23.1.16). The default is `int`; `long`/`ulong` are
/// the wide forms the runtime tracks for values past `int32`.
fn enum_constant_encoding(underlying: &TypeSymbol) -> Result<(u8, usize), crate::EmitError> {
    let TypeSymbol::Special(special) = underlying else {
        return Err(crate::EmitError::Unsupported(
            "an enum underlying type that is not a primitive",
        ));
    };
    Ok(match special {
        SpecialType::SByte => (element::I1, 1),
        SpecialType::Byte => (element::U1, 1),
        SpecialType::Int16 => (element::I2, 2),
        SpecialType::UInt16 => (element::U2, 2),
        SpecialType::Char => (element::CHAR, 2),
        SpecialType::Int32 => (element::I4, 4),
        SpecialType::UInt32 => (element::U4, 4),
        SpecialType::Int64 => (element::I8, 8),
        SpecialType::UInt64 => (element::U8, 8),
        _ => {
            return Err(crate::EmitError::Unsupported(
                "an enum underlying type that is not integral",
            ));
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn emit_type(
    image: &mut ImageBuilder,
    binder: &mut Binder,
    object: Token,
    tokens: &mut Tokens,
    entry_point: &mut Option<Token>,
    namespace: &str,
    declaration: &TypeDecl,
    debug: Option<&DebugContext>,
) -> Result<(), crate::EmitError> {
    let is_struct = declaration.kind == TypeKind::Struct;
    let enclosing = named_symbol(namespace, &declaration.name);
    if matches!(declaration.kind, TypeKind::Interface) {
        return emit_interface(image, tokens, namespace, declaration);
    }
    let (base_class, nested_in): (Option<TypeSymbol>, Option<Box<str>>) = {
        let info = binder.model().get_by_symbol(&enclosing);
        let base = if is_struct {
            None
        } else {
            info.and_then(|info| info.base.clone())
        };
        (base, info.and_then(|info| info.enclosing.clone()))
    };
    let (base, flags) = if is_struct {
        (image.type_ref("System", "ValueType"), PUBLIC_STRUCT)
    } else {
        let base_token = base_class
            .as_ref()
            .and_then(|symbol| {
                tokens.type_token(symbol).or_else(|| {
                    split_type_name(symbol)
                        .map(|(namespace, name)| image.type_ref(&namespace, &name))
                })
            })
            .unwrap_or(object);
        (base_token, PUBLIC_CLASS)
    };
    let (metadata_namespace, flags) = if nested_in.is_some() {
        ("", (flags & !0x0000_0007) | 0x0000_0002)
    } else {
        (namespace, flags)
    };
    let type_token = image.add_type(metadata_namespace, &declaration.name, base, flags);
    if let Some(enclosing_full) = &nested_in {
        if let Some(enclosing_token) = tokens.type_token(&type_symbol_from_dotted(enclosing_full)) {
            image.add_nested_class(type_token, enclosing_token);
        }
    }
    let interface_tokens: Vec<Token> = {
        let model = binder.model();
        model
            .get_by_symbol(&enclosing)
            .map(|info| {
                info.bases
                    .iter()
                    .filter(|base| {
                        model
                            .get_by_symbol(base)
                            .is_some_and(|b| b.kind == lamella_binder::TypeKind::Interface)
                    })
                    .filter_map(|base| tokens.type_token(base))
                    .collect()
            })
            .unwrap_or_default()
    };
    let implements_interface = !interface_tokens.is_empty();
    for interface in interface_tokens {
        image.add_interface_impl(type_token, interface);
    }
    for member in &declaration.members {
        if let Member::Field {
            modifiers,
            ty,
            declarators,
            ..
        } = member
        {
            emit_field(image, tokens, modifiers, ty, declarators)?;
        }
        if let Member::EventField {
            modifiers,
            ty,
            declarators,
            ..
        } = member
        {
            emit_field(image, tokens, modifiers, ty, declarators)?;
        }
    }
    if !is_struct && !declares_instance_constructor(declaration) {
        let base_ctor = base_class
            .as_ref()
            .and_then(|symbol| tokens.method(symbol, ".ctor", &[]))
            .unwrap_or_else(|| image.object_ctor());
        let body = Stmt::new(StmtKind::Block(Vec::new()), declaration.span);
        emit_constructor(
            image,
            binder,
            &enclosing,
            tokens,
            declaration,
            &[],
            None,
            &body,
            Some(base_ctor),
            debug,
        )?;
    }
    if needs_static_constructor(declaration) {
        let mut statements = static_field_initializer_statements(declaration);
        if let Some(static_body) = static_constructor_body(declaration) {
            statements.push(static_body.clone());
        }
        let body = Stmt::new(StmtKind::Block(statements), declaration.span);
        emit_method_body(
            image,
            binder,
            tokens,
            &enclosing,
            ".cctor",
            &TypeSymbol::Special(SpecialType::Void),
            &[],
            &[],
            &body,
            true,
            CCTOR_FLAGS,
            None,
            debug,
        )?;
    }
    for member in &declaration.members {
        match member {
            Member::Method {
                modifiers,
                return_type,
                name,
                parameters,
                body: Some(body),
                explicit_interface,
                ..
            } => {
                let token = emit_one_method(
                    image,
                    binder,
                    &enclosing,
                    tokens,
                    modifiers,
                    name,
                    return_type,
                    parameters,
                    body,
                    implements_interface,
                    explicit_interface.as_ref(),
                    debug,
                )?;
                if entry_point.is_none()
                    && &**name == "Main"
                    && modifiers.contains(&Modifier::Static)
                {
                    *entry_point = Some(token);
                }
            }
            Member::Operator {
                modifiers,
                return_type,
                operator,
                parameters,
                body,
                ..
            } => {
                emit_one_method(
                    image,
                    binder,
                    &enclosing,
                    tokens,
                    modifiers,
                    operator.method_name(parameters.len()),
                    return_type,
                    parameters,
                    body,
                    implements_interface,
                    None,
                    debug,
                )?;
            }
            Member::ConversionOperator {
                modifiers,
                direction,
                target,
                parameters,
                body,
                ..
            } => {
                emit_one_method(
                    image,
                    binder,
                    &enclosing,
                    tokens,
                    modifiers,
                    direction.method_name(),
                    target,
                    parameters,
                    body,
                    implements_interface,
                    None,
                    debug,
                )?;
            }
            Member::Constructor {
                modifiers,
                parameters,
                initializer,
                body,
                ..
            } if !is_static_constructor(modifiers) => {
                let base_ctor = if is_struct {
                    None
                } else {
                    Some(
                        base_class
                            .as_ref()
                            .and_then(|symbol| tokens.method(symbol, ".ctor", &[]))
                            .unwrap_or_else(|| image.object_ctor()),
                    )
                };
                emit_constructor(
                    image,
                    binder,
                    &enclosing,
                    tokens,
                    declaration,
                    parameters,
                    initializer.as_ref(),
                    body,
                    base_ctor,
                    debug,
                )?;
            }
            Member::Destructor { body, .. } => {
                emit_destructor(image, binder, &enclosing, tokens, body, debug)?;
            }
            _ => {}
        }
    }
    let mut first_property = None;
    for member in &declaration.members {
        if let Member::Property {
            modifiers,
            ty,
            name,
            getter,
            setter,
            ..
        } = member
        {
            let property = emit_property(
                image,
                binder,
                tokens,
                &enclosing,
                modifiers,
                ty,
                name,
                getter.as_ref().and_then(|accessor| accessor.body.as_ref()),
                setter.as_ref().and_then(|accessor| accessor.body.as_ref()),
                debug,
            )?;
            first_property.get_or_insert(property);
        }
    }
    if let Some(first) = first_property {
        image.add_property_map(type_token, first);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_one_method(
    image: &mut ImageBuilder,
    binder: &mut Binder,
    enclosing: &TypeSymbol,
    tokens: &mut Tokens,
    modifiers: &[Modifier],
    name: &str,
    return_type: &TypeRef,
    parameters: &[Parameter],
    body: &Stmt,
    interface_impl: bool,
    explicit_interface: Option<&TypeRef>,
    debug: Option<&DebugContext>,
) -> Result<Token, crate::EmitError> {
    let return_symbol = bind_type(return_type);
    let params: Vec<(Box<str>, TypeSymbol)> = parameters
        .iter()
        .map(|parameter| (parameter.name.clone(), bind_type(&parameter.ty)))
        .collect();
    let byref_flags = byref_flags(parameters);
    if let Some(interface) = explicit_interface {
        let method_name = explicit_interface_member_name(interface, name);
        let flags =
            METHOD_PRIVATE | METHOD_VIRTUAL | METHOD_FINAL | METHOD_NEWSLOT | METHOD_HIDEBYSIG;
        let body_token = emit_method_body(
            image,
            binder,
            tokens,
            enclosing,
            &method_name,
            &return_symbol,
            &params,
            &byref_flags,
            body,
            false,
            flags,
            None,
            debug,
        )?;
        emit_explicit_interface_impl(
            image,
            tokens,
            enclosing,
            interface,
            name,
            &params,
            &return_symbol,
            body_token,
        )?;
        return Ok(body_token);
    }
    let is_static = modifiers.contains(&Modifier::Static);
    let is_virtual = modifiers.contains(&Modifier::Virtual);
    let is_override = modifiers.contains(&Modifier::Override);
    let mut flags = METHOD_PUBLIC;
    if is_static {
        flags |= METHOD_STATIC;
    }
    if is_virtual || is_override {
        flags |= METHOD_VIRTUAL | METHOD_HIDEBYSIG;
        if is_virtual {
            flags |= METHOD_NEWSLOT;
        }
    } else if interface_impl && !is_static {
        flags |= METHOD_VIRTUAL | METHOD_NEWSLOT | METHOD_FINAL | METHOD_HIDEBYSIG;
    }
    emit_method_body(
        image,
        binder,
        tokens,
        enclosing,
        name,
        &return_symbol,
        &params,
        &byref_flags,
        body,
        is_static,
        flags,
        None,
        debug,
    )
}

/// Emits the `MethodImpl` row that wires an explicit interface implementation: it
/// links `body` (the class's own private `MethodDef`) to the interface method it
/// overrides. The interface method is a this-module `MethodDef` when the interface is
/// declared here, otherwise a minted `MemberRef` to the BCL interface method.
#[allow(clippy::too_many_arguments)]
fn emit_explicit_interface_impl(
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
    enclosing: &TypeSymbol,
    interface: &TypeRef,
    member: &str,
    params: &[(Box<str>, TypeSymbol)],
    return_symbol: &TypeSymbol,
    body: Token,
) -> Result<(), crate::EmitError> {
    let class = tokens
        .type_token(enclosing)
        .ok_or(crate::EmitError::Unsupported(
            "an explicit interface impl on a type with no metadata token",
        ))?;
    let interface_symbol = bind_type(interface);
    let parameter_types: Vec<TypeSymbol> = params.iter().map(|(_, ty)| ty.clone()).collect();
    let declaration = match tokens.method(&interface_symbol, member, &parameter_types) {
        Some(token) => token,
        None => {
            let (namespace, name) =
                split_type_name(&interface_symbol).ok_or(crate::EmitError::Unsupported(
                    "an explicit interface impl of an unresolvable interface",
                ))?;
            let parameter_sigs: Vec<TypeSig> = parameter_types
                .iter()
                .map(|ty| type_sig(tokens, ty))
                .collect::<Result<_, _>>()?;
            let signature =
                method_signature(true, &parameter_sigs, &type_sig(tokens, return_symbol)?);
            let type_ref = image.type_ref(&namespace, &name);
            let member_token = image.member_ref(type_ref, member, &signature);
            tokens.insert_method(&interface_symbol, member, &parameter_types, member_token);
            member_token
        }
    };
    image.add_method_impl(class, body, declaration);
    Ok(())
}

/// The `ref`/`out` (byref) flag of each parameter, in order -- parallel to the bound
/// parameter list, driving the byref signature and the deref of body reads/writes.
fn byref_flags(parameters: &[Parameter]) -> Vec<bool> {
    parameters
        .iter()
        .map(|parameter| {
            matches!(
                parameter.modifier,
                Some(ParameterModifier::Ref | ParameterModifier::Out)
            )
        })
        .collect()
}

/// Emits an explicit constructor as an instance `.ctor`. A class constructor chains to
/// `base_ctor` first (`ldarg.0; call base..ctor`); a struct (`base_ctor` is `None`)
/// has no base constructor and just initializes its fields through `this`. `new T(args)`
/// lowers to `newobj` of the token this records.
#[allow(clippy::too_many_arguments)]
fn emit_constructor(
    image: &mut ImageBuilder,
    binder: &mut Binder,
    enclosing: &TypeSymbol,
    tokens: &mut Tokens,
    declaration: &TypeDecl,
    parameters: &[Parameter],
    initializer: Option<&ConstructorInitializer>,
    body: &lamella_syntax::ast::Stmt,
    base_ctor: Option<Token>,
    debug: Option<&DebugContext>,
) -> Result<Token, crate::EmitError> {
    let params: Vec<(Box<str>, TypeSymbol)> = parameters
        .iter()
        .map(|parameter| (parameter.name.clone(), bind_type(&parameter.ty)))
        .collect();
    let base_prologue = || base_ctor.map(|ctor| ConstructorPrologue {
        ctor,
        arguments: Vec::new(),
    });
    let prologue = match initializer {
        Some(init) => binder
            .bind_constructor_chain(enclosing, &params, init)
            .map(|(method, arguments)| {
                let ctor = tokens
                    .method(&method.declaring_type, ".ctor", &method.parameters)
                    .unwrap_or_else(|| {
                        mint_member_ref(&method, image, tokens);
                        tokens
                            .method(&method.declaring_type, ".ctor", &method.parameters)
                            .unwrap_or_else(|| image.object_ctor())
                    });
                ConstructorPrologue { ctor, arguments }
            })
            .or_else(base_prologue),
        None => base_prologue(),
    };
    let chains_to_this = matches!(
        initializer.map(|init| &init.kind),
        Some(ConstructorInitializerKind::This)
    );
    let body = if chains_to_this {
        body.clone()
    } else {
        body_with_field_initializers(declaration, body)
    };
    emit_method_body(
        image,
        binder,
        tokens,
        enclosing,
        ".ctor",
        &TypeSymbol::Special(SpecialType::Void),
        &params,
        &byref_flags(parameters),
        &body,
        false,
        CTOR_FLAGS,
        prologue.as_ref(),
        debug,
    )
}

/// Emits a destructor as the parameterless `Finalize` override -- a `family virtual`
/// method reusing System.Object::Finalize's slot, so a dropped object's body runs at
/// finalization. The body is emitted directly; deriving from System.Object, the base
/// finalizer is a no-op, so no base call is synthesized.
fn emit_destructor(
    image: &mut ImageBuilder,
    binder: &mut Binder,
    enclosing: &TypeSymbol,
    tokens: &mut Tokens,
    body: &lamella_syntax::ast::Stmt,
    debug: Option<&DebugContext>,
) -> Result<(), crate::EmitError> {
    emit_method_body(
        image,
        binder,
        tokens,
        enclosing,
        "Finalize",
        &TypeSymbol::Special(SpecialType::Void),
        &[],
        &[],
        body,
        false,
        FINALIZE_FLAGS,
        None,
        debug,
    )?;
    Ok(())
}

/// Binds a method body, lowers it to CIL, and adds the `MethodDef`, returning its
/// token. Shared by ordinary methods, constructors' callers, and property
/// accessors -- the parameters and return are already bound symbols.
#[allow(clippy::too_many_arguments)]
fn emit_method_body(
    image: &mut ImageBuilder,
    binder: &mut Binder,
    tokens: &mut Tokens,
    enclosing: &TypeSymbol,
    name: &str,
    return_symbol: &TypeSymbol,
    params: &[(Box<str>, TypeSymbol)],
    byref_flags: &[bool],
    body: &lamella_syntax::ast::Stmt,
    is_static: bool,
    flags: u16,
    prologue: Option<&ConstructorPrologue>,
    debug: Option<&DebugContext>,
) -> Result<Token, crate::EmitError> {
    let bound = binder.bind_method(
        Some(enclosing.clone()),
        name,
        return_symbol.clone(),
        params,
        body,
    );

    mint_references(&bound, image, tokens);

    let arg_base = u16::from(!is_static);
    let parameter_names: Vec<Box<str>> = params.iter().map(|(name, _)| name.clone()).collect();
    let byref_params: Vec<(Box<str>, TypeSymbol)> = params
        .iter()
        .enumerate()
        .filter(|(index, _)| byref_flags.get(*index).copied().unwrap_or(false))
        .map(|(_, (name, ty))| (name.clone(), ty.clone()))
        .collect();
    let EmittedBody {
        code,
        local_types,
        local_names,
        sequence_points,
        handlers,
        pinned_slots,
    } = emit_body(
        &parameter_names,
        &byref_params,
        &bound,
        tokens,
        arg_base,
        return_symbol,
        prologue,
    )?;
    let local_var_sig = if local_types.is_empty() {
        None
    } else {
        let locals: Vec<TypeSig> = local_types
            .iter()
            .enumerate()
            .map(|(slot, ty)| {
                let sig = type_sig(tokens, ty)?;
                Ok(if pinned_slots.contains(&(slot as u16)) {
                    TypeSig::Pinned(Box::new(sig))
                } else {
                    sig
                })
            })
            .collect::<Result<_, crate::EmitError>>()?;
        Some(image.add_standalone_sig(&local_signature(&locals)))
    };
    let local_signature_rid = local_var_sig.map_or(0, Token::row);

    let method_debug = debug
        .map(|context| {
            build_method_debug(
                &code,
                &sequence_points,
                &local_names,
                local_signature_rid,
                context,
            )
        })
        .transpose()?;

    let max_stack = if handlers.is_empty() {
        max_stack(&code)
    } else {
        max_stack(&code).max(1)
    };
    let body_image = MethodBodyImage {
        max_stack,
        init_locals: local_var_sig.is_some(),
        local_var_sig,
        code: code.into_boxed_slice(),
        handlers: handlers.into_boxed_slice(),
    };
    let body_bytes = write_method_body(&body_image)
        .map_err(|_| crate::EmitError::Unsupported("method body could not be written"))?;

    let parameter_sigs: Vec<TypeSig> = params
        .iter()
        .enumerate()
        .map(|(index, (_, ty))| {
            let sig = type_sig(tokens, ty)?;
            Ok(if byref_flags.get(index).copied().unwrap_or(false) {
                TypeSig::ByRef(Box::new(sig))
            } else {
                sig
            })
        })
        .collect::<Result<_, _>>()?;
    let signature = method_signature(
        !is_static,
        &parameter_sigs,
        &type_sig(tokens, return_symbol)?,
    );
    let method = image.add_method(
        name,
        &signature,
        &body_bytes,
        flags,
        IL_MANAGED,
        &parameter_names,
    );
    if let Some(debug) = method_debug {
        image.set_method_debug(method, debug);
    }
    Ok(method)
}

/// Builds a method's [`MethodDebug`]: its sequence points (instruction byte offsets
/// via `encode_with_offsets`, spans to line/column via the line map), its named
/// locals (slot index plus name), and the body's IL length for the local scope.
fn build_method_debug(
    code: &[Instruction],
    points: &[crate::method::SequencePoint],
    local_names: &[Box<str>],
    local_signature: u32,
    context: &DebugContext,
) -> Result<MethodDebug, crate::EmitError> {
    let (code_bytes, offsets) = encode_with_offsets(code)
        .map_err(|_| crate::EmitError::Unsupported("method body could not be encoded"))?;
    let sequence_points = points
        .iter()
        .map(|(index, span)| {
            let lines = context.lines.span_lines(context.source, *span);
            SequencePoint {
                il_offset: offsets[*index as usize],
                start_line: lines.start_line,
                start_column: lines.start_column,
                end_line: lines.end_line,
                end_column: lines.end_column,
            }
        })
        .collect();
    let locals = local_names
        .iter()
        .enumerate()
        .map(|(index, name)| LocalVariable {
            index: index as u16,
            name: name.clone(),
        })
        .collect();
    Ok(MethodDebug {
        sequence_points,
        local_signature,
        locals,
        scope_length: code_bytes.len() as u32,
    })
}

/// Emits a property's accessors as `get_Name`/`set_Name` methods (a getter
/// returning the property type, a setter taking `value`).
const SEMANTICS_SETTER: u16 = 0x0001;
const SEMANTICS_GETTER: u16 = 0x0002;

#[allow(clippy::too_many_arguments)]
fn emit_property(
    image: &mut ImageBuilder,
    binder: &mut Binder,
    tokens: &mut Tokens,
    enclosing: &TypeSymbol,
    modifiers: &[Modifier],
    ty: &lamella_syntax::ast::TypeRef,
    name: &str,
    getter_body: Option<&lamella_syntax::ast::Stmt>,
    setter_body: Option<&lamella_syntax::ast::Stmt>,
    debug: Option<&DebugContext>,
) -> Result<Token, crate::EmitError> {
    let property_ty = bind_type(ty);
    let is_static = modifiers.contains(&Modifier::Static);
    let flags = METHOD_PUBLIC | SPECIAL_NAME | if is_static { METHOD_STATIC } else { 0 };

    let signature = property_signature(!is_static, &type_sig(tokens, &property_ty)?);
    let property = image.add_property(name, &signature, 0);

    if let Some(body) = getter_body {
        let getter = emit_method_body(
            image,
            binder,
            tokens,
            enclosing,
            &accessor_name("get_", name),
            &property_ty,
            &[],
            &[],
            body,
            is_static,
            flags,
            None,
            debug,
        )?;
        image.add_method_semantics(SEMANTICS_GETTER, getter, property);
    }
    if let Some(body) = setter_body {
        let setter = emit_method_body(
            image,
            binder,
            tokens,
            enclosing,
            &accessor_name("set_", name),
            &TypeSymbol::Special(SpecialType::Void),
            &[(Box::from("value"), property_ty.clone())],
            &[],
            body,
            is_static,
            flags,
            None,
            debug,
        )?;
        image.add_method_semantics(SEMANTICS_SETTER, setter, property);
    }
    Ok(property)
}

/// The `get_`/`set_` accessor method name for a property.
fn accessor_name(prefix: &str, property: &str) -> String {
    let mut name = String::from(prefix);
    name.push_str(property);
    name
}

/// Adds a `Field` row per declarator, with the field's signature and flags. Field
/// initializers (which would run in a constructor) are not emitted yet.
fn emit_field(
    image: &mut ImageBuilder,
    tokens: &Tokens,
    modifiers: &[Modifier],
    ty: &lamella_syntax::ast::TypeRef,
    declarators: &[VariableDeclarator],
) -> Result<(), crate::EmitError> {
    let signature = field_signature(&type_sig(tokens, &bind_type(ty))?);
    let flags = FIELD_PUBLIC
        | if modifiers.contains(&Modifier::Static) {
            FIELD_STATIC
        } else {
            0
        };
    for declarator in declarators {
        image.add_field(&declarator.name, &signature, flags);
    }
    Ok(())
}

/// Walks a bound body, minting tokens for the things it references so emission can
/// look them up: string literals go into the `#US` heap.
fn mint_references(stmt: &BoundStmt, image: &mut ImageBuilder, tokens: &mut Tokens) {
    match &stmt.kind {
        BoundStmtKind::Block(statements) => {
            for statement in statements {
                mint_references(statement, image, tokens);
            }
        }
        BoundStmtKind::Local { ty, declarators } => {
            mint_named_type_token(ty, image, tokens);
            for declarator in declarators {
                if let Some(initializer) = &declarator.initializer {
                    mint_in_expr(initializer, image, tokens);
                }
            }
        }
        BoundStmtKind::Expression(expr) => mint_in_expr(expr, image, tokens),
        BoundStmtKind::Return(Some(value)) => mint_in_expr(value, image, tokens),
        BoundStmtKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            mint_in_expr(condition, image, tokens);
            mint_references(then_branch, image, tokens);
            if let Some(else_branch) = else_branch {
                mint_references(else_branch, image, tokens);
            }
        }
        BoundStmtKind::While { condition, body } | BoundStmtKind::DoWhile { body, condition } => {
            mint_in_expr(condition, image, tokens);
            mint_references(body, image, tokens);
        }
        BoundStmtKind::For {
            initializer,
            condition,
            iterators,
            body,
        } => {
            for statement in initializer {
                mint_references(statement, image, tokens);
            }
            if let Some(condition) = condition {
                mint_in_expr(condition, image, tokens);
            }
            for iterator in iterators {
                mint_in_expr(iterator, image, tokens);
            }
            mint_references(body, image, tokens);
        }
        BoundStmtKind::Checked(inner) | BoundStmtKind::Unchecked(inner) => {
            mint_references(inner, image, tokens);
        }
        BoundStmtKind::Throw(Some(expr)) => mint_in_expr(expr, image, tokens),
        BoundStmtKind::Try {
            body,
            catches,
            finally,
        } => {
            mint_references(body, image, tokens);
            for catch in catches {
                let ty = catch
                    .exception_type
                    .clone()
                    .unwrap_or(TypeSymbol::Special(SpecialType::Object));
                mint_type_token(image, tokens, &ty);
                mint_references(&catch.body, image, tokens);
            }
            if let Some(finally) = finally {
                mint_references(finally, image, tokens);
            }
        }
        BoundStmtKind::Switch {
            expression,
            sections,
        } => {
            mint_in_expr(expression, image, tokens);
            let mut has_string_case = false;
            for section in sections {
                for label in &section.labels {
                    if let lamella_binder::BoundSwitchLabel::CaseString(text) = label {
                        let token = image.user_string(text);
                        tokens.insert_string(text, token);
                        has_string_case = true;
                    }
                }
                for statement in &section.statements {
                    mint_references(statement, image, tokens);
                }
            }
            if has_string_case {
                mint_member_ref(&string_equality_reference(), image, tokens);
            }
        }
        BoundStmtKind::ForEach {
            collection, body, ..
        } => {
            mint_in_expr(collection, image, tokens);
            mint_references(body, image, tokens);
        }
        BoundStmtKind::Fixed {
            element,
            init,
            body,
            ..
        } => {
            mint_in_expr(init, image, tokens);
            mint_type_token(image, tokens, element);
            mint_references(body, image, tokens);
        }
        _ => {}
    }
}

/// Mints tokens an expression and its sub-expressions reference.
fn mint_in_expr(expr: &BoundExpr, image: &mut ImageBuilder, tokens: &mut Tokens) {
    match &expr.kind {
        BoundExprKind::Literal(Literal::String(text)) => {
            let token = image.user_string(text);
            tokens.insert_string(text, token);
        }
        BoundExprKind::Binary {
            left,
            right,
            operator,
            ..
        } => {
            mint_in_expr(left, image, tokens);
            mint_in_expr(right, image, tokens);
            use lamella_syntax::ast::BinaryOperator as Op;
            if matches!(operator, Op::Add) && is_string(&expr.ty) {
                let both = is_string(&left.ty) && is_string(&right.ty);
                mint_member_ref(&string_concat_reference(both), image, tokens);
            } else if matches!(operator, Op::Equal | Op::NotEqual)
                && is_string(&left.ty)
                && is_string(&right.ty)
            {
                mint_member_ref(&string_equality_reference(), image, tokens);
            }
        }
        BoundExprKind::Unary { operand, .. }
        | BoundExprKind::Postfix { operand, .. }
        | BoundExprKind::Ref { operand, .. } => {
            mint_in_expr(operand, image, tokens);
        }
        BoundExprKind::Conversion {
            operand,
            conversion,
        } => {
            mint_in_expr(operand, image, tokens);
            if matches!(conversion, ConversionKind::Boxing) {
                mint_value_type_token(&operand.ty, image, tokens);
            }
        }
        BoundExprKind::Cast { operand, .. } => {
            mint_in_expr(operand, image, tokens);
            if matches!(operand.ty, TypeSymbol::Special(SpecialType::Object))
                && is_value_type(&expr.ty, tokens)
            {
                mint_value_type_token(&expr.ty, image, tokens);
            }
        }
        BoundExprKind::Checked(inner) | BoundExprKind::Unchecked(inner) => {
            mint_in_expr(inner, image, tokens);
        }
        BoundExprKind::Call {
            callee,
            arguments,
            method,
        } => {
            mint_in_expr(callee, image, tokens);
            for argument in arguments {
                mint_in_expr(argument, image, tokens);
            }
            if let (BoundExprKind::MethodGroup { receiver, .. }, Some(method)) =
                (&callee.kind, method)
            {
                if is_value_type(&receiver.ty, tokens) && method.declaring_type != receiver.ty {
                    mint_value_type_token(&receiver.ty, image, tokens);
                }
            }
            if let Some(method) = method {
                if tokens
                    .method(&method.declaring_type, &method.name, &method.parameters)
                    .is_none()
                {
                    mint_member_ref(method, image, tokens);
                }
            }
        }
        BoundExprKind::ObjectCreation {
            arguments,
            constructor,
        } => {
            for argument in arguments {
                mint_in_expr(argument, image, tokens);
            }
            if let Some(constructor) = constructor {
                if tokens
                    .method(
                        &constructor.declaring_type,
                        &constructor.name,
                        &constructor.parameters,
                    )
                    .is_none()
                {
                    mint_member_ref(constructor, image, tokens);
                }
            }
        }
        BoundExprKind::FieldAccess { receiver, .. }
        | BoundExprKind::MethodGroup { receiver, .. } => mint_in_expr(receiver, image, tokens),
        BoundExprKind::PropertyAccess {
            receiver,
            declaring_type,
            name,
        } => {
            mint_in_expr(receiver, image, tokens);
            let getter = lamella_binder::MethodReference {
                declaring_type: declaring_type.clone(),
                name: accessor_name("get_", name).into(),
                parameters: Vec::new(),
                return_type: expr.ty.clone(),
                is_static: matches!(receiver.kind, BoundExprKind::TypeReference(_)),
            };
            if tokens
                .method(&getter.declaring_type, &getter.name, &getter.parameters)
                .is_none()
            {
                mint_member_ref(&getter, image, tokens);
            }
        }
        BoundExprKind::ArrayCreation { lengths, elements } => {
            for length in lengths {
                mint_in_expr(length, image, tokens);
            }
            for element in elements {
                mint_in_expr(element, image, tokens);
            }
            if let TypeSymbol::Array { element, .. } = &expr.ty {
                mint_type_token(image, tokens, element);
            }
            mint_array_members(&expr.ty, image, tokens);
        }
        BoundExprKind::ElementAccess { receiver, indices } => {
            mint_in_expr(receiver, image, tokens);
            for index in indices {
                mint_in_expr(index, image, tokens);
            }
            mint_array_members(&receiver.ty, image, tokens);
            if matches!(receiver.ty, TypeSymbol::Special(SpecialType::String)) {
                let getter = lamella_binder::MethodReference {
                    declaring_type: TypeSymbol::Special(SpecialType::String),
                    name: "get_Chars".into(),
                    parameters: alloc::vec![TypeSymbol::Special(SpecialType::Int32)],
                    return_type: TypeSymbol::Special(SpecialType::Char),
                    is_static: false,
                };
                if tokens
                    .method(&getter.declaring_type, &getter.name, &getter.parameters)
                    .is_none()
                {
                    mint_member_ref(&getter, image, tokens);
                }
            }
        }
        BoundExprKind::Assignment {
            target,
            value,
            operator,
        } => {
            mint_in_expr(target, image, tokens);
            mint_in_expr(value, image, tokens);
            if matches!(operator, lamella_syntax::ast::AssignmentOperator::Add)
                && is_string(&target.ty)
            {
                mint_member_ref(&string_concat_reference(is_string(&value.ty)), image, tokens);
            }
        }
        BoundExprKind::Conditional {
            condition,
            when_true,
            when_false,
        } => {
            mint_in_expr(condition, image, tokens);
            mint_in_expr(when_true, image, tokens);
            mint_in_expr(when_false, image, tokens);
        }
        BoundExprKind::TypeTest { operand, target, .. } => {
            mint_in_expr(operand, image, tokens);
            mint_type_token(image, tokens, target);
        }
        BoundExprKind::TypeOf(target) => {
            mint_type_token(image, tokens, target);
            mint_type_token(image, tokens, &crate::expr::system_type_symbol());
            let handle = crate::expr::runtime_type_handle_symbol();
            mint_type_token(image, tokens, &handle);
            tokens.insert_struct(&handle);
            mint_member_ref(&get_type_from_handle_reference(), image, tokens);
        }
        _ => {}
    }
}

/// Mints a `MemberRef` for an external (BCL) method `method`: a `TypeRef` to its
/// declaring type, then a `MemberRef` with its encoded signature, recorded in the
/// token table. Skipped (left for emission to report) if a type cannot be encoded.
/// Whether `ty` is `string`.
fn is_string(ty: &TypeSymbol) -> bool {
    matches!(ty, TypeSymbol::Special(SpecialType::String))
}

/// `System.Type System.Type::GetTypeFromHandle(System.RuntimeTypeHandle)` -- the
/// method the `typeof` lowering calls to turn a `ldtoken` handle into a `Type`.
fn get_type_from_handle_reference() -> lamella_binder::MethodReference {
    lamella_binder::MethodReference {
        declaring_type: crate::expr::system_type_symbol(),
        name: "GetTypeFromHandle".into(),
        parameters: alloc::vec![crate::expr::runtime_type_handle_symbol()],
        return_type: crate::expr::system_type_symbol(),
        is_static: true,
    }
}

/// The `String.Concat` overload a concatenation lowers to: `Concat(string, string)` when
/// both operands are strings, otherwise `Concat(object, object)` (a non-string operand was
/// boxed/typed to object by the binder).
fn string_concat_reference(both_strings: bool) -> lamella_binder::MethodReference {
    let string = TypeSymbol::Special(SpecialType::String);
    let arg = TypeSymbol::Special(if both_strings {
        SpecialType::String
    } else {
        SpecialType::Object
    });
    lamella_binder::MethodReference {
        declaring_type: string.clone(),
        name: "Concat".into(),
        parameters: alloc::vec![arg.clone(), arg],
        return_type: string,
        is_static: true,
    }
}

/// `bool System.String::op_Equality(string, string)` -- string value equality, the
/// target of `string == string` (and, negated, `!=`).
fn string_equality_reference() -> lamella_binder::MethodReference {
    let string = TypeSymbol::Special(SpecialType::String);
    lamella_binder::MethodReference {
        declaring_type: string.clone(),
        name: "op_Equality".into(),
        parameters: alloc::vec![string.clone(), string.clone()],
        return_type: TypeSymbol::Special(SpecialType::Boolean),
        is_static: true,
    }
}

fn mint_member_ref(
    method: &lamella_binder::MethodReference,
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
) {
    let Some((namespace, name)) = split_type_name(&method.declaring_type) else {
        return;
    };
    mint_named_type_token(&method.declaring_type, image, tokens);
    for parameter in &method.parameters {
        mint_named_type_token(parameter, image, tokens);
    }
    mint_named_type_token(&method.return_type, image, tokens);
    let parameter_sigs: Result<Vec<TypeSig>, _> = method
        .parameters
        .iter()
        .map(|ty| type_sig(tokens, ty))
        .collect();
    let (Ok(parameter_sigs), Ok(return_sig)) =
        (parameter_sigs, type_sig(tokens, &method.return_type))
    else {
        return;
    };
    let signature = method_signature(!method.is_static, &parameter_sigs, &return_sig);
    let type_ref = image.type_ref(&namespace, &name);
    let member = image.member_ref(type_ref, &method.name, &signature);
    tokens.insert_method(
        &method.declaring_type,
        &method.name,
        &method.parameters,
        member,
    );
}

/// Mints a `TypeRef` token for a type used where a token is needed (e.g. an array
/// element type), unless one already exists (a source `TypeDef`, or a previously
/// minted ref). Primitives resolve to their `System` type in the BCL.
/// Mints a multi-dimensional array type's metadata: a `TypeSpec` for `T[,]` and the
/// `.ctor`/`Get`/`Set` member references the runtime synthesizes on it (II.14.2),
/// recorded under the array type so emission can name them. A no-op for a rank-1 array
/// (which uses the `newarr`/`ldelem`/`stelem` opcodes) or one already minted.
fn mint_array_members(array_ty: &TypeSymbol, image: &mut ImageBuilder, tokens: &mut Tokens) {
    let TypeSymbol::Array { element, rank } = array_ty else {
        return;
    };
    let rank = *rank as usize;
    if rank < 2 {
        return;
    }
    let int_params = crate::expr::array_int_params(rank);
    if tokens.method(array_ty, "Get", &int_params).is_some() {
        return;
    }
    let Ok(element_sig) = type_sig(tokens, element) else {
        return;
    };
    let array_sig = TypeSig::Array {
        element: Box::new(element_sig.clone()),
        rank: rank as u32,
    };
    let type_spec = image.type_spec(&type_signature(&array_sig));
    let int_sigs: Vec<TypeSig> = (0..rank).map(|_| TypeSig::Int32).collect();
    let ctor_sig = method_signature(true, &int_sigs, &TypeSig::Void);
    let ctor = image.member_ref(type_spec, ".ctor", &ctor_sig);
    tokens.insert_method(array_ty, ".ctor", &int_params, ctor);
    let get_sig = method_signature(true, &int_sigs, &element_sig);
    let get = image.member_ref(type_spec, "Get", &get_sig);
    tokens.insert_method(array_ty, "Get", &int_params, get);
    let mut set_sigs = int_sigs;
    set_sigs.push(element_sig);
    let set_sig = method_signature(true, &set_sigs, &TypeSig::Void);
    let set = image.member_ref(type_spec, "Set", &set_sig);
    let mut set_params = int_params;
    set_params.push((**element).clone());
    tokens.insert_method(array_ty, "Set", &set_params, set);
}

fn mint_type_token(image: &mut ImageBuilder, tokens: &mut Tokens, ty: &TypeSymbol) {
    if tokens.type_token(ty).is_some() {
        return;
    }
    let reference = match ty {
        TypeSymbol::Special(special) => {
            system_type_name(*special).map(|(namespace, name)| image.type_ref(namespace, name))
        }
        TypeSymbol::Named(_) => {
            split_type_name(ty).map(|(namespace, name)| image.type_ref(&namespace, &name))
        }
        TypeSymbol::Array { element, .. } => {
            mint_type_token(image, tokens, element);
            type_sig(tokens, ty)
                .ok()
                .map(|sig| image.type_spec(&type_signature(&sig)))
        }
        TypeSymbol::Pointer(element) => {
            mint_type_token(image, tokens, element);
            None
        }
        TypeSymbol::Error => None,
    };
    if let Some(token) = reference {
        tokens.insert_type(ty, token);
    }
}

/// The `System` namespace and name of a primitive type, for a `TypeRef`.
fn system_type_name(special: SpecialType) -> Option<(&'static str, &'static str)> {
    Some(match special {
        SpecialType::Boolean => ("System", "Boolean"),
        SpecialType::Byte => ("System", "Byte"),
        SpecialType::SByte => ("System", "SByte"),
        SpecialType::Int16 => ("System", "Int16"),
        SpecialType::UInt16 => ("System", "UInt16"),
        SpecialType::Int32 => ("System", "Int32"),
        SpecialType::UInt32 => ("System", "UInt32"),
        SpecialType::Int64 => ("System", "Int64"),
        SpecialType::UInt64 => ("System", "UInt64"),
        SpecialType::Char => ("System", "Char"),
        SpecialType::Single => ("System", "Single"),
        SpecialType::Double => ("System", "Double"),
        SpecialType::String => ("System", "String"),
        SpecialType::Object => ("System", "Object"),
        SpecialType::Decimal => ("System", "Decimal"),
        SpecialType::Void => return None,
    })
}

/// Mints + records a `TypeRef` for a named type used in a signature -- a BCL reference
/// type (StringBuilder, ArrayList, ...) or any named type not yet tokenized -- so
/// `type_sig` can encode it (a `Class`, or `ValueType` for a value type). A no-op for a
/// predefined type, an array, the error type, or a type already tokenized (a this-module
/// `TypeDef` or a previously minted ref).
fn mint_named_type_token(ty: &TypeSymbol, image: &mut ImageBuilder, tokens: &mut Tokens) {
    if !matches!(ty, TypeSymbol::Named(_)) || tokens.type_token(ty).is_some() {
        return;
    }
    if let Some((namespace, name)) = split_type_name(ty) {
        let token = image.type_ref(&namespace, &name);
        tokens.insert_type(ty, token);
    }
}

/// Mints the metadata token a `box`/`unbox.any` names for the value type `ty`. A
/// module struct already has its `TypeDef` token (nothing to do); a primitive needs a
/// `System.*` `TypeRef`.
fn mint_value_type_token(ty: &TypeSymbol, image: &mut ImageBuilder, tokens: &mut Tokens) {
    if tokens.type_token(ty).is_some() {
        return;
    }
    if let TypeSymbol::Special(special) = ty {
        if let Some((namespace, name)) = system_type_name(*special) {
            let token = image.type_ref(namespace, name);
            tokens.insert_type(ty, token);
        }
    }
}

/// Splits a named type into `(namespace, name)`, e.g. `System.Console` -> `("System",
/// "Console")`. Returns `None` for a non-named type.
fn split_type_name(ty: &TypeSymbol) -> Option<(String, String)> {
    if let TypeSymbol::Special(special) = ty {
        let (namespace, name) = system_type_name(*special)?;
        return Some((String::from(namespace), String::from(name)));
    }
    let TypeSymbol::Named(parts) = ty else {
        return None;
    };
    let (name, namespace) = parts.split_last()?;
    let namespace = namespace
        .iter()
        .map(|part| &**part)
        .collect::<Vec<&str>>()
        .join(".");
    Some((namespace, String::from(&**name)))
}


/// Synthesizes `this.<field> = <init>;` for each instance field initializer, in
/// declaration order. These run at the start of every constructor that chains to the
/// base (after the base call -- the strict before-base order is a v1 gap). Static and
/// const fields are excluded (a const folds; a static initializer is a separate gap).
fn field_initializer_statements(declaration: &TypeDecl) -> Vec<Stmt> {
    let mut statements = Vec::new();
    for member in &declaration.members {
        let Member::Field {
            modifiers,
            declarators,
            ..
        } = member
        else {
            continue;
        };
        if modifiers
            .iter()
            .any(|m| matches!(m, Modifier::Static | Modifier::Const))
        {
            continue;
        }
        for declarator in declarators {
            let Some(init) = &declarator.initializer else {
                continue;
            };
            let span = declarator.span;
            let target = Expr::new(
                ExprKind::MemberAccess {
                    receiver: Box::new(Expr::new(ExprKind::This, span)),
                    name: declarator.name.clone(),
                },
                span,
            );
            let assignment = Expr::new(
                ExprKind::Assignment {
                    operator: AssignmentOperator::Assign,
                    target: Box::new(target),
                    value: Box::new(init.clone()),
                },
                span,
            );
            statements.push(Stmt::new(StmtKind::Expression(assignment), span));
        }
    }
    statements
}

/// `body` with the type's field initializers prepended (as a block), so they run
/// before the rest of a constructor. Returns `body` unchanged when there are none.
fn body_with_field_initializers(declaration: &TypeDecl, body: &Stmt) -> Stmt {
    let mut statements = field_initializer_statements(declaration);
    if statements.is_empty() {
        return body.clone();
    }
    statements.push(body.clone());
    Stmt::new(StmtKind::Block(statements), body.span)
}

/// Whether `modifiers` mark a `static` constructor.
fn is_static_constructor(modifiers: &[Modifier]) -> bool {
    modifiers.iter().any(|m| matches!(m, Modifier::Static))
}

/// The body of the type's static constructor, if it declares one.
fn static_constructor_body(declaration: &TypeDecl) -> Option<&Stmt> {
    declaration.members.iter().find_map(|member| match member {
        Member::Constructor {
            modifiers, body, ..
        } if is_static_constructor(modifiers) => Some(body),
        _ => None,
    })
}

/// Synthesizes `<field> = <init>;` for each static (non-const) field initializer, in
/// declaration order -- the statements that run first in the static constructor.
fn static_field_initializer_statements(declaration: &TypeDecl) -> Vec<Stmt> {
    let mut statements = Vec::new();
    for member in &declaration.members {
        let Member::Field {
            modifiers,
            declarators,
            ..
        } = member
        else {
            continue;
        };
        let is_static = modifiers.iter().any(|m| matches!(m, Modifier::Static));
        let is_const = modifiers.iter().any(|m| matches!(m, Modifier::Const));
        if !is_static || is_const {
            continue;
        }
        for declarator in declarators {
            let Some(init) = &declarator.initializer else {
                continue;
            };
            let span = declarator.span;
            let target = Expr::new(ExprKind::Name(declarator.name.clone()), span);
            let assignment = Expr::new(
                ExprKind::Assignment {
                    operator: AssignmentOperator::Assign,
                    target: Box::new(target),
                    value: Box::new(init.clone()),
                },
                span,
            );
            statements.push(Stmt::new(StmtKind::Expression(assignment), span));
        }
    }
    statements
}

/// Whether the type needs a static constructor `.cctor`: it has a declared static
/// constructor or any static field initializer.
fn needs_static_constructor(declaration: &TypeDecl) -> bool {
    static_constructor_body(declaration).is_some()
        || !static_field_initializer_statements(declaration).is_empty()
}

/// Whether the type declares an INSTANCE constructor (a static constructor does not
/// suppress the implicit default instance one).
fn declares_instance_constructor(declaration: &TypeDecl) -> bool {
    declaration.members.iter().any(|member| {
        matches!(member, Member::Constructor { modifiers, .. } if !is_static_constructor(modifiers))
    })
}

/// Maps a bound type to its signature form. A named type resolves to the `Class`
/// of its `TypeDef` token; array types come later.
fn type_sig(tokens: &Tokens, ty: &TypeSymbol) -> Result<TypeSig, crate::EmitError> {
    let special = match ty {
        TypeSymbol::Special(special) => special,
        TypeSymbol::Named(_) if tokens.is_struct(ty) || tokens.is_enum(ty) => {
            return tokens.type_token(ty).map(TypeSig::ValueType).ok_or(
                crate::EmitError::Unsupported("a value type outside this module in a signature"),
            );
        }
        TypeSymbol::Named(_) => {
            return tokens
                .type_token(ty)
                .map(TypeSig::Class)
                .ok_or(crate::EmitError::Unsupported(
                    "a named type outside this module in a signature",
                ));
        }
        TypeSymbol::Array { element, .. } => {
            return Ok(TypeSig::SzArray(Box::new(type_sig(tokens, element)?)));
        }
        TypeSymbol::Pointer(element) => {
            return Ok(TypeSig::Pointer(Box::new(type_sig(tokens, element)?)));
        }
        TypeSymbol::Error => {
            return Err(crate::EmitError::Unsupported(
                "the error type has no signature",
            ));
        }
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

/// Walks the unit in emission order, assigning each method its `MethodDef` token
/// (`1..`) so a body can name a forward call. The order must match the emission
/// walk so the tokens line up with the rows `add_method` produces.
fn assign_tokens(unit: &CompilationUnit) -> Tokens {
    let mut tokens = Tokens::new();
    let mut next_type = 1u32;
    let mut next_field = 0u32;
    let mut next_method = 0u32;
    collect_tokens(
        &mut tokens,
        &mut next_type,
        &mut next_field,
        &mut next_method,
        &unit.members,
        "",
    );
    tokens
}

fn collect_tokens(
    tokens: &mut Tokens,
    next_type: &mut u32,
    next_field: &mut u32,
    next_method: &mut u32,
    members: &[NamespaceMember],
    namespace: &str,
) {
    for member in members {
        match member {
            NamespaceMember::Type(declaration) => {
                let declaring = named_symbol(namespace, &declaration.name);
                *next_type += 1;
                tokens.insert_type(&declaring, Token::new(TYPE_DEF, *next_type));
                let is_struct = declaration.kind == TypeKind::Struct;
                let is_interface = declaration.kind == TypeKind::Interface;
                if is_struct {
                    tokens.insert_struct(&declaring);
                }
                if is_interface {
                    tokens.insert_interface(&declaring);
                }
                for member in &declaration.members {
                    if let Member::Field { declarators, .. } = member {
                        for declarator in declarators {
                            *next_field += 1;
                            tokens.insert_field(
                                &declaring,
                                &declarator.name,
                                Token::new(FIELD, *next_field),
                            );
                        }
                    }
                    if let Member::EventField { declarators, .. } = member {
                        for declarator in declarators {
                            *next_field += 1;
                            tokens.insert_field(
                                &declaring,
                                &declarator.name,
                                Token::new(FIELD, *next_field),
                            );
                        }
                    }
                }
                if !is_struct && !is_interface && !declares_instance_constructor(declaration) {
                    *next_method += 1;
                    tokens.insert_method(
                        &declaring,
                        ".ctor",
                        &[],
                        Token::new(METHOD_DEF, *next_method),
                    );
                }
                if needs_static_constructor(declaration) {
                    *next_method += 1;
                    tokens.insert_method(
                        &declaring,
                        ".cctor",
                        &[],
                        Token::new(METHOD_DEF, *next_method),
                    );
                }
                for member in &declaration.members {
                    match member {
                        Member::Method {
                            name,
                            parameters,
                            body,
                            explicit_interface,
                            ..
                        } if body.is_some() || is_interface => {
                            *next_method += 1;
                            let params: Vec<TypeSymbol> =
                                parameters.iter().map(|p| bind_type(&p.ty)).collect();
                            let token = Token::new(METHOD_DEF, *next_method);
                            match explicit_interface {
                                Some(interface) => tokens.insert_method(
                                    &declaring,
                                    &explicit_interface_member_name(interface, name),
                                    &params,
                                    token,
                                ),
                                None => tokens.insert_method(&declaring, name, &params, token),
                            }
                        }
                        Member::Operator {
                            operator,
                            parameters,
                            ..
                        } => {
                            *next_method += 1;
                            let params: Vec<TypeSymbol> =
                                parameters.iter().map(|p| bind_type(&p.ty)).collect();
                            tokens.insert_method(
                                &declaring,
                                operator.method_name(parameters.len()),
                                &params,
                                Token::new(METHOD_DEF, *next_method),
                            );
                        }
                        Member::ConversionOperator {
                            direction,
                            parameters,
                            ..
                        } => {
                            *next_method += 1;
                            let params: Vec<TypeSymbol> =
                                parameters.iter().map(|p| bind_type(&p.ty)).collect();
                            tokens.insert_method(
                                &declaring,
                                direction.method_name(),
                                &params,
                                Token::new(METHOD_DEF, *next_method),
                            );
                        }
                        Member::Constructor {
                            modifiers,
                            parameters,
                            ..
                        } if !is_static_constructor(modifiers) => {
                            *next_method += 1;
                            let params: Vec<TypeSymbol> =
                                parameters.iter().map(|p| bind_type(&p.ty)).collect();
                            tokens.insert_method(
                                &declaring,
                                ".ctor",
                                &params,
                                Token::new(METHOD_DEF, *next_method),
                            );
                        }
                        Member::Destructor { .. } => {
                            *next_method += 1;
                            tokens.insert_method(
                                &declaring,
                                "Finalize",
                                &[],
                                Token::new(METHOD_DEF, *next_method),
                            );
                        }
                        _ => {}
                    }
                }
                for member in &declaration.members {
                    if let Member::Property {
                        ty,
                        name,
                        getter,
                        setter,
                        ..
                    } = member
                    {
                        let property_ty = bind_type(ty);
                        if getter.as_ref().and_then(|a| a.body.as_ref()).is_some() {
                            *next_method += 1;
                            tokens.insert_method(
                                &declaring,
                                &accessor_name("get_", name),
                                &[],
                                Token::new(METHOD_DEF, *next_method),
                            );
                        }
                        if setter.as_ref().and_then(|a| a.body.as_ref()).is_some() {
                            *next_method += 1;
                            tokens.insert_method(
                                &declaring,
                                &accessor_name("set_", name),
                                &[property_ty],
                                Token::new(METHOD_DEF, *next_method),
                            );
                        }
                    }
                }
                let enclosing_full = qualified_dotted(namespace, &declaration.name);
                for member in &declaration.members {
                    if let Member::NestedType(nested) = member {
                        if matches!(
                            nested.as_ref(),
                            NamespaceMember::Type(_)
                                | NamespaceMember::Enum(_)
                                | NamespaceMember::Delegate(_)
                        ) {
                            collect_tokens(
                                tokens,
                                next_type,
                                next_field,
                                next_method,
                                core::slice::from_ref(nested.as_ref()),
                                &enclosing_full,
                            );
                        }
                    }
                }
            }
            NamespaceMember::Namespace(declaration) => {
                let inner = join_namespace(namespace, &declaration.name);
                collect_tokens(
                    tokens,
                    next_type,
                    next_field,
                    next_method,
                    &declaration.members,
                    &inner,
                );
            }
            NamespaceMember::Enum(declaration) => {
                let enum_ty = named_symbol(namespace, &declaration.name);
                *next_type += 1;
                tokens.insert_type(&enum_ty, Token::new(TYPE_DEF, *next_type));
                tokens.insert_enum(&enum_ty);
                *next_field += 1 + declaration.members.len() as u32;
            }
            NamespaceMember::Delegate(declaration) => {
                let declaring = named_symbol(namespace, &declaration.name);
                *next_type += 1;
                tokens.insert_type(&declaring, Token::new(TYPE_DEF, *next_type));
                *next_method += 1;
                tokens.insert_method(
                    &declaring,
                    ".ctor",
                    &[],
                    Token::new(METHOD_DEF, *next_method),
                );
                *next_method += 1;
                let params: Vec<TypeSymbol> = declaration
                    .parameters
                    .iter()
                    .map(|p| bind_type(&p.ty))
                    .collect();
                tokens.insert_method(
                    &declaring,
                    "Invoke",
                    &params,
                    Token::new(METHOD_DEF, *next_method),
                );
            }
        }
    }
}

/// Joins a namespace (possibly empty) and a simple name into a dotted full name -- used
/// to key a nested type under its enclosing type (e.g. `"Outer"` + `"Inner"`).
fn qualified_dotted(namespace: &str, name: &str) -> String {
    if namespace.is_empty() {
        String::from(name)
    } else {
        format!("{namespace}.{name}")
    }
}

/// A named-type symbol from a dotted full name (e.g. `"Outer"` or `"N.Outer"`), matching
/// how the type was keyed in the token table.
fn type_symbol_from_dotted(full: &str) -> TypeSymbol {
    TypeSymbol::Named(full.split('.').map(Box::<str>::from).collect())
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
    fn type_spec_signature_decodes_a_2d_array() {
        let unit = parse_compilation_unit(
            "class Program { static int Main() { int[,] m = new int[2, 3]; \
                m[0, 0] = 42; return m[0, 0]; } }",
        )
        .unit;
        let result = compile_unit(&unit, "arr2d.dll", "arr2d");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let image = result.image.expect("an image");
        let assembly = Assembly::read(&image).expect("the reader parses the image");

        let get = assembly
            .member_refs()
            .find(|member| member.name() == Some("Get"))
            .expect("a Get member reference");
        let spec = get.parent();
        let sig = assembly
            .type_spec_signature(spec)
            .expect("the TypeSpec signature decodes");
        match sig {
            lamella_metadata::signature::SigType::Array { element, rank } => {
                assert_eq!(rank, 2);
                assert!(
                    matches!(*element, lamella_metadata::signature::SigType::I4),
                    "element was {element:?}"
                );
            }
            other => panic!("expected SigType::Array, got {other:?}"),
        }
    }

    #[test]
    fn compiles_a_static_call() {
        let unit = parse_compilation_unit(
            "class Program { \
                static int Main() { return Twice(21); } \
                static int Twice(int n) { return n + n; } \
             }",
        )
        .unit;
        let result = compile_unit(&unit, "call.dll", "call");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let image = result.image.expect("an image");
        assert!(lamella_metadata::image::MetadataImage::read(&image).is_ok());
    }

    #[test]
    fn compiles_static_field_access() {
        let unit = parse_compilation_unit(
            "class Program { \
                static int counter; \
                static int Main() { counter = 42; return counter; } \
             }",
        )
        .unit;
        let result = compile_unit(&unit, "field.dll", "field");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let image = result.image.expect("an image");
        assert!(lamella_metadata::image::MetadataImage::read(&image).is_ok());
    }

    #[test]
    fn compiles_object_creation_and_instance_fields() {
        let unit = parse_compilation_unit(
            "class Box { public int value; } \
             class Program { \
                static int Main() { Box b = new Box(); b.value = 42; return b.value; } \
             }",
        )
        .unit;
        let result = compile_unit(&unit, "box.dll", "box");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let image = result.image.expect("an image");
        assert!(lamella_metadata::image::MetadataImage::read(&image).is_ok());
    }

    #[test]
    fn compiles_instance_methods_and_void_calls() {
        let unit = parse_compilation_unit(
            "class Counter { \
                int n; \
                public void Add(int delta) { n = n + delta; } \
                public int Get() { return n; } \
             } \
             class Program { \
                static int Main() { Counter c = new Counter(); c.Add(40); c.Add(2); return c.Get(); } \
             }",
        )
        .unit;
        let result = compile_unit(&unit, "counter.dll", "counter");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some());
    }

    #[test]
    fn compiles_array_creation_and_element_access() {
        let unit = parse_compilation_unit(
            "class Program { \
                static int Main() { \
                    int[] a = new int[3]; \
                    a[0] = 10; a[1] = 20; a[2] = 12; \
                    return a[0] + a[1] + a[2]; \
                } \
             }",
        )
        .unit;
        let result = compile_unit(&unit, "arr.dll", "arr");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some(), "{:?}", result.emit_error);
    }

    #[test]
    fn compiles_properties() {
        let unit = parse_compilation_unit(
            "class Box { \
                int width; \
                public int Width { get { return width; } set { width = value; } } \
             } \
             class Program { \
                static int Main() { Box b = new Box(); b.Width = 42; return b.Width; } \
             }",
        )
        .unit;
        let result = compile_unit(&unit, "prop.dll", "prop");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some(), "{:?}", result.emit_error);
    }

    #[test]
    fn compiles_short_circuit_and_conditional() {
        let unit = parse_compilation_unit(
            "class Program { \
                static int Main() { \
                    int a = 5; int b = 0; int r = 0; \
                    if (a > 0 && b == 0) { r = r + 10; } \
                    if (a > 100 || b == 0) { r = r + 30; } \
                    r = r + (a > b ? 2 : 99); \
                    return r; \
                } \
             }",
        )
        .unit;
        let result = compile_unit(&unit, "logic.dll", "logic");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some(), "{:?}", result.emit_error);
    }

    #[test]
    fn debug_build_emits_a_portable_pdb() {
        let source = "class Program { static int Main() { int x = 6; return x * 7; } }";
        let unit = parse_compilation_unit(source).unit;
        let result = compile_unit_with_debug(&unit, "app.dll", "app", &[], source, "app.cs");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some(), "{:?}", result.emit_error);
        let pdb = result.pdb.expect("a pdb when debug is requested");
        assert_eq!(&pdb[0..4], b"BSJB");
        assert!(
            pdb.windows(b"app.cs".len())
                .any(|window| window == b"app.cs")
        );
    }

    #[test]
    fn portable_pdb_round_trips_through_the_metadata_reader() {
        let source = "class Program { static int Main() { int x = 6; return x * 7; } }";
        let unit = parse_compilation_unit(source).unit;
        let pdb_bytes = compile_unit_with_debug(&unit, "app.dll", "app", &[], source, "app.cs")
            .pdb
            .expect("a pdb");
        let pdb = lamella_metadata::PortablePdb::read(&pdb_bytes).expect("read the pdb");

        assert!(pdb.document_name(1).unwrap().contains("app.cs"));
        assert!((1..=3).any(|rid| !pdb.sequence_points(rid).is_empty()));
        assert!(
            (1..=3)
                .flat_map(|rid| pdb.local_variables(rid))
                .any(|local| local.index == 0 && local.name == "x")
        );
    }

    #[test]
    fn pdb_queries_map_source_lines_and_breakpoints() {
        let source = "class Program\n{\n    static int Main()\n    {\n        int x = 6;\n        return x * 7;\n    }\n}\n";
        let unit = parse_compilation_unit(source).unit;
        let pdb_bytes = compile_unit_with_debug(&unit, "app.dll", "app", &[], source, "app.cs")
            .pdb
            .expect("a pdb");
        let pdb = lamella_metadata::PortablePdb::read(&pdb_bytes).expect("read the pdb");

        let points = pdb.sequence_points(2);
        assert_eq!(
            points.iter().map(|p| p.start_line).collect::<Vec<_>>(),
            [5, 6]
        );

        assert_eq!(
            pdb.source_location(2, points[1].il_offset)
                .unwrap()
                .start_line,
            6
        );
        assert_eq!(pdb.source_location(2, 0).unwrap().start_line, 5);
        assert!(pdb.method_document(2).unwrap().contains("app.cs"));

        assert_eq!(
            pdb.resolve_breakpoint("app.cs", 6),
            Some((2, points[1].il_offset))
        );
    }

    #[test]
    fn release_build_emits_no_pdb() {
        let unit = parse_compilation_unit("class Program { static int Main() { return 0; } }").unit;
        let result = compile_unit(&unit, "app.dll", "app");
        assert!(result.image.is_some());
        assert!(result.pdb.is_none());
    }

    #[test]
    fn local_variables_round_trip_through_the_reader() {
        use lamella_metadata::{Assembly, SigType};
        let unit = parse_compilation_unit(
            "class P { static int Run() { int a = 1; double b = 2.0; long c = 3; return a; } }",
        )
        .unit;
        let image = compile_unit(&unit, "lv.dll", "lv")
            .image
            .expect("the method emits");
        let assembly = Assembly::read(&image).expect("the image reads back");
        let run = assembly
            .find_type("", "P")
            .expect("type P is present")
            .methods()
            .find(|method| method.name() == Some("Run"))
            .expect("Run is present");
        assert_eq!(
            run.local_variables(),
            [SigType::I4, SigType::R8, SigType::I8]
        );
    }

    #[test]
    fn a_warning_does_not_block_emission() {
        let result = compile_source(
            "#warning carry on\nclass Program { static int Main() { return 0; } }",
            "w.cs",
            "w.dll",
            "w",
            &[],
            false,
        );
        assert!(result.image.is_some(), "{:?}", result.emit_error);
        assert_eq!(result.diagnostics.len(), 1);
        assert!(!result.diagnostics[0].is_error());
    }

    #[test]
    fn compiles_numeric_and_enum_casts() {
        let unit = parse_compilation_unit(
            "enum E { A, B, C } \
             class P { static int Main() { double d = 42.9; E c = E.C; return (int)d + (int)c; } }",
        )
        .unit;
        let result = compile_unit(&unit, "k.dll", "k");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some(), "{:?}", result.emit_error);
    }

    #[test]
    fn compiles_enum_typed_local_param_and_return() {
        let unit = parse_compilation_unit(
            "enum Color { Red, Green, Blue } \
             class P { \
                static Color Pick() { return Color.Blue; } \
                static int Rank(Color c) { if (c == Color.Blue) { return 42; } return 0; } \
                static int Main() { Color c = Pick(); return Rank(c); } \
             }",
        )
        .unit;
        let result = compile_unit(&unit, "c.dll", "c");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some(), "{:?}", result.emit_error);
    }

    #[test]
    fn compiles_delegate_creation_and_invocation() {
        let unit = parse_compilation_unit(
            "delegate int D(int x); \
             class P { static int Twice(int x) { return x * 2; } \
                static int Main() { D d = new D(Twice); return d(21); } }",
        )
        .unit;
        let result = compile_unit(&unit, "d.dll", "d");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some(), "{:?}", result.emit_error);
    }

    #[test]
    fn compiles_interface_dispatch() {
        let unit = parse_compilation_unit(
            "interface IAnimal { int Legs(); } \
             class Dog : IAnimal { public int Legs() { return 4; } } \
             class Spider : IAnimal { public int Legs() { return 8; } } \
             class P { static int Count(IAnimal a) { return a.Legs(); } \
                static int Main() { return Count(new Dog()) * 10 + Count(new Spider()) - 6; } }",
        )
        .unit;
        let result = compile_unit(&unit, "i.dll", "i");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some(), "{:?}", result.emit_error);
    }

    #[test]
    fn compiles_virtual_dispatch_and_inheritance() {
        let unit = parse_compilation_unit(
            "class A { public int X; public virtual int F() { return 1; } } \
             class B : A { public override int F() { return base.F() + 40; } \
                public int G() { return X; } } \
             class P { static int Main() { \
                B b = new B(); b.X = 1; A a = b; return a.F() + b.G(); } }",
        )
        .unit;
        let result = compile_unit(&unit, "v.dll", "v");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some(), "{:?}", result.emit_error);
    }

    #[test]
    fn compiles_blittable_struct() {
        let unit = parse_compilation_unit(
            "struct Point { public int X; public int Y; } \
             class P { static int Main() { \
                Point p = new Point(); p.X = 40; p.Y = 2; \
                Point q = p; q.X = 100; \
                return p.X + p.Y; \
             } }",
        )
        .unit;
        let result = compile_unit(&unit, "s.dll", "s");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some(), "{:?}", result.emit_error);
    }

    #[test]
    fn compiles_struct_method_and_field_return() {
        let unit = parse_compilation_unit(
            "struct Point { public int X; public int Y; public int Sum() { return X + Y; } } \
             class P { static int Main() { \
                Point p = new Point(); p.X = 13; p.Y = 8; \
                return p.Sum() + p.X + p.Y; \
             } }",
        )
        .unit;
        let result = compile_unit(&unit, "m.dll", "m");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some(), "{:?}", result.emit_error);
    }

    #[test]
    fn compiles_nested_struct_field_access() {
        let unit = parse_compilation_unit(
            "struct Inner { public int V; } struct Outer { public Inner I; public int N; } \
             class P { static int Main() { \
                Outer o = new Outer(); o.I.V = 40; o.N = 2; return o.I.V + o.N; } }",
        )
        .unit;
        let result = compile_unit(&unit, "n.dll", "n");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some(), "{:?}", result.emit_error);
    }

    #[test]
    fn compiles_explicit_struct_constructor() {
        let unit = parse_compilation_unit(
            "struct Point { public int X; public int Y; \
                public Point(int x, int y) { X = x; Y = y; } } \
             class P { static int Main() { \
                Point p = new Point(40, 2); Point q = new Point(); \
                return p.X + p.Y + q.X; } }",
        )
        .unit;
        let result = compile_unit(&unit, "c.dll", "c");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some(), "{:?}", result.emit_error);
    }

    #[test]
    fn compiles_class_constructor_and_base_chain() {
        for src in [
            "class Foo { public int V; public Foo(int v) { V = v; } } \
             class P { static int Main() { Foo f = new Foo(42); return f.V; } }",
            "class A { public int X; } \
             class B : A { public int Y; public B(int x, int y) { X = x; Y = y; } } \
             class P { static int Main() { B b = new B(40, 2); return b.X + b.Y; } }",
        ] {
            let unit = parse_compilation_unit(src).unit;
            let result = compile_unit(&unit, "c.dll", "c");
            assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
            assert!(result.image.is_some(), "{:?}", result.emit_error);
        }
    }

    #[test]
    fn compiles_string_concatenation() {
        let unit = parse_compilation_unit(
            "class P { static string J(string a, string b) { return a + b; } \
             static int Main() { J(\"x\", \"y\"); return 0; } }",
        )
        .unit;
        let result = compile_unit(&unit, "s.dll", "s");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some(), "{:?}", result.emit_error);
    }

    #[test]
    fn compiles_boxing_and_unboxing() {
        for src in [
            "class P { static int Main() { int n = 42; object o = n; return (int)o; } }",
            "struct Pt { public int X; public int Y; } \
             class P { static int Main() { Pt p = new Pt(); p.X = 40; p.Y = 2; \
                object o = p; Pt q = (Pt)o; return q.X + q.Y; } }",
        ] {
            let unit = parse_compilation_unit(src).unit;
            let result = compile_unit(&unit, "b.dll", "b");
            assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
            assert!(result.image.is_some(), "{:?}", result.emit_error);
        }
    }

    #[test]
    fn struct_layout_round_trips_through_the_reader() {
        use lamella_metadata::{Assembly, TargetLayout};
        let unit = parse_compilation_unit(
            "struct Point { public int X; public int Y; } \
             class P { static int Main() { Point p = new Point(); return p.X + p.Y; } }",
        )
        .unit;
        let image = compile_unit(&unit, "s.dll", "s").image.expect("emits");
        let assembly = Assembly::read(&image).expect("reads back");
        let point = assembly.find_type("", "Point").expect("Point type");
        let layout = assembly
            .value_type_layout(point.token(), &TargetLayout::ilp32())
            .expect("lays out");
        assert_eq!(layout.field_offsets, [0, 4]);
        assert_eq!(layout.size, 8);
        assert_eq!(layout.alignment, 4);
        assert!(layout.reference_offsets.is_empty());
    }

    #[test]
    fn field_offset_resolves_a_field_token_to_its_layout_offset() {
        use lamella_metadata::{Assembly, TargetLayout};
        let unit = parse_compilation_unit(
            "struct Holder { public string Tag; public int N; } \
             class P { static int Main() { Holder h = new Holder(); h.N = 1; return h.N; } }",
        )
        .unit;
        let image = compile_unit(&unit, "f.dll", "f").image.expect("emits");
        let asm = Assembly::read(&image).expect("reads back");
        let holder = asm.find_type("", "Holder").expect("Holder type");
        let tag = holder.fields().find(|f| f.name() == Some("Tag")).unwrap();
        let n = holder.fields().find(|f| f.name() == Some("N")).unwrap();
        let target = TargetLayout::ilp32();
        assert_eq!(asm.field_offset(tag.token(), &target), Some(0));
        assert_eq!(asm.field_offset(n.token(), &target), Some(4));
    }

    #[test]
    fn reference_struct_layout_reports_the_gc_map() {
        use lamella_metadata::{Assembly, TargetLayout};
        let unit = parse_compilation_unit(
            "struct Holder { public string Tag; public int N; } \
             class P { static int Main() { Holder h = new Holder(); h.N = 1; return h.N; } }",
        )
        .unit;
        let image = compile_unit(&unit, "h.dll", "h").image.expect("emits");
        let assembly = Assembly::read(&image).expect("reads back");
        let holder = assembly.find_type("", "Holder").expect("Holder type");
        let layout = assembly
            .value_type_layout(holder.token(), &TargetLayout::ilp32())
            .expect("lays out");
        assert_eq!(layout.field_offsets, [0, 4]);
        assert_eq!(layout.size, 8);
        assert_eq!(layout.reference_offsets, [0]);
    }

    #[test]
    fn compiles_enum_bitwise_and_case_labels() {
        let unit = parse_compilation_unit(
            "enum Perm { None = 0, Read = 1, Write = 2 } \
             class P { static int Main() { \
                Perm p = Perm.Read | Perm.Write; \
                switch (p & Perm.Write) { case Perm.Write: return 42; default: return 0; } \
             } }",
        )
        .unit;
        let result = compile_unit(&unit, "f.dll", "f");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some(), "{:?}", result.emit_error);
    }

    #[test]
    fn compiles_enum_members_and_comparison() {
        let unit = parse_compilation_unit(
            "enum E { A, B = 5, C } \
             class Program { static int Main() { if (E.C == E.B) { return 0; } return 42; } }",
        )
        .unit;
        let result = compile_unit(&unit, "e.dll", "e");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some(), "{:?}", result.emit_error);
    }

    #[test]
    fn compiles_foreach_over_an_array() {
        let unit = parse_compilation_unit(
            "class Program { \
                static int Main() { \
                    int[] a = new int[3]; a[0] = 20; a[1] = 14; a[2] = 8; \
                    int sum = 0; \
                    foreach (int x in a) { sum = sum + x; } \
                    return sum; \
                } \
             }",
        )
        .unit;
        let result = compile_unit(&unit, "fe.dll", "fe");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some(), "{:?}", result.emit_error);
    }

    #[test]
    fn compiles_switch() {
        let unit = parse_compilation_unit(
            "class Program { \
                static int Main() { \
                    int x = 2; \
                    switch (x) { \
                        case 1: return 10; \
                        case 2: return 42; \
                        default: return 0; \
                    } \
                } \
             }",
        )
        .unit;
        let result = compile_unit(&unit, "s.dll", "s");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some(), "{:?}", result.emit_error);
    }

    #[test]
    fn compiles_real_literals() {
        let unit = parse_compilation_unit(
            "class Program { \
                static int Main() { \
                    double d = 42.0; float f = 1.5f; \
                    if (d > 41.5 && f > 1.0f) { return 42; } \
                    return 0; \
                } \
             }",
        )
        .unit;
        let result = compile_unit(&unit, "r.dll", "r");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some(), "{:?}", result.emit_error);
    }

    #[test]
    fn compiles_try_catch_with_a_return_inside() {
        let unit = parse_compilation_unit(
            "class Program { \
                static int Main() { \
                    try { int x = 0; return 10 / x; } catch { return 42; } \
                } \
             }",
        )
        .unit;
        let result = compile_unit(&unit, "t.dll", "t");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some(), "{:?}", result.emit_error);
    }

    #[test]
    fn compiles_try_finally() {
        let unit = parse_compilation_unit(
            "class Program { \
                static int result; \
                static int Main() { \
                    try { result = 10; } finally { result = result + 32; } \
                    return result; \
                } \
             }",
        )
        .unit;
        let result = compile_unit(&unit, "t.dll", "t");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some(), "{:?}", result.emit_error);
    }

    #[test]
    fn resolve_method_reads_call_targets_back() {
        let unit = parse_compilation_unit(
            "class Program { \
                static int Helper() { return 5; } \
                static int Main() { return Helper(); } \
             }",
        )
        .unit;
        let image = compile_unit(&unit, "p.dll", "p").image.expect("an image");
        let assembly = lamella_metadata::Assembly::read(&image).expect("read");

        let helper = (1..=4)
            .filter_map(|rid| assembly.resolve_method(Token::new(0x06, rid)))
            .find(|method| method.name == Some("Helper"))
            .expect("Helper resolves");
        assert!(matches!(
            helper.kind,
            lamella_metadata::MethodKind::Definition(_)
        ));
        assert_eq!(helper.declaring_type.map(|name| name.name), Some("Program"));

        let object_ctor = assembly
            .resolve_method(Token::new(0x0A, 1))
            .expect("a member reference");
        assert_eq!(object_ctor.name, Some(".ctor"));
        assert_eq!(
            object_ctor
                .declaring_type
                .map(|name| (name.namespace, name.name)),
            Some(("System", "Object"))
        );
        assert_eq!(object_ctor.kind, lamella_metadata::MethodKind::Reference);
    }

    #[test]
    fn binding_errors_block_emission() {
        let unit = parse_compilation_unit("class C { int M() { return \"s\"; } }").unit;
        let result = compile_unit(&unit, "c.dll", "c");
        assert!(!result.diagnostics.is_empty());
        assert!(result.image.is_none());
    }

    #[test]
    fn compile_source_compiles_clean_source_with_a_pdb() {
        let result = compile_source(
            "class Program { static int Main() { return 42; } }",
            "app.cs",
            "app.dll",
            "app",
            &[],
            true,
        );
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some());
        assert!(result.pdb.is_some());
    }

    #[test]
    fn compile_source_surfaces_syntax_errors_without_a_binder_cascade() {
        let result = compile_source(
            "class Program { static int Main() { int x = $; return Missing(); } }",
            "app.cs",
            "app.dll",
            "app",
            &[],
            false,
        );
        assert!(result.image.is_none());
        assert!(!result.diagnostics.is_empty());
        assert!(
            !result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == 103),
            "binder cascade was not suppressed: {:?}",
            result.diagnostics
        );
    }
}
