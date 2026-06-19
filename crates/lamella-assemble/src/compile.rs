//! Compiling a bound program to a managed PE: the bridge over the whole back end.

use crate::debug::LineMap;
use crate::method::{EmittedBody, emit_body, max_stack};
use crate::tokens::Tokens;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use lamella_binder::{
    Binder, BoundExpr, BoundExprKind, BoundStmt, BoundStmtKind, Diagnostic as BinderDiagnostic,
    Model, SpecialType, TypeSymbol, bind_compilation_unit_with_references, bind_type, collect_into,
    load_assembly,
};
use lamella_cil::{
    Instruction, MethodBodyImage, Opcode, Operand, encode_with_offsets, write_method_body,
};
use lamella_metadata::Assembly;
use lamella_pe::{
    ImageBuilder, LocalVariable, MethodDebug, SequencePoint, TypeSig, field_signature,
    local_signature, method_signature, property_signature,
};
use lamella_syntax::ast::{
    CompilationUnit, Literal, Member, Modifier, NamespaceMember, Parameter, QualifiedName,
    TypeDecl, UsingDirective, UsingKind, VariableDeclarator,
};
use lamella_syntax::diagnostic::Diagnostic as SyntaxDiagnostic;
use lamella_syntax::parser::parse_compilation_unit;
use lamella_syntax::span::Span;
use lamella_token::Token;

const TYPE_DEF: u8 = 0x02;
const FIELD: u8 = 0x04;
const METHOD_DEF: u8 = 0x06;
const PUBLIC_CLASS: u32 = 0x0000_0001;
const METHOD_PUBLIC: u16 = 0x0006;
const METHOD_STATIC: u16 = 0x0010;
const FIELD_PUBLIC: u16 = 0x0006;
const FIELD_STATIC: u16 = 0x0010;
const CTOR_FLAGS: u16 = 0x0006 | 0x0800 | 0x1000;
const SPECIAL_NAME: u16 = 0x0800;
const IL_MANAGED: u16 = 0x0000;

/// A diagnostic from any stage of compilation -- parsing or binding -- reduced to
/// what a driver reports: the `CSxxxx` code, the rendered message, and the span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// The C# compiler code (`CSxxxx`).
    pub code: u16,
    /// The rendered message.
    pub message: String,
    /// The source location.
    pub span: Span,
}

impl Diagnostic {
    fn from_syntax(diagnostic: &SyntaxDiagnostic) -> Diagnostic {
        Diagnostic {
            code: diagnostic.code(),
            message: format!("{}", diagnostic.kind),
            span: diagnostic.span,
        }
    }

    fn from_binder(diagnostic: &BinderDiagnostic) -> Diagnostic {
        Diagnostic {
            code: diagnostic.code(),
            message: format!("{}", diagnostic.kind),
            span: diagnostic.span,
        }
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
    if !parsed.diagnostics.is_empty() {
        return Compilation {
            diagnostics: parsed
                .diagnostics
                .iter()
                .map(Diagnostic::from_syntax)
                .collect(),
            image: None,
            pdb: None,
            emit_error: None,
        };
    }
    let debug = emit_debug.then_some((source, source_path));
    compile(&parsed.unit, module_name, assembly_name, references, debug)
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
    if !diagnostics.is_empty() {
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
            NamespaceMember::Enum(_) | NamespaceMember::Delegate(_) => {}
        }
    }
    binder.restore_import_scope(scope);
    Ok(())
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
    let type_token = image.add_type(namespace, &declaration.name, object, PUBLIC_CLASS);
    let enclosing = named_symbol(namespace, &declaration.name);
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
    }
    if !declares_constructor(declaration) {
        emit_default_constructor(image)?;
    }
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
                tokens,
                modifiers,
                name,
                return_type,
                parameters,
                body,
                debug,
            )?;
            if entry_point.is_none() && &**name == "Main" && modifiers.contains(&Modifier::Static) {
                *entry_point = Some(token);
            }
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
    return_type: &lamella_syntax::ast::TypeRef,
    parameters: &[Parameter],
    body: &lamella_syntax::ast::Stmt,
    debug: Option<&DebugContext>,
) -> Result<Token, crate::EmitError> {
    let return_symbol = bind_type(return_type);
    let params: Vec<(Box<str>, TypeSymbol)> = parameters
        .iter()
        .map(|parameter| (parameter.name.clone(), bind_type(&parameter.ty)))
        .collect();
    let is_static = modifiers.contains(&Modifier::Static);
    let flags = METHOD_PUBLIC | if is_static { METHOD_STATIC } else { 0 };
    emit_method_body(
        image,
        binder,
        tokens,
        enclosing,
        name,
        &return_symbol,
        &params,
        body,
        is_static,
        flags,
        debug,
    )
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
    body: &lamella_syntax::ast::Stmt,
    is_static: bool,
    flags: u16,
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
    let EmittedBody {
        code,
        local_types,
        local_names,
        sequence_points,
        handlers,
    } = emit_body(&parameter_names, &bound, tokens, arg_base, return_symbol)?;
    let local_var_sig = if local_types.is_empty() {
        None
    } else {
        let locals: Vec<TypeSig> = local_types
            .iter()
            .map(|ty| type_sig(tokens, ty))
            .collect::<Result<_, _>>()?;
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
        .map(|(_, ty)| type_sig(tokens, ty))
        .collect::<Result<_, _>>()?;
    let signature = method_signature(
        !is_static,
        &parameter_sigs,
        &type_sig(tokens, return_symbol)?,
    );
    let method = image.add_method(name, &signature, &body_bytes, flags, IL_MANAGED);
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
            body,
            is_static,
            flags,
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
            body,
            is_static,
            flags,
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
        BoundStmtKind::Local { declarators, .. } => {
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
        BoundExprKind::Binary { left, right, .. } => {
            mint_in_expr(left, image, tokens);
            mint_in_expr(right, image, tokens);
        }
        BoundExprKind::Unary { operand, .. }
        | BoundExprKind::Postfix { operand, .. }
        | BoundExprKind::Cast { operand }
        | BoundExprKind::Conversion { operand, .. } => mint_in_expr(operand, image, tokens),
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
            if let Some(method) = method {
                if tokens.type_token(&method.declaring_type).is_none()
                    && tokens
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
                if tokens.type_token(&constructor.declaring_type).is_none()
                    && tokens
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
        | BoundExprKind::PropertyAccess { receiver, .. }
        | BoundExprKind::MethodGroup { receiver, .. } => mint_in_expr(receiver, image, tokens),
        BoundExprKind::ArrayCreation { lengths } => {
            for length in lengths {
                mint_in_expr(length, image, tokens);
            }
            if let TypeSymbol::Array { element, .. } = &expr.ty {
                mint_type_token(image, tokens, element);
            }
        }
        BoundExprKind::ElementAccess { receiver, indices } => {
            mint_in_expr(receiver, image, tokens);
            for index in indices {
                mint_in_expr(index, image, tokens);
            }
        }
        BoundExprKind::Assignment { target, value, .. } => {
            mint_in_expr(target, image, tokens);
            mint_in_expr(value, image, tokens);
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
        _ => {}
    }
}

/// Mints a `MemberRef` for an external (BCL) method `method`: a `TypeRef` to its
/// declaring type, then a `MemberRef` with its encoded signature, recorded in the
/// token table. Skipped (left for emission to report) if a type cannot be encoded.
fn mint_member_ref(
    method: &lamella_binder::MethodReference,
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
) {
    let Some((namespace, name)) = split_type_name(&method.declaring_type) else {
        return;
    };
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

/// Splits a named type into `(namespace, name)`, e.g. `System.Console` -> `("System",
/// "Console")`. Returns `None` for a non-named type.
fn split_type_name(ty: &TypeSymbol) -> Option<(String, String)> {
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

/// Whether a type declares any constructor (so no implicit default is added).
fn declares_constructor(declaration: &TypeDecl) -> bool {
    declaration
        .members
        .iter()
        .any(|member| matches!(member, Member::Constructor { .. }))
}

/// Emits the implicit default constructor: `this`, a call to the base `Object`
/// constructor, and `ret`. (Field initializers are not run yet.)
fn emit_default_constructor(image: &mut ImageBuilder) -> Result<(), crate::EmitError> {
    let object_ctor = image.object_ctor();
    let code = [
        Instruction::new(Opcode::Ldarg, Operand::Variable(0)),
        Instruction::new(Opcode::Call, Operand::Token(object_ctor)),
        Instruction::simple(Opcode::Ret),
    ];
    let body = MethodBodyImage {
        max_stack: 1,
        init_locals: false,
        local_var_sig: None,
        code: Box::from(code),
        handlers: Box::new([]),
    };
    let body_bytes = write_method_body(&body)
        .map_err(|_| crate::EmitError::Unsupported("constructor body could not be written"))?;
    let signature = method_signature(true, &[], &TypeSig::Void);
    image.add_method(".ctor", &signature, &body_bytes, CTOR_FLAGS, IL_MANAGED);
    Ok(())
}

/// Maps a bound type to its signature form. A named type resolves to the `Class`
/// of its `TypeDef` token; array types come later.
fn type_sig(tokens: &Tokens, ty: &TypeSymbol) -> Result<TypeSig, crate::EmitError> {
    let special = match ty {
        TypeSymbol::Special(special) => special,
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
                }
                if !declares_constructor(declaration) {
                    *next_method += 1;
                    tokens.insert_method(
                        &declaring,
                        ".ctor",
                        &[],
                        Token::new(METHOD_DEF, *next_method),
                    );
                }
                for member in &declaration.members {
                    if let Member::Method {
                        name,
                        parameters,
                        body: Some(_),
                        ..
                    } = member
                    {
                        *next_method += 1;
                        let params: Vec<TypeSymbol> =
                            parameters.iter().map(|p| bind_type(&p.ty)).collect();
                        tokens.insert_method(
                            &declaring,
                            name,
                            &params,
                            Token::new(METHOD_DEF, *next_method),
                        );
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
            NamespaceMember::Enum(_) | NamespaceMember::Delegate(_) => {}
        }
    }
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
    fn release_build_emits_no_pdb() {
        let unit = parse_compilation_unit("class Program { static int Main() { return 0; } }").unit;
        let result = compile_unit(&unit, "app.dll", "app");
        assert!(result.image.is_some());
        assert!(result.pdb.is_none());
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
