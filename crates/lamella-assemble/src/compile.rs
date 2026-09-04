//! Compiling a bound program to a managed PE: the bridge over the whole back end.

use crate::debug::LineMap;
use crate::expr::is_value_type;
use crate::method::{ConstructorPrologue, EmittedBody, emit_body, max_stack};
use crate::tokens::Tokens;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use lamella_syntax::version::LanguageVersion;
use lamella_binder::{
    Binder, BoundExpr, BoundExprKind, BoundStmt, BoundStmtKind, CodeNamespace, ConversionKind,
    Diagnostic as BinderDiagnostic, FieldReference, Model, SpecialType, TypeSymbol,
    bind_type, collect_into, load_assembly,
    parameter_symbol, resolve_constants,
};
use lamella_cil::{Instruction, MethodBodyImage, Opcode, Operand, encode_with_offsets, write_method_body};
use lamella_metadata::signature::element;
use lamella_metadata::{Assembly, encode_exception_base_chain, exception_tag_for_name};
use lamella_pe::{
    DebugDocument, ImageBuilder, LocalVariable, MethodDebug, PARAM_HAS_DEFAULT, PARAM_OPTIONAL,
    PARAM_OUT, ParamRow, SequencePoint,
    TypeSig,
    field_signature, generic_method_signature, local_signature, method_signature,
    method_spec_signature, property_signature, type_signature, vararg_call_site_signature,
    vararg_method_signature,
};
use lamella_syntax::ast::{
    AssignmentOperator, AttributeArgument, AttributeSection, CompilationUnit, ConstructorInitializer,
    ConstructorInitializerKind, DelegateDecl, EnumDecl, Expr, ExprKind, Literal, Member, Modifier,
    NamespaceMember, Parameter, ParameterModifier, QualifiedName, Stmt, StmtKind, TypeDecl, TypeKind,
    TypeParameter,
    TypeRef, UsingDirective, VariableDeclarator, auto_property_backing_field_name,
    explicit_interface_member_name, is_auto_property,
};
use lamella_syntax::diagnostic::{Diagnostic as SyntaxDiagnostic, Severity};
use lamella_syntax::lexer::LexOptions;
use lamella_syntax::parser::parse_compilation_unit_with;
use lamella_syntax::span::Span;
use lamella_token::Token;

const TYPE_REF: u8 = 0x01;
const TYPE_DEF: u8 = 0x02;
const FIELD: u8 = 0x04;
const METHOD_DEF: u8 = 0x06;
const PUBLIC_CLASS: u32 = 0x0000_0001;
const PUBLIC_STRUCT: u32 = 0x0000_0001 | 0x0000_0008 | 0x0000_0100;
const TYPE_ABSTRACT: u32 = 0x0000_0080;
const TYPE_SEALED: u32 = 0x0000_0100;
/// `beforefieldinit` (II.23.1.15): the type initializer may run at any point AT OR BEFORE the first
/// static field access, rather than precisely at it.
///
/// **ITS ABSENCE IS A DEMAND, NOT A DEFAULT.** A type without this flag requires PRECISE timing, so
/// emitting it for nothing -- which this compiler did for every type it ever produced -- says every
/// type in the image needs a first-access check. Measured on corlib: 100% of type-initializer
/// trigger sites need a runtime check when the flag is omitted, against 4% under csc's on the same
/// sources. The flag is most of a lazy initializer's cost, decided before the mechanism is written.
const TYPE_BEFORE_FIELD_INIT: u32 = 0x0010_0000;

/// The Nested* visibility bits (II.23.1.15) for a nested type, from its declared accessibility.
/// A nested type is PRIVATE by default (10.5.1); the explicit `private` lands here too.
/// The `TypeAttributes` visibility bits (II.23.1.15) a type declaration asks for, in the top-level
/// range or the `Nested*` range as the type's POSITION requires -- the two ranges are disjoint and
/// a `TypeDef` with an enclosing type must use the nested one or the image is refused.
///
/// **EVERY TYPE KIND ASKS HERE, AND THAT IS WHY THERE IS ONE STATEMENT RATHER THAN FOUR.** A kind
/// whose emitter writes a literal `Public` bit instead -- an interface, a delegate, an enum -- makes
/// `internal interface`, `internal delegate` and `internal enum` ship PUBLIC: the declared boundary
/// of the assembly, visible to every consumer, with nothing anywhere saying so.
fn type_visibility(modifiers: &[Modifier], is_nested: bool) -> u32 {
    if is_nested {
        nested_visibility(modifiers)
    } else if modifiers.contains(&Modifier::Public) {
        0x0000_0001
    } else {
        0x0000_0000
    }
}

fn nested_visibility(modifiers: &[Modifier]) -> u32 {
    if modifiers.contains(&Modifier::Public) {
        0x0000_0002
    } else if modifiers.contains(&Modifier::Protected) && modifiers.contains(&Modifier::Internal) {
        0x0000_0007
    } else if modifiers.contains(&Modifier::Protected) {
        0x0000_0004
    } else if modifiers.contains(&Modifier::Internal) {
        0x0000_0005
    } else {
        0x0000_0003
    }
}
const METHOD_PUBLIC: u16 = 0x0006;
const METHOD_PRIVATE: u16 = 0x0001;
const METHOD_STATIC: u16 = 0x0010;
const METHOD_VIRTUAL: u16 = 0x0040;
const METHOD_HIDEBYSIG: u16 = 0x0080;
const METHOD_NEWSLOT: u16 = 0x0100;
const METHOD_PINVOKE_IMPL: u16 = 0x2000;
const METHOD_FINAL: u16 = 0x0020;
const METHOD_ABSTRACT: u16 = 0x0400;
const METHOD_CHECK_ACCESS_ON_OVERRIDE: u16 = 0x0200;
const INTERFACE_FLAGS: u32 =
    0x0000_0001 | 0x0000_0020 | 0x0000_0080 | TYPE_BEFORE_FIELD_INIT;
const IFACE_METHOD_FLAGS: u16 =
    METHOD_PUBLIC | METHOD_VIRTUAL | METHOD_ABSTRACT | METHOD_NEWSLOT | METHOD_HIDEBYSIG;
const DELEGATE_TYPE_FLAGS: u32 = 0x0000_0001 | 0x0000_0100;
const DELEGATE_CTOR_FLAGS: u16 = METHOD_PUBLIC | METHOD_HIDEBYSIG | 0x0800 | 0x1000;
const DELEGATE_INVOKE_FLAGS: u16 =
    METHOD_PUBLIC | METHOD_HIDEBYSIG | METHOD_VIRTUAL | METHOD_NEWSLOT;
const FIELD_PUBLIC: u16 = 0x0006;
const FIELD_PRIVATE: u16 = 0x0001;
const FIELD_STATIC: u16 = 0x0010;
const FIELD_INITONLY: u16 = 0x0020;
const FIELD_LITERAL: u16 = 0x0040;
const FIELD_HAS_DEFAULT: u16 = 0x8000;
const CTOR_SHARED_FLAGS: u16 = METHOD_HIDEBYSIG | 0x0800 | 0x1000;

/// A constructor's `MethodDef` flags, from the modifiers it was DECLARED with.
///
/// **AN IMPLICIT CONSTRUCTOR IS NOT ALWAYS PUBLIC, AND REASONING ABOUT THIS GETS IT WRONG.** 10.10.4
/// gives the default constructor the accessibility `public` EXCEPT on an `abstract` class, where it
/// is `protected` -- there is nothing to instantiate from outside, only to chain to. Callers pass
/// the synthesized modifier list that says so rather than relying on this to guess, so the rule
/// lives at the one site that knows whether the type is abstract.
fn ctor_flags(modifiers: &[Modifier]) -> u16 {
    member_visibility(modifiers) | CTOR_SHARED_FLAGS
}
const CCTOR_FLAGS: u16 = 0x0001 | METHOD_STATIC | METHOD_HIDEBYSIG | 0x0800 | 0x1000;
const SPECIAL_NAME: u16 = 0x0800;
const SETTER_VALUE_PARAMETER: &str = "value";
const DELEGATE_CTOR_TARGET: &str = "object";
const DELEGATE_CTOR_METHOD: &str = "method";
const IL_MANAGED: u16 = 0x0000;
const FINALIZE_FLAGS: u16 = 0x0004 | METHOD_VIRTUAL | METHOD_HIDEBYSIG;
const ENUM_TYPE_FLAGS: u32 = 0x0000_0001 | 0x0000_0100;
const ENUM_VALUE_FIELD_FLAGS: u16 = FIELD_PUBLIC | 0x0200 | 0x0400;
const ENUM_MEMBER_FIELD_FLAGS: u16 = FIELD_PUBLIC | FIELD_STATIC | FIELD_LITERAL | FIELD_HAS_DEFAULT;

/// A diagnostic from any stage of compilation -- parsing or binding -- reduced to
/// what a driver reports: the code with its namespace, the rendered message, and the span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// The numeric part of the code. Render it with [`Diagnostic::namespace`]'s prefix, never with
    /// a hard-coded `CS` -- not every diagnostic lcsc emits is one csc has a concept of.
    pub code: u16,
    /// Which namespace the code belongs to: `CS` where csc shares the condition, `LAM` where it
    /// does not.
    pub namespace: CodeNamespace,
    /// Whether it stops compilation (an error) or not (a warning).
    pub severity: Severity,
    /// The rendered message.
    pub message: String,
    /// The source location.
    pub span: Span,
}

impl Diagnostic {
    pub(crate) fn from_syntax(diagnostic: &SyntaxDiagnostic) -> Diagnostic {
        Diagnostic {
            code: diagnostic.code(),
            namespace: diagnostic.kind.namespace(),
            severity: diagnostic.severity(),
            message: format!("{}", diagnostic.kind),
            span: diagnostic.span,
        }
    }

    pub(crate) fn from_binder(diagnostic: &BinderDiagnostic) -> Diagnostic {
        Diagnostic {
            code: diagnostic.code(),
            namespace: diagnostic.namespace(),
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
    compile(unit, module_name, assembly_name, references, None, false, false, false, LanguageVersion::DEFAULT)
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
        false,
        false,
        false,
        LanguageVersion::DEFAULT,
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
    compile_source_with(
        source,
        source_path,
        module_name,
        assembly_name,
        references,
        emit_debug,
        LexOptions::default(),
    )
}

/// Like [`compile_source`], but scans `source` under `options` (9.4.2): how identifiers are
/// folded (`Normalization`) and whether the csc typed-reference operators (`__makeref`/
/// `__refvalue`/`__reftype`) are recognized. The defaults match csc and strict ISO-1.
pub fn compile_source_with(
    source: &str,
    source_path: &str,
    module_name: &str,
    assembly_name: &str,
    references: &[Assembly],
    emit_debug: bool,
    options: LexOptions,
) -> Compilation {
    let native_interop = options.native_interop;
    let language_version = options.version;
    let unsafe_option_missing = !options.unsafe_code;
    let embed_pdb = options.embed_pdb;
    let parsed = parse_compilation_unit_with(source, options);
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
    let pragmas = parsed.pragma_warnings.clone();
    let mut compiled = compile(
        &parsed.unit,
        module_name,
        assembly_name,
        references,
        debug,
        native_interop,
        unsafe_option_missing,
        embed_pdb,
        language_version,
    );
    if !parse_diagnostics.is_empty() {
        let mut diagnostics = parse_diagnostics;
        diagnostics.append(&mut compiled.diagnostics);
        compiled.diagnostics = diagnostics;
    }
    compiled.diagnostics = without_suppressed_warnings(compiled.diagnostics, &pragmas);
    compiled
}

/// Drops the warnings a file's `#pragma warning disable` regions suppress (9.5.8).
///
/// **A PRAGMA SUPPRESSES BY POSITION, NOT BY FILE**, so each diagnostic is tested against the LAST
/// directive before it: `disable 649` then `restore 649` half way down puts CS0649 back for
/// everything below. A bare `#pragma warning disable` names no codes and means every warning,
/// which is why an empty list is a wildcard rather than a no-op.
///
/// **ERRORS ARE NEVER SUPPRESSED**, whatever a pragma says -- csc ignores a `disable` naming an
/// error's number, and so does this: a directive that could silence an error would be a way to
/// ship a program the compiler refused.
fn without_suppressed_warnings(
    diagnostics: Vec<Diagnostic>,
    pragmas: &[lamella_syntax::lexer::PragmaWarning],
) -> Vec<Diagnostic> {
    if pragmas.is_empty() {
        return diagnostics;
    }
    diagnostics
        .into_iter()
        .filter(|diagnostic| {
            if diagnostic.is_error() {
                return true;
            }
            let at = diagnostic.span.start;
            let code = diagnostic.code;
            let mut suppressed = false;
            for pragma in pragmas {
                if pragma.position > at {
                    break;
                }
                if pragma.codes.is_empty() || pragma.codes.contains(&code) {
                    suppressed = pragma.disable;
                }
            }
            !suppressed
        })
        .collect()
}

fn compile(
    unit: &CompilationUnit,
    module_name: &str,
    assembly_name: &str,
    references: &[Assembly],
    debug: Option<(&str, &str)>,
    native_interop: bool,
    unsafe_option_missing: bool,
    embed_pdb: bool,
    language_version: LanguageVersion,
) -> Compilation {
    let diagnostics: Vec<Diagnostic> = lamella_binder::bind_compilation_unit_with_options(
        unit,
        references,
        lamella_binder::BindOptions {
            unsafe_option_missing,
            language_version,
            compiling_assembly: assembly_name,
        },
    )
    .iter()
    .map(Diagnostic::from_binder)
    .collect();
    let had_error = diagnostics.iter().any(Diagnostic::is_error);
    let units = core::slice::from_ref(unit);
    let Some(program) = ValidatedProgram::from_clean_bind(units, references, had_error) else {
        return Compilation {
            diagnostics,
            image: None,
            pdb: None,
            emit_error: None,
        };
    };
    let debug_sources = debug.map(|pair| [pair]);
    let debug = debug_sources.as_ref().map(|slice| &slice[..]);
    match build_image(&program, module_name, assembly_name, debug, native_interop, embed_pdb) {
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

/// A multi-file compilation's outcome: one diagnostic list PER SOURCE (parallel to the
/// input order, so a driver attributes each to its own file), and the assembled image
/// when every file is error-free.
#[derive(Debug)]
pub struct MultiCompilation {
    /// Each source's diagnostics, in input order.
    pub diagnostics: Vec<Vec<Diagnostic>>,
    /// The emitted assembly image, when no source had an error and lowering succeeded.
    pub image: Option<Vec<u8>>,
    /// The standalone Portable PDB (multi-document, one row per source), when debug
    /// info was requested and emitted.
    pub pdb: Option<Vec<u8>>,
    /// Why lowering failed, when parsing and binding were clean but emission was not.
    pub emit_error: Option<crate::EmitError>,
}

/// Parses, binds, and assembles several sources into ONE assembly (a multi-file
/// compilation, 16.1): every file's types enter one model -- so each file names the
/// others' types -- then bodies bind and lower in file order. A syntax or binder error
/// in any file blocks emission. `sources` pairs each file's decoded text with its path
/// (used both for diagnostics and, when `emit_debug` is set, as the PDB's per-file
/// `Document` -- a method attributes its sequence points to its own source file).
#[must_use]
pub fn compile_sources_with(
    sources: &[(&str, &str)],
    module_name: &str,
    assembly_name: &str,
    references: &[Assembly],
    emit_debug: bool,
    options: LexOptions,
) -> MultiCompilation {
    let mut diagnostics: Vec<Vec<Diagnostic>> = Vec::with_capacity(sources.len());
    let mut units: Vec<CompilationUnit> = Vec::with_capacity(sources.len());
    let mut syntax_error = false;
    let native_interop = options.native_interop;
    let language_version = options.version;
    let embed_pdb = options.embed_pdb;
    let unsafe_option_missing = !options.unsafe_code;
    let mut pragmas: Vec<Vec<lamella_syntax::lexer::PragmaWarning>> =
        Vec::with_capacity(sources.len());
    for (source, _path) in sources {
        let parsed = parse_compilation_unit_with(source, options.clone());
        let parse_diagnostics: Vec<Diagnostic> = parsed
            .diagnostics
            .iter()
            .map(Diagnostic::from_syntax)
            .collect();
        syntax_error |= parse_diagnostics.iter().any(Diagnostic::is_error);
        diagnostics.push(parse_diagnostics);
        pragmas.push(parsed.pragma_warnings);
        units.push(parsed.unit);
    }
    if syntax_error {
        return MultiCompilation {
            diagnostics,
            image: None,
            pdb: None,
            emit_error: None,
        };
    }
    let mut any_error = false;
    for (per_unit, unit_diagnostics) in diagnostics
        .iter_mut()
        .zip(lamella_binder::bind_compilation_units_with_options(
            &units,
            references,
            lamella_binder::BindOptions {
                unsafe_option_missing,
                language_version,
                compiling_assembly: assembly_name,
            },
        ))
    {
        let bound: Vec<Diagnostic> =
            unit_diagnostics.iter().map(Diagnostic::from_binder).collect();
        any_error |= bound.iter().any(Diagnostic::is_error);
        per_unit.extend(bound);
    }
    for (per_unit, file_pragmas) in diagnostics.iter_mut().zip(&pragmas) {
        let suppressed = without_suppressed_warnings(core::mem::take(per_unit), file_pragmas);
        *per_unit = suppressed;
    }
    let debug = emit_debug.then_some(sources);
    let Some(program) = ValidatedProgram::from_clean_bind(&units, references, any_error) else {
        return MultiCompilation {
            diagnostics,
            image: None,
            pdb: None,
            emit_error: None,
        };
    };
    match build_image(
        &program,
        module_name,
        assembly_name,
        debug,
        native_interop,
        embed_pdb,
    ) {
        Ok((image, pdb)) => MultiCompilation {
            diagnostics,
            image: Some(image),
            pdb,
            emit_error: None,
        },
        Err(error) => MultiCompilation {
            diagnostics,
            image: None,
            pdb: None,
            emit_error: Some(error),
        },
    }
}

/// The binder model for `units` over their references: the reference types first,
/// then every unit's own, with signature names qualified TO WHAT THEY MEAN IN THEIR OWN SCOPE
/// and the base chain linked across the whole. The qualification step matches the diagnostic
/// path ([`bind_compilation_unit_with_references`], via the binder crate's
/// `qualify_declared_signatures`); without it a method parameter written as a single-part
/// reference type (`void F(Type t)`, resolved through a `using`) stays unqualified and never
/// matches a qualified argument, so the emit-time call resolution silently fails ("a call that
/// did not resolve") -- and a simple name shared by two namespaces resolves to the WRONG one or
/// to none.
fn reference_model(
    units: &[CompilationUnit],
    references: &[Assembly],
    assembly_name: &str,
) -> Model {
    let mut model = Model::new();
    for reference in references {
        load_assembly(&mut model, reference, assembly_name);
    }
    for unit in units {
        collect_into(&mut model, unit);
    }
    let mut binder = Binder::with_model(model);
    for unit in units {
        lamella_binder::program::qualify_declared_signatures(
            &mut binder,
            &unit.usings,
            &unit.members,
            "",
        );
    }
    let mut model = binder.into_model();
    model.link_bases();
    resolve_constants(&mut model, units);
    model
}

/// The emit-trust barrier (T1.2): a program whose declarations and bodies have bound without any
/// error. [`build_image`] -- the PE/metadata emitter -- accepts ONLY this, and it can be minted
/// ONLY by [`ValidatedProgram::from_clean_bind`] from an error-free bind, so no program still
/// carrying errors (or recovery nodes) can reach emission. It carries just the units and references
/// today; it is the seam Tier 3 grows into a fully-validated typed tree -- declaration validation,
/// control flow, attributes, unsafe rules all discharged (the audit's C04 boundary) -- crossing
/// here with its invariants proven.
struct ValidatedProgram<'a, 'r> {
    units: &'a [CompilationUnit],
    references: &'a [Assembly<'r>],
}

impl<'a, 'r> ValidatedProgram<'a, 'r> {
    /// Mints the barrier token iff the bind reported no error. `had_error` is the caller's verdict
    /// over its own diagnostics (a flat list for one file, per-file for several), so the shape stays
    /// in the caller while this stays the SOLE constructor. Holding a `ValidatedProgram` is therefore
    /// proof the program bound cleanly -- and [`build_image`] cannot be reached without one.
    fn from_clean_bind(
        units: &'a [CompilationUnit],
        references: &'a [Assembly<'r>],
        had_error: bool,
    ) -> Option<Self> {
        (!had_error).then_some(ValidatedProgram { units, references })
    }
}

fn build_image(
    program: &ValidatedProgram,
    module_name: &str,
    assembly_name: &str,
    debug: Option<&[(&str, &str)]>,
    native_interop: bool,
    embed_pdb: bool,
) -> Result<(Vec<u8>, Option<Vec<u8>>), crate::EmitError> {
    let units = program.units;
    let references = program.references;
    let model = reference_model(units, references, assembly_name);
    let mut binder = Binder::with_model(model);
    let contexts: Vec<DebugContext> = debug
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, (source, _))| DebugContext {
            source,
            lines: LineMap::new(source),
            document: index as u32 + 1,
        })
        .collect();
    let partials = index_partial_types(units, &contexts);
    let mut tokens = assign_tokens(units, &mut binder, &partials);
    tokens.set_native_interop(native_interop);
    mark_external_value_types(binder.model(), &mut tokens);
    let mut image = ImageBuilder::new(module_name, assembly_name);
    if let Some(sources) = debug {
        let mut content: Vec<u8> = assembly_name.as_bytes().to_vec();
        for &(source, _) in sources {
            content.push(0);
            content.extend_from_slice(source.as_bytes());
        }
        image.set_content_id(&content);
    }
    register_external_type_scopes(binder.model(), &mut image);
    register_assembly_identities(references, &mut image);
    let object =
        declared_system_type(&tokens, "Object").unwrap_or_else(|| image.object_type());
    let mut entry_point = None;
    for (index, unit) in units.iter().enumerate() {
        binder.set_defined_symbols(unit.defined_symbols.clone());
        emit_namespace(
            &mut image,
            &mut binder,
            object,
            &mut tokens,
            &mut entry_point,
            &unit.usings,
            &unit.members,
            "",
            contexts.get(index),
            &partials,
        )?;
    }
    let pending_async = core::mem::take(&mut tokens.pending_async);
    for pending in pending_async {
        emit_async_machine(&mut image, &mut binder, &mut tokens, pending)?;
    }
    emit_is_readonly_attribute(&mut image, binder.model(), &mut tokens);
    for unit in units {
        emit_global_attributes(&mut image, &binder, &mut tokens, &unit.global_attributes);
    }
    emit_exception_base_chains(&mut image, binder.model(), &tokens);
    let is_dll = entry_point.is_none();
    let entry = entry_point.unwrap_or(Token::new(0, 0));
    let documents: Option<Vec<DebugDocument>> = debug.map(|sources| {
        sources
            .iter()
            .map(|(source, path)| DebugDocument { path, source })
            .collect()
    });
    let (image, pdb) = match documents.as_deref() {
        Some(docs) if embed_pdb => (
            image.finish_with_embedded_debug(entry, is_dll, docs, &pdb_file_name(module_name)),
            None,
        ),
        Some(docs) => {
            let pdb = image.build_pdb(docs, entry);
            (image.finish_with_debug(entry, is_dll, &pdb_file_name(module_name)), Some(pdb))
        }
        None => (image.finish(entry, is_dll), None),
    };
    Ok((image, pdb))
}

/// Emits an `<ExceptionBaseChain>` custom attribute on each referenced EXTERNAL exception
/// type, carrying its base-chain tag vector, so the AOT -- which loads only the program, not
/// the BCL -- can read a BCL throwable's `[tag(E), tag(base), ..., tag(System.Exception)]`
/// for the middle-base subtype test. An in-program exception is a `TypeDef` whose chain the
/// AOT walks itself, so it gets no attribute. The marker `<ExceptionBaseChain>::.ctor` is
/// minted once, lazily, only when there is at least one exception type to annotate.
fn emit_exception_base_chains(image: &mut ImageBuilder, model: &Model, tokens: &Tokens) {
    let mut chains: Vec<(Token, Vec<u32>)> = Vec::new();
    for (namespace, name) in model.type_keys() {
        let symbol = named_symbol(namespace, name);
        let Some(token) = tokens.type_token(&symbol) else {
            continue;
        };
        if token.table() != TYPE_REF {
            continue;
        }
        if let Some(chain) = exception_base_chain_tags(model, &symbol) {
            chains.push((token, chain));
        }
    }
    if chains.is_empty() {
        return;
    }
    let marker = image.type_ref("", "<ExceptionBaseChain>");
    let ctor = image.member_ref(marker, ".ctor", &method_signature(true, &[], &TypeSig::Void));
    for (token, chain) in chains {
        image.add_custom_attribute(token, ctor, &encode_exception_base_chain(&chain));
    }
}

/// The base-chain tag vector for an exception type -- `[tag(E), tag(base(E)), ...,
/// tag(System.Exception)]`, leaf first -- or `None` if `symbol` does not derive from
/// `System.Exception` per the model. Tags are by name, matching `Assembly::exception_tag`.
fn exception_base_chain_tags(model: &Model, symbol: &TypeSymbol) -> Option<Vec<u32>> {
    let mut chain = Vec::new();
    let mut current = symbol.clone();
    loop {
        let (namespace, name) = split_type_name(&current)?;
        chain.push(exception_tag_for_name(&namespace, &name));
        if namespace == "System" && name == "Exception" {
            return Some(chain);
        }
        if chain.len() > 64 {
            return None;
        }
        current = model.get_by_symbol(&current).and_then(|info| info.base.clone())?;
    }
}

/// Emits a `CustomAttribute` row for each user attribute applied to `parent` (24.2): the
/// attribute type's constructor (matched by positional-argument count) and a value blob of
/// its fixed arguments (II.23.3). An attribute whose type/constructor does not resolve, or
/// whose arguments are not constant literals this encodes, is skipped (lenient -- the same
/// posture as an unlowered construct).
///
/// A `[return:]` section on a method (24.4) attaches to the method's return value: a `Param`
/// row of sequence 0 is minted for the return and the attribute hangs off it. Other explicit
/// targets are not routed here yet.
fn emit_attributes(
    image: &mut ImageBuilder,
    binder: &Binder,
    tokens: &mut Tokens,
    enclosing: &TypeSymbol,
    parent: Token,
    sections: &[AttributeSection],
) {
    let mut return_param: Option<Token> = None;
    for section in sections {
        let attribute_parent = match section.target.as_deref() {
            None => parent,
            Some("return") if parent.table() == METHOD_DEF => {
                if return_param.is_none() {
                    return_param = Some(image.add_return_param());
                }
                return_param.expect("return param just set")
            }
            Some(_) => continue,
        };
        for attribute in &section.attributes {
            emit_one_attribute(image, binder, tokens, enclosing, attribute_parent, attribute);
        }
    }
}

/// Emits a `CustomAttribute` row for each global `[assembly: ...]` / `[module: ...]` attribute
/// (24.2), attached to the assembly's (or module's) manifest row rather than to a declaration --
/// the parser routes a top-level targeted section here. The profile validator reads these back
/// from metadata (e.g. `[assembly: Lamella.Runtime.RequiresCapability("net.tls")]`).
fn emit_global_attributes(
    image: &mut ImageBuilder,
    binder: &Binder,
    tokens: &mut Tokens,
    sections: &[AttributeSection],
) {
    let enclosing = TypeSymbol::Special(SpecialType::Object);
    for section in sections {
        let is_assembly = section.target.as_deref() != Some("module");
        let parent = match section.target.as_deref() {
            Some("module") => image.module_token(),
            _ => image.assembly_token(),
        };
        for attribute in &section.attributes {
            if is_assembly {
                if let Some(version) = assembly_version_from_attribute(attribute) {
                    image.set_assembly_version(version);
                    continue;
                }
                if let Some(flags) = assembly_flags_from_attribute(attribute) {
                    image.set_assembly_flags(flags);
                    continue;
                }
                if let Some(algorithm) = assembly_algorithm_id_from_attribute(attribute) {
                    image.set_assembly_hash_algorithm(algorithm);
                    continue;
                }
                if let Some(culture) = assembly_culture_from_attribute(attribute) {
                    image.set_assembly_culture(&culture);
                    continue;
                }
            }
            emit_one_attribute(image, binder, tokens, &enclosing, parent, attribute);
        }
    }
}

/// The version from an `[assembly: AssemblyVersion("a.b.c.d")]` attribute, matched by its
/// well-known name (like `[DllImport]`), or `None` if this is a different attribute. csc treats
/// `AssemblyVersion` specially by name -- the type need not be resolvable -- so this reads it
/// syntactically.
fn assembly_version_from_attribute(
    attribute: &lamella_syntax::ast::Attribute,
) -> Option<(u16, u16, u16, u16)> {
    let last = attribute.name.parts.last()?;
    if &**last != "AssemblyVersion" && &**last != "AssemblyVersionAttribute" {
        return None;
    }
    let text = attribute.arguments.iter().find_map(|argument| match argument {
        AttributeArgument::Positional(expr) => string_literal_value(expr),
        AttributeArgument::Named { .. } => None,
    })?;
    parse_assembly_version(&text)
}

/// The flags from an `[assembly: AssemblyFlags(n)]` attribute, consumed into the Assembly row's
/// `Flags` column (II.22.2) rather than emitted as a `CustomAttribute` -- csc's behaviour, matched
/// by well-known name like `AssemblyVersion`. `None` for a different attribute.
fn assembly_flags_from_attribute(attribute: &lamella_syntax::ast::Attribute) -> Option<u32> {
    assembly_u32_from_attribute(attribute, "AssemblyFlags")
}

/// The algorithm id from an `[assembly: AssemblyAlgorithmId(n)]` attribute, consumed into the
/// Assembly row's `HashAlgId` column (II.22.2), like `AssemblyFlags`.
fn assembly_algorithm_id_from_attribute(attribute: &lamella_syntax::ast::Attribute) -> Option<u32> {
    assembly_u32_from_attribute(attribute, "AssemblyAlgorithmId")
}

/// The `u32` argument of a well-known assembly attribute whose last name part is `simple_name` (with
/// or without the `Attribute` suffix), read syntactically from a single integer-literal positional
/// argument. `None` if this is a different attribute, the argument is not an integer constant, or it
/// does not fit `u32` (a well-formed `AssemblyFlags`/`AssemblyAlgorithmId` argument always does).
fn assembly_u32_from_attribute(
    attribute: &lamella_syntax::ast::Attribute,
    simple_name: &str,
) -> Option<u32> {
    let last = &**attribute.name.parts.last()?;
    if last != simple_name && last.strip_suffix("Attribute") != Some(simple_name) {
        return None;
    }
    let value = attribute.arguments.iter().find_map(|argument| match argument {
        AttributeArgument::Positional(expr) => match &expr.kind {
            ExprKind::Literal(literal) => lamella_binder::literal_int_value(literal),
            _ => None,
        },
        AttributeArgument::Named { .. } => None,
    })?;
    u32::try_from(value).ok()
}

/// The culture from an `[assembly: AssemblyCulture("name")]` attribute, consumed into the Assembly
/// row's `Culture` column (II.22.2). csc treats the empty string as the neutral culture (a nil
/// column), which the string interner already maps to heap offset 0. `None` for a different
/// attribute.
fn assembly_culture_from_attribute(attribute: &lamella_syntax::ast::Attribute) -> Option<String> {
    let last = &**attribute.name.parts.last()?;
    if last != "AssemblyCulture" && last != "AssemblyCultureAttribute" {
        return None;
    }
    attribute.arguments.iter().find_map(|argument| match argument {
        AttributeArgument::Positional(expr) => string_literal_value(expr),
        AttributeArgument::Named { .. } => None,
    })
}

/// Parses an assembly version string: 1..=4 dot-separated `u16` parts, missing trailing parts
/// padding with 0 (`"1.0"` -> `(1, 0, 0, 0)`). Returns `None` on more than four parts, an empty
/// string, a non-`u16` part, or the csc wildcard form (`"1.0.*"`) -- this compiler emits
/// byte-deterministic assemblies, and the wildcard's auto-generated build/revision are not.
fn parse_assembly_version(text: &str) -> Option<(u16, u16, u16, u16)> {
    let mut parts = [0u16; 4];
    let mut seen = 0usize;
    for (index, piece) in text.split('.').enumerate() {
        if index >= 4 {
            return None;
        }
        parts[index] = piece.trim().parse::<u16>().ok()?;
        seen = index + 1;
    }
    (seen > 0).then_some((parts[0], parts[1], parts[2], parts[3]))
}

fn emit_one_attribute(
    image: &mut ImageBuilder,
    binder: &Binder,
    tokens: &mut Tokens,
    enclosing: &TypeSymbol,
    parent: Token,
    attribute: &lamella_syntax::ast::Attribute,
) {
    let mut positional: Vec<&Expr> = Vec::new();
    let mut named: Vec<(&str, &Expr)> = Vec::new();
    for argument in &attribute.arguments {
        match argument {
            AttributeArgument::Positional(expr) => positional.push(expr),
            AttributeArgument::Named { name, value } => named.push((name, value)),
        }
    }
    let Some((attribute_ty, parameters, is_params)) =
        resolve_attribute(binder, &attribute.name, positional.len())
    else {
        return;
    };
    let mut blob = alloc::vec![0x01u8, 0x00];
    for (index, parameter) in parameters.iter().enumerate() {
        if is_params && index + 1 == parameters.len() {
            let TypeSymbol::Array { element, rank: 1 } = binder.resolve_type(parameter) else {
                return;
            };
            let rest = &positional[index.min(positional.len())..];
            let direct = rest.len() == 1
                && matches!(
                    rest[0].kind,
                    ExprKind::ArrayCreation { .. } | ExprKind::Literal(Literal::Null)
                );
            if direct {
                if encode_value(binder, tokens, enclosing, rest[0], parameter, &mut blob).is_none() {
                    return;
                }
            } else {
                let Ok(count) = u32::try_from(rest.len()) else {
                    return;
                };
                blob.extend_from_slice(&count.to_le_bytes());
                for arg in rest {
                    if encode_value(binder, tokens, enclosing, arg, &element, &mut blob).is_none() {
                        return;
                    }
                }
            }
            break;
        }
        let Some(expr) = positional.get(index) else {
            return;
        };
        if encode_value(binder, tokens, enclosing, expr, parameter, &mut blob).is_none() {
            return;
        }
    }
    let Ok(named_count) = u16::try_from(named.len()) else {
        return;
    };
    blob.extend_from_slice(&named_count.to_le_bytes());
    for (name, value) in &named {
        if encode_named_argument(binder, tokens, enclosing, &attribute_ty, name, value, &mut blob)
            .is_none()
        {
            return;
        }
    }
    if tokens.method(&attribute_ty, ".ctor", &parameters).is_none() {
        let constructor_ref = lamella_binder::MethodReference {
            declaring_type: attribute_ty.clone(),
            name: ".ctor".into(),
            parameters: parameters.clone(),
            return_type: TypeSymbol::Special(SpecialType::Void),
            is_static: false,
            is_vararg: false,
            instantiation: None,
            declaring_instantiation: None,
        };
        mint_member_ref(&constructor_ref, image, tokens);
    }
    let Some(constructor) = tokens.method(&attribute_ty, ".ctor", &parameters) else {
        return;
    };
    image.add_custom_attribute(parent, constructor, &blob);
}

/// Resolves an attribute name to its type and the parameter types of the constructor taking
/// `arg_count` positional arguments, trying the name as written and with an `Attribute`
/// suffix (24.2). `None` if neither resolves to a type with such a constructor.
fn resolve_attribute(
    binder: &Binder,
    name: &QualifiedName,
    arg_count: usize,
) -> Option<(TypeSymbol, Vec<TypeSymbol>, bool)> {
    let model = binder.model();
    for candidate in attribute_candidates(name) {
        let resolved = binder.resolve_type(&candidate);
        if let Some(info) = model.get_by_symbol(&resolved) {
            let constructor = info
                .constructors
                .iter()
                .find(|constructor| constructor.parameters.len() == arg_count)
                .or_else(|| {
                    info.constructors.iter().find(|constructor| {
                        constructor.is_params
                            && !constructor.parameters.is_empty()
                            && arg_count + 1 >= constructor.parameters.len()
                    })
                });
            if let Some(constructor) = constructor {
                return Some((resolved, constructor.parameters.clone(), constructor.is_params));
            }
        }
    }
    None
}

/// The candidate type symbols for an attribute name: as written, and with an `Attribute`
/// suffix on the final identifier (`[My]` -> `My`, then `MyAttribute`).
fn attribute_candidates(name: &QualifiedName) -> Vec<TypeSymbol> {
    let parts: Vec<Box<str>> = name.parts.iter().cloned().collect();
    let mut suffixed = parts.clone();
    if let Some(last) = suffixed.last_mut() {
        let mut full = String::from(&**last);
        full.push_str("Attribute");
        *last = full.into();
    }
    alloc::vec![
        TypeSymbol::Named(parts.into()),
        TypeSymbol::Named(suffixed.into()),
    ]
}

/// Encodes one attribute argument value into the blob by its target type (II.23.3): an
/// integral/bool/char/string literal, a `typeof(T)` (the type's name as a SerString), an
/// enum constant (its underlying integer), or a single-dimension array (`SZARRAY`: the
/// element count then each element, or `0xFFFFFFFF` for null). `None` (skip the attribute)
/// for anything else. The target type is first resolved through the namespaces in scope, so
/// a simple-named enum or array element (`AttributeTargets`, not `System.AttributeTargets`)
/// finds its model entry.
fn encode_value(
    binder: &Binder,
    tokens: &Tokens,
    enclosing: &TypeSymbol,
    expr: &Expr,
    ty: &TypeSymbol,
    blob: &mut Vec<u8>,
) -> Option<()> {
    let resolved = binder.resolve_type(ty);
    if let TypeSymbol::Array { element, rank: 1 } = &resolved {
        return encode_array(binder, tokens, enclosing, expr, element, blob);
    }
    if let ExprKind::Name { name, .. } = &expr.kind {
        if let Some(constant) = binder
            .model()
            .get_by_symbol(enclosing)
            .and_then(|info| info.find_field(name).and_then(|f| f.constant.clone()))
        {
            return encode_literal(&constant, &resolved, blob);
        }
    }
    match &expr.kind {
        ExprKind::Literal(literal) => encode_literal(literal, &resolved, blob),
        ExprKind::TypeOf(target) => {
            encode_ser_string(&type_serialization_name(target), blob);
            Some(())
        }
        _ => {
            let (enum_ty, value) = enum_argument_value(binder, &resolved, expr)?;
            let underlying = tokens
                .enum_underlying(&enum_ty)
                .unwrap_or_else(|| enum_underlying(binder.model(), &enum_ty));
            encode_integer(underlying, value as u64, blob)
        }
    }
}

/// Encodes a `SZARRAY` fixed-argument value (II.23.3): a 4-byte element count (`0xFFFFFFFF`
/// for a null array), then each element encoded in turn at the array's `element` type. The
/// argument is a `null` literal or a `new T[] { ... }` creation with a constant initializer;
/// anything else yields `None` (skip the attribute).
fn encode_array(
    binder: &Binder,
    tokens: &Tokens,
    enclosing: &TypeSymbol,
    expr: &Expr,
    element: &TypeSymbol,
    blob: &mut Vec<u8>,
) -> Option<()> {
    if let ExprKind::Literal(Literal::Null) = &expr.kind {
        blob.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        return Some(());
    }
    let ExprKind::ArrayCreation {
        initializer: Some(initializer),
        ..
    } = &expr.kind
    else {
        return None;
    };
    let ExprKind::ArrayInitializer(elements) = &initializer.kind else {
        return None;
    };
    let count = u32::try_from(elements.len()).ok()?;
    blob.extend_from_slice(&count.to_le_bytes());
    for element_expr in elements {
        encode_value(binder, tokens, enclosing, element_expr, element, blob)?;
    }
    Some(())
}

/// The enum type and underlying integer value of an enum-constant argument `E.Member`. When
/// the target type `resolved` is the enum, its member gives the value directly (so a
/// reference-assembly enum resolves without a bare-name model lookup); otherwise the enum is
/// derived from the expression itself (a simple-named, this-module enum).
fn enum_argument_value(
    binder: &Binder,
    resolved: &TypeSymbol,
    expr: &Expr,
) -> Option<(TypeSymbol, i64)> {
    let model = binder.model();
    if let ExprKind::MemberAccess { name, .. } = &expr.kind {
        if let Some(info) = model.get_by_symbol(resolved) {
            if info.kind == lamella_binder::TypeKind::Enum {
                if let Some(value) = info
                    .find_field(name)
                    .and_then(|field| field.constant.as_ref())
                    .and_then(lamella_binder::literal_int_value)
                {
                    return Some((resolved.clone(), value));
                }
            }
        }
    }
    enum_member_constant(model, expr)
}

/// Encodes a constant literal by its target type.
fn encode_literal(literal: &Literal, ty: &TypeSymbol, blob: &mut Vec<u8>) -> Option<()> {
    let TypeSymbol::Special(special) = ty else {
        return None;
    };
    match (special, literal) {
        (SpecialType::Boolean, Literal::Boolean(value)) => blob.push(u8::from(*value)),
        (SpecialType::Char, Literal::Character(value)) => {
            blob.extend_from_slice(&value.to_le_bytes());
        }
        (SpecialType::String, Literal::String(units)) => {
            encode_ser_string_units(units, blob);
        }
        (_, Literal::Integer { value, .. }) => return encode_integer(*special, *value, blob),
        _ => return None,
    }
    Some(())
}

/// Encodes a named attribute argument (II.23.3): the FIELD (0x53) / PROPERTY (0x54) tag, the
/// target's element type, its name, and the value. The target is resolved as a field or
/// property of the attribute type. `None` (skip) if it is neither or cannot be encoded.
fn encode_named_argument(
    binder: &Binder,
    tokens: &Tokens,
    enclosing: &TypeSymbol,
    attribute_ty: &TypeSymbol,
    name: &str,
    value: &Expr,
    blob: &mut Vec<u8>,
) -> Option<()> {
    let (tag, target_ty) = {
        let info = binder.model().get_by_symbol(attribute_ty)?;
        if let Some(field) = info.find_field(name) {
            (0x53u8, field.ty.clone())
        } else if let Some(property) = info.find_property(name) {
            (0x54u8, property.ty.clone())
        } else {
            return None;
        }
    };
    let target_ty = binder.resolve_type(&target_ty);
    blob.push(tag);
    encode_element_type(binder.model(), &target_ty, blob)?;
    encode_ser_string(name, blob);
    encode_value(binder, tokens, enclosing, value, &target_ty, blob)
}

/// Encodes the FieldOrPropType of a named argument (II.23.3): a primitive's element-type
/// code, `0x50` for `System.Type`, or `0x55` and the enum's name for an enum.
fn encode_element_type(model: &Model, ty: &TypeSymbol, blob: &mut Vec<u8>) -> Option<()> {
    if let TypeSymbol::Special(special) = ty {
        blob.push(primitive_element_code(*special)?);
        return Some(());
    }
    if is_system_type(ty, "Type") {
        blob.push(0x50);
        return Some(());
    }
    if model
        .get_by_symbol(ty)
        .is_some_and(|info| info.kind == lamella_binder::TypeKind::Enum)
    {
        blob.push(0x55);
        encode_ser_string(&type_name(ty), blob);
        return Some(());
    }
    None
}

/// The blob element-type code (II.23.1.16) of a primitive type, or `None` for one with none.
fn primitive_element_code(special: SpecialType) -> Option<u8> {
    Some(match special {
        SpecialType::Boolean => 0x02,
        SpecialType::Char => 0x03,
        SpecialType::SByte => 0x04,
        SpecialType::Byte => 0x05,
        SpecialType::Int16 => 0x06,
        SpecialType::UInt16 => 0x07,
        SpecialType::Int32 => 0x08,
        SpecialType::UInt32 => 0x09,
        SpecialType::Int64 => 0x0A,
        SpecialType::UInt64 => 0x0B,
        SpecialType::Single => 0x0C,
        SpecialType::Double => 0x0D,
        SpecialType::String => 0x0E,
        _ => return None,
    })
}

/// The CLR name a `typeof(T)` serializes to in a custom attribute (II.23.3) -- the type's
/// namespace-qualified name (the runtime resolves it in the attribute's assembly / mscorlib).
fn type_serialization_name(target: &TypeRef) -> String {
    type_name(&bind_type(target))
}

/// Whether a static `Main` has a valid entry-point signature (10.1): return type `void` or `int`,
/// and either no parameters or a single `string[]` parameter. Distinguishes the real entry point
/// from an unrelated overload such as `Main(int)`.
fn is_entry_point_signature(parameters: &[Parameter], return_type: &TypeRef) -> bool {
    let ret = bind_type(return_type);
    let ret_ok = ret.is_void() || matches!(ret, TypeSymbol::Special(SpecialType::Int32));
    let params_ok = match parameters {
        [] => true,
        [only] => matches!(
            bind_type(&only.ty),
            TypeSymbol::Array { element, rank: 1 } if matches!(*element, TypeSymbol::Special(SpecialType::String))
        ),
        _ => false,
    };
    ret_ok && params_ok
}

/// A type's `namespace.name` (or bare `name` in the global namespace).
fn type_name(ty: &TypeSymbol) -> String {
    if let TypeSymbol::Special(special) = ty {
        let (namespace, name) = special.full_name();
        return joined_name(namespace, name);
    }
    match split_type_name(ty) {
        Some((namespace, name)) => joined_name(&namespace, &name),
        None => String::new(),
    }
}

/// Joins `namespace.name`, or just `name` when the namespace is empty.
fn joined_name(namespace: &str, name: &str) -> String {
    if namespace.is_empty() {
        return String::from(name);
    }
    let mut full = String::from(namespace);
    full.push('.');
    full.push_str(name);
    full
}

/// Whether `ty` is the named BCL type `System.<name>`.
fn is_system_type(ty: &TypeSymbol, name: &str) -> bool {
    matches!(split_type_name(ty), Some((namespace, type_name)) if namespace == "System" && type_name == name)
}

/// Resolves an enum-constant argument `E.V` to its enum type and underlying integer value.
fn enum_member_constant(model: &Model, expr: &Expr) -> Option<(TypeSymbol, i64)> {
    let ExprKind::MemberAccess { receiver, name } = &expr.kind else {
        return None;
    };
    let ExprKind::Name { name: enum_name, .. } = &receiver.kind else {
        return None;
    };
    let enum_ty = TypeSymbol::Named([enum_name.clone()].into());
    let info = model.get_by_symbol(&enum_ty)?;
    if info.kind != lamella_binder::TypeKind::Enum {
        return None;
    }
    let value = info
        .find_field(name)?
        .constant
        .as_ref()
        .and_then(lamella_binder::literal_int_value)?;
    Some((enum_ty, value))
}

/// Whether `ty` is System.Enum or System.ValueType -- the two abstract classes that extend
/// System.ValueType in metadata but are themselves REFERENCE types (a value of that static type is
/// a boxed object). They must be encoded as Class, never as a value type, in signatures.
fn is_reference_base_class(ty: &TypeSymbol) -> bool {
    matches!(ty, TypeSymbol::Named(parts)
        if matches!(&**parts, [ns, name] if &**ns == "System" && (&**name == "Enum" || &**name == "ValueType")))
}

/// An enum's underlying integral type (from its `value__` field), defaulting to `int`.
fn enum_underlying(model: &Model, enum_ty: &TypeSymbol) -> SpecialType {
    match model
        .get_by_symbol(enum_ty)
        .and_then(|info| info.find_field("value__").map(|field| field.ty.clone()))
    {
        Some(TypeSymbol::Special(special)) => special,
        _ => SpecialType::Int32,
    }
}

/// Encodes an integer constant of width `special` little-endian; `None` for a non-integral.
fn encode_integer(special: SpecialType, value: u64, blob: &mut Vec<u8>) -> Option<()> {
    match special {
        SpecialType::SByte | SpecialType::Byte => blob.push(value as u8),
        SpecialType::Int16 | SpecialType::UInt16 => {
            blob.extend_from_slice(&(value as u16).to_le_bytes());
        }
        SpecialType::Int32 | SpecialType::UInt32 => {
            blob.extend_from_slice(&(value as u32).to_le_bytes());
        }
        SpecialType::Int64 | SpecialType::UInt64 => blob.extend_from_slice(&value.to_le_bytes()),
        _ => return None,
    }
    Some(())
}

/// A `SerString` (II.23.3): a compressed unsigned byte-length, then the UTF-8 bytes.
fn encode_ser_string(text: &str, blob: &mut Vec<u8>) {
    encode_compressed_u32(text.len() as u32, blob);
    blob.extend_from_slice(text.as_bytes());
}

/// Encodes UTF-16 code units as a SerString (II.23.3), combining a well-formed surrogate pair but
/// preserving a LONE surrogate as its own 3-byte form (WTF-8). A lossy `from_utf16` would collapse a
/// lone surrogate to one U+FFFD; csc keeps it, so the value round-trips through reflection the same.
fn encode_ser_string_units(units: &[u16], blob: &mut Vec<u8>) {
    let mut utf8: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < units.len() {
        let unit = u32::from(units[i]);
        let code = if (0xD800..=0xDBFF).contains(&unit)
            && i + 1 < units.len()
            && (0xDC00..=0xDFFF).contains(&u32::from(units[i + 1]))
        {
            i += 1;
            0x1_0000 + ((unit - 0xD800) << 10) + (u32::from(units[i]) - 0xDC00)
        } else {
            unit
        };
        i += 1;
        push_utf8(code, &mut utf8);
    }
    encode_compressed_u32(utf8.len() as u32, blob);
    blob.extend_from_slice(&utf8);
}

/// Appends a Unicode scalar (or a lone surrogate, encoded as WTF-8) as UTF-8 bytes.
fn push_utf8(code: u32, out: &mut Vec<u8>) {
    if code < 0x80 {
        out.push(code as u8);
    } else if code < 0x800 {
        out.push(0xC0 | (code >> 6) as u8);
        out.push(0x80 | (code & 0x3F) as u8);
    } else if code < 0x1_0000 {
        out.push(0xE0 | (code >> 12) as u8);
        out.push(0x80 | ((code >> 6) & 0x3F) as u8);
        out.push(0x80 | (code & 0x3F) as u8);
    } else {
        out.push(0xF0 | (code >> 18) as u8);
        out.push(0x80 | ((code >> 12) & 0x3F) as u8);
        out.push(0x80 | ((code >> 6) & 0x3F) as u8);
        out.push(0x80 | (code & 0x3F) as u8);
    }
}

/// Compresses an unsigned integer into the metadata blob form (II.23.2).
fn encode_compressed_u32(value: u32, blob: &mut Vec<u8>) {
    if value < 0x80 {
        blob.push(value as u8);
    } else if value < 0x4000 {
        blob.push((0x80 | (value >> 8)) as u8);
        blob.push(value as u8);
    } else {
        blob.push((0xC0 | (value >> 24)) as u8);
        blob.push((value >> 16) as u8);
        blob.push((value >> 8) as u8);
        blob.push(value as u8);
    }
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

/// Emits the incremental-REPL BOOTSTRAP module: a library assembly defining
/// `<repl>.__Repl` as an empty `public class` extending `System.Object`, with a public
/// parameterless instance `.ctor` (`ldarg.0; call object::.ctor(); ret`). The runtime
/// loads this once at session open and creates the single persistent `__Repl` instance;
/// every later submission delta references this type by name and grows it. Defining
/// `__Repl` here single-sources its identity in the compiler. (See `session.rs`.)
pub(crate) fn build_bootstrap_delta(
    module_name: &str,
    assembly_name: &str,
) -> Result<Vec<u8>, crate::EmitError> {
    let tokens = Tokens::new();
    let mut image = ImageBuilder::new(module_name, assembly_name);
    let object = image.object_type();
    image.add_type("<repl>", "__Repl", object, PUBLIC_CLASS);

    let prologue = ConstructorPrologue {
        ctor: image.object_ctor(),
        span: None,
        arguments: Vec::new(),
        leading_body: 0,
        zero_initialize: None,
    };
    let empty = BoundStmt {
        kind: BoundStmtKind::Block(Vec::new()),
        span: Span::empty_at(0),
    };
    let emitted = emit_body(
        &[],
        &[],
        &[],
        &empty,
        &tokens,
        1,
        &TypeSymbol::Special(SpecialType::Void),
        Some(&prologue),
        None,
    )?;
    let body_image = MethodBodyImage {
        max_stack: max_stack(&emitted.code).max(1),
        init_locals: false,
        local_var_sig: None,
        code: emitted.code.into_boxed_slice(),
        handlers: emitted.handlers.into_boxed_slice(),
    };
    let body_bytes = write_method_body(&body_image)
        .map_err(|_| crate::EmitError::Unsupported("bootstrap .ctor body could not be written"))?;
    let ctor_sig = method_signature(true, &[], &TypeSig::Void);
    image.add_method(
        ".ctor",
        &ctor_sig,
        &body_bytes,
        ctor_flags(&[Modifier::Public]),
        IL_MANAGED,
        &[],
    );
    Ok(image.finish(Token::new(0, 0), true))
}

/// Emits one incremental-REPL SUBMISSION delta: a library module that references the
/// persistent `<repl>.__Repl` (a `TypeRef`, never a `TypeDef`) and carries one static
/// method `Submit$index(__Repl s)` whose body is `bound`. Session variables are fields of
/// `s` reached by `ldarg.0` + `ldfld`/`stfld` of `<repl>.__Repl::name` `FieldRef`s; a
/// field the runtime cannot resolve against the loaded `__Repl` is a NEW session variable
/// it adds (inference). The method lives on a fresh holder type `<repl>.Submission$index`,
/// unique per submission so holders do not collide when deltas merge into the one
/// persistent module. `return_type` is `void` for a statement submission (and `object`,
/// boxed, for an expression submission -- a following increment).
pub(crate) fn build_submission_delta(
    bound: &BoundStmt,
    repl_type: &TypeSymbol,
    index: u64,
    return_type: &TypeSymbol,
    type_members: &[NamespaceMember],
    model: &Model,
    module_name: &str,
    assembly_name: &str,
) -> Result<Vec<u8>, crate::EmitError> {
    let mut tokens = Tokens::new();
    let mut image = ImageBuilder::new(module_name, assembly_name);
    let object = image.object_type();

    if !type_members.is_empty() {
        let mut next_type = 1u32;
        let mut next_field = 0u32;
        let mut next_method = 0u32;
        let mut binder = Binder::with_model(model.clone());
        let partials = PartialIndex::new();
        collect_tokens(
            &mut tokens,
            &mut binder,
            &mut next_type,
            &mut next_field,
            &mut next_method,
            &[],
            type_members,
            "",
            &partials,
        );
        let mut entry_point = None;
        emit_namespace(
            &mut image,
            &mut binder,
            object,
            &mut tokens,
            &mut entry_point,
            &[],
            type_members,
            "",
            None,
            &partials,
        )?;
    }

    mint_named_type_token(repl_type, &mut image, &mut tokens);
    mint_references(bound, &mut image, &mut tokens);

    let holder_name = format!("Submission${index}");
    image.add_type("<repl>", &holder_name, object, PUBLIC_CLASS);

    let parameter_names = [ParamRow::from("s")];
    let emitted = emit_body(
        &param_row_names(&parameter_names),
        &[],
        &[],
        bound,
        &tokens,
        0,
        return_type,
        None,
        None,
    )?;
    let local_var_sig = if emitted.local_types.is_empty() {
        None
    } else {
        let locals: Vec<TypeSig> = emitted
            .local_types
            .iter()
            .map(|ty| type_sig(&tokens, ty))
            .collect::<Result<_, _>>()?;
        Some(image.add_standalone_sig(&local_signature(&locals)))
    };
    let max_stack = if emitted.handlers.is_empty() {
        max_stack(&emitted.code)
    } else {
        max_stack(&emitted.code).max(1)
    };
    let body_image = MethodBodyImage {
        max_stack,
        init_locals: local_var_sig.is_some(),
        local_var_sig,
        code: emitted.code.into_boxed_slice(),
        handlers: emitted.handlers.into_boxed_slice(),
    };
    let body_bytes = write_method_body(&body_image)
        .map_err(|_| crate::EmitError::Unsupported("submission body could not be written"))?;
    let signature = method_signature(
        false,
        &[type_sig(&tokens, repl_type)?],
        &type_sig(&tokens, return_type)?,
    );
    let method_name = format!("Submit${index}");
    image.add_method(
        &method_name,
        &signature,
        &body_bytes,
        METHOD_PUBLIC | METHOD_STATIC,
        IL_MANAGED,
        &parameter_names,
    );
    Ok(image.finish(Token::new(0, 0), true))
}

/// Source context for resolving a statement's span to line/column while emitting.
/// One per source file in a compilation; `document` is that file's 1-based `Document`
/// row, which every method emitted from this unit attributes its points to.
struct DebugContext<'a> {
    source: &'a str,
    lines: LineMap,
    document: u32,
}

#[allow(clippy::too_many_arguments)]
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
    partials: &PartialIndex<'_>,
) -> Result<(), crate::EmitError> {
    let scope = binder.import_scope();
    for using in usings {
        binder.import_using(&using.kind);
    }
    let mut prefix = String::new();
    for part in namespace.split('.').filter(|part| !part.is_empty()) {
        if !prefix.is_empty() {
            prefix.push('.');
        }
        prefix.push_str(part);
        binder.import_namespace(&prefix);
    }
    for member in members {
        match member {
            NamespaceMember::Type(declaration) => {
                if is_later_partial_part(partials, namespace, declaration) {
                    continue;
                }
                let later = continuation_parts(partials, namespace, declaration).unwrap_or(&[]);
                emit_type(
                    image,
                    binder,
                    object,
                    tokens,
                    entry_point,
                    namespace,
                    declaration,
                    debug,
                    None,
                    later,
                    partials,
                )?;
                let enclosing_full = lamella_binder::declared_full_name(namespace, declaration);
                emit_nested_types(
                    image,
                    binder,
                    object,
                    tokens,
                    entry_point,
                    &enclosing_full,
                    declaration,
                    debug,
                    partials,
                )?;
                for part in later {
                    let part_scope = enter_part(binder, part);
                    let emitted = emit_nested_types(
                        image,
                        binder,
                        object,
                        tokens,
                        entry_point,
                        &enclosing_full,
                        part.declaration,
                        part.debug,
                        partials,
                    );
                    leave_part(binder, part_scope);
                    emitted?;
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
                    partials,
                )?;
            }
            NamespaceMember::Delegate(declaration) => {
                emit_delegate(image, binder, tokens, namespace, declaration)?;
            }
            NamespaceMember::Enum(declaration) => {
                emit_enum(image, binder, tokens, namespace, declaration)?;
            }
        }
    }
    binder.restore_import_scope(scope);
    Ok(())
}

/// Emits the types NESTED in one declaration, depth-first under the enclosing type's full name.
///
/// Split out of [`emit_namespace`] so a PARTIAL type can run it once per part: a nested type
/// declared in the second part of `partial class Outer` is Outer's nested type just as much as one
/// declared in the first, and the walk that reached it before skipped every later part whole.
#[allow(clippy::too_many_arguments)]
fn emit_nested_types(
    image: &mut ImageBuilder,
    binder: &mut Binder,
    object: Token,
    tokens: &mut Tokens,
    entry_point: &mut Option<Token>,
    enclosing_full: &str,
    declaration: &TypeDecl,
    debug: Option<&DebugContext>,
    partials: &PartialIndex<'_>,
) -> Result<(), crate::EmitError> {
    for member in &declaration.members {
        if let Member::NestedType(nested) = member {
            if matches!(
                nested.as_ref(),
                NamespaceMember::Type(_) | NamespaceMember::Enum(_) | NamespaceMember::Delegate(_)
            ) {
                emit_namespace(
                    image,
                    binder,
                    object,
                    tokens,
                    entry_point,
                    &[],
                    core::slice::from_ref(nested.as_ref()),
                    enclosing_full,
                    debug,
                    partials,
                )?;
            }
        }
    }
    Ok(())
}

/// Brings one partial part's FILE CONTEXT into the binder for the length of its emission: its
/// `using` directives and aliases, and its `#define` set. Returns the import scope to restore.
///
/// **A PART IN ANOTHER FILE WAS BOUND UNDER THAT FILE'S SCOPE, AND EMISSION RE-RESOLVES.** A member
/// whose body names `Console` under its own file's `using System;` has to find it here too, and a
/// name its file did NOT import must not resolve just because the first part's file did.
fn enter_part(binder: &mut Binder, part: &PartialPart<'_>) -> PartScope {
    let imports = binder.import_scope();
    let defined = binder.replace_defined_symbols(part.defined_symbols.clone());
    for using in part.usings {
        binder.import_using(&using.kind);
    }
    PartScope { imports, defined }
}

/// What [`enter_part`] displaced, to be handed to [`leave_part`].
struct PartScope {
    imports: lamella_binder::bound::ImportScope,
    defined: alloc::collections::BTreeSet<Box<str>>,
}

/// Puts back the file context [`enter_part`] displaced. The walk that reached the part is still
/// inside ANOTHER file's unit, and every type after it there is entitled to that file's scope.
fn leave_part(binder: &mut Binder, scope: PartScope) {
    binder.restore_import_scope(scope.imports);
    binder.set_defined_symbols(scope.defined);
}

/// Emits one declaration's `GenericParam` rows (II.22.20) WITH their constraint flag word, plus a
/// `GenericParamConstraint` row (II.22.21) for each named constraint.
///
/// **ONE IMPLEMENTATION FOR EVERY DECLARATION SITE** -- a class, an interface, and a generic method.
/// All three previously called `add_generic_param` with a hard-coded `0` flags word, which is
/// exactly the shape where a fourth site arrives without constraints and nothing looks wrong. The
/// constraints come from `lamella_binder::constraints_by_parameter`, the SAME function the binder
/// checks against, so the metadata and the diagnostics cannot disagree about what the source wrote.
///
/// **THE FLAG BITS AND THE ROWS ARE DIFFERENT ENCODINGS AND BOTH ARE NEEDED.** `class`/`struct`/
/// `new()` are bits `0x001C` of the flag word; a named class, interface or type parameter is a ROW.
/// A parameter with only `where T : class` therefore produces no row at all.
///
/// **`struct` IMPLIES THE DEFAULT-CONSTRUCTOR BIT HERE, THOUGH NOT IN THE SOURCE.** II.10.1.7
/// requires an emitter that sets `0x0008` to set `0x0010` with it -- every value type has a
/// parameterless constructor. C# forbids WRITING both (CS0451), so the model keeps the source fact
/// and the implication is applied at this boundary rather than in the checker.
/// A `System.<name>` token: this module's own `TypeDef` when it declares the type (a corlib
/// self-build does), else a `TypeRef`. The same two-step every other external reference takes, so a
/// corlib build names `System.ValueType` by definition rather than referencing itself.
fn system_type_token(image: &mut ImageBuilder, tokens: &Tokens, name: &str) -> Token {
    let symbol = named_symbol("System", name);
    tokens
        .type_token(&symbol)
        .unwrap_or_else(|| image.type_ref("System", name))
}

fn emit_generic_parameters(
    image: &mut ImageBuilder,
    tokens: &Tokens,
    owner: Token,
    names: &[Box<str>],
    clauses: &[lamella_syntax::ast::TypeParameterConstraintClause],
) {
    if names.is_empty() {
        return;
    }
    let constraints = lamella_binder::constraints_by_parameter(names, clauses);
    for (number, parameter) in names.iter().enumerate() {
        let written = constraints.get(number);
        let mut flags = 0u16;
        if let Some(written) = written {
            if written.reference_type {
                flags |= 0x0004;
            }
            if written.value_type {
                flags |= 0x0008 | 0x0010;
            }
            if written.default_constructor {
                flags |= 0x0010;
            }
        }
        let param_token = image.add_generic_param(owner, number as u16, flags, parameter);
        let Some(written) = written else {
            continue;
        };
        if written.value_type {
            let value_type = system_type_token(image, tokens, "ValueType");
            image.add_generic_param_constraint(param_token, value_type);
        }
        for constraint in &written.types {
            let token = tokens.type_token(constraint).or_else(|| {
                split_type_name(constraint)
                    .map(|(namespace, name)| image.type_ref(&namespace, &name))
            });
            if let Some(token) = token {
                image.add_generic_param_constraint(param_token, token);
            }
        }
    }
}

/// Emits an interface as a `TypeDef` with no base, no constructor, and abstract
/// methods (II.22.37 semantics). Implementing classes get an `InterfaceImpl` row.
///
/// `continuation` carries the row a PARTIAL interface's first part already wrote, for the same
/// reason [`emit_type_inner`] takes one: the parts are one type, and a second `TypeDef` of the same
/// name is an image the CLR refuses to load.
fn emit_interface(
    image: &mut ImageBuilder,
    binder: &Binder,
    tokens: &mut Tokens,
    namespace: &str,
    declaration: &TypeDecl,
    continuation: Option<Token>,
) -> Result<Token, crate::EmitError> {
    let enclosing = declared_type_symbol(namespace, declaration);
    let enclosing_type = binder
        .model()
        .get_by_symbol(&enclosing)
        .and_then(|info| info.enclosing.clone());
    let type_token = match continuation {
        Some(token) => token,
        None => {
            let nil = Token::new(TYPE_DEF, 0);
            let metadata_namespace = if enclosing_type.is_some() { "" } else { namespace };
            let flags = (INTERFACE_FLAGS & !0x0000_0007)
                | type_visibility(&declaration.modifiers, enclosing_type.is_some());
            let token =
                image.add_type(metadata_namespace, &declared_type_name(declaration), nil, flags);
            if let Some(enclosing_full) = &enclosing_type {
                if let Some(enclosing_token) =
                    tokens.type_token(&type_symbol_from_dotted(enclosing_full))
                {
                    image.add_nested_class(token, enclosing_token);
                }
            }
            emit_generic_parameters(
                image,
                tokens,
                token,
                &type_parameter_names(binder, &enclosing, declaration),
                &declaration.constraints,
            );
            token
        }
    };
    let direct: Vec<TypeSymbol> = if continuation.is_some() {
        Vec::new()
    } else {
        binder
            .model()
            .get_by_symbol(&enclosing)
            .map(|info| {
                info.bases
                    .iter()
                    .filter_map(|base| binder.model().resolve_interface_base(base))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut interfaces: Vec<TypeSymbol> = Vec::new();
    for interface in direct {
        collect_interface_closure(binder.model(), interface, &mut interfaces);
    }
    let mut interface_tokens: Vec<Token> = Vec::new();
    for interface in &interfaces {
        mint_named_type_token(interface, image, tokens);
        if let Some(token) = tokens.type_token(interface) {
            interface_tokens.push(token);
        }
    }
    for interface in interface_tokens {
        image.add_interface_impl(type_token, interface);
    }
    for member in &declaration.members {
        if let Member::Method {
            return_type,
            name,
            type_parameters,
            parameters,
            ..
        } = member
        {
            if !type_parameters.is_empty() {
                return Err(crate::EmitError::Unsupported(
                    "a generic method declared on an interface",
                ));
            }
            let parameter_sigs: Vec<TypeSig> = parameters
                .iter()
                .map(|parameter| {
                    member_type_sig(tokens, &enclosing, &binder.canonicalize(&parameter_symbol(parameter)))
                })
                .collect::<Result<_, _>>()?;
            let return_sig = member_type_sig(tokens, &enclosing, &binder.canonicalize(&bind_type(return_type)))?;
            let return_sig = bodyless_return_sig(return_type, return_sig, image, tokens);
            let signature = method_signature(true, &parameter_sigs, &return_sig);
            let method =
                image.add_abstract_method(name, &signature, IFACE_METHOD_FLAGS, &parameter_names(parameters));
            if is_readonly_ref(return_type) {
                mark_readonly_return(image, tokens, method);
            }
            emit_declared_parameter_metadata(image, binder, tokens, &enclosing, method, parameters);
        }
    }
    let mut first_property = None;
    for member in &declaration.members {
        if let Member::Property {
            ty, name, getter, setter, ..
        } = member
        {
            let property_ty = binder.canonicalize(&bind_type(ty));
            let element = member_type_sig(tokens, &enclosing, &property_ty)?;
            let property_sig = bodyless_return_sig(ty, element.clone(), image, tokens);
            let property = image.add_property(name, &property_signature(true, &[], &property_sig), 0);
            if is_readonly_ref(ty) {
                attach_is_readonly(tokens, property);
            }
            if getter.is_some() {
                let signature =
                    method_signature(true, &[], &bodyless_return_sig(ty, element.clone(), image, tokens));
                let token = image.add_abstract_method(
                    &accessor_name("get_", name),
                    &signature,
                    IFACE_METHOD_FLAGS | SPECIAL_NAME,
                    &[],
                );
                if is_readonly_ref(ty) {
                    mark_readonly_return(image, tokens, token);
                }
                image.add_method_semantics(SEMANTICS_GETTER, token, property);
            }
            if setter.is_some() {
                let signature = method_signature(true, &[element.clone()], &TypeSig::Void);
                let token = image.add_abstract_method(
                    &accessor_name("set_", name),
                    &signature,
                    IFACE_METHOD_FLAGS | SPECIAL_NAME,
                    &[SETTER_VALUE_PARAMETER.into()],
                );
                image.add_method_semantics(SEMANTICS_SETTER, token, property);
            }
            first_property.get_or_insert(property);
        }
        if let Member::Indexer {
            ty,
            parameters,
            getter,
            setter,
            attributes,
            ..
        } = member
        {
            let name = indexer_name(attributes);
            let element = member_type_sig(tokens, &enclosing, &binder.canonicalize(&bind_type(ty)))?;
            let indices: Vec<TypeSig> = parameters
                .iter()
                .map(|parameter| {
                    member_type_sig(tokens, &enclosing, &binder.canonicalize(&bind_type(&parameter.ty)))
                })
                .collect::<Result<_, _>>()?;
            let property =
                image.add_property(&name, &property_signature(true, &indices, &element), 0);
            if getter.is_some() {
                let signature = method_signature(true, &indices, &element);
                let token = image.add_abstract_method(
                    &accessor_name("get_", &name),
                    &signature,
                    IFACE_METHOD_FLAGS | SPECIAL_NAME,
                    &parameter_names(parameters),
                );
                image.add_method_semantics(SEMANTICS_GETTER, token, property);
            }
            if setter.is_some() {
                let mut signature_params = indices.clone();
                signature_params.push(element.clone());
                let signature = method_signature(true, &signature_params, &TypeSig::Void);
                let mut names = parameter_names(parameters);
                names.push(SETTER_VALUE_PARAMETER.into());
                let token = image.add_abstract_method(
                    &accessor_name("set_", &name),
                    &signature,
                    IFACE_METHOD_FLAGS | SPECIAL_NAME,
                    &names,
                );
                image.add_method_semantics(SEMANTICS_SETTER, token, property);
            }
            first_property.get_or_insert(property);
        }
    }
    if let Some(first) = first_property {
        image.add_property_map(type_token, first);
    }
    emit_default_member_attribute(image, tokens, type_token, &declaration.members);
    let mut first_event = None;
    for member in &declaration.members {
        if let Member::EventField {
            ty, declarators, ..
        } = member
        {
            let event_ty = binder.canonicalize(&bind_type(ty));
            let event_type_token =
                tokens
                    .type_token(&event_ty)
                    .ok_or(crate::EmitError::Unsupported(
                        "an interface event whose delegate type has no metadata token",
                    ))?;
            let signature = method_signature(true, &[member_type_sig(tokens, &enclosing, &event_ty)?], &TypeSig::Void);
            for declarator in declarators {
                let event = image.add_event(&declarator.name, event_type_token);
                let add = image.add_abstract_method(
                    &accessor_name("add_", &declarator.name),
                    &signature,
                    IFACE_METHOD_FLAGS | SPECIAL_NAME,
                    &[SETTER_VALUE_PARAMETER.into()],
                );
                image.add_method_semantics(SEMANTICS_ADDON, add, event);
                let remove = image.add_abstract_method(
                    &accessor_name("remove_", &declarator.name),
                    &signature,
                    IFACE_METHOD_FLAGS | SPECIAL_NAME,
                    &[SETTER_VALUE_PARAMETER.into()],
                );
                image.add_method_semantics(SEMANTICS_REMOVEON, remove, event);
                first_event.get_or_insert(event);
            }
        }
    }
    if let Some(first) = first_event {
        image.add_event_map(type_token, first);
    }
    Ok(type_token)
}

/// Emits a delegate as a sealed class extending `System.MulticastDelegate`, with its
/// runtime-implemented `.ctor(object, native int)` and `Invoke(params) -> ret`. The
/// runtime supplies both bodies; `new D(method)` is `ldftn`/`ldvirtftn` + `newobj .ctor`, and
/// `d(args)` is `callvirt Invoke`. A `ref`/`out` delegate parameter carries its byref (`&`)
/// through to the `Invoke` signature, so it agrees with the byref target and the call site.
fn emit_delegate(
    image: &mut ImageBuilder,
    binder: &Binder,
    tokens: &mut Tokens,
    namespace: &str,
    declaration: &DelegateDecl,
) -> Result<(), crate::EmitError> {
    let inherited = enclosing_type_parameters(binder, &named_symbol(namespace, &declaration.name));
    mint_signature_type(
        binder,
        &binder.canonicalize(&bind_type(&declaration.return_type)),
        &[],
        &inherited,
        image,
        tokens,
    )?;
    for parameter in &declaration.parameters {
        mint_signature_type(binder, &binder.canonicalize(&bind_type(&parameter.ty)), &[], &inherited, image, tokens)?;
    }
    let base = system_base(image, tokens, "MulticastDelegate");
    let delegate_ty = named_symbol(namespace, &declaration.name);
    let enclosing_type = binder
        .model()
        .get_by_symbol(&delegate_ty)
        .and_then(|info| info.enclosing.clone());
    let metadata_namespace = if enclosing_type.is_some() { "" } else { namespace };
    let flags = (DELEGATE_TYPE_FLAGS & !0x0000_0007)
        | type_visibility(&declaration.modifiers, enclosing_type.is_some());
    let delegate_token = image.add_type(metadata_namespace, &declaration.name, base, flags);
    if let Some(enclosing_full) = &enclosing_type {
        if let Some(enclosing_token) = tokens.type_token(&type_symbol_from_dotted(enclosing_full)) {
            image.add_nested_class(delegate_token, enclosing_token);
        }
    }
    let ctor_signature =
        method_signature(true, &[TypeSig::Object, TypeSig::NativeInt], &TypeSig::Void);
    image.add_runtime_method(
        ".ctor",
        &ctor_signature,
        DELEGATE_CTOR_FLAGS,
        &[DELEGATE_CTOR_TARGET.into(), DELEGATE_CTOR_METHOD.into()],
    );
    let return_sig = type_sig(tokens, &binder.canonicalize(&bind_type(&declaration.return_type)))?;
    let parameter_sigs: Vec<TypeSig> = declaration
        .parameters
        .iter()
        .map(|parameter| {
            let base = type_sig(tokens, &binder.canonicalize(&bind_type(&parameter.ty)))?;
            Ok(
                if matches!(
                    parameter.modifier,
                    Some(ParameterModifier::Ref | ParameterModifier::Out)
                ) {
                    TypeSig::ByRef(Box::new(base))
                } else {
                    base
                },
            )
        })
        .collect::<Result<_, _>>()?;
    let invoke_signature = method_signature(true, &parameter_sigs, &return_sig);
    let invoke = image.add_runtime_method(
        "Invoke",
        &invoke_signature,
        DELEGATE_INVOKE_FLAGS,
        &parameter_names(&declaration.parameters),
    );
    emit_declared_parameter_metadata(image, binder, tokens, &named_symbol(namespace, &declaration.name), invoke, &declaration.parameters);
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
    tokens: &mut Tokens,
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

    let base = system_base(image, tokens, "Enum");
    let enclosing = binder
        .model()
        .get_by_symbol(&enum_ty)
        .and_then(|info| info.enclosing.clone());
    let metadata_namespace = if enclosing.is_some() { "" } else { namespace };
    let flags = (ENUM_TYPE_FLAGS & !0x0000_0007)
        | type_visibility(&declaration.modifiers, enclosing.is_some());
    let enum_type_token = image.add_type(metadata_namespace, &declaration.name, base, flags);
    if let Some(enclosing_full) = &enclosing {
        if let Some(enclosing_token) = tokens.type_token(&type_symbol_from_dotted(enclosing_full)) {
            image.add_nested_class(enum_type_token, enclosing_token);
        }
    }
    emit_attributes(image, binder, tokens, &enum_ty, enum_type_token, &declaration.attributes);
    let value_field_sig = field_signature(&type_sig(tokens, &underlying)?);
    image.add_field("value__", &value_field_sig, ENUM_VALUE_FIELD_FLAGS);
    let member_field_sig = field_signature(&TypeSig::ValueType(enum_token));
    let members: Vec<(Box<str>, i64)> = binder
        .model()
        .get_by_symbol(&enum_ty)
        .map(|info| {
            info.fields
                .iter()
                .map(|field| {
                    let value = field
                        .constant
                        .as_ref()
                        .and_then(lamella_binder::literal_int_value)
                        .unwrap_or(0);
                    (field.name.clone(), value)
                })
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

/// Adds `interface` and, transitively, every interface it inherits -- its own base
/// interfaces, and theirs, recursively (13.4.4) -- to `closure`, skipping any already
/// present. A type's declared interface set is the transitive closure of the interfaces
/// named after its `:`, so an implementing class emits an `InterfaceImpl` row (II.22.23)
/// for each: a member reached through an inherited base interface dispatches only when the
/// class itself declares that base interface, not merely the derived one that named it.
/// Depth-first, an interface before the base interfaces it names; order is immaterial as
/// the table is unsorted (II.24.2.6) and read by scan.
fn collect_interface_closure(model: &Model, interface: TypeSymbol, closure: &mut Vec<TypeSymbol>) {
    if closure.contains(&interface) {
        return;
    }
    let bases: Vec<TypeSymbol> = model
        .get_by_symbol(&interface)
        .map(|info| {
            info.bases
                .iter()
                .filter_map(|base| model.resolve_interface_base(base))
                .collect()
        })
        .unwrap_or_default();
    closure.push(interface);
    for base in bases {
        collect_interface_closure(model, base, closure);
    }
}

/// The compilation's OWN `TypeDef` for `System.<name>`, when it declares one -- a
/// corlib-style build (no platform references) closes the `extends` chain in-module
/// (II.22.37) instead of referencing the platform assembly.
fn declared_system_type(tokens: &Tokens, name: &str) -> Option<Token> {
    let symbol = named_symbol("System", name);
    tokens
        .type_token(&symbol)
        .filter(|token| token.table() == TYPE_DEF)
}

/// The base token for a `System.<name>` root (`ValueType`, `Enum`, `MulticastDelegate`):
/// the compilation's own `TypeDef` when declared here, else a `TypeRef` into the
/// referenced platform assembly.
fn system_base(image: &mut ImageBuilder, tokens: &Tokens, name: &str) -> Token {
    declared_system_type(tokens, name).unwrap_or_else(|| image.type_ref("System", name))
}

/// The `.ctor` a constructor chains to when the class declares no base: the declared
/// `System.Object`'s own constructor in a corlib-style build, else the platform
/// `Object::.ctor` member reference.
fn object_base_ctor(image: &mut ImageBuilder, tokens: &Tokens) -> Token {
    tokens
        .method(&named_symbol("System", "Object"), ".ctor", &[])
        .unwrap_or_else(|| image.object_ctor())
}

/// The parameterless `.ctor` of a class's declared base, for the implicit base call of a
/// synthesized default constructor (and of an explicit constructor with no `: base(...)` chain).
/// A base declared in THIS assembly has its constructor registered by the token pre-pass; a base
/// in a REFERENCED assembly does not, so mint a `MemberRef` to it -- mirroring how the `extends`
/// clause resolves the base type, which is [`base_type_token`] and its three shapes.
/// Falling back to `Object::.ctor` here (the previous behaviour) silently skips the base's own
/// construction -- its field initializers and constructor body never run.
fn base_class_ctor(
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
    base_class: &TypeSymbol,
    own_parameters: &[Box<str>],
) -> Token {
    if matches!(base_class, TypeSymbol::Instantiation { .. }) {
        if let Some(parent) = base_type_token(base_class, own_parameters, image, tokens) {
            return image.member_ref(parent, ".ctor", &method_signature(true, &[], &TypeSig::Void));
        }
    } else if let Some(token) = tokens.method(base_class, ".ctor", &[]) {
        return token;
    } else if let Some((namespace, name)) = split_type_name(base_class) {
        let parent = image.type_ref(&namespace, &name);
        return image.member_ref(parent, ".ctor", &method_signature(true, &[], &TypeSig::Void));
    }
    object_base_ctor(image, tokens)
}

/// The token naming a BASE TYPE in a type header -- the `extends` slot (II.22.37, a `TypeDefOrRef`)
/// and the parent of the base constructor a derived type chains to.
///
/// **THREE SHAPES, AND A NAME-BASED LOOKUP ANSWERS FOR ONLY TWO.** This module's `TypeDef` for a
/// sibling base, a `TypeRef` for one in a referenced assembly, and -- the shape that had no arm at
/// either position -- a `TypeSpec` for a CONSTRUCTED generic base, `class D : B<int>`. Without it
/// `split_type_name` returned `None` for the instantiation, the `extends` slot took its
/// `System.Object` fallback, and the derived type shipped with no base at all: exit 0, no
/// diagnostic, and the base's fields, virtuals and identity gone from the metadata BOTH TIERS read.
///
/// **THE DECLARING TYPE'S PARAMETERS ARE IN SCOPE HERE, WHICH IS WHY THEY ARE A PARAMETER OF THIS
/// FUNCTION AND NOT THE CALLER'S BUSINESS.** `class Bag<T> : Base<T>` names `` Base`1<!0> ``, and
/// the minting walk reads that position out of [`Tokens::body_scope`] -- which no method has
/// entered at a type header, so a `T` left to it is minted as a `TypeRef` to a class no assembly
/// declares. The `InterfaceImpl` list states the same rule at the same point for the same reason.
/// It is ONE rule at three positions in this header and only the interface one ever had it.
fn base_type_token(
    ty: &TypeSymbol,
    own_parameters: &[Box<str>],
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
) -> Option<Token> {
    if matches!(ty, TypeSymbol::Instantiation { .. }) {
        let saved = tokens.enter_body_scope(&[], own_parameters);
        let token = mint_composite_type_spec(ty, image, tokens);
        tokens.restore_body_scope(saved);
        return token;
    }
    tokens
        .type_token(ty)
        .or_else(|| split_type_name(ty).map(|(namespace, name)| image.type_ref(&namespace, &name)))
}

/// The base constructor an IMPLICIT `: base()` calls when it is not the parameterless one, with
/// the default arguments it needs -- `None` when the ordinary parameterless lookup is right.
///
/// **"TAKES NO PARAMETERS" AND "IS CALLABLE WITH NO ARGUMENTS" ARE DIFFERENT QUESTIONS**, and
/// `base_class_ctor` asks the first. Against `class B { public B(int v = 9) { } }` it finds
/// nothing, and its fallback mints a parameterless `.ctor` `MemberRef` on a fabricated `TypeRef` --
/// which, having no resolution scope, resolves against `mscorlib`. The CLR then answers
/// `TypeLoadException: Could not load type 'B' from assembly 'mscorlib'` for a type this module
/// DEFINES. Measured by running the image; every compile-time gate called it clean.
fn implicit_base_chain(
    image: &mut ImageBuilder,
    binder: &Binder,
    tokens: &mut Tokens,
    enclosing: &TypeSymbol,
) -> Option<(Token, Vec<BoundExpr>)> {
    let base = binder.model().get_by_symbol(enclosing)?.base.clone()?;
    let constructors = binder.model().get_by_symbol(&base)?.constructors.clone();
    if constructors.iter().any(|c| c.parameters.is_empty()) {
        return None;
    }
    let chosen = constructors
        .iter()
        .find(|c| c.accepts_argument_count(0) && !c.parameters.is_empty())?;
    let arguments: Vec<BoundExpr> = (0..chosen.parameters.len())
        .map(|index| {
            let literal = chosen.parameter_default(index)?.clone();
            Some(default_argument_expr(&chosen.parameters[index], &literal))
        })
        .collect::<Option<Vec<_>>>()?;
    let reference = lamella_binder::MethodReference {
        declaring_type: base.clone(),
        name: ".ctor".into(),
        parameters: chosen.parameters.clone(),
        return_type: TypeSymbol::Special(SpecialType::Void),
        is_static: false,
        is_vararg: chosen.is_vararg,
        instantiation: None,
        declaring_instantiation: None,
    };
    let ctor = tokens
        .method(&base, ".ctor", &chosen.parameters)
        .or_else(|| {
            mint_member_ref(&reference, image, tokens);
            tokens.method(&base, ".ctor", &chosen.parameters)
        })?;
    Some((ctor, arguments))
}

/// A default argument as a bound expression of the parameter's own type -- the emitter's copy of
/// the rule `Binder::default_argument` applies at a call site, and it must stay the same rule:
/// `Literal::Null` is csc's ENCODING for four different defaults, one of which is a struct's.
fn default_argument_expr(parameter_ty: &TypeSymbol, literal: &Literal) -> BoundExpr {
    if matches!(literal, Literal::Null) && !matches!(parameter_ty, TypeSymbol::Special(SpecialType::String | SpecialType::Object | SpecialType::Null)) {
        return BoundExpr {
            kind: BoundExprKind::DefaultValue(parameter_ty.clone()),
            ty: parameter_ty.clone(),
        };
    }
    BoundExpr {
        kind: BoundExprKind::Literal(literal.clone()),
        ty: parameter_ty.clone(),
    }
}

/// Emits one type, with its own type parameters IN SCOPE for the whole emission.
///
/// **THE EMITTER RE-BINDS EVERY METHOD BODY, AND IT WAS DOING SO IN A DIFFERENT SCOPE FROM THE
/// DIAGNOSTIC PASS.** `lamella_binder::program::bind_type_bodies` wraps body binding in
/// `enter_type_parameters`, so `T` resolves there and no diagnostic is produced; this stage binds
/// the same bodies again through `Binder::bind_method` and had never entered that scope, so `T`
/// resolved to the ERROR type here and only here.
///
/// **THE TWO HALVES FAILED IN DIFFERENT PHASES, WHICH IS WHY IT LOOKED LIKE AN EMIT BUG.** A local
/// `T x;` inside `class Box<T>` bound clean and then failed with *"the error type has no
/// signature"* -- and an emit-time diagnostic reaches no one, so nothing said `T` had gone
/// unresolved. Measured: a program with BOTH `Nope y;` and `T x;` reports CS0246 for `Nope` alone,
/// which is what proves `T` resolves in the binder and not here.
///
/// **THE WRAPPER EXISTS SO THE SCOPE ALWAYS CLOSES.** The body has `?` early returns, and
/// `unshadow` with nothing displaced REMOVES the name -- so a leaked scope would delete a real type
/// called `T` for the rest of the compilation. Same shape, and same reason, as the binder's own
/// `bind_type_bodies` / `bind_type_bodies_inner` split.
#[allow(clippy::too_many_arguments)]
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
    continuation: Option<Token>,
    later: &[PartialPart<'_>],
    partials: &PartialIndex<'_>,
) -> Result<(), crate::EmitError> {
    let entered =
        binder.enter_type_parameters(&declaration.type_parameters, &declaration.constraints);
    let outer = binder.replace_current_type(Some(declared_type_symbol(namespace, declaration)));
    let emitted = emit_type_inner(
        image,
        binder,
        object,
        tokens,
        entry_point,
        namespace,
        declaration,
        debug,
        continuation,
        later,
        partials,
    );
    binder.replace_current_type(outer);
    binder.exit_type_parameters(entered);
    emitted
}

/// Emits one type: its `TypeDef` row and every row its members own.
///
/// **`continuation` IS WHAT MAKES SEVERAL DECLARATIONS ONE TYPE.** For the FIRST part of a partial
/// type it is `None` and this writes the `TypeDef`, its generic parameters, its `NestedClass` row
/// and its attributes; for a LATER part it carries the token that row already has, and only the
/// part's MEMBERS are emitted -- into the ranges the first part opened. `later` is the remaining
/// parts, which this emits immediately after its own members, each under its own file's context,
/// because a row written between them would end the type's range (II.22.37).
#[allow(clippy::too_many_arguments)]
fn emit_type_inner(
    image: &mut ImageBuilder,
    binder: &mut Binder,
    object: Token,
    tokens: &mut Tokens,
    entry_point: &mut Option<Token>,
    namespace: &str,
    declaration: &TypeDecl,
    debug: Option<&DebugContext>,
    continuation: Option<Token>,
    later: &[PartialPart<'_>],
    partials: &PartialIndex<'_>,
) -> Result<(), crate::EmitError> {
    let is_struct = declaration.kind == TypeKind::Struct;
    let enclosing = declared_type_symbol(namespace, declaration);
    let own_parameters = type_parameter_names(binder, &enclosing, declaration);
    for index in 0..own_parameters.len() as u32 {
        if tokens.var_spec(index).is_none() {
            let spec = image.type_spec(&type_signature(&TypeSig::Var(index)));
            tokens.insert_var_spec(index, spec);
        }
    }
    if matches!(declaration.kind, TypeKind::Interface) {
        mint_member_signature_types(
            binder,
            &declaration.members,
            &own_parameters,
            &[],
            image,
            tokens,
        )?;
        let interface_token =
            emit_interface(image, binder, tokens, namespace, declaration, continuation)?;
        for part in later {
            let part_scope = enter_part(binder, part);
            let emitted = emit_type(
                image,
                binder,
                object,
                tokens,
                entry_point,
                namespace,
                part.declaration,
                part.debug,
                Some(interface_token),
                &[],
                partials,
            );
            leave_part(binder, part_scope);
            emitted?;
        }
        return Ok(());
    }
    let (base_class, nested_in): (Option<TypeSymbol>, Option<Box<str>>) = {
        let info = binder.model().get_by_symbol(&enclosing);
        let (base, enclosing_of) = match &info {
            Some(info) => (info.base.clone(), info.enclosing.clone()),
            None => (None, None),
        };
        (if is_struct { None } else { base }, enclosing_of)
    };
    let is_system_object = namespace == "System" && &*declaration.name == "Object";
    let type_token = match continuation {
        Some(token) => {
            emit_attributes(image, binder, tokens, &enclosing, token, &declaration.attributes);
            token
        }
        None => {
        let (base, flags) = if is_struct {
            (system_base(image, tokens, "ValueType"), PUBLIC_STRUCT)
        } else if is_system_object {
            (Token::new(TYPE_DEF, 0), PUBLIC_CLASS)
        } else {
            let base_token = base_class
                .as_ref()
                .and_then(|symbol| base_type_token(symbol, &own_parameters, image, tokens))
                .unwrap_or(object);
            (base_token, PUBLIC_CLASS)
        };
        let merged = merged_modifiers(declaration, later);
        let modifiers = merged.as_slice();
        let metadata_namespace = if nested_in.is_some() { "" } else { namespace };
        let mut flags = (flags & !0x0000_0007) | type_visibility(modifiers, nested_in.is_some());
        if modifiers.contains(&Modifier::Abstract) {
            flags |= TYPE_ABSTRACT;
        }
        if modifiers.contains(&Modifier::Sealed) {
            flags |= TYPE_SEALED;
        }
        if modifiers.contains(&Modifier::Static) {
            flags |= TYPE_ABSTRACT | TYPE_SEALED;
        }
        let declares_static_constructor = core::iter::once(declaration)
            .chain(later.iter().map(|part| part.declaration))
            .flat_map(|part| part.members.iter())
            .any(|member| {
                matches!(
                    member,
                    Member::Constructor { modifiers, .. } if modifiers.contains(&Modifier::Static)
                )
            });
        if !declares_static_constructor {
            flags |= TYPE_BEFORE_FIELD_INIT;
        }
        let type_token = image.add_type(metadata_namespace, &declared_type_name(declaration), base, flags);
        emit_generic_parameters(
            image,
            tokens,
            type_token,
            &own_parameters,
            &declaration.constraints,
        );
        if let Some(enclosing_full) = &nested_in {
            if let Some(enclosing_token) = tokens.type_token(&type_symbol_from_dotted(enclosing_full)) {
                image.add_nested_class(type_token, enclosing_token);
            }
        }
        emit_attributes(image, binder, tokens, &enclosing, type_token, &declaration.attributes);
        type_token
        }
    };
    mint_member_signature_types(
        binder,
        &declaration.members,
        &own_parameters,
        &[],
        image,
        tokens,
    )?;
    let direct_interfaces: Vec<TypeSymbol> = if continuation.is_some() {
        Vec::new()
    } else {
        binder
            .model()
            .get_by_symbol(&enclosing)
            .map(|info| {
                info.bases
                    .iter()
                    .filter_map(|base| binder.model().resolve_interface_base(base))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut interfaces: Vec<TypeSymbol> = Vec::new();
    for interface in direct_interfaces {
        collect_interface_closure(binder.model(), interface, &mut interfaces);
    }
    let mut interface_tokens: Vec<Token> = Vec::new();
    let saved_scope = tokens.enter_body_scope(&[], &own_parameters);
    for interface in &interfaces {
        mint_named_type_token(interface, image, tokens);
        if let Some(token) = tokens.instruction_type_token(interface) {
            interface_tokens.push(token);
        }
    }
    tokens.restore_body_scope(saved_scope);
    for interface in interface_tokens {
        image.add_interface_impl(type_token, interface);
    }
    for member in &declaration.members {
        if let Member::Field {
            modifiers,
            ty,
            declarators,
            attributes,
            ..
        } = member
        {
            emit_field(image, binder, tokens, &enclosing, modifiers, ty, declarators)?;
            for declarator in declarators {
                if let Some(field_token) = tokens.field(&enclosing, &declarator.name) {
                    emit_attributes(image, binder, tokens, &enclosing, field_token, attributes);
                    if modifiers.contains(&Modifier::Required) {
                        emit_required_member_marker(image, tokens, field_token);
                    }
                }
            }
        }
        if let Member::Property {
            modifiers,
            ty,
            name,
            getter,
            setter,
            explicit_interface,
            ..
        } = member
        {
            if is_auto_property(
                modifiers,
                getter.as_ref(),
                setter.as_ref(),
                declaration.kind == TypeKind::Interface,
            ) {
                let field_ty = binder.canonicalize(&bind_type(ty));
                let signature = field_signature(&member_type_sig(tokens, &enclosing, &field_ty)?);
                let flags = FIELD_PRIVATE
                    | if modifiers.contains(&Modifier::Static) {
                        FIELD_STATIC
                    } else {
                        0
                    }
                    | if setter.is_none()
                        || setter.as_ref().is_some_and(|accessor| accessor.is_init)
                    {
                        FIELD_INITONLY
                    } else {
                        0
                    };
                let field = image.add_field(
                    &auto_property_backing_field_name(explicit_interface.as_ref(), name),
                    &signature,
                    flags,
                );
                emit_compiler_generated_marker(image, tokens, field);
                emit_debugger_browsable_never(image, tokens, field);
            }
        }
        if let Member::EventField {
            modifiers,
            ty,
            declarators,
            ..
        } = member
        {
            if event_has_backing_field(modifiers, declaration.kind == TypeKind::Interface) {
                let signature =
                    field_signature(&type_sig(tokens, &binder.canonicalize(&bind_type(ty)))?);
                let flags = FIELD_PRIVATE
                    | if modifiers.contains(&Modifier::Static) {
                        FIELD_STATIC
                    } else {
                        0
                    };
                for declarator in declarators {
                    image.add_field(&declarator.name, &signature, flags);
                }
            }
        }
    }
    if !is_struct
        && continuation.is_none()
        && !any_part_declares_instance_constructor(declaration, later)
        && !any_part_is_static(declaration, later)
    {
        let base_ctor = if is_system_object {
            None
        } else if let Some(symbol) = base_class.as_ref() {
            Some(base_class_ctor(image, tokens, symbol, &own_parameters))
        } else {
            Some(object_base_ctor(image, tokens))
        };
        let body = Stmt::new(StmtKind::Block(Vec::new()), declaration.span);
        let implicit_accessibility = if merged_modifiers(declaration, later)
            .contains(&Modifier::Abstract)
        {
            [Modifier::Protected]
        } else {
            [Modifier::Public]
        };
        let token = emit_constructor(
            image,
            binder,
            &enclosing,
            tokens,
            declaration,
            &implicit_accessibility,
            &[],
            false,
            None,
            &body,
            base_ctor,
            None,
            debug,
        )?;
        if binder.has_required_members_in_chain(&enclosing) {
            emit_required_members_constructor_guard(image, tokens, token);
        }
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
            &[],
            &[],
            ".cctor",
            &TypeSymbol::Special(SpecialType::Void),
            &[],
            &[],
            &body,
            true,
            false,
            false,
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
                type_parameters,
                constraints,
                parameters,
                is_vararg,
                body: Some(body),
                explicit_interface,
                attributes,
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
                    type_parameters,
                    constraints,
                    parameters,
                    *is_vararg,
                    body,
                    explicit_interface.as_ref(),
                    0,
                    debug,
                )?;
                emit_attributes(image, binder, tokens, &enclosing, token, attributes);
                if entry_point.is_none()
                    && &**name == "Main"
                    && modifiers.contains(&Modifier::Static)
                    && is_entry_point_signature(parameters, return_type)
                {
                    *entry_point = Some(token);
                }
            }
            Member::Method {
                modifiers,
                return_type,
                name,
                type_parameters,
                parameters,
                body: None,
                attributes,
                ..
            } if find_dll_import(name, attributes).is_some() => {
                emit_pinvoke_method(
                    image, binder, tokens, &enclosing, modifiers, name, return_type, parameters,
                    attributes,
                )?;
            }
            Member::Method {
                modifiers,
                return_type,
                name,
                type_parameters,
                parameters,
                body: None,
                attributes,
                ..
            } if modifiers.contains(&Modifier::Abstract) => {
                let token =
                    emit_abstract_method(
                        image, binder, tokens, &enclosing, modifiers, name, return_type, parameters,
                    )?;
                emit_attributes(image, binder, tokens, &enclosing, token, attributes);
            }
            Member::Operator {
                modifiers,
                return_type,
                operator,
                parameters,
                body,
                attributes,
                ..
            } => {
                let token = emit_one_method(
                    image,
                    binder,
                    &enclosing,
                    tokens,
                    modifiers,
                    operator.method_name(parameters.len()),
                    return_type,
                    &[],
                    &[],
                    parameters,
                    false,
                    body,
                    None,
                    SPECIAL_NAME,
                    debug,
                )?;
                emit_attributes(image, binder, tokens, &enclosing, token, attributes);
            }
            Member::ConversionOperator {
                modifiers,
                direction,
                target,
                parameters,
                body,
                attributes,
                ..
            } => {
                let token = emit_one_method(
                    image,
                    binder,
                    &enclosing,
                    tokens,
                    modifiers,
                    direction.method_name(),
                    target,
                    &[],
                    &[],
                    parameters,
                    false,
                    body,
                    None,
                    SPECIAL_NAME,
                    debug,
                )?;
                emit_attributes(image, binder, tokens, &enclosing, token, attributes);
            }
            Member::Constructor {
                modifiers,
                parameters,
                is_vararg,
                initializer,
                body,
                header_span,
                attributes,
                ..
            } if !is_static_constructor(modifiers) => {
                let base_ctor = if is_struct || is_system_object || initializer.is_some() {
                    None
                } else if let Some(symbol) = base_class.as_ref() {
                    Some(base_class_ctor(image, tokens, symbol, &own_parameters))
                } else {
                    Some(object_base_ctor(image, tokens))
                };
                let token = emit_constructor(
                    image,
                    binder,
                    &enclosing,
                    tokens,
                    declaration,
                    modifiers,
                    parameters,
                    *is_vararg,
                    initializer.as_ref(),
                    body,
                    base_ctor,
                    Some(*header_span),
                    debug,
                )?;
                emit_attributes(image, binder, tokens, &enclosing, token, attributes);
                if !sets_required_members(attributes)
                    && binder.has_required_members_in_chain(&enclosing)
                {
                    emit_required_members_constructor_guard(image, tokens, token);
                }
            }
            Member::Destructor {
                body, attributes, ..
            } => {
                let token = emit_destructor(
                    image,
                    binder,
                    &enclosing,
                    tokens,
                    base_class.as_ref(),
                    body,
                    debug,
                )?;
                emit_attributes(image, binder, tokens, &enclosing, token, attributes);
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
            explicit_interface,
            attributes,
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
                getter.as_ref(),
                setter.as_ref(),
                explicit_interface.as_ref(),
                debug,
            )?;
            emit_attributes(image, binder, tokens, &enclosing, property, attributes);
            if modifiers.contains(&Modifier::Required) {
                emit_required_member_marker(image, tokens, property);
            }
            first_property.get_or_insert(property);
        }
        if let Member::Indexer {
            modifiers,
            ty,
            parameters,
            getter,
            setter,
            attributes,
            ..
        } = member
        {
            let property = emit_indexer(
                image,
                binder,
                tokens,
                &enclosing,
                &indexer_name(attributes),
                modifiers,
                ty,
                parameters,
                getter.as_ref(),
                setter.as_ref(),
                debug,
            )?;
            first_property.get_or_insert(property);
        }
    }
    if let Some(first) = first_property {
        image.add_property_map(type_token, first);
    }
    emit_default_member_attribute(image, tokens, type_token, &declaration.members);
    if declares_required_member(declaration) {
        emit_required_member_marker(image, tokens, type_token);
    }
    let mut first_event = None;
    for member in &declaration.members {
        if let Member::EventField {
            modifiers,
            ty,
            declarators,
            attributes,
            ..
        } = member
        {
            let event_ty = binder.canonicalize(&bind_type(ty));
            for declarator in declarators {
                let event = emit_event(
                    image,
                    binder,
                    tokens,
                    &enclosing,
                    &declarator.name,
                    &event_ty,
                    modifiers,
                    debug,
                )?;
                emit_attributes(image, binder, tokens, &enclosing, event, attributes);
                first_event.get_or_insert(event);
            }
        }
        if let Member::Event {
            modifiers,
            ty,
            name,
            adder,
            remover,
            explicit_interface,
            attributes,
            ..
        } = member
        {
            let event_ty = binder.canonicalize(&bind_type(ty));
            let event = emit_custom_event(
                image,
                binder,
                tokens,
                &enclosing,
                name,
                &event_ty,
                adder.as_ref().and_then(|accessor| accessor.body.as_ref()),
                remover.as_ref().and_then(|accessor| accessor.body.as_ref()),
                explicit_interface.as_ref(),
                modifiers,
                debug,
            )?;
            emit_attributes(image, binder, tokens, &enclosing, event, attributes);
            first_event.get_or_insert(event);
        }
    }
    if let Some(first) = first_event {
        image.add_event_map(type_token, first);
    }
    for part in later {
        let part_scope = enter_part(binder, part);
        let emitted = emit_type(
            image,
            binder,
            object,
            tokens,
            entry_point,
            namespace,
            part.declaration,
            part.debug,
            Some(type_token),
            &[],
            partials,
        );
        leave_part(binder, part_scope);
        emitted?;
    }
    Ok(())
}

/// Emits a field-like event (17.7): `add_E`/`remove_E` accessors that combine/remove
/// a handler on the private backing field (`E += value` / `E -= value`, via the existing
/// delegate-combine lowering), plus an Event row linking them through MethodSemantics. When
/// the event implements an interface event (13.4.4), its accessors take the interface-impl
/// slot flags (Virtual | NewSlot | Final | HideBySig, II.23.1.10), filling the interface's
/// accessor slots as an interface-implementing property's accessors do; otherwise they are
/// plain public non-virtual.
#[allow(clippy::too_many_arguments)]
fn emit_event(
    image: &mut ImageBuilder,
    binder: &mut Binder,
    tokens: &mut Tokens,
    enclosing: &TypeSymbol,
    name: &str,
    event_ty: &TypeSymbol,
    modifiers: &[Modifier],
    debug: Option<&DebugContext>,
) -> Result<Token, crate::EmitError> {
    let is_static = modifiers.contains(&Modifier::Static);
    let void = TypeSymbol::Special(SpecialType::Void);
    let interface_impl = binder.member_implements_interface(
        enclosing,
        &accessor_name("add_", name),
        &[event_ty.clone()],
    );
    let flags = if interface_impl && !is_static {
        METHOD_PUBLIC
            | METHOD_VIRTUAL
            | METHOD_NEWSLOT
            | METHOD_FINAL
            | METHOD_HIDEBYSIG
            | SPECIAL_NAME
    } else {
        member_visibility(modifiers)
            | SPECIAL_NAME
            | METHOD_HIDEBYSIG
            | if is_static {
                METHOD_STATIC
            } else {
                slot_flags(modifiers, member_visibility(modifiers))
            }
    };
    let params = [(Box::<str>::from("value"), event_ty.clone())];
    let (add, remove) = if modifiers.contains(&Modifier::Abstract) {
        let signature = method_signature(
            true,
            &[member_type_sig(tokens, enclosing, event_ty)?],
            &TypeSig::Void,
        );
        let value = [ParamRow::from(SETTER_VALUE_PARAMETER)];
        let add = image.add_abstract_method(&accessor_name("add_", name), &signature, flags, &value);
        let remove =
            image.add_abstract_method(&accessor_name("remove_", name), &signature, flags, &value);
        (add, remove)
    } else {
        let add = emit_method_body(
            image,
            binder,
            tokens,
            enclosing,
            &[],
            &[],
            &accessor_name("add_", name),
            &void,
            &params,
            &[],
            &event_accessor_body(name, AssignmentOperator::Add),
            is_static,
            false,
            false,
            flags,
            None,
            debug,
        )?;
        let remove = emit_method_body(
            image,
            binder,
            tokens,
            enclosing,
            &[],
            &[],
            &accessor_name("remove_", name),
            &void,
            &params,
            &[],
            &event_accessor_body(name, AssignmentOperator::Subtract),
            is_static,
            false,
            false,
            flags,
            None,
            debug,
        )?;
        (add, remove)
    };
    let event_type_token = tokens
        .type_token(event_ty)
        .ok_or(crate::EmitError::Unsupported(
            "an event whose delegate type has no metadata token",
        ))?;
    let event = image.add_event(name, event_type_token);
    image.add_method_semantics(SEMANTICS_ADDON, add, event);
    image.add_method_semantics(SEMANTICS_REMOVEON, remove, event);
    Ok(event)
}

/// Emits a custom-accessor event (`event H E { add {...} remove {...} }`, 17.7.1): its
/// user-written add/remove bodies plus an Event row. An explicit-interface event names its
/// accessors `I.add_E`/`I.remove_E`, private hidebysig newslot virtual final with a
/// MethodImpl (like an explicit method); an ordinary one's accessors are public.
///
/// A `static` event's accessors are static too (17.7.1) -- `is_static` carries the declared
/// modifier to both the accessor flags (II.23.1.10 Static) and the body's frame, where it
/// decides whether argument slot 0 is `this` or the implicit `value`. An explicit-interface
/// event cannot be static (13.4.1), so that branch keeps its instance slot flags.
#[allow(clippy::too_many_arguments)]
fn emit_custom_event(
    image: &mut ImageBuilder,
    binder: &mut Binder,
    tokens: &mut Tokens,
    enclosing: &TypeSymbol,
    name: &str,
    event_ty: &TypeSymbol,
    add_body: Option<&lamella_syntax::ast::Stmt>,
    remove_body: Option<&lamella_syntax::ast::Stmt>,
    explicit_interface: Option<&lamella_syntax::ast::TypeRef>,
    modifiers: &[Modifier],
    debug: Option<&DebugContext>,
) -> Result<Token, crate::EmitError> {
    let is_static = modifiers.contains(&Modifier::Static);
    let void = TypeSymbol::Special(SpecialType::Void);
    let is_static = is_static && explicit_interface.is_none();
    let flags = if explicit_interface.is_some() {
        METHOD_PRIVATE
            | METHOD_VIRTUAL
            | METHOD_FINAL
            | METHOD_NEWSLOT
            | METHOD_HIDEBYSIG
            | SPECIAL_NAME
    } else {
        member_visibility(modifiers)
            | SPECIAL_NAME
            | METHOD_HIDEBYSIG
            | if is_static {
                METHOD_STATIC
            } else {
                slot_flags(modifiers, member_visibility(modifiers))
            }
    };
    let params = [(Box::<str>::from("value"), event_ty.clone())];
    let accessor_token = |prefix: &str,
                          body: Option<&lamella_syntax::ast::Stmt>,
                          image: &mut ImageBuilder,
                          binder: &mut Binder,
                          tokens: &mut Tokens|
     -> Result<Option<Token>, crate::EmitError> {
        let Some(body) = body else { return Ok(None) };
        let accessor = accessor_name(prefix, name);
        let method_name = explicit_accessor_name(explicit_interface, &accessor);
        let token = emit_method_body(
            image, binder, tokens, enclosing, &[], &[], &method_name, &void, &params, &[], body, is_static,
            false, false, flags, None, debug,
        )?;
        if let Some(interface) = explicit_interface {
            emit_explicit_interface_impl(
                image,
                binder,
                tokens,
                enclosing,
                interface,
                &accessor,
                &[event_ty.clone()],
                &void,
                token,
            )?;
        }
        Ok(Some(token))
    };
    let add = accessor_token("add_", add_body, image, binder, tokens)?;
    let remove = accessor_token("remove_", remove_body, image, binder, tokens)?;
    let event_type_token = tokens
        .type_token(event_ty)
        .ok_or(crate::EmitError::Unsupported(
            "an event whose delegate type has no metadata token",
        ))?;
    let event_name = match explicit_interface {
        Some(interface) => explicit_interface_member_name(interface, name),
        None => String::from(name),
    };
    let event = image.add_event(&event_name, event_type_token);
    if let Some(add) = add {
        image.add_method_semantics(SEMANTICS_ADDON, add, event);
    }
    if let Some(remove) = remove {
        image.add_method_semantics(SEMANTICS_REMOVEON, remove, event);
    }
    Ok(event)
}

/// The synthesized body of an event accessor: `{ E op= value; }` -- a compound assignment
/// of the implicit `value` parameter onto the backing field, which the binder lowers to
/// `Delegate.Combine`/`Remove` exactly as a source `E += h` inside the type would.
fn event_accessor_body(field: &str, operator: AssignmentOperator) -> Stmt {
    let span = Span::new(0, 0);
    let reference = |text: &str| Expr::name(text, span);
    let assignment = Expr::new(
        ExprKind::Assignment {
            operator,
            target: Box::new(reference(field)),
            value: Box::new(reference("value")),
        },
        span,
    );
    Stmt::new(
        StmtKind::Block(alloc::vec![Stmt::new(
            StmtKind::Expression(assignment),
            span
        )]),
        span,
    )
}

/// Emits an abstract method as a bodyless `MethodDef` (RVA 0) whose flags carry
/// Abstract | Virtual | NewSlot (II.23.1.10), so a `callvirt` through the declaring type
/// dispatches to the overriding method in a derived type.
fn emit_abstract_method(
    image: &mut ImageBuilder,
    binder: &Binder,
    tokens: &mut Tokens,
    enclosing: &TypeSymbol,
    modifiers: &[Modifier],
    name: &str,
    return_type: &TypeRef,
    parameters: &[Parameter],
) -> Result<Token, crate::EmitError> {
    let parameter_sigs: Vec<TypeSig> = parameters
        .iter()
        .map(|parameter| {
            member_type_sig(tokens, &enclosing, &binder.canonicalize(&parameter_symbol(parameter)))
        })
        .collect::<Result<_, _>>()?;
    let return_sig = member_type_sig(tokens, &enclosing, &binder.canonicalize(&bind_type(return_type)))?;
    let return_sig = bodyless_return_sig(return_type, return_sig, image, tokens);
    let signature = method_signature(true, &parameter_sigs, &return_sig);
    let visibility = member_visibility(modifiers);
    let flags = visibility | slot_flags(modifiers, visibility);
    let method = image.add_abstract_method(name, &signature, flags, &parameter_names(parameters));
    if is_readonly_ref(return_type) {
        mark_readonly_return(image, tokens, method);
    }
    emit_declared_parameter_metadata(image, binder, tokens, enclosing, method, parameters);
    Ok(method)
}

/// Emits one method, with its OWN type parameters in scope for the whole emission.
///
/// **THE SAME DEFECT AS [`emit_type`]'s, ONE NUMBERING SPACE OVER, AND THAT FUNCTION'S DOC COMMENT
/// DESCRIBES IT.** The emitter re-binds every body through `Binder::bind_method`; `emit_type` opens
/// the DECLARING TYPE's parameter scope around that, so a `T x;` inside `class Box<T>` resolves.
/// A method's own `T` had no such scope, so `Box<T> x;` inside `static int M<T>()` resolved to the
/// ERROR type HERE and nowhere else -- the diagnostic pass binds the same body inside
/// `enter_type_parameters` and reports nothing, and an emit-time diagnostic reaches no one, so the
/// only evidence was *"the error type has no signature"* three steps downstream.
///
/// **MEASURED AS A POSITION TABLE, WHICH IS WHAT SHOWED IT WAS ONE CELL OF FOUR.** A `Box<T>`
/// PARAMETER of a generic method compiled, a `Box<T>` LOCAL inside a generic TYPE compiled, a
/// `Box<int>` local inside a generic method compiled -- only the method's own parameter in a LOCAL
/// failed. Any single example would have declared the area broken or fine.
///
/// A WRAPPER, so the scope is closed on every exit rather than the last: the body below returns
/// through several `?`s, and a leaked scope would resolve the NEXT method's `T` against this one's.
#[allow(clippy::too_many_arguments)]
/// `extra_flags` carries what the DECLARATION KIND means and the modifiers cannot express:
/// `SPECIAL_NAME` for an operator, nothing for an ordinary method.
///
/// **IT IS A REQUIRED PARAMETER RATHER THAN A DEFAULT, SO A NEW MEMBER KIND HAS TO DECIDE.**
/// `SpecialName` is what MAKES a method an operator (II.10.3), and an emitter that omits the flag
/// while also not requiring it agrees with itself -- the two halves cancel, and only a consumer
/// outside the compilation sees that `==` is an ordinary method rather than an operator.
///
/// Keying on the `op_` name prefix instead would mark a plain method spelled `op_Foo`, which csc
/// leaves unmarked; the declaration kind is the thing that knows, so the declaration kind says so.
fn emit_one_method(
    image: &mut ImageBuilder,
    binder: &mut Binder,
    enclosing: &TypeSymbol,
    tokens: &mut Tokens,
    modifiers: &[Modifier],
    name: &str,
    return_type: &TypeRef,
    type_parameters: &[TypeParameter],
    constraints: &[lamella_syntax::ast::TypeParameterConstraintClause],
    parameters: &[Parameter],
    is_vararg: bool,
    body: &Stmt,
    explicit_interface: Option<&TypeRef>,
    extra_flags: u16,
    debug: Option<&DebugContext>,
) -> Result<Token, crate::EmitError> {
    let entered = binder.enter_type_parameters(type_parameters, constraints);
    let emitted = emit_one_method_in_scope(
        image,
        binder,
        enclosing,
        tokens,
        modifiers,
        name,
        return_type,
        type_parameters,
        constraints,
        parameters,
        is_vararg,
        body,
        explicit_interface,
        extra_flags,
        debug,
    );
    binder.exit_type_parameters(entered);
    emitted
}

/// [`emit_one_method`] with the method's own type parameters already in scope. Never called
/// directly -- the wrapper is what guarantees they are withdrawn again.
#[allow(clippy::too_many_arguments)]
fn emit_one_method_in_scope(
    image: &mut ImageBuilder,
    binder: &mut Binder,
    enclosing: &TypeSymbol,
    tokens: &mut Tokens,
    modifiers: &[Modifier],
    name: &str,
    return_type: &TypeRef,
    type_parameters: &[TypeParameter],
    constraints: &[lamella_syntax::ast::TypeParameterConstraintClause],
    parameters: &[Parameter],
    is_vararg: bool,
    body: &Stmt,
    explicit_interface: Option<&TypeRef>,
    extra_flags: u16,
    debug: Option<&DebugContext>,
) -> Result<Token, crate::EmitError> {
    let method_type_parameters: Vec<Box<str>> =
        type_parameters.iter().map(|p| p.name.clone()).collect();
    let method_constraints = constraints;
    let is_async = modifiers.contains(&Modifier::Async);
    let return_symbol = binder.canonicalize(&bind_type(return_type));
    let params: Vec<(Box<str>, TypeSymbol)> = parameters
        .iter()
        .map(|parameter| (parameter.name.clone(), binder.canonicalize(&bind_type(&parameter.ty))))
        .collect();
    let byref_flags = byref_flags(parameters);
    if let Some(interface) = explicit_interface {
        let method_name = explicit_interface_member_name(interface, name);
        let flags =
            METHOD_PRIVATE | METHOD_VIRTUAL | METHOD_FINAL | METHOD_NEWSLOT | METHOD_HIDEBYSIG;
        tokens.next_return_is_readonly_ref = is_readonly_ref(return_type);
        let body_token = emit_method_body(
            image,
            binder,
            tokens,
            enclosing,
            &[],
            &[],
            &method_name,
            &return_symbol,
            &params,
            &byref_flags,
            body,
            false,
            is_async,
            is_vararg,
            flags,
            None,
            debug,
        )?;
        let signature_params: Vec<TypeSymbol> = parameters
            .iter()
            .map(|parameter| binder.canonicalize(&parameter_symbol(parameter)))
            .collect();
        emit_explicit_interface_impl(
            image,
            binder,
            tokens,
            enclosing,
            interface,
            name,
            &signature_params,
            &return_symbol,
            body_token,
        )?;
        emit_declared_parameter_metadata(image, binder, tokens, enclosing, body_token, parameters);
        return Ok(body_token);
    }
    let is_static = modifiers.contains(&Modifier::Static);
    let mut flags = member_visibility(modifiers) | extra_flags;
    if is_static {
        flags |= METHOD_STATIC;
    }
    let slots = slot_flags(modifiers, member_visibility(modifiers));
    if slots != 0 {
        flags |= slots;
    } else if !is_static
        && binder.member_implements_interface(
            enclosing,
            name,
            &params.iter().map(|(_, ty)| ty.clone()).collect::<Vec<_>>(),
        )
    {
        flags |= METHOD_VIRTUAL | METHOD_NEWSLOT | METHOD_FINAL | METHOD_HIDEBYSIG;
    }
    tokens.next_return_is_readonly_ref = is_readonly_ref(return_type);
    let method = emit_method_body(
        image,
        binder,
        tokens,
        enclosing,
        &method_type_parameters,
        method_constraints,
        name,
        &return_symbol,
        &params,
        &byref_flags,
        body,
        is_static,
        is_async,
        is_vararg,
        flags,
        None,
        debug,
    )?;
    emit_declared_parameter_metadata(image, binder, tokens, enclosing, method, parameters);
    Ok(method)
}

/// Emits the `MethodImpl` row that wires an explicit interface implementation: it
/// links `body` (the class's own private `MethodDef`) to the interface method it
/// overrides. The interface method is a this-module `MethodDef` when the interface is
/// declared here, otherwise a minted `MemberRef` to the BCL interface method.
///
/// **THE QUALIFIER IS RESOLVED BEFORE IT BECOMES A `TypeRef`, WHICH IS THE WHOLE OF ITS
/// CORRECTNESS.** A written name is not a type: `IEnumerator IEnumerable.GetEnumerator()` names
/// `System.Collections.IEnumerable` through a `using`, and taking the spelling as written mints a
/// `TypeRef` with an EMPTY namespace scoped to mscorlib. That image is well-formed, links, and
/// throws `TypeLoadException: Could not load type 'IEnumerable'` on the first use of the type --
/// while the same program written fully qualified runs. `mint_signature_type` states the same rule
/// for signatures and resolves through the binder; this is that rule's other home.
#[allow(clippy::too_many_arguments)]
fn emit_explicit_interface_impl(
    image: &mut ImageBuilder,
    binder: &Binder,
    tokens: &mut Tokens,
    enclosing: &TypeSymbol,
    interface: &TypeRef,
    member: &str,
    parameter_types: &[TypeSymbol],
    return_symbol: &TypeSymbol,
    body: Token,
) -> Result<(), crate::EmitError> {
    let class = tokens
        .type_token(enclosing)
        .ok_or(crate::EmitError::Unsupported(
            "an explicit interface impl on a type with no metadata token",
        ))?;
    let interface_symbol = binder.canonicalize(&binder.resolve_type(&bind_type(interface)));
    if let TypeSymbol::Instantiation {
        definition,
        arguments,
    } = &interface_symbol
    {
        let declaration = constructed_interface_member_ref(
            image,
            binder,
            tokens,
            enclosing,
            &interface_symbol,
            definition,
            arguments,
            member,
            parameter_types,
            return_symbol,
        )?;
        image.add_method_impl(class, body, declaration);
        return Ok(());
    }
    let declaration = match tokens.method(&interface_symbol, member, parameter_types) {
        Some(token) => token,
        None => {
            let (namespace, name) =
                split_type_name(&interface_symbol).ok_or(crate::EmitError::Unsupported(
                    "an explicit interface impl of an unresolvable interface",
                ))?;
            let parameter_sigs: Vec<TypeSig> = parameter_types
                .iter()
                .map(|ty| member_type_sig(tokens, enclosing, ty))
                .collect::<Result<_, _>>()?;
            let signature =
                method_signature(true, &parameter_sigs, &member_type_sig(tokens, enclosing, return_symbol)?);
            let type_ref = image.type_ref(&namespace, &name);
            let member_token = image.member_ref(type_ref, member, &signature);
            tokens.insert_method(&interface_symbol, member, parameter_types, member_token);
            member_token
        }
    };
    image.add_method_impl(class, body, declaration);
    Ok(())
}

/// The `MemberRef` a `MethodImpl` names when the interface it implements is CONSTRUCTED
/// (`IEnumerable<T>`, `IBox<int>`): a `TypeSpec` for the instantiation, and the member's signature
/// written in terms of the INTERFACE DEFINITION'S OWN parameters (`!0`), never the implementing
/// class's.
///
/// **THE SIGNATURE COMES FROM THE INTERFACE, NOT FROM THE DECLARATION THAT IMPLEMENTS IT.** The two
/// coincide for `Bag<T> : IEnumerable<T>`, where the class's `T` and the interface's `!0` are the
/// same position -- and they do NOT for `Two : IBox<int>`, whose `int IBox<int>.Get()` is written
/// with the argument substituted while the referenced member is still `!0 Get()`. Taking the
/// declaration's types would name a member the interface does not declare, and II.22.25 wants the
/// one it does.
#[allow(clippy::too_many_arguments)]
fn constructed_interface_member_ref(
    image: &mut ImageBuilder,
    binder: &Binder,
    tokens: &mut Tokens,
    enclosing: &TypeSymbol,
    interface: &TypeSymbol,
    definition: &[Box<str>],
    arguments: &[TypeSymbol],
    member: &str,
    parameter_types: &[TypeSymbol],
    return_symbol: &TypeSymbol,
) -> Result<Token, crate::EmitError> {
    let declaring_parameters = tokens.type_parameters(enclosing).to_vec();
    let saved_scope = tokens.enter_body_scope(&[], &declaring_parameters);
    let minted = mint_type_spec(interface, definition, arguments, image, tokens);
    tokens.restore_body_scope(saved_scope);
    let parent = minted.ok_or(crate::EmitError::Unsupported(
        "a constructed interface with no TypeSpec",
    ))?;
    let definition_symbol = definition_symbol(definition, arguments.len());
    let wanted_parameters: Vec<TypeSymbol> =
        parameter_types.iter().map(|ty| binder.canonicalize(ty)).collect();
    let wanted_return = binder.canonicalize(return_symbol);
    let index = {
        let substituted = binder.model().get_by_symbol(interface).ok_or(
            crate::EmitError::Unsupported("an explicit interface impl of an unresolvable interface"),
        )?;
        substituted
            .methods
            .iter()
            .position(|candidate| {
                &*candidate.name == member
                    && candidate.parameters.len() == wanted_parameters.len()
                    && candidate
                        .parameters
                        .iter()
                        .zip(&wanted_parameters)
                        .all(|(a, b)| binder.canonicalize(a) == *b)
                    && binder.canonicalize(&candidate.return_type) == wanted_return
            })
            .ok_or(crate::EmitError::Unsupported(
                "an explicit interface impl of a member the interface does not declare",
            ))?
    };
    let (type_parameters, declared) = {
        let info = binder.model().get_by_symbol(&definition_symbol).ok_or(
            crate::EmitError::Unsupported("an explicit interface impl of an unresolvable interface"),
        )?;
        let declared = info.methods.get(index).cloned().ok_or(
            crate::EmitError::Unsupported("an explicit interface impl of a member the interface does not declare"),
        )?;
        (info.type_parameters.clone(), declared)
    };
    for ty in declared
        .parameters
        .iter()
        .chain(core::iter::once(&declared.return_type))
    {
        if !mentions_type_parameter(ty, &type_parameters) {
            mint_named_type_token(ty, image, tokens);
        }
    }
    let scope = GenericScope {
        method: &[],
        declaring: &type_parameters,
    };
    let parameter_sigs: Vec<TypeSig> = declared
        .parameters
        .iter()
        .map(|ty| open_type_sig(tokens, ty, scope))
        .collect::<Result<_, _>>()?;
    let return_sig = open_type_sig(tokens, &declared.return_type, scope)?;
    let signature = method_signature(true, &parameter_sigs, &return_sig);
    Ok(image.member_ref(parent, member, &signature))
}

/// The declared name of each parameter, in order -- the `Param` rows (II.22.33) that make up a
/// method's `ParamList` run, so a debugger shows `count` rather than `arg1` and a parameter-targeted
/// attribute has a row to hang off.
fn parameter_names(parameters: &[Parameter]) -> Vec<ParamRow> {
    parameters
        .iter()
        .map(|parameter| ParamRow {
            name: parameter.name.clone(),
            flags: param_flags(parameter.modifier),
        })
        .collect()
}

/// An `i64` as an unsuffixed integer [`Literal`], the shape every integral constant path here
/// wants.
fn integer_constant(value: i64) -> Literal {
    Literal::Integer {
        value: value as u64,
        suffix: lamella_syntax::token::IntegerSuffix::None,
    }
}

/// The `ParameterAttributes` a declared modifier means (II.23.1.13).
///
/// **`out` AND `ref` ARE THE SAME SIGNATURE AND DIFFERENT METADATA.** Both encode a byref in the
/// method signature, so the only thing that tells a consumer, a debugger or reflection which one
/// the author wrote is this bit -- and it was never emitted, so every `out` parameter in every
/// assembly this compiler has built has been published as a `ref`.
fn param_flags(modifier: Option<ParameterModifier>) -> u16 {
    match modifier {
        Some(ParameterModifier::Out) => PARAM_OUT,
        _ => 0,
    }
}

/// Just the names out of a `Param` row list, for [`emit_body`] -- which builds the frame and has
/// no use for the metadata flags the rows also carry.
fn param_row_names(rows: &[ParamRow]) -> Vec<Box<str>> {
    rows.iter().map(|row| row.name.clone()).collect()
}

/// [`bound_parameter_names`] for a list that CANNOT contain a `ref` or `out` parameter: a property
/// or event accessor's `value`, and an indexer's indices, which C# forbids from being byref at all
/// (CS0631). Naming the reason once beats an unexplained empty slice at four call sites.
fn by_value_parameter_names(parameters: &[(Box<str>, TypeSymbol)]) -> Vec<ParamRow> {
    bound_parameter_names(parameters, &[])
}

/// The names out of a BOUND parameter list -- the `(name, type)` pairs the accessor and body
/// paths build, as against [`parameter_names`]'s syntactic ones.
fn bound_parameter_names(
    parameters: &[(Box<str>, TypeSymbol)],
    passing: &[ParamPassing],
) -> Vec<ParamRow> {
    parameters
        .iter()
        .enumerate()
        .map(|(index, (name, _))| ParamRow {
            name: name.clone(),
            flags: match passing.get(index) {
                Some(ParamPassing::Out) => PARAM_OUT,
                _ => 0,
            },
        })
        .collect()
}

/// Whether `parameters` ends in a `params` array (17.5.1.4). Only the LAST parameter can be one,
/// which is why the marker below has a single row to find -- the last DECLARED parameter, which is
/// not necessarily the last row of the method's run.
fn ends_in_params_array(parameters: &[Parameter]) -> bool {
    parameters
        .last()
        .is_some_and(|parameter| parameter.modifier == Some(ParameterModifier::Params))
}

/// Emits the metadata a method's DECLARED parameters carry beyond their types: the
/// `[ParamArrayAttribute]` marker on a `params` array, and the `Optional`/`HasDefault` bits plus
/// `Constant` row of every optional parameter.
///
/// **ONE FUNCTION AT ONE SET OF SITES, because these are the same question asked twice.** The
/// obvious home for the default looked like `parameter_names`, which builds the `Param` rows -- but
/// it builds them for the ABSTRACT, interface, delegate and P/Invoke paths only; an ordinary
/// method's rows come from `bound_parameter_names` off the BOUND parameter list, which has no
/// syntax to read a default from. Putting the case in one of two builders is how a rule with
/// several implementations gains it in none of them, and the tell here was a 34-row control table
/// in which every form compiled and every flag read `0x0000`.
///
/// This function already ran at every site that emits a method from declared parameters, already
/// had the method token, and already reached back into the finished `Param` run. It is the seam.
///
/// ---
///
/// The `params` half: `[System.ParamArrayAttribute]` on the last declared parameter when
/// `parameters` ends in a `params` array (17.5.1.4) -- the empty-argument marker blob, matching
/// what csc writes byte for byte.
///
/// **THIS IS THE ONLY RECORD THAT SURVIVES THE ASSEMBLY BOUNDARY.** `params` is a call-site rule,
/// and a call site in another assembly has nothing but metadata to read: without this row the
/// method's signature says `int[]` and every expanded call against it -- `Add(1, 2, 3)` -- draws
/// CS1501 instead of binding. A single-file program never notices, because there the binder still
/// has the source. Reading the attribute back has always worked -- it is how a BCL `params` method
/// binds -- so the gap was one-sided: a library this compiler produced was one it could not then
/// call in expanded form.
///
/// Silently skipped if `System.ParamArrayAttribute` does not resolve -- a corlib being compiled
/// before that type exists, which is the same lenient posture the other synthesized markers take.
fn emit_declared_parameter_metadata(
    image: &mut ImageBuilder,
    binder: &Binder,
    tokens: &mut Tokens,
    enclosing: &TypeSymbol,
    method: Token,
    parameters: &[Parameter],
) {
    let rows = image.method_parameters(method);
    for (index, parameter) in parameters.iter().enumerate() {
        let Some(expr) = &parameter.default_value else {
            continue;
        };
        let Some(&param) = rows.get(index) else {
            continue;
        };
        let mut flags = image.param_flags(param) | PARAM_OPTIONAL;
        let ty = binder.canonicalize(&parameter_symbol(parameter));
        if let Some(literal) = lamella_binder::parameter_default_in_model(binder.model(), enclosing, expr) {
            if let Some((element, value)) = constant_row(binder.model(), &ty, &literal) {
                flags |= PARAM_HAS_DEFAULT;
                image.add_constant(param, element, &value);
            } else if matches!(ty, TypeSymbol::Special(SpecialType::Decimal)) {
                emit_decimal_parameter_attribute(image, tokens, &literal, param);
            }
        }
        image.set_param_flags(param, flags);
    }
    if !ends_in_params_array(parameters) {
        return;
    }
    let Some(&param) = rows.get(parameters.len() - 1) else {
        return;
    };
    let Some(constructor) =
        synthesized_attribute_ctor(image, tokens, "System", "ParamArrayAttribute", &[])
    else {
        return;
    };
    image.add_custom_attribute(param, constructor, &[0x01, 0x00, 0x00, 0x00]);
}

/// The `ref`/`out` (byref) flag of each parameter, in order -- parallel to the bound
/// parameter list, driving the byref signature and the deref of body reads/writes.
/// How one argument travels: by value, or as a byref the callee reads (`ref`) or writes (`out`).
///
/// **BODY LOWERING ASKS ONE QUESTION AND METADATA ASKS ANOTHER, WHICH IS WHY THIS IS NOT A BOOL.**
/// A `ref` and an `out` parameter are the same thing to the emitter -- an address to deref through
/// -- and different things to a consumer, and collapsing them to "is it byref" is what left the
/// `Out` flag with nowhere to come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParamPassing {
    ByValue,
    Ref,
    Out,
}

impl ParamPassing {
    /// Whether the argument slot holds an ADDRESS -- true for both `ref` and `out`, which is the
    /// only thing the body lowering needs to know.
    fn is_byref(self) -> bool {
        !matches!(self, ParamPassing::ByValue)
    }
}

fn byref_flags(parameters: &[Parameter]) -> Vec<ParamPassing> {
    parameters
        .iter()
        .map(|parameter| match parameter.modifier {
            Some(ParameterModifier::Ref) => ParamPassing::Ref,
            Some(ParameterModifier::Out) => ParamPassing::Out,
            _ => ParamPassing::ByValue,
        })
        .collect()
}

/// The data a `[DllImport]` attribute carries: the unmanaged library, the native entry-point name
/// (defaulting to the method's own name), and the `ImplMap` MappingFlags (II.23.1.8).
struct DllImport {
    library: String,
    entry_point: String,
    mapping_flags: u16,
}

/// Reads a `[DllImport("lib", EntryPoint = "...")]` among a method's attributes. CharSet /
/// CallingConvention / SetLastError are not read yet -- the default MappingFlags (Winapi calling
/// convention; the runtime marshals strings as the platform default) suffice for a first P/Invoke.
fn find_dll_import(method_name: &str, attributes: &[AttributeSection]) -> Option<DllImport> {
    for section in attributes {
        for attribute in &section.attributes {
            let last = attribute.name.parts.last()?;
            if &**last != "DllImport" && &**last != "DllImportAttribute" {
                continue;
            }
            let library = attribute.arguments.iter().find_map(|argument| match argument {
                AttributeArgument::Positional(expr) => string_literal_value(expr),
                AttributeArgument::Named { .. } => None,
            })?;
            let entry_point = attribute
                .arguments
                .iter()
                .find_map(|argument| match argument {
                    AttributeArgument::Named { name, value } if &**name == "EntryPoint" => {
                        string_literal_value(value)
                    }
                    _ => None,
                })
                .unwrap_or_else(|| String::from(method_name));
            return Some(DllImport {
                library,
                entry_point,
                mapping_flags: 0x0102,
            });
        }
    }
    None
}

/// The text of a string-literal expression (a `[DllImport]` library / entry point), else `None`.
fn string_literal_value(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Literal(Literal::String(units)) => Some(String::from_utf16_lossy(units)),
        _ => None,
    }
}

/// Emits a `[DllImport]` extern method as a P/Invoke (II.15.5): a body-less `MethodDef` carrying
/// `PinvokeImpl`, a `ModuleRef` for the library, and an `ImplMap` naming the entry point. Gated on
/// the `native_interop` knob -- off (pure-managed / NETMF) rejects it rather than emit an unmanaged
/// boundary the target cannot honor.
fn emit_pinvoke_method(
    image: &mut ImageBuilder,
    binder: &Binder,
    tokens: &mut Tokens,
    enclosing: &TypeSymbol,
    modifiers: &[Modifier],
    name: &str,
    return_type: &TypeRef,
    parameters: &[Parameter],
    attributes: &[AttributeSection],
) -> Result<(), crate::EmitError> {
    let Some(dll_import) = find_dll_import(name, attributes) else {
        return Err(crate::EmitError::Unsupported(
            "an extern method without a [DllImport] is not lowered",
        ));
    };
    if !tokens.native_interop() {
        return Err(crate::EmitError::Unsupported(
            "[DllImport] (P/Invoke) needs native interop, which is off -- pass lcsc /native-interop",
        ));
    }
    let parameter_sigs: Vec<TypeSig> = parameters
        .iter()
        .map(|parameter| {
            let base = type_sig(tokens, &binder.canonicalize(&bind_type(&parameter.ty)))?;
            Ok(
                if matches!(
                    parameter.modifier,
                    Some(ParameterModifier::Ref | ParameterModifier::Out)
                ) {
                    TypeSig::ByRef(Box::new(base))
                } else {
                    base
                },
            )
        })
        .collect::<Result<Vec<_>, crate::EmitError>>()?;
    let return_sig = type_sig(tokens, &binder.canonicalize(&bind_type(return_type)))?;
    let signature = method_signature(false, &parameter_sigs, &return_sig);
    let flags =
        member_visibility(modifiers) | METHOD_STATIC | METHOD_HIDEBYSIG | METHOD_PINVOKE_IMPL;
    let method = image.add_pinvoke_method(name, &signature, flags, &parameter_names(parameters));
    emit_declared_parameter_metadata(image, binder, tokens, enclosing, method, parameters);
    let module_ref = image.add_module_ref(&dll_import.library);
    image.add_impl_map(method, dll_import.mapping_flags, &dll_import.entry_point, module_ref);
    Ok(())
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
    modifiers: &[Modifier],
    parameters: &[Parameter],
    is_vararg: bool,
    initializer: Option<&ConstructorInitializer>,
    body: &lamella_syntax::ast::Stmt,
    base_ctor: Option<Token>,
    header_span: Option<Span>,
    debug: Option<&DebugContext>,
) -> Result<Token, crate::EmitError> {
    let params: Vec<(Box<str>, TypeSymbol)> = parameters
        .iter()
        .map(|parameter| (parameter.name.clone(), binder.canonicalize(&bind_type(&parameter.ty))))
        .collect();
    let implicit_base = implicit_base_chain(image, binder, tokens, enclosing);
    let base_prologue = || match &implicit_base {
        Some((ctor, arguments)) => Some(ConstructorPrologue {
            ctor: *ctor,
            span: header_span,
            arguments: arguments.clone(),
            leading_body: 0,
            zero_initialize: None,
        }),
        None => base_ctor.map(|ctor| ConstructorPrologue {
            ctor,
            span: header_span,
            arguments: Vec::new(),
            leading_body: 0,
            zero_initialize: None,
        }),
    };
    let zero_initialize = match initializer {
        Some(init)
            if init.kind == ConstructorInitializerKind::This
                && init.arguments.is_empty()
                && tokens.is_struct(enclosing) =>
        {
            self_type_token(image, tokens, enclosing)
        }
        _ => None,
    };
    if let Some(ty) = zero_initialize {
        let prologue = ConstructorPrologue {
            ctor: image.object_ctor(),
            span: initializer.map(|init| init.span),
            arguments: Vec::new(),
            leading_body: 0,
            zero_initialize: Some(ty),
        };
        let ctor = emit_method_body(
            image,
            binder,
            tokens,
            enclosing,
            &[],
            &[],
            ".ctor",
            &TypeSymbol::Special(SpecialType::Void),
            &params,
            &byref_flags(parameters),
            body,
            false,
            false,
            is_vararg,
            ctor_flags(modifiers),
            Some(&prologue),
            debug,
        )?;
        emit_declared_parameter_metadata(image, binder, tokens, enclosing, ctor, parameters);
        return Ok(ctor);
    }
    let mut prologue = match initializer {
        Some(init) => Some(
            binder
                .bind_constructor_chain(enclosing, &params, init)
                .map(|(method, arguments)| {
                    let chain_key = if method.is_vararg {
                        crate::expr::vararg_lookup_params(&method.parameters, &[])
                    } else {
                        method.parameters.clone()
                    };
                    let ctor = tokens
                        .method(&method.declaring_type, ".ctor", &chain_key)
                        .unwrap_or_else(|| {
                            mint_member_ref(&method, image, tokens);
                            tokens
                                .method(&method.declaring_type, ".ctor", &chain_key)
                                .unwrap_or_else(|| image.object_ctor())
                        });
                    ConstructorPrologue {
                        ctor,
                        span: Some(init.span),
                        arguments,
                        leading_body: 0,
                        zero_initialize: None,
                    }
                })
                .ok_or(crate::EmitError::Unsupported(
                    "a constructor initializer chain that did not resolve",
                ))?,
        ),
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
    if let Some(prologue) = prologue.as_mut() {
        if !chains_to_this {
            prologue.leading_body = field_initializer_statements(declaration).len();
        }
    }
    let ctor = emit_method_body(
        image,
        binder,
        tokens,
        enclosing,
        &[],
        &[],
        ".ctor",
        &TypeSymbol::Special(SpecialType::Void),
        &params,
        &byref_flags(parameters),
        &body,
        false,
        false,
        is_vararg,
        ctor_flags(modifiers),
        prologue.as_ref(),
        debug,
    )?;
    emit_declared_parameter_metadata(image, binder, tokens, enclosing, ctor, parameters);
    Ok(ctor)
}

/// Emits a destructor as the parameterless `Finalize` override -- a `family virtual`
/// method reusing System.Object::Finalize's slot, so a dropped object's body runs at
/// finalization (17.12). The body is wrapped in `try { <body> } finally { base.Finalize(); }`
/// so the base finalizer runs afterwards, the chain ending at System.Object::Finalize.
/// Returns the `Finalize` MethodDef token, which the destructor's own attributes attach to.
fn emit_destructor(
    image: &mut ImageBuilder,
    binder: &mut Binder,
    enclosing: &TypeSymbol,
    tokens: &mut Tokens,
    base_class: Option<&TypeSymbol>,
    body: &lamella_syntax::ast::Stmt,
    debug: Option<&DebugContext>,
) -> Result<Token, crate::EmitError> {
    let void = TypeSymbol::Special(SpecialType::Void);
    let bound =
        binder.bind_method(Some(enclosing.clone()), "Finalize", void.clone(), &[], &[], false, false, body);
    let bound = wrap_finalizer(bound, &base_finalizer_reference(base_class, tokens));
    let finalize = emit_bound_body(
        image,
        tokens,
        enclosing,
        &[],
        &[],
        "Finalize",
        &void,
        &[],
        &[],
        &bound,
        false,
        false,
        FINALIZE_FLAGS,
        None,
        debug,
    )?;
    let class = tokens
        .type_token(enclosing)
        .ok_or(crate::EmitError::Unsupported(
            "a destructor on a type with no metadata token",
        ))?;
    let object = TypeSymbol::Special(SpecialType::Object);
    let declaration = match tokens.method(&object, "Finalize", &[]) {
        Some(token) => token,
        None => {
            let signature = method_signature(true, &[], &member_type_sig(tokens, enclosing, &void)?);
            let type_ref = image.type_ref("System", "Object");
            let member_token = image.member_ref(type_ref, "Finalize", &signature);
            tokens.insert_method(&object, "Finalize", &[], member_token);
            member_token
        }
    };
    image.add_method_impl(class, finalize, declaration);
    Ok(finalize)
}

/// The base type's `Finalize` a destructor chains to (17.12): the direct base's own
/// `Finalize` when it declares a destructor (a this-module method), otherwise
/// `System.Object::Finalize`, the finalizer every reference type ultimately inherits.
fn base_finalizer_reference(
    base_class: Option<&TypeSymbol>,
    tokens: &Tokens,
) -> lamella_binder::MethodReference {
    let declaring_type = match base_class {
        Some(symbol) if tokens.method(symbol, "Finalize", &[]).is_some() => symbol.clone(),
        _ => TypeSymbol::Special(SpecialType::Object),
    };
    lamella_binder::MethodReference {
        declaring_type,
        name: "Finalize".into(),
        parameters: Vec::new(),
        return_type: TypeSymbol::Special(SpecialType::Void),
        is_static: false,
        is_vararg: false,
        instantiation: None,
        declaring_instantiation: None,
    }
}

/// Wraps a bound destructor body in `try { <body> } finally { base.Finalize(); }`, so the
/// base finalizer runs after it (17.12). The synthesized `base.Finalize()` is a non-virtual
/// `call` on `this` (a `Base` receiver), matching a `base.member` invocation.
fn wrap_finalizer(body: BoundStmt, base: &lamella_binder::MethodReference) -> BoundStmt {
    let span = body.span;
    let receiver = BoundExpr {
        kind: BoundExprKind::Base,
        ty: base.declaring_type.clone(),
    };
    let callee = BoundExpr {
        kind: BoundExprKind::MethodGroup {
            receiver: Box::new(receiver),
            name: "Finalize".into(),
        },
        ty: TypeSymbol::Error,
    };
    let call = BoundExpr {
        kind: BoundExprKind::Call {
            callee: Box::new(callee),
            arguments: Vec::new(),
            method: Some(base.clone()),
        },
        ty: TypeSymbol::Special(SpecialType::Void),
    };
    let finally = BoundStmt {
        kind: BoundStmtKind::Expression(call),
        span,
    };
    BoundStmt {
        kind: BoundStmtKind::Try {
            body: Box::new(body),
            catches: Vec::new(),
            finally: Some(Box::new(finally)),
        },
        span,
    }
}

/// Binds a method body, lowers it to CIL, and adds the `MethodDef`, returning its
/// token. Shared by ordinary methods, constructors' callers, and property
/// accessors -- the parameters and return are already bound symbols.
#[allow(clippy::too_many_arguments)]
/// Wraps `return_sig` in `modreq(System.Runtime.CompilerServices.IsExternalInit)` when the caller
/// marked the member being emitted as an `init` accessor, and CONSUMES the mark.
///
/// **THE ONE PLACE THE MARK IS READ**, so an `init` accessor cannot be emitted down one path with
/// the modifier and down another without it -- both signature paths in `emit_method_body` (the
/// async stub and the ordinary one) call this, and each takes the mark or leaves it clear.
///
/// A modifier this build cannot MINT is dropped rather than faked: the type comes from the corlib
/// in scope, and a compilation with no such type has nothing to name. That is the only silent
/// path, and it is the same one every synthesized attribute takes.
fn apply_init_only_modifier(
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
    return_sig: TypeSig,
) -> Result<TypeSig, crate::EmitError> {
    if !core::mem::take(&mut tokens.next_return_is_external_init) {
        return Ok(return_sig);
    }
    let marker = named_symbol("System.Runtime.CompilerServices", "IsExternalInit");
    mint_named_type_token(&marker, image, tokens);
    let Some(modifier) = tokens.type_token(&marker) else {
        return Ok(return_sig);
    };
    Ok(TypeSig::Modified {
        modifier,
        inner: Box::new(return_sig),
    })
}

/// Wraps a `ref readonly T` return in its `modreq`, when the member being emitted was written that
/// way. The sibling of [`apply_init_only_modifier`], and deliberately the same shape.
///
/// The modifier NAMES a type, so the type has to exist: a reference set that does not declare
/// `InAttribute` cannot express the feature at all, and the modifier is then left off rather
/// than a fabricated reference being minted.
/// Every required modifier a member's RETURN can carry, applied in one place.
///
/// **`emit_method_body` BUILDS A SIGNATURE AT TWO POINTS** -- the deferred-body path an async method
/// takes, and the ordinary `add_method` one -- and a modifier applied at one of them is absent from
/// the other with nothing to say so. The `init` modifier was at both and the `ref readonly` one was
/// at the first alone, which put `ref readonly int M()` and `ref int M()` in the metadata as the
/// same method: csc reading it accepted a write through the returned reference at all five
/// positions. **The repair is this function rather than a second careful call**, so a third
/// modifier is added once instead of being added at one site and forgotten at the other.
/// Marks a member's RETURN as read-only the way csc does: a sequence-0 `Param` row (II.22.33)
/// carrying `[System.Runtime.CompilerServices.IsReadOnlyAttribute]`.
///
/// **THE `modreq` ALONE IS NOT ENOUGH, AND THAT IS THE WHOLE REASON THIS EXISTS.** Emitting
/// `T& modreq(InAttribute)` and nothing else produces a member csc cannot use AT ALL -- not even to
/// READ it: `CS0570: 'Bag.RoAt(int)' is not supported by the language`. A bare `modreq(InAttribute)`
/// on a RETURN is not a C# concept; the attribute on the return parameter is what says the member
/// is `ref readonly` rather than something the language cannot express. Measured by diffing csc's
/// own metadata against ours, where the signature blobs were byte-equivalent and csc's had a
/// sequence-0 `Param` row that ours did not.
///
/// **CALL IT WHILE THIS METHOD IS THE LAST ONE ADDED.** `add_return_param` appends to the `Param`
/// table, and a `MethodDef`'s parameter run is read as "from my `ParamList` to the next method's" --
/// so a row added after another method exists lands in that method's run instead.
/// The return `TypeSig` of a BODYLESS member -- abstract, or on an interface -- with the
/// `ref readonly` modifier applied when the declaration carries one.
///
/// **THE BODIED PATH CANNOT SERVE THESE.** `emit_method_body` builds its signature from a mark this
/// crate sets just before calling it; a member with no body never goes through it, so an abstract
/// or interface `ref readonly T M()` would emit a plain `T&` and every consumer would assign
/// through it. Measured: csc refuses the write at both positions against its own metadata.
fn bodyless_return_sig(
    return_type: &TypeRef,
    sig: TypeSig,
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
) -> TypeSig {
    if is_readonly_ref(return_type) {
        readonly_ref_sig(image, tokens, sig)
    } else {
        sig
    }
}

fn mark_readonly_return(image: &mut ImageBuilder, tokens: &mut Tokens, method: Token) {
    let _ = method;
    let return_param = image.add_return_param();
    attach_is_readonly(tokens, return_param);
}

/// Attaches `[System.Runtime.CompilerServices.IsReadOnlyAttribute]` to `parent`.
///
/// **TWO PARENTS, AND BOTH ARE REQUIRED FOR THE MEMBER KINDS THAT HAVE THEM.** csc puts it on the
/// sequence-0 `Param` of every `ref readonly` member AND on the `Property` row of a `ref readonly`
/// property or indexer -- measured on csc's own output. Marking only the accessor leaves a property
/// csc reads as an ordinary `ref`, and it will let a caller assign through it.
fn attach_is_readonly(tokens: &mut Tokens, parent: Token) {
    tokens.pending_is_readonly.push(parent);
}

/// The namespace and name of the `ref readonly` marker attribute, in one place because the
/// synthesis, the reference lookup and the `TypeRef` all have to spell the same type.
const IS_READONLY_ATTRIBUTE: (&str, &str) = ("System.Runtime.CompilerServices", "IsReadOnlyAttribute");

/// Writes `[IsReadOnlyAttribute]` onto every position [`attach_is_readonly`] recorded, and -- when
/// no reference supplies the attribute -- SYNTHESIZES the type into this assembly first.
///
/// **AN ASSEMBLY THAT NAMES A TYPE NOTHING DECLARES IS NOT MERELY MISSING A MARKER: A CORLIB LIKE
/// THAT DOES NOT LOAD AT ALL.** The emitter minted a `TypeRef` for the attribute and attached it
/// to the sequence-0 `Param` of every `ref readonly` member, resolving into a reference that does
/// not have it. csc reading such a core library reports `CS0518: predefined type 'System.Object'
/// is not defined or imported` **for a one-line program** -- because a reference that fails to load
/// reads as an EMPTY reference, and the diagnostic is about the first predefined type that goes
/// missing rather than about the member that caused it. Isolated to one variable and proved in both
/// directions: the same core library compiles and loads with `ref` in place of `ref readonly`, and
/// loads again with the attribute declared in source.
///
/// **AN ORDINARY LIBRARY SURVIVED IT, WHICH IS WHY IT WENT UNNOTICED.** csc reads a `ref readonly`
/// member out of a non-corlib assembly with the same dangling reference and still answers `CS8331`
/// for a write through it, exactly as it does for its own output. Only the core library -- the
/// assembly whose failure to load takes `System.Object` with it -- turns the loose end into a
/// compilation that cannot start.
///
/// **SYNTHESIZED ONLY WHEN NOBODY ELSE HAS IT**, which is what csc does: compiled against a
/// reference set that declares the attribute it emits a `TypeRef` and no type at all. The lookup
/// covers this compilation's own declarations too, so a core library that declares the attribute in
/// source gets one copy rather than two.
///
fn emit_is_readonly_attribute(image: &mut ImageBuilder, model: &Model, tokens: &mut Tokens) {
    let pending = core::mem::take(&mut tokens.pending_is_readonly);
    if pending.is_empty() {
        return;
    }
    let (namespace, name) = IS_READONLY_ATTRIBUTE;
    let marker = named_symbol(namespace, name);
    let constructor = if model.get(namespace, name).is_some() {
        mint_named_type_token(&marker, image, tokens);
        match tokens.type_token(&marker) {
            Some(ty) => image.member_ref(ty, ".ctor", &method_signature(true, &[], &TypeSig::Void)),
            None => return,
        }
    } else {
        synthesize_is_readonly_attribute(image, tokens)
    };
    for parent in pending {
        image.add_custom_attribute(parent, constructor, &[0x01, 0x00, 0x00, 0x00]);
    }
}

/// Adds `internal sealed class IsReadOnlyAttribute : Attribute` and its public parameterless
/// constructor, returning the constructor's `MethodDef` token.
///
/// **THE BASE AND ITS CONSTRUCTOR GO THROUGH THE ORDINARY LOOKUPS**, which is the whole reason this
/// works in a core library: there `System.Attribute` is a `TypeDef` in this very module and its
/// `.ctor` a `MethodDef`, while in an ordinary compilation both are references. `base_type_token`
/// and `base_class_ctor` already answer for both, so neither shape is spelled here.
///
/// **THE TYPE AND ITS CONSTRUCTOR ARE ADDED BACK TO BACK AND LAST**, because `add_type` fixes the
/// type's `first_method` from the current row count and a type owns every `MethodDef` up to the
/// next one's.
fn synthesize_is_readonly_attribute(image: &mut ImageBuilder, tokens: &mut Tokens) -> Token {
    let (namespace, name) = IS_READONLY_ATTRIBUTE;
    let attribute = named_symbol("System", "Attribute");
    let extends = base_type_token(&attribute, &[], image, tokens)
        .unwrap_or_else(|| image.type_ref("System", "Attribute"));
    image.add_type(namespace, name, extends, TYPE_SEALED | TYPE_BEFORE_FIELD_INIT);
    let base_ctor = base_class_ctor(image, tokens, &attribute, &[]);
    let code = alloc::vec![
        Instruction::new(Opcode::Ldarg, Operand::Variable(0)),
        Instruction::new(Opcode::Call, Operand::Token(base_ctor)),
        Instruction::simple(Opcode::Ret),
    ];
    let body = MethodBodyImage {
        max_stack: max_stack(&code).max(1),
        init_locals: false,
        local_var_sig: None,
        code: code.into_boxed_slice(),
        handlers: alloc::boxed::Box::new([]),
    };
    let Ok(body_bytes) = write_method_body(&body) else {
        return Token::new(0, 0);
    };
    image.add_method(
        ".ctor",
        &method_signature(true, &[], &TypeSig::Void),
        &body_bytes,
        ctor_flags(&[Modifier::Public]),
        IL_MANAGED,
        &[],
    )
}

fn apply_return_modifiers(
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
    return_sig: TypeSig,
) -> Result<TypeSig, crate::EmitError> {
    let return_sig = apply_init_only_modifier(image, tokens, return_sig)?;
    Ok(apply_readonly_ref_modifier(image, tokens, return_sig))
}

fn apply_readonly_ref_modifier(
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
    return_sig: TypeSig,
) -> TypeSig {
    if !core::mem::take(&mut tokens.next_return_is_readonly_ref) {
        return return_sig;
    }
    readonly_ref_sig(image, tokens, return_sig)
}

/// [`apply_readonly_ref_modifier`] without the pre-set: for the BODILESS positions -- an abstract
/// member, an interface member -- which build their signature outright rather than through
/// [`emit_method_body`], and so have the declaration in hand instead of a mark.
fn readonly_ref_sig(image: &mut ImageBuilder, tokens: &mut Tokens, return_sig: TypeSig) -> TypeSig {
    let marker = named_symbol("System.Runtime.InteropServices", "InAttribute");
    mint_named_type_token(&marker, image, tokens);
    match tokens.type_token(&marker) {
        Some(modifier) => TypeSig::Modified {
            modifier,
            inner: Box::new(return_sig),
        },
        None => return_sig,
    }
}

/// Whether a member's return type was written `ref readonly`, read from the SYNTAX because the
/// bound symbol cannot say: `ref readonly T` and `ref T` are one `TypeSymbol::ByRef`.
fn is_readonly_ref(return_type: &TypeRef) -> bool {
    matches!(
        return_type.kind,
        lamella_syntax::ast::TypeRefKind::ByRef {
            is_readonly: true,
            ..
        }
    )
}

fn emit_method_body(
    image: &mut ImageBuilder,
    binder: &mut Binder,
    tokens: &mut Tokens,
    enclosing: &TypeSymbol,
    method_type_parameters: &[Box<str>],
    method_constraints: &[lamella_syntax::ast::TypeParameterConstraintClause],
    name: &str,
    return_symbol: &TypeSymbol,
    params: &[(Box<str>, TypeSymbol)],
    byref_flags: &[ParamPassing],
    body: &lamella_syntax::ast::Stmt,
    is_static: bool,
    is_async: bool,
    is_vararg: bool,
    flags: u16,
    prologue: Option<&ConstructorPrologue>,
    debug: Option<&DebugContext>,
) -> Result<Token, crate::EmitError> {
    if is_vararg {
        binder.set_next_method_vararg();
    }
    let bound = binder.bind_method(
        Some(enclosing.clone()),
        name,
        return_symbol.clone(),
        params,
        &[],
        is_static,
        is_async,
        body,
    );
    if is_async {
        let returns_task = !return_symbol.is_void();
        let machine_index = tokens.next_async_index(enclosing);
        let machine_name = format!("<{name}>d__{machine_index}");
        let lowering = crate::awaitlower::lower_async_method(
            enclosing,
            &machine_name,
            is_static,
            returns_task,
            params,
            &bound,
        )?;
        let scope = GenericScope {
            method: &[],
            declaring: &[],
        };
        let parameter_sigs: Vec<TypeSig> = params
            .iter()
            .enumerate()
            .map(|(index, (_, ty))| {
                let sig = open_type_sig(tokens, ty, scope)?;
                Ok(if byref_flags.get(index).copied().unwrap_or(ParamPassing::ByValue).is_byref() {
                    TypeSig::ByRef(Box::new(sig))
                } else {
                    sig
                })
            })
            .collect::<Result<_, _>>()?;
        let return_sig = open_type_sig(tokens, return_symbol, scope)?;
        let return_sig = apply_return_modifiers(image, tokens, return_sig)?;
        let signature = method_signature(!is_static, &parameter_sigs, &return_sig);
        let stub_token = image.add_method_deferred_body(
            name,
            &signature,
            flags,
            IL_MANAGED,
            &by_value_parameter_names(params),
        );
        tokens.pending_async.push(PendingAsync {
            stub_token,
            lowering,
            enclosing: enclosing.clone(),
            params: params.to_vec(),
            byref_flags: byref_flags.to_vec(),
            return_symbol: return_symbol.clone(),
            is_static,
            flags,
        });
        return Ok(stub_token);
    }
    emit_bound_body(
        image,
        tokens,
        enclosing,
        method_type_parameters,
        method_constraints,
        name,
        return_symbol,
        params,
        byref_flags,
        &bound,
        is_static,
        is_vararg,
        flags,
        prologue,
        debug,
    )
}

/// Lowers an already-bound method body to CIL and adds the `MethodDef`, returning its
/// token. Split from [`emit_method_body`] so a destructor can wrap its bound body in the
/// `try`/`finally` that chains to the base finalizer (17.12) before lowering.
///
/// **THIS IS A WRAPPER SO THE BODY'S GENERIC SCOPE IS RESTORED ON EVERY EXIT, NOT ON THE LAST
/// ONE.** [`emit_bound_body_in_scope`] leaves through a dozen `?`s; a restore written at its
/// bottom would run on one path of twelve, and every path after a refused method would then lower
/// its own `T` against this method's parameter list. Same repair as the constraint check that was
/// written at the bottom of a resolver with six exits -- rename the body, make the public name the
/// wrapper, and "every exit" becomes true by construction rather than by inspection.
#[allow(clippy::too_many_arguments)]
fn emit_bound_body(
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
    enclosing: &TypeSymbol,
    method_type_parameters: &[Box<str>],
    method_constraints: &[lamella_syntax::ast::TypeParameterConstraintClause],
    name: &str,
    return_symbol: &TypeSymbol,
    params: &[(Box<str>, TypeSymbol)],
    byref_flags: &[ParamPassing],
    bound: &BoundStmt,
    is_static: bool,
    is_vararg: bool,
    flags: u16,
    prologue: Option<&ConstructorPrologue>,
    debug: Option<&DebugContext>,
) -> Result<Token, crate::EmitError> {
    for index in 0..method_type_parameters.len() as u32 {
        if tokens.mvar_spec(index).is_none() {
            let spec = image.type_spec(&type_signature(&TypeSig::MVar(index)));
            tokens.insert_mvar_spec(index, spec);
        }
    }
    let declaring_parameters = tokens.type_parameters(enclosing).to_vec();
    let saved = tokens.enter_body_scope(method_type_parameters, &declaring_parameters);
    let emitted = emit_bound_body_in_scope(
        image,
        tokens,
        &declaring_parameters,
        method_type_parameters,
        method_constraints,
        name,
        return_symbol,
        params,
        byref_flags,
        bound,
        is_static,
        is_vararg,
        flags,
        prologue,
        debug,
        None,
    );
    tokens.restore_body_scope(saved);
    emitted
}

/// [`emit_bound_body`] into a `MethodDef` ROW THAT ALREADY EXISTS -- one added through
/// [`ImageBuilder::add_method_deferred_body`], whose row had to precede its bytes. Two callers,
/// both the async lowering's: the STUB (its row sits in the source type's method run, its body
/// names the machine's members, and the machine's rows land after every source type) and
/// `MoveNext` itself (its body takes `ldftn` of its OWN token for the resumption delegate).
#[allow(clippy::too_many_arguments)]
fn emit_bound_body_into(
    into: Token,
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
    enclosing: &TypeSymbol,
    name: &str,
    return_symbol: &TypeSymbol,
    params: &[(Box<str>, TypeSymbol)],
    byref_flags: &[ParamPassing],
    bound: &BoundStmt,
    is_static: bool,
    flags: u16,
    debug: Option<&DebugContext>,
) -> Result<Token, crate::EmitError> {
    let declaring_parameters = tokens.type_parameters(enclosing).to_vec();
    let saved = tokens.enter_body_scope(&[], &declaring_parameters);
    let emitted = emit_bound_body_in_scope(
        image,
        tokens,
        &declaring_parameters,
        &[],
        &[],
        name,
        return_symbol,
        params,
        byref_flags,
        bound,
        is_static,
        false,
        flags,
        None,
        debug,
        Some(into),
    );
    tokens.restore_body_scope(saved);
    emitted
}

/// [`emit_bound_body`] with the body's generic scope already open on `tokens`. Never called
/// directly -- the wrapper is what guarantees the scope is closed again.
#[allow(clippy::too_many_arguments)]
fn emit_bound_body_in_scope(
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
    declaring_parameters: &[Box<str>],
    method_type_parameters: &[Box<str>],
    method_constraints: &[lamella_syntax::ast::TypeParameterConstraintClause],
    name: &str,
    return_symbol: &TypeSymbol,
    params: &[(Box<str>, TypeSymbol)],
    byref_flags: &[ParamPassing],
    bound: &BoundStmt,
    is_static: bool,
    is_vararg: bool,
    flags: u16,
    prologue: Option<&ConstructorPrologue>,
    debug: Option<&DebugContext>,
    deferred: Option<Token>,
) -> Result<Token, crate::EmitError> {
    mint_references(bound, image, tokens);
    if let Some(prologue) = prologue {
        for argument in &prologue.arguments {
            mint_in_expr(argument, image, tokens);
        }
    }

    let scope = GenericScope {
        method: method_type_parameters,
        declaring: declaring_parameters,
    };
    let arg_base = u16::from(!is_static);
    let parameter_names = bound_parameter_names(params, byref_flags);
    let byref_params: Vec<(Box<str>, TypeSymbol)> = params
        .iter()
        .enumerate()
        .filter(|(index, _)| byref_flags.get(*index).copied().unwrap_or(ParamPassing::ByValue).is_byref())
        .map(|(_, (name, ty))| (name.clone(), ty.clone()))
        .collect();
    let debug_source = debug.map(|context| context.source.as_bytes());
    let EmittedBody {
        code,
        local_types,
        local_names,
        sequence_points,
        handlers,
        pinned_slots,
    } = emit_body(
        &param_row_names(&parameter_names),
        &byref_params,
        scope.declaring,
        bound,
        tokens,
        arg_base,
        return_symbol,
        prologue,
        debug_source,
    )
    .map_err(|error| error.in_method(name))?;
    let local_var_sig = if local_types.is_empty() {
        None
    } else {
        let locals: Vec<TypeSig> = local_types
            .iter()
            .enumerate()
            .map(|(slot, ty)| {
                let sig = open_type_sig(tokens, ty, scope)?;
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

    if let Some(token) = deferred {
        return land_deferred_body(image, token, &body_bytes, method_debug);
    }

    let parameter_sigs: Vec<TypeSig> = params
        .iter()
        .enumerate()
        .map(|(index, (_, ty))| {
            let sig = open_type_sig(tokens, ty, scope)?;
            Ok(if byref_flags.get(index).copied().unwrap_or(ParamPassing::ByValue).is_byref() {
                TypeSig::ByRef(Box::new(sig))
            } else {
                sig
            })
        })
        .collect::<Result<_, _>>()?;
    let return_sig = open_type_sig(tokens, return_symbol, scope)?;
    let readonly_return = tokens.next_return_is_readonly_ref;
    let return_sig = apply_return_modifiers(image, tokens, return_sig)?;
    let signature = if is_vararg {
        vararg_method_signature(!is_static, &parameter_sigs, &return_sig)
    } else if method_type_parameters.is_empty() {
        method_signature(!is_static, &parameter_sigs, &return_sig)
    } else {
        generic_method_signature(
            !is_static,
            method_type_parameters.len() as u32,
            &parameter_sigs,
            &return_sig,
        )
    };
    let method = image.add_method(
        name,
        &signature,
        &body_bytes,
        flags,
        IL_MANAGED,
        &parameter_names,
    );
    if readonly_return {
        mark_readonly_return(image, tokens, method);
    }
    emit_generic_parameters(image, tokens, method, method_type_parameters, method_constraints);
    if let Some(debug) = method_debug {
        image.set_method_debug(method, debug);
    }
    Ok(method)
}

/// One async method awaiting its machine: everything `emit_async_machine` needs once every
/// source type's rows are placed. Queued on [`Tokens`] by `emit_method_body`'s async arm.
#[derive(Debug)]
pub(crate) struct PendingAsync {
    /// The async method's own `MethodDef`, added with a deferred body in its type's method run.
    pub(crate) stub_token: Token,
    /// The rewrite: machine shape, `MoveNext` body, stub body.
    pub(crate) lowering: crate::awaitlower::AsyncLowering,
    /// The declaring type, for the `NestedClass` row and the stub's emission scope.
    pub(crate) enclosing: TypeSymbol,
    /// The stub's frame facts, fixed when its row was added.
    pub(crate) params: Vec<(Box<str>, TypeSymbol)>,
    pub(crate) byref_flags: Vec<ParamPassing>,
    pub(crate) return_symbol: TypeSymbol,
    pub(crate) is_static: bool,
    pub(crate) flags: u16,
}

/// Emits one queued async lowering: the machine (a private sealed nested class -- its TypeDef,
/// fields, default `.ctor` and `MoveNext`), then the STUB's deferred body, which can only encode
/// now that the machine's tokens exist. `MoveNext` itself is added deferred too: its body takes
/// `ldftn` of its OWN token for the resumption delegate.
fn emit_async_machine(
    image: &mut ImageBuilder,
    binder: &mut Binder,
    tokens: &mut Tokens,
    pending: PendingAsync,
) -> Result<(), crate::EmitError> {
    let PendingAsync {
        stub_token,
        lowering,
        enclosing,
        params,
        byref_flags,
        return_symbol,
        is_static,
        flags,
    } = pending;
    let machine = &lowering.machine_symbol;
    let base = system_base(image, tokens, "Object");
    let machine_token = image.add_type("", &lowering.machine_name, base, 0x0000_0003 | TYPE_SEALED);
    tokens.insert_type(machine, machine_token);
    if let Some(enclosing_token) = tokens.type_token(&enclosing) {
        image.add_nested_class(machine_token, enclosing_token);
    }
    for (field_name, field_ty) in &lowering.fields {
        mint_signature_type(binder, field_ty, &[], &[], image, tokens)?;
        let signature = field_signature(&type_sig(tokens, field_ty)?);
        let token = image.add_field(field_name, &signature, FIELD_PUBLIC);
        tokens.insert_field(machine, field_name, token);
    }
    let ctor_prologue = ConstructorPrologue {
        ctor: object_base_ctor(image, tokens),
        span: None,
        arguments: Vec::new(),
        leading_body: 0,
        zero_initialize: None,
    };
    let empty_body = BoundStmt {
        kind: lamella_binder::BoundStmtKind::Block(Vec::new()),
        span: lamella_syntax::span::Span::empty_at(0),
    };
    let ctor_token = emit_bound_body(
        image,
        tokens,
        machine,
        &[],
        &[],
        ".ctor",
        &TypeSymbol::Special(SpecialType::Void),
        &[],
        &[],
        &empty_body,
        false,
        false,
        ctor_flags(&[Modifier::Public]),
        Some(&ctor_prologue),
        None,
    )?;
    tokens.insert_method(machine, ".ctor", &[], ctor_token);
    let move_next_signature = method_signature(true, &[], &TypeSig::Void);
    let move_next_token = image.add_method_deferred_body(
        "MoveNext",
        &move_next_signature,
        METHOD_PUBLIC | METHOD_HIDEBYSIG,
        IL_MANAGED,
        &[],
    );
    tokens.insert_method(machine, "MoveNext", &[], move_next_token);
    mint_references(&lowering.move_next_body, image, tokens);
    emit_bound_body_into(
        move_next_token,
        image,
        tokens,
        machine,
        "MoveNext",
        &TypeSymbol::Special(SpecialType::Void),
        &[],
        &[],
        &lowering.move_next_body,
        false,
        METHOD_PUBLIC | METHOD_HIDEBYSIG,
        None,
    )?;
    mint_references(&lowering.stub_body, image, tokens);
    emit_bound_body_into(
        stub_token,
        image,
        tokens,
        &enclosing,
        "<async stub>",
        &return_symbol,
        &params,
        &byref_flags,
        &lowering.stub_body,
        is_static,
        flags,
        None,
    )?;
    Ok(())
}

/// The tail [`emit_bound_body_in_scope`] takes when the row pre-exists: land the bytes, fill the
/// debug row, and hand the same token back.
fn land_deferred_body(
    image: &mut ImageBuilder,
    token: Token,
    body_bytes: &[u8],
    method_debug: Option<MethodDebug>,
) -> Result<Token, crate::EmitError> {
    image.set_method_body(token, body_bytes);
    if let Some(debug) = method_debug {
        image.set_method_debug(token, debug);
    }
    Ok(token)
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
    let mut sequence_points: Vec<SequencePoint> = Vec::new();
    for (index, span) in points.iter() {
        let point = match span {
            None => SequencePoint::hidden(offsets[*index as usize]),
            Some(span) if span.start == span.end => continue,
            Some(span) => {
                let lines = context.lines.span_lines(context.source, *span);
                SequencePoint {
                    il_offset: offsets[*index as usize],
                    start_line: lines.start_line,
                    start_column: lines.start_column,
                    end_line: lines.end_line,
                    end_column: lines.end_column,
                    is_hidden: false,
                }
            }
        };
        if sequence_points
            .last()
            .is_some_and(|last| last.il_offset == point.il_offset)
        {
            *sequence_points.last_mut().unwrap() = point;
        } else {
            sequence_points.push(point);
        }
    }
    let locals = local_names
        .iter()
        .enumerate()
        .filter(|(_, name)| !name.is_empty() && !matches!(name.as_bytes()[0], b'<' | b'$'))
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
        document: context.document,
    })
}

/// Emits a property's accessors as `get_Name`/`set_Name` methods (a getter
/// returning the property type, a setter taking `value`).
const SEMANTICS_SETTER: u16 = 0x0001;
const SEMANTICS_GETTER: u16 = 0x0002;
const SEMANTICS_ADDON: u16 = 0x0008;
const SEMANTICS_REMOVEON: u16 = 0x0010;

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn emit_property(
    image: &mut ImageBuilder,
    binder: &mut Binder,
    tokens: &mut Tokens,
    enclosing: &TypeSymbol,
    modifiers: &[Modifier],
    ty: &lamella_syntax::ast::TypeRef,
    name: &str,
    getter: Option<&lamella_syntax::ast::Accessor>,
    setter: Option<&lamella_syntax::ast::Accessor>,
    explicit_interface: Option<&lamella_syntax::ast::TypeRef>,
    debug: Option<&DebugContext>,
) -> Result<Token, crate::EmitError> {
    let property_ty = binder.canonicalize(&bind_type(ty));
    let is_static = explicit_interface.is_none() && modifiers.contains(&Modifier::Static);
    let is_abstract = modifiers.contains(&Modifier::Abstract);

    let property_name = match explicit_interface {
        Some(interface) => explicit_interface_member_name(interface, name),
        None => String::from(name),
    };
    let property_sig = member_type_sig(tokens, enclosing, &property_ty)?;
    let property_sig = bodyless_return_sig(ty, property_sig, image, tokens);
    let signature = property_signature(!is_static, &[], &property_sig);
    let property = image.add_property(&property_name, &signature, 0);
    if is_readonly_ref(ty) {
        attach_is_readonly(tokens, property);
    }

    let void = TypeSymbol::Special(SpecialType::Void);
    let backing = if is_auto_property(modifiers, getter, setter, false) {
        auto_backing_field(
            image,
            tokens,
            enclosing,
            &auto_property_backing_field_name(explicit_interface, name),
            &property_ty,
        )
    } else {
        None
    };

    if let Some(getter) = getter {
        let accessor = accessor_name("get_", name);
        let flags = property_accessor_flags(
            binder,
            enclosing,
            modifiers,
            &getter.modifiers,
            is_static,
            explicit_interface.is_some(),
            &accessor,
            &[],
        );
        let method_name = explicit_accessor_name(explicit_interface, &accessor);
        let token = if let Some(body) = &getter.body {
            tokens.next_return_is_readonly_ref = is_readonly_ref(ty);
            let token = emit_method_body(
                image, binder, tokens, enclosing, &[], &[], &method_name, &property_ty, &[], &[], body,
                is_static, false, false, flags, None, debug,
            )?;
            if let Some(interface) = explicit_interface {
                emit_explicit_interface_impl(
                    image, binder, tokens, enclosing, interface, &accessor, &[], &property_ty,
                    token,
                )?;
            }
            Some(token)
        } else if is_abstract {
            let signature = method_signature(true, &[], &member_type_sig(tokens, enclosing, &property_ty)?);
            Some(image.add_abstract_method(&method_name, &signature, flags, &[]))
        } else if let Some(field) = backing {
            let signature =
                method_signature(!is_static, &[], &member_type_sig(tokens, enclosing, &property_ty)?);
            let token = emit_auto_accessor(
                image,
                &method_name,
                &signature,
                flags,
                &[],
                field,
                is_static,
                false,
            )?;
            emit_compiler_generated_marker(image, tokens, token);
            if let Some(interface) = explicit_interface {
                emit_explicit_interface_impl(
                    image, binder, tokens, enclosing, interface, &accessor, &[], &property_ty,
                    token,
                )?;
            }
            Some(token)
        } else {
            None
        };
        if let Some(token) = token {
            emit_attributes(image, binder, tokens, enclosing, token, &getter.attributes);
            image.add_method_semantics(SEMANTICS_GETTER, token, property);
        }
    }
    if let Some(setter) = setter {
        let accessor = accessor_name("set_", name);
        let flags = property_accessor_flags(
            binder,
            enclosing,
            modifiers,
            &setter.modifiers,
            is_static,
            explicit_interface.is_some(),
            &accessor,
            &[property_ty.clone()],
        );
        let method_name = explicit_accessor_name(explicit_interface, &accessor);
        let params = [(Box::from("value"), property_ty.clone())];
        let setter_void = if setter.is_init {
            let marker = named_symbol("System.Runtime.CompilerServices", "IsExternalInit");
            mint_named_type_token(&marker, image, tokens);
            match tokens.type_token(&marker) {
                Some(modifier) => TypeSig::Modified {
                    modifier,
                    inner: Box::new(TypeSig::Void),
                },
                None => TypeSig::Void,
            }
        } else {
            TypeSig::Void
        };
        let token = if let Some(body) = &setter.body {
            tokens.next_return_is_external_init = setter.is_init;
            let token = emit_method_body(
                image, binder, tokens, enclosing, &[], &[], &method_name, &void, &params, &[], body,
                is_static, false, false, flags, None, debug,
            )?;
            if let Some(interface) = explicit_interface {
                emit_explicit_interface_impl(
                    image,
                    binder,
                    tokens,
                    enclosing,
                    interface,
                    &accessor,
                    &[property_ty.clone()],
                    &void,
                    token,
                )?;
            }
            Some(token)
        } else if is_abstract {
            let signature =
                method_signature(true, &[member_type_sig(tokens, enclosing, &property_ty)?], &setter_void);
            Some(image.add_abstract_method(&method_name, &signature, flags, &by_value_parameter_names(&params)))
        } else if let Some(field) = backing {
            let signature = method_signature(
                !is_static,
                &[member_type_sig(tokens, enclosing, &property_ty)?],
                &setter_void,
            );
            let token = emit_auto_accessor(
                image,
                &method_name,
                &signature,
                flags,
                &by_value_parameter_names(&params),
                field,
                is_static,
                true,
            )?;
            emit_compiler_generated_marker(image, tokens, token);
            if let Some(interface) = explicit_interface {
                emit_explicit_interface_impl(
                    image,
                    binder,
                    tokens,
                    enclosing,
                    interface,
                    &accessor,
                    &[property_ty.clone()],
                    &void,
                    token,
                )?;
            }
            Some(token)
        } else {
            None
        };
        if let Some(token) = token {
            emit_attributes(image, binder, tokens, enclosing, token, &setter.attributes);
            image.add_method_semantics(SEMANTICS_SETTER, token, property);
        }
    }
    Ok(property)
}

/// Whether a property is an AUTOMATICALLY IMPLEMENTED one (C# 3.0 10.7.3): BOTH accessors present
/// and BOTH written without a body, on a member that is not abstract, extern, or an interface's.
///
/// Whether a field-like event declaration (`event H E;`) carries a synthesized backing delegate
/// field. An ordinary one does; an `abstract` one does not, because it declares only the contract
/// -- its `add_E`/`remove_E` are bodyless and there is nothing to combine a handler onto. An
/// interface event is abstract for the same reason.
///
/// **BOTH POSITIONS OF THIS RULE CALL IT.** The token pre-pass reserves the field row and the
/// emitter writes it, and a `TypeDef`'s field range runs to where the next type's begins
/// (II.22.37) -- so the two answering differently does not lose a field, it shifts every field
/// token after it. This is [`is_auto_property`]'s shape for the member kind beside it.
fn event_has_backing_field(modifiers: &[Modifier], is_interface: bool) -> bool {
    !is_interface && !modifiers.contains(&Modifier::Abstract)
}

/// The token a synthesized accessor must use to reach its own backing field, which is NOT always
/// the `FieldDef` the pre-pass reserved.
///
/// **INSIDE A GENERIC TYPE A `FieldDef` OPERAND IS UNVERIFIABLE IL.** `ldarg.0` in an instance
/// method of `Bag<T>` has type `Bag<!0>`, and a `ldfld` naming the OPEN definition does not match
/// it (II.9.7). The runtime runs it anyway -- which is what makes this worth stating: the program
/// passes its behavioral test and `ilverify` refuses the assembly. So a generic type's own field is
/// reached through a `MemberRef` parented on the `TypeSpec` for `Bag<!0>`, as csc emits it.
///
/// Returns the plain `FieldDef` for a non-generic type, and `None` when the pre-pass reserved no
/// field at all -- which is the caller's signal that this property is not an auto-property.
fn auto_backing_field(
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
    enclosing: &TypeSymbol,
    name: &str,
    field_ty: &TypeSymbol,
) -> Option<Token> {
    let definition = tokens.field(enclosing, name)?;
    let parameters = tokens.type_parameters(enclosing).to_vec();
    if parameters.is_empty() {
        return Some(definition);
    }
    let parent = self_type_token(image, tokens, enclosing)?;
    let signature = open_type_sig(
        tokens,
        field_ty,
        GenericScope {
            method: &[],
            declaring: &parameters,
        },
    );
    match signature {
        Ok(signature) => Some(image.member_ref(parent, name, &field_signature(&signature))),
        Err(_) => Some(definition),
    }
}

/// Synthesizes an AUTO-PROPERTY accessor -- the whole of what `{ get; set; }` means. The getter
/// reads the backing field and the setter writes it; there is nothing else in either body.
///
/// ```text
/// instance getter:  ldarg.0; ldfld  <backing>;         ret
/// instance setter:  ldarg.0; ldarg.1; stfld <backing>; ret
/// static   getter:  ldsfld <backing>;                  ret
/// static   setter:  ldarg.0; stsfld <backing>;         ret
/// ```
///
/// The setter's value argument is slot 1 on an instance accessor (slot 0 is `this`) and slot 0 on
/// a static one, which is the only difference between the two pairs beyond the field opcode.
#[allow(clippy::too_many_arguments)]
fn emit_auto_accessor(
    image: &mut ImageBuilder,
    method_name: &str,
    signature: &[u8],
    flags: u16,
    parameter_names: &[ParamRow],
    field: Token,
    is_static: bool,
    is_setter: bool,
) -> Result<Token, crate::EmitError> {
    let mut code = Vec::new();
    if !is_static {
        code.push(Instruction::new(Opcode::Ldarg, Operand::Variable(0)));
    }
    if is_setter {
        code.push(Instruction::new(
            Opcode::Ldarg,
            Operand::Variable(u16::from(!is_static)),
        ));
        code.push(Instruction::new(
            if is_static { Opcode::Stsfld } else { Opcode::Stfld },
            Operand::Token(field),
        ));
    } else {
        code.push(Instruction::new(
            if is_static { Opcode::Ldsfld } else { Opcode::Ldfld },
            Operand::Token(field),
        ));
    }
    code.push(Instruction::simple(Opcode::Ret));
    let body_image = MethodBodyImage {
        max_stack: max_stack(&code).max(1),
        init_locals: false,
        local_var_sig: None,
        code: code.into_boxed_slice(),
        handlers: Box::new([]),
    };
    let body_bytes = write_method_body(&body_image).map_err(|_| {
        crate::EmitError::Unsupported("an auto-property accessor body could not be written")
    })?;
    Ok(image.add_method(
        method_name,
        signature,
        &body_bytes,
        flags,
        IL_MANAGED,
        parameter_names,
    ))
}

/// The token naming the type a body is INSIDE, where an instruction operand needs one: the
/// `TypeDef` for an ordinary type, and the `TypeSpec` for `C<!0, !1, ...>` when the type is
/// generic -- because inside `Bag<T>`, `this` has type `Bag<!0>` and not the open definition.
fn self_type_token(
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
    enclosing: &TypeSymbol,
) -> Option<Token> {
    let parameters = tokens.type_parameters(enclosing).to_vec();
    if parameters.is_empty() {
        return tokens.type_token(enclosing);
    }
    let TypeSymbol::Named(parts) = enclosing else {
        return tokens.type_token(enclosing);
    };
    let mut definition_parts = parts.clone();
    if let Some(last) = definition_parts.last_mut() {
        *last = lamella_binder::unmangled_type_name(last);
    }
    let arguments: Box<[TypeSymbol]> = parameters
        .iter()
        .map(|parameter| TypeSymbol::Named(Box::from([parameter.clone()])))
        .collect();
    let instantiation = TypeSymbol::Instantiation {
        definition: definition_parts.clone(),
        arguments: arguments.clone(),
    };
    let saved = tokens.enter_body_scope(&[], &parameters);
    let spec = mint_type_spec(&instantiation, &definition_parts, &arguments, image, tokens);
    tokens.restore_body_scope(saved);
    spec.or_else(|| tokens.type_token(enclosing))
}

/// The method flags for a property/indexer accessor `accessor` (`get_X`/`set_X`) with `params`.
/// An explicit-interface accessor is a private sealed virtual (20.4.1). Otherwise it is a sealed
/// virtual (`public virtual final newslot`) only when it implicitly implements an interface member
/// -- its name + signature match one -- so an accessor implementing nothing stays non-virtual even
/// on an interface-implementing type (its vtable-slot flags follow its own modifiers).
#[allow(clippy::too_many_arguments)]
fn property_accessor_flags(
    binder: &Binder,
    enclosing: &TypeSymbol,
    modifiers: &[Modifier],
    accessor_modifiers: &[Modifier],
    is_static: bool,
    explicit: bool,
    accessor: &str,
    params: &[TypeSymbol],
) -> u16 {
    if explicit {
        METHOD_PRIVATE
            | METHOD_VIRTUAL
            | METHOD_FINAL
            | METHOD_NEWSLOT
            | METHOD_HIDEBYSIG
            | SPECIAL_NAME
    } else if !is_static
        && !modifiers.contains(&Modifier::Abstract)
        && binder.member_implements_interface(enclosing, accessor, params)
    {
        METHOD_PUBLIC
            | METHOD_VIRTUAL
            | METHOD_NEWSLOT
            | METHOD_FINAL
            | METHOD_HIDEBYSIG
            | SPECIAL_NAME
    } else {
        let visibility = if accessor_modifiers.is_empty() {
            modifiers
        } else {
            accessor_modifiers
        };
        let level = member_visibility(visibility);
        let mut flags = level | SPECIAL_NAME;
        if is_static {
            flags |= METHOD_STATIC;
        } else {
            flags |= slot_flags(modifiers, level);
        }
        flags
    }
}

/// Emits an indexer (17.8) as the property `Item` whose signature carries the index
/// parameters, with `get_Item(indices)` / `set_Item(indices, value)` `specialname` accessors
/// and their `MethodSemantics`. The accessors take the vtable-slot flags of the indexer's
/// `virtual`/`override`/`abstract` modifiers (II.23.1.10), or the interface-implementation
/// slot when the enclosing type implements an interface, exactly as an ordinary property's
/// accessors do.
#[allow(clippy::too_many_arguments)]
fn emit_indexer(
    image: &mut ImageBuilder,
    binder: &mut Binder,
    tokens: &mut Tokens,
    enclosing: &TypeSymbol,
    name: &str,
    modifiers: &[Modifier],
    ty: &lamella_syntax::ast::TypeRef,
    parameters: &[Parameter],
    getter: Option<&lamella_syntax::ast::Accessor>,
    setter: Option<&lamella_syntax::ast::Accessor>,
    debug: Option<&DebugContext>,
) -> Result<Token, crate::EmitError> {
    let element_ty = binder.canonicalize(&bind_type(ty));
    let index_params: Vec<(Box<str>, TypeSymbol)> = parameters
        .iter()
        .map(|parameter| (parameter.name.clone(), binder.canonicalize(&bind_type(&parameter.ty))))
        .collect();
    let is_abstract = modifiers.contains(&Modifier::Abstract);
    let index_param_types: Vec<TypeSymbol> =
        index_params.iter().map(|(_, ty)| ty.clone()).collect();
    let index_sigs: Vec<TypeSig> = index_params
        .iter()
        .map(|(_, ty)| member_type_sig(tokens, enclosing, ty))
        .collect::<Result<_, _>>()?;
    let element_sig = member_type_sig(tokens, enclosing, &element_ty)?;
    let property_sig = bodyless_return_sig(ty, element_sig.clone(), image, tokens);
    let property =
        image.add_property(name, &property_signature(true, &index_sigs, &property_sig), 0);
    if is_readonly_ref(ty) {
        attach_is_readonly(tokens, property);
    }
    let void = TypeSymbol::Special(SpecialType::Void);
    if let Some(getter) = getter {
        let getter_name = accessor_name("get_", name);
        let flags = property_accessor_flags(
            binder, enclosing, modifiers, &getter.modifiers, false, false, &getter_name,
            &index_param_types,
        );
        let token = if let Some(body) = &getter.body {
            tokens.next_return_is_readonly_ref = is_readonly_ref(ty);
            Some(emit_method_body(
                image, binder, tokens, enclosing, &[], &[], &getter_name, &element_ty, &index_params, &[],
                body, false, false, false, flags, None, debug,
            )?)
        } else if is_abstract {
            let signature = method_signature(true, &index_sigs, &element_sig);
            Some(image.add_abstract_method(
                &getter_name,
                &signature,
                flags,
                &by_value_parameter_names(&index_params),
            ))
        } else {
            None
        };
        if let Some(token) = token {
            emit_attributes(image, binder, tokens, enclosing, token, &getter.attributes);
            emit_declared_parameter_metadata(image, binder, tokens, enclosing, token, parameters);
            image.add_method_semantics(SEMANTICS_GETTER, token, property);
        }
    }
    if let Some(setter) = setter {
        let mut params = index_params.clone();
        params.push((Box::from("value"), element_ty.clone()));
        let setter_name = accessor_name("set_", name);
        let mut setter_param_types = index_param_types.clone();
        setter_param_types.push(element_ty.clone());
        let flags = property_accessor_flags(
            binder, enclosing, modifiers, &setter.modifiers, false, false, &setter_name,
            &setter_param_types,
        );
        let token = if let Some(body) = &setter.body {
            Some(emit_method_body(
                image, binder, tokens, enclosing, &[], &[], &setter_name, &void, &params, &[],
                body, false, false, false, flags, None, debug,
            )?)
        } else if is_abstract {
            let mut signature_params = index_sigs.clone();
            signature_params.push(element_sig.clone());
            let signature = method_signature(true, &signature_params, &TypeSig::Void);
            Some(image.add_abstract_method(
                &setter_name,
                &signature,
                flags,
                &by_value_parameter_names(&params),
            ))
        } else {
            None
        };
        if let Some(token) = token {
            emit_attributes(image, binder, tokens, enclosing, token, &setter.attributes);
            emit_declared_parameter_metadata(image, binder, tokens, enclosing, token, parameters);
            image.add_method_semantics(SEMANTICS_SETTER, token, property);
        }
    }
    Ok(property)
}

/// The name a `[System.Runtime.CompilerServices.IndexerName("X")]` gives an indexer's accessors
/// (`get_X`/`set_X`), its `DefaultMember`, and its Property row. Defaults to `"Item"` (17.8) when
/// the attribute is absent -- as on String's indexer (`get_Chars`) and StringBuilder's.
fn indexer_name(attributes: &[AttributeSection]) -> String {
    for section in attributes {
        for attribute in &section.attributes {
            let Some(last) = attribute.name.parts.last() else {
                continue;
            };
            if &**last != "IndexerName" && &**last != "IndexerNameAttribute" {
                continue;
            }
            if let Some(name) = attribute.arguments.iter().find_map(|argument| match argument {
                AttributeArgument::Positional(expr) => string_literal_value(expr),
                AttributeArgument::Named { .. } => None,
            }) {
                return name;
            }
        }
    }
    String::from("Item")
}

/// Records an indexer accessor's `MethodDef` under the name it is EMITTED as -- `get_Chars` for
/// an indexer carrying `[IndexerName("Chars")]` -- beside the `get_Item` the model synthesizes for
/// every indexer regardless. The token pre-pass calls this for both accessors; see the reasoning
/// there for why one member is reachable under two spellings at all.
///
/// A no-op when the two names agree (every indexer with no `[IndexerName]`), and a no-op when the
/// emitted name is already taken: a type declaring both a renamed indexer and an ordinary method
/// of the accessor's name is a member collision for the binder to report, and overwriting here
/// would answer it by silently retargeting the method's call sites.
fn alias_renamed_accessor(
    tokens: &mut Tokens,
    declaring: &TypeSymbol,
    prefix: &str,
    indexer: &str,
    parameters: &[TypeSymbol],
    token: Token,
) {
    if indexer == "Item" {
        return;
    }
    let emitted = accessor_name(prefix, indexer);
    if tokens.method(declaring, &emitted, parameters).is_some() {
        return;
    }
    tokens.insert_method(declaring, &emitted, parameters, token);
}

/// Emits `[System.Reflection.DefaultMemberAttribute("Item")]` on a type that declares an
/// indexer (17.8), naming the member its `Item` accessors index -- how a consumer discovers
/// the indexer. A no-op for a type with no indexer. The attribute constructor's `MemberRef`
/// is minted on demand, then a `CustomAttribute` row carries the single `"Item"` string
/// argument (II.23.3).
fn emit_default_member_attribute(
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
    type_token: Token,
    members: &[Member],
) {
    let Some(name) = members.iter().find_map(|member| match member {
        Member::Indexer { attributes, .. } => Some(indexer_name(attributes)),
        _ => None,
    }) else {
        return;
    };
    let declaring = TypeSymbol::Named(
        [
            Box::from("System"),
            Box::from("Reflection"),
            Box::from("DefaultMemberAttribute"),
        ]
        .into(),
    );
    let parameters = alloc::vec![TypeSymbol::Special(SpecialType::String)];
    if tokens.method(&declaring, ".ctor", &parameters).is_none() {
        let constructor_ref = lamella_binder::MethodReference {
            declaring_type: declaring.clone(),
            name: ".ctor".into(),
            parameters: parameters.clone(),
            return_type: TypeSymbol::Special(SpecialType::Void),
            is_static: false,
            is_vararg: false,
            instantiation: None,
            declaring_instantiation: None,
        };
        mint_member_ref(&constructor_ref, image, tokens);
    }
    let Some(constructor) = tokens.method(&declaring, ".ctor", &parameters) else {
        return;
    };
    let mut blob = alloc::vec![0x01u8, 0x00];
    encode_ser_string(&name, &mut blob);
    blob.extend_from_slice(&0u16.to_le_bytes());
    image.add_custom_attribute(type_token, constructor, &blob);
}

/// The message csc puts in a required-members constructor guard, byte for byte. Measured from a
/// csc-built assembly, not transcribed from documentation: a guard whose text drifts is still a
/// guard, but it stops being the SAME guard, and the point of matching csc is that a consumer
/// cannot tell this compiler's output from its.
const REQUIRED_MEMBERS_OBSOLETE_MESSAGE: &str =
    "Constructors of types with required members are not supported in this version of your compiler.";

/// Mints the `MemberRef` for an attribute constructor the PROGRAM never names -- the compiler
/// synthesizes these -- and returns its token. `None` when the type or constructor cannot be
/// resolved, which is the same lenient posture [`emit_attributes`] takes.
///
/// **OWED, AND LENIENCY IS THE WRONG POSTURE HERE, unlike for a user-written attribute.** A
/// user-written attribute that does not resolve was already reported by the binder; these are
/// synthesized, so nothing reports them. Against a reference set with no
/// `RequiredMemberAttribute` this silently emits an assembly whose required members are NOT
/// MARKED -- a consumer then reads them as ordinary and constructs the type with them unset,
/// which is the exact failure the feature exists to prevent, arrived at silently. csc's answer is
/// **CS0656 "missing compiler required member"**, and matching it belongs in the BINDER (which has
/// a diagnostic sink; emission does not): when a type declares a required member, require the
/// attribute to resolve. Not built here rather than built badly, and written down rather than
/// left to be discovered.
fn synthesized_attribute_ctor(
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
    namespace: &str,
    name: &str,
    parameters: &[TypeSymbol],
) -> Option<Token> {
    let declaring = named_symbol(namespace, name);
    if tokens.method(&declaring, ".ctor", parameters).is_none() {
        let constructor_ref = lamella_binder::MethodReference {
            declaring_type: declaring.clone(),
            name: ".ctor".into(),
            parameters: parameters.to_vec(),
            return_type: TypeSymbol::Special(SpecialType::Void),
            is_static: false,
            is_vararg: false,
            instantiation: None,
            declaring_instantiation: None,
        };
        mint_member_ref(&constructor_ref, image, tokens);
    }
    tokens.method(&declaring, ".ctor", parameters)
}

/// `[System.Runtime.CompilerServices.RequiredMemberAttribute]` on a required field, a required
/// property, or the type that DECLARES one.
///
/// `required` has no metadata flag of its own (II.22.15/II.22.34 have no bit for it), so this
/// attribute IS the encoding -- which is why a consumer that does not decode custom attributes
/// reads every imported member as not-required.
/// `[System.Runtime.CompilerServices.CompilerGenerated]` on a member the COMPILER wrote -- an
/// auto-property's backing field and its two accessors.
///
/// **IT IS NOT DECORATION: it is how a tool tells a synthesized member from a declared one.** A
/// serializer that walks fields, a debugger deciding what to show, and a reflection-based mapper
/// all key on it, and csc puts it on every one of the three.
fn emit_compiler_generated_marker(image: &mut ImageBuilder, tokens: &mut Tokens, target: Token) {
    let Some(constructor) = synthesized_attribute_ctor(
        image,
        tokens,
        "System.Runtime.CompilerServices",
        "CompilerGeneratedAttribute",
        &[],
    ) else {
        return;
    };
    image.add_custom_attribute(target, constructor, &[0x01, 0x00, 0x00, 0x00]);
}

/// `[System.Diagnostics.DebuggerBrowsable(DebuggerBrowsableState.Never)]` on an auto-property's
/// backing field, as csc emits: a debugger then shows the PROPERTY and not the field behind it,
/// which is the whole reason the field has a name no program can type.
///
/// The argument is an enum, so its blob is the underlying `int32` -- `Never` is 0 (II.23.3).
fn emit_debugger_browsable_never(image: &mut ImageBuilder, tokens: &mut Tokens, target: Token) {
    let state = named_symbol("System.Diagnostics", "DebuggerBrowsableState");
    let Some(constructor) = synthesized_attribute_ctor(
        image,
        tokens,
        "System.Diagnostics",
        "DebuggerBrowsableAttribute",
        core::slice::from_ref(&state),
    ) else {
        return;
    };
    image.add_custom_attribute(
        target,
        constructor,
        &[0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    );
}

fn emit_required_member_marker(image: &mut ImageBuilder, tokens: &mut Tokens, target: Token) {
    let Some(constructor) = synthesized_attribute_ctor(
        image,
        tokens,
        "System.Runtime.CompilerServices",
        "RequiredMemberAttribute",
        &[],
    ) else {
        return;
    };
    image.add_custom_attribute(target, constructor, &[0x01, 0x00, 0x00, 0x00]);
}

/// The pair csc puts on every constructor of a type with required members that is not itself
/// `[SetsRequiredMembers]`: `[Obsolete(<message>, error: true)]` and
/// `[CompilerFeatureRequired("RequiredMembers")]`.
///
/// **TWO ATTRIBUTES BECAUSE THEY GUARD AGAINST TWO DIFFERENT CONSUMERS, and emitting only one
/// leaves a real hole.** A compiler that knows the feature keys off `CompilerFeatureRequired` and
/// suppresses the obsolete diagnostic; a compiler too old to know EITHER attribute still refuses
/// the constructor, because `Obsolete` with `error: true` has been a hard error since .NET 1.0.
/// Drop the `Obsolete` half and a down-level consumer silently constructs an object with unset
/// required members -- which is the entire failure the feature exists to prevent.
fn emit_required_members_constructor_guard(
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
    constructor_token: Token,
) {
    if let Some(obsolete) = synthesized_attribute_ctor(
        image,
        tokens,
        "System",
        "ObsoleteAttribute",
        &[
            TypeSymbol::Special(SpecialType::String),
            TypeSymbol::Special(SpecialType::Boolean),
        ],
    ) {
        let mut blob = alloc::vec![0x01u8, 0x00];
        encode_ser_string(REQUIRED_MEMBERS_OBSOLETE_MESSAGE, &mut blob);
        blob.push(0x01);
        blob.extend_from_slice(&0u16.to_le_bytes());
        image.add_custom_attribute(constructor_token, obsolete, &blob);
    }
    if let Some(feature) = synthesized_attribute_ctor(
        image,
        tokens,
        "System.Runtime.CompilerServices",
        "CompilerFeatureRequiredAttribute",
        &[TypeSymbol::Special(SpecialType::String)],
    ) {
        let mut blob = alloc::vec![0x01u8, 0x00];
        encode_ser_string("RequiredMembers", &mut blob);
        blob.extend_from_slice(&0u16.to_le_bytes());
        image.add_custom_attribute(constructor_token, feature, &blob);
    }
}

/// Whether an attribute list carries `[SetsRequiredMembers]` -- the one thing that exempts a
/// constructor from the guard pair and from `CS9035`.
///
/// Matched on the LAST name part, with and without the `Attribute` suffix, exactly as
/// [`indexer_name`] matches: a program may write the attribute qualified or not, and C# lets the
/// suffix be omitted (17.2).
fn sets_required_members(attributes: &[AttributeSection]) -> bool {
    attributes.iter().any(|section| {
        section.attributes.iter().any(|attribute| {
            attribute.name.parts.last().is_some_and(|last| {
                &**last == "SetsRequiredMembers" || &**last == "SetsRequiredMembersAttribute"
            })
        })
    })
}

/// Whether this declaration itself declares a `required` field or property -- the condition for the
/// TYPE-level marker, which is deliberately not the condition for the constructor guard. See
/// [`lamella_binder::Binder::has_required_members_in_chain`].
fn declares_required_member(declaration: &TypeDecl) -> bool {
    declaration.members.iter().any(|member| match member {
        Member::Field { modifiers, .. } | Member::Property { modifiers, .. } => {
            modifiers.contains(&Modifier::Required)
        }
        _ => false,
    })
}

/// The `MethodDef` name of a property accessor: `I.get_P` for an explicit-interface
/// implementation (matching the token pre-pass and the model), else the plain `get_P`.
fn explicit_accessor_name(
    explicit_interface: Option<&lamella_syntax::ast::TypeRef>,
    accessor: &str,
) -> String {
    match explicit_interface {
        Some(interface) => explicit_interface_member_name(interface, accessor),
        None => String::from(accessor),
    }
}

/// The `get_`/`set_` accessor method name for a property.
fn accessor_name(prefix: &str, property: &str) -> String {
    let mut name = String::from(prefix);
    name.push_str(property);
    name
}

/// The vtable-slot attributes (II.23.1.10) implied by a member's `virtual`/`override`/
/// `abstract`/`sealed` modifiers, on top of its accessibility: `virtual` and plain `abstract` open
/// a fresh slot (Virtual | NewSlot); `override` reuses the inherited slot (Virtual, no
/// NewSlot); `abstract` additionally marks the method bodyless (Abstract); `sealed override`
/// closes the slot to further overriding (Final, 17.5.6). A member with none of these keeps the
/// default non-virtual binding (0).
///
/// **THE ONE STATEMENT OF THIS RULE.** Every member kind that can carry these modifiers --
/// method, property, indexer and both forms of event -- calls this function rather than
/// restating it, so a slot attribute is decided in one place for all of them.
fn slot_flags(modifiers: &[Modifier], visibility: u16) -> u16 {
    let is_abstract = modifiers.contains(&Modifier::Abstract);
    let is_virtual = modifiers.contains(&Modifier::Virtual);
    let is_override = modifiers.contains(&Modifier::Override);
    if !(is_abstract || is_virtual || is_override) {
        return 0;
    }
    let mut flags = METHOD_VIRTUAL | METHOD_HIDEBYSIG;
    if is_virtual || (is_abstract && !is_override) {
        flags |= METHOD_NEWSLOT;
    }
    if is_abstract {
        flags |= METHOD_ABSTRACT;
    }
    if modifiers.contains(&Modifier::Sealed) {
        flags |= METHOD_FINAL;
    }
    if visibility & 0x0007 == 0x0003 {
        flags |= METHOD_CHECK_ACCESS_ON_OVERRIDE;
    }
    flags
}

/// The MemberAccess bits (II.23.1.5 / .10) for a member's declared modifiers: Public (6),
/// `protected` = Family (4), `internal` = Assembly (3), `protected internal` = FamORAssem (5),
/// else Private (1, the C# default for a class member). So reflection's NonPublic/Public
/// binding flags see the real accessibility (a `private` field is not reported as public).
fn member_visibility(modifiers: &[Modifier]) -> u16 {
    if modifiers.contains(&Modifier::Public) {
        0x0006
    } else if modifiers.contains(&Modifier::Protected) {
        if modifiers.contains(&Modifier::Internal) {
            0x0005
        } else {
            0x0004
        }
    } else if modifiers.contains(&Modifier::Internal) {
        0x0003
    } else {
        0x0001
    }
}

/// Adds a `Field` row per declarator, with the field's signature and flags. Field
/// initializers (which would run in a constructor) are not emitted yet.
fn emit_field(
    image: &mut ImageBuilder,
    binder: &Binder,
    tokens: &mut Tokens,
    enclosing: &TypeSymbol,
    modifiers: &[Modifier],
    ty: &lamella_syntax::ast::TypeRef,
    declarators: &[VariableDeclarator],
) -> Result<(), crate::EmitError> {
    let field_ty = binder.canonicalize(&bind_type(ty));
    let signature = field_signature(&member_type_sig(tokens, enclosing, &field_ty)?);
    let visibility = member_visibility(modifiers);
    let is_const = modifiers.contains(&Modifier::Const);
    let is_static = is_const || modifiers.contains(&Modifier::Static);
    for declarator in declarators {
        let constant = if is_const {
            const_field_row(binder.model(), enclosing, &declarator.name, &field_ty)
        } else {
            None
        };
        let mut flags = visibility;
        if is_static {
            flags |= FIELD_STATIC;
        }
        if modifiers.contains(&Modifier::Readonly) {
            flags |= FIELD_INITONLY;
        }
        let is_const_decimal =
            is_const && matches!(field_ty, TypeSymbol::Special(SpecialType::Decimal));
        if constant.is_some() {
            flags |= FIELD_LITERAL | FIELD_HAS_DEFAULT;
        } else if is_const_decimal {
            flags |= FIELD_INITONLY;
        }
        let field = image.add_field(&declarator.name, &signature, flags);
        if let Some((element, value)) = constant {
            image.add_constant(field, element, &value);
        } else if is_const_decimal {
            emit_decimal_constant_attribute(image, tokens, binder, enclosing, &declarator.name, field);
        }
    }
    Ok(())
}

/// The `Constant` row (II.22.9) for a `const` field: its element-type byte and value bytes, taken
/// from the literal the binder folded for the field. `None` if the field has no folded value or is
/// a type with no `Constant` encoding -- e.g. `decimal`, which uses a `DecimalConstantAttribute`
/// (not yet emitted), so a `const decimal` falls back to a plain static field.
fn const_field_row(
    model: &Model,
    enclosing: &TypeSymbol,
    name: &str,
    field_ty: &TypeSymbol,
) -> Option<(u8, alloc::vec::Vec<u8>)> {
    let literal = model.get_by_symbol(enclosing)?.find_field(name)?.constant.clone()?;
    constant_row(model, field_ty, &literal)
}

/// A `Constant` row's (element type, little-endian blob) for `literal` held at type `ty`
/// (II.22.9), or `None` when the type has no constant encoding.
///
/// **SHARED BY `const` FIELDS AND OPTIONAL PARAMETERS, because II.22.9 is one table and they write
/// the same row.** Three of its rules are ones a second copy would get wrong, and all three were
/// measured against csc rather than read off the standard:
///
/// * a NULL reference is `CLASS` (0x12) with four zero bytes -- and csc writes it for `= null`,
///   for `default(string)`, for `int? x = null` AND for `default(S)` on a struct;
/// * an ENUM constant takes its UNDERLYING integer's element type and width, not the enum's;
/// * a `decimal` has no encoding here at all and travels in a `[DecimalConstant]` attribute.
fn constant_row(
    model: &Model,
    field_ty: &TypeSymbol,
    literal: &Literal,
) -> Option<(u8, alloc::vec::Vec<u8>)> {
    let literal = literal.clone();
    if matches!(literal, Literal::Null) {
        return Some((0x12, alloc::vec![0u8; 4]));
    }
    if model
        .get_by_symbol(field_ty)
        .is_some_and(|info| info.kind == lamella_binder::TypeKind::Enum)
    {
        let underlying = TypeSymbol::Special(enum_underlying(model, field_ty));
        let (element, width) = enum_constant_encoding(&underlying).ok()?;
        let value = lamella_binder::literal_int_value(&literal)?;
        return Some((element, value.to_le_bytes()[..width].to_vec()));
    }
    let field_ty = lamella_binder::nullable_underlying(field_ty).unwrap_or(field_ty);
    let TypeSymbol::Special(special) = field_ty else {
        return None;
    };
    let element = primitive_element_code(*special)?;
    let mut value = alloc::vec::Vec::new();
    match (special, &literal) {
        (SpecialType::Boolean, Literal::Boolean(set)) => value.push(u8::from(*set)),
        (SpecialType::Char, Literal::Character(unit)) => value.extend_from_slice(&unit.to_le_bytes()),
        (SpecialType::String, Literal::String(units)) => {
            for unit in units.iter() {
                value.extend_from_slice(&unit.to_le_bytes());
            }
        }
        (SpecialType::Single, Literal::Real { bits, .. }) => {
            value.extend_from_slice(&(f64::from_bits(*bits) as f32).to_le_bytes());
        }
        (SpecialType::Double, Literal::Real { bits, .. }) => {
            value.extend_from_slice(&f64::from_bits(*bits).to_le_bytes());
        }
        (_, Literal::Integer { value: int, .. }) => encode_integer(*special, *int, &mut value)?,
        _ => return None,
    }
    Some((element, value))
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
                if let Some(filter) = &catch.filter {
                    mint_in_expr(filter, image, tokens);
                }
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
            if matches!(init.ty, TypeSymbol::Special(SpecialType::String)) {
                mint_member_ref(&offset_to_string_data_reference(), image, tokens);
            }
            mint_array_members(&init.ty, image, tokens);
            mint_references(body, image, tokens);
        }
        BoundStmtKind::Labeled { body, .. } => mint_references(body, image, tokens),
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
        BoundExprKind::Literal(Literal::Decimal { .. }) => {
            mint_decimal_ctor(image, tokens);
        }
        BoundExprKind::Binary {
            left,
            right,
            operator,
            ..
        } => {
            use lamella_syntax::ast::BinaryOperator as Op;
            if let Some(call) = crate::expr::concat_call(expr) {
                match call {
                    crate::expr::ConcatCall::Literal(text) => {
                        let token = image.user_string(&text);
                        tokens.insert_string(&text, token);
                    }
                    crate::expr::ConcatCall::Fixed {
                        reference,
                        arguments,
                    } => {
                        for argument in &arguments {
                            mint_in_expr(argument, image, tokens);
                        }
                        mint_member_ref(&reference, image, tokens);
                    }
                    crate::expr::ConcatCall::Packed { reference, array } => {
                        mint_in_expr(&array, image, tokens);
                        mint_member_ref(&reference, image, tokens);
                    }
                }
                return;
            }
            mint_in_expr(left, image, tokens);
            mint_in_expr(right, image, tokens);
            if matches!(operator, Op::Equal | Op::NotEqual)
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
            if matches!(operand.ty, TypeSymbol::Special(SpecialType::Decimal)) {
                mint_decimal_step("op_Increment", image, tokens);
                mint_decimal_step("op_Decrement", image, tokens);
            }
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
                || matches!(expr.ty, TypeSymbol::Special(SpecialType::String))
            {
                mint_value_type_token(&expr.ty, image, tokens);
            }
            if matches!(expr.ty, TypeSymbol::Special(SpecialType::Object))
                && is_value_type(&operand.ty, tokens)
            {
                mint_value_type_token(&operand.ty, image, tokens);
            }
            if matches!(expr.ty, TypeSymbol::Named(_)) && !is_value_type(&expr.ty, tokens)
                || matches!(expr.ty, TypeSymbol::Array { .. })
            {
                mint_type_token(image, tokens, &expr.ty);
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
                if let Some(instantiation) = method.instantiation.as_deref() {
                    mint_generic_call_site(method, instantiation, image, tokens);
                } else {
                    if let (_, Some(extras)) = crate::expr::split_vararg_arguments(arguments) {
                        if !extras.is_empty() {
                            mint_vararg_site_ref(method, extras, image, tokens);
                        }
                    }
                    let def_key = if method.is_vararg {
                        crate::expr::vararg_lookup_params(&method.parameters, &[])
                    } else {
                        method.parameters.clone()
                    };
                    if tokens
                        .method(&method.declaring_type, &method.name, &def_key)
                        .is_none()
                    {
                        mint_member_ref(method, image, tokens);
                    }
                }
            }
        }
        BoundExprKind::ObjectCreation {
            arguments,
            constructor,
            initializer,
        } => {
            for argument in arguments {
                mint_in_expr(argument, image, tokens);
            }
            if let Some(initializer) = initializer {
                mint_in_initializer(initializer, image, tokens);
            }
            if let Some(constructor) = constructor {
                if let (_, Some(extras)) = crate::expr::split_vararg_arguments(arguments) {
                    if !extras.is_empty() {
                        mint_vararg_site_ref(constructor, extras, image, tokens);
                    }
                }
                let def_key = if constructor.is_vararg {
                    crate::expr::vararg_lookup_params(&constructor.parameters, &[])
                } else {
                    constructor.parameters.clone()
                };
                let initobj_creation =
                    constructor.parameters.is_empty() && is_value_type(&constructor.declaring_type, tokens);
                if initobj_creation {
                    mint_value_type_token(&constructor.declaring_type, image, tokens);
                } else if tokens
                    .method(&constructor.declaring_type, &constructor.name, &def_key)
                    .is_none()
                {
                    mint_member_ref(constructor, image, tokens);
                }
            }
        }
        BoundExprKind::DelegateCreation {
            delegate_type,
            target,
            receiver,
        } => {
            if let Some(receiver) = receiver {
                mint_in_expr(receiver, image, tokens);
            }
            if tokens
                .method(&target.declaring_type, &target.name, &target.parameters)
                .is_none()
            {
                mint_member_ref(target, image, tokens);
            }
            if tokens.method(delegate_type, ".ctor", &[]).is_none() {
                let ctor_sig =
                    method_signature(true, &[TypeSig::Object, TypeSig::NativeInt], &TypeSig::Void);
                let parent = match delegate_type {
                    TypeSymbol::Instantiation {
                        definition,
                        arguments,
                    } => mint_type_spec(delegate_type, definition, arguments, image, tokens),
                    _ => split_type_name(delegate_type).map(|(namespace, name)| {
                        mint_named_type_token(delegate_type, image, tokens);
                        image.type_ref(&namespace, &name)
                    }),
                };
                if let Some(parent) = parent {
                    let ctor = image.member_ref(parent, ".ctor", &ctor_sig);
                    tokens.insert_method(delegate_type, ".ctor", &[], ctor);
                }
            }
        }
        BoundExprKind::FieldAccess {
            receiver, field, ..
        } => {
            mint_in_expr(receiver, image, tokens);
            if let Some(field) = field {
                if field.constant.is_none()
                    && tokens.field(&field.declaring_type, &field.name).is_none()
                {
                    mint_field_ref(field, image, tokens);
                }
                if let Some(Literal::String(text)) = &field.constant {
                    let token = image.user_string(text);
                    tokens.insert_string(text, token);
                }
                if matches!(&field.constant, Some(Literal::Decimal { .. })) {
                    mint_decimal_ctor(image, tokens);
                }
            }
        }
        BoundExprKind::MethodGroup { receiver, .. } => mint_in_expr(receiver, image, tokens),
        BoundExprKind::PropertyAccess {
            receiver,
            declaring_type,
            getter_instantiation,
            name,
            ..
        } => {
            mint_in_expr(receiver, image, tokens);
            let getter = lamella_binder::MethodReference {
                declaring_type: declaring_type.clone(),
                name: accessor_name("get_", name).into(),
                parameters: Vec::new(),
                return_type: expr.ty.clone(),
                is_static: matches!(receiver.kind, BoundExprKind::TypeReference(_)),
                is_vararg: false,
                instantiation: None,
                declaring_instantiation: getter_instantiation.clone(),
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
            mint_type_token(image, tokens, &expr.ty);
            if matches!(receiver.ty, TypeSymbol::Special(SpecialType::String)) {
                let getter = lamella_binder::MethodReference {
                    declaring_type: TypeSymbol::Special(SpecialType::String),
                    name: "get_Chars".into(),
                    parameters: alloc::vec![TypeSymbol::Special(SpecialType::Int32)],
                    return_type: TypeSymbol::Special(SpecialType::Char),
                    is_static: false,
                    is_vararg: false,
                    instantiation: None,
                    declaring_instantiation: None,
                };
                if tokens
                    .method(&getter.declaring_type, &getter.name, &getter.parameters)
                    .is_none()
                {
                    mint_member_ref(&getter, image, tokens);
                }
            }
        }
        BoundExprKind::IndexerAccess {
            receiver,
            indices,
            setter,
        } => {
            mint_in_expr(receiver, image, tokens);
            for index in indices {
                mint_in_expr(index, image, tokens);
            }
            if tokens
                .method(&setter.declaring_type, &setter.name, &setter.parameters)
                .is_none()
            {
                mint_member_ref(setter, image, tokens);
            }
        }
        BoundExprKind::Assignment {
            target,
            value,
            operator,
            ..
        } => {
            mint_in_expr(target, image, tokens);
            let spliced = matches!(operator, lamella_syntax::ast::AssignmentOperator::Add)
                && matches!(
                    target.ty,
                    TypeSymbol::Special(SpecialType::String | SpecialType::Object)
                )
                && matches!(
                    crate::expr::compound_concat(&target.ty, Some(value)),
                    crate::expr::CompoundConcat::Spliced { .. }
                );
            if !spliced {
                mint_in_expr(value, image, tokens);
            }
            if let BoundExprKind::PropertyAccess {
                receiver,
                setter_declaring_type,
                setter_instantiation,
                name,
                ..
            } = &target.kind
            {
                let setter = lamella_binder::MethodReference {
                    declaring_type: setter_declaring_type.clone(),
                    name: accessor_name("set_", name).into(),
                    parameters: alloc::vec![target.ty.clone()],
                    return_type: TypeSymbol::Special(SpecialType::Void),
                    is_static: matches!(receiver.kind, BoundExprKind::TypeReference(_)),
                    is_vararg: false,
                    instantiation: None,
                    declaring_instantiation: setter_instantiation.clone(),
                };
                if tokens
                    .method(&setter.declaring_type, &setter.name, &setter.parameters)
                    .is_none()
                {
                    mint_member_ref(&setter, image, tokens);
                }
            }
            if matches!(operator, lamella_syntax::ast::AssignmentOperator::Add)
                && matches!(
                    target.ty,
                    TypeSymbol::Special(SpecialType::String | SpecialType::Object)
                )
            {
                match crate::expr::compound_concat(&target.ty, Some(value)) {
                    crate::expr::CompoundConcat::Spliced {
                        reference,
                        arguments,
                    } => {
                        for argument in &arguments {
                            mint_in_expr(argument, image, tokens);
                        }
                        mint_member_ref(&reference, image, tokens);
                    }
                    crate::expr::CompoundConcat::Pairwise(reference) => {
                        mint_member_ref(&reference, image, tokens);
                    }
                }
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
        BoundExprKind::NullCoalescing { left, right } => {
            mint_in_expr(left, image, tokens);
            mint_in_expr(right, image, tokens);
        }
        BoundExprKind::Sequence { spilled, value } => {
            for operand in spilled {
                mint_in_expr(operand, image, tokens);
            }
            mint_in_expr(value, image, tokens);
        }
        BoundExprKind::TypeTest { operand, target, .. } => {
            mint_in_expr(operand, image, tokens);
            if is_value_type(&operand.ty, tokens) {
                mint_value_type_token(&operand.ty, image, tokens);
            }
            mint_type_token(image, tokens, target);
        }
        BoundExprKind::Throw(operand) => mint_in_expr(operand, image, tokens),
        BoundExprKind::TypeOf(target) => {
            mint_type_token(image, tokens, target);
            mint_type_token(image, tokens, &crate::expr::system_type_symbol());
            let handle = crate::expr::runtime_type_handle_symbol();
            mint_type_token(image, tokens, &handle);
            tokens.insert_struct(&handle);
            mint_member_ref(&get_type_from_handle_reference(), image, tokens);
        }
        BoundExprKind::MakeRef(operand) => {
            mint_in_expr(operand, image, tokens);
            mint_type_token(image, tokens, &operand.ty);
        }
        BoundExprKind::RefType(reference) => {
            mint_in_expr(reference, image, tokens);
            mint_type_token(image, tokens, &crate::expr::system_type_symbol());
            let handle = crate::expr::runtime_type_handle_symbol();
            mint_type_token(image, tokens, &handle);
            tokens.insert_struct(&handle);
            mint_member_ref(&get_type_from_handle_reference(), image, tokens);
        }
        BoundExprKind::RefValue { reference, target } => {
            mint_in_expr(reference, image, tokens);
            mint_type_token(image, tokens, target);
        }
        BoundExprKind::ArgListLiteral(elements) => {
            for element in elements {
                mint_in_expr(element, image, tokens);
            }
        }
        BoundExprKind::SizeOf(target) => {
            mint_named_type_token(target, image, tokens);
        }
        BoundExprKind::DefaultValue(target) => {
            mint_type_token(image, tokens, target);
        }
        BoundExprKind::Dereference { operand } | BoundExprKind::AddressOf { operand } => {
            mint_in_expr(operand, image, tokens);
        }
        _ => {}
    }
}

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
        is_vararg: false,
        instantiation: None,
        declaring_instantiation: None,
    }
}

/// `RuntimeHelpers.get_OffsetToStringData()` -- the byte offset from a string reference to its first
/// UTF-16 character, which `fixed (char* p = str)` (18.6) adds to the pinned string's address.
pub(crate) fn offset_to_string_data_reference() -> lamella_binder::MethodReference {
    lamella_binder::MethodReference {
        declaring_type: named_symbol("System.Runtime.CompilerServices", "RuntimeHelpers"),
        name: "get_OffsetToStringData".into(),
        parameters: alloc::vec![],
        return_type: TypeSymbol::Special(SpecialType::Int32),
        is_static: true,
        is_vararg: false,
        instantiation: None,
        declaring_instantiation: None,
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
        is_vararg: false,
        instantiation: None,
        declaring_instantiation: None,
    }
}

/// Mints a `MemberRef` for an external (BCL) method `method`: a `TypeRef` to its
/// declaring type, then a `MemberRef` with its encoded signature, recorded in the
/// token table. Skipped (left for emission to report) if a type cannot be encoded.
/// Mints the tokens an OBJECT or COLLECTION INITIALIZER needs (C# 3.0 7.6.10.2/3), which the
/// expression walk cannot reach: an initializer is not an expression, it hangs off one.
///
/// **A VALUE IN AN INITIALIZER IS AN ORDINARY EXPRESSION AND NEEDS EXACTLY WHAT ONE ANYWHERE ELSE
/// NEEDS.** Skip the walk and the refusals that follow are clean, specific and wrong: a string
/// literal reports that it was never interned, a `new` reports a constructor outside this module.
/// Each names a real precondition, and each rejects a program the rest of the feature builds.
///
/// The TARGETS need tokens too, and for the same reason a plain assignment's do: a field on an
/// external type is a `FieldRef` and a property is a `MemberRef` to its `set_`/`get_` accessor.
/// A member this module declares already has one from the pre-pass and is skipped.
///
/// ONE SHAPE IS NOT REACHED FROM HERE: a collection initializer whose `Add` is declared by an
/// INSTANTIATED external type (`new List<string> { "a" }`). That row's parent is a `TypeSpec` and
/// its signature is the DEFINITION's, and the definition cannot be recovered from the substituted
/// one -- see [`lamella_binder::MethodInstantiation`], *substitution is not invertible*. The open
/// signature has to arrive from the binder, as a call's does; the emit site refuses by name until
/// it can.
fn mint_in_initializer(
    initializer: &lamella_binder::BoundInitializer,
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
) {
    match initializer {
        lamella_binder::BoundInitializer::Collection(elements) => {
            for element in elements {
                mint_in_expr(element, image, tokens);
            }
        }
        lamella_binder::BoundInitializer::Object(members) => {
            for member in members {
                match &member.value {
                    lamella_binder::BoundMemberInitializerValue::Expression(value) => {
                        mint_in_expr(value, image, tokens);
                    }
                    lamella_binder::BoundMemberInitializerValue::Nested(nested) => {
                        mint_in_initializer(nested, image, tokens);
                    }
                }
                match &member.target {
                    lamella_binder::BoundInitializerTarget::Field(field) => {
                        if field.constant.is_none()
                            && tokens.field(&field.declaring_type, &field.name).is_none()
                        {
                            mint_field_ref(field, image, tokens);
                        }
                    }
                    lamella_binder::BoundInitializerTarget::Property {
                        setter_declaring_type,
                        ty,
                    } => {
                        let nested = matches!(
                            member.value,
                            lamella_binder::BoundMemberInitializerValue::Nested(_)
                        );
                        let (prefix, parameters, return_type) = if nested {
                            ("get_", Vec::new(), ty.clone())
                        } else {
                            (
                                "set_",
                                alloc::vec![ty.clone()],
                                TypeSymbol::Special(SpecialType::Void),
                            )
                        };
                        let accessor = lamella_binder::MethodReference {
                            declaring_type: setter_declaring_type.clone(),
                            name: accessor_name(prefix, &member.name).into(),
                            parameters,
                            return_type,
                            is_static: false,
                            is_vararg: false,
                            instantiation: None,
                            declaring_instantiation: None,
                        };
                        if tokens
                            .method(&accessor.declaring_type, &accessor.name, &accessor.parameters)
                            .is_none()
                        {
                            mint_member_ref(&accessor, image, tokens);
                        }
                    }
                    lamella_binder::BoundInitializerTarget::Unresolved => {}
                }
            }
        }
    }
}

fn mint_member_ref(
    method: &lamella_binder::MethodReference,
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
) {
    if let Some(declaring) = method.declaring_instantiation.as_deref() {
        mint_instantiated_member_ref(method, declaring, image, tokens);
        return;
    }
    if tokens
        .type_token(&method.declaring_type)
        .is_some_and(|token| token.table() == TYPE_DEF)
    {
        return;
    }
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
    let signature = if method.is_vararg {
        vararg_method_signature(!method.is_static, &parameter_sigs, &return_sig)
    } else {
        method_signature(!method.is_static, &parameter_sigs, &return_sig)
    };
    let key_params = if method.is_vararg {
        crate::expr::vararg_lookup_params(&method.parameters, &[])
    } else {
        method.parameters.clone()
    };
    let type_ref = image.type_ref(&namespace, &name);
    let member = image.member_ref(type_ref, &method.name, &signature);
    tokens.insert_method(
        &method.declaring_type,
        &crate::tokens::conversion_key_name(&method.name, &method.return_type),
        &key_params,
        member,
    );
}

/// Mints the `MemberRef` naming a member of an INSTANTIATED generic type -- the `.ctor` of
/// `new Box<int>(41)`, the `Get()` of `b.Get()` -- as ECMA-335 4th ed II.23.2.1 requires:
///
/// - the PARENT is a `TypeSpec` (II.23.2.14) whose blob is the `GENERICINST` of the definition
///   over this site's type arguments, so the `<int>` is part of the row rather than lost;
/// - the SIGNATURE is the DEFINITION's, spelling `!n` wherever the declaring type's parameters
///   appear -- `instance void .ctor(!0)`, not the substituted `instance void .ctor(int32)`.
///
/// **NEITHER HALF IS RECOVERABLE FROM THE BOUND CALL ALONE, AND BOTH FAIL SILENTLY.** The
/// substituted signature reads as an ordinary non-generic one, so a `MemberRef` built from it
/// decodes cleanly and describes a method that does not exist; a `TypeRef` parent naming
/// `` Box`1 `` decodes cleanly too and names the open definition. `TypeInstantiation` exists to
/// carry the missing half, exactly as `MethodInstantiation` does one axis over.
///
/// **THE PARENT'S ARGUMENTS ARE CLOSED TYPES AND ARE ENCODED IN AN EMPTY SCOPE, DELIBERATELY.**
/// `Box<int>`'s `<int>` is a type; a `Box<T>` written INSIDE a generic type would need the
/// enclosing type's parameters, which is a different list from the declaring type's and is not in
/// reach here. Encoding it in the declaring scope would silently number it against the wrong list,
/// so that case refuses instead.
///
/// Recorded under the SUBSTITUTED parameter key, because that is what the call site holds and what
/// `emit_call`/`emit_object_creation` look up -- a row minted under any other key is a row nothing
/// finds, which refuses the call rather than emitting it.
fn mint_instantiated_member_ref(
    method: &lamella_binder::MethodReference,
    declaring: &lamella_binder::TypeInstantiation,
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
) {
    let TypeSymbol::Instantiation {
        definition,
        arguments,
    } = &method.declaring_type
    else {
        return;
    };
    let Some(parent) = mint_type_spec(&method.declaring_type, definition, arguments, image, tokens)
    else {
        return;
    };
    for ty in declaring
        .parameters
        .iter()
        .chain(core::iter::once(&declaring.return_type))
    {
        if !mentions_type_parameter(ty, &declaring.type_parameters) {
            mint_named_type_token(ty, image, tokens);
        }
    }
    let scope = GenericScope {
        method: &[],
        declaring: &declaring.type_parameters,
    };
    let parameter_sigs: Result<Vec<TypeSig>, _> = declaring
        .parameters
        .iter()
        .map(|ty| open_type_sig(tokens, ty, scope))
        .collect();
    let (Ok(parameter_sigs), Ok(return_sig)) = (
        parameter_sigs,
        open_type_sig(tokens, &declaring.return_type, scope),
    ) else {
        return;
    };
    let mut return_sig = return_sig;
    for modifier in &declaring.return_required_modifiers {
        mint_named_type_token(modifier, image, tokens);
        let Some(token) = tokens.type_token(modifier) else {
            return;
        };
        return_sig = TypeSig::Modified {
            modifier: token,
            inner: Box::new(return_sig),
        };
    }
    let signature = method_signature(!method.is_static, &parameter_sigs, &return_sig);
    let member = image.member_ref(parent, &method.name, &signature);
    tokens.insert_method(
        &method.declaring_type,
        &crate::tokens::conversion_key_name(&method.name, &method.return_type),
        &method.parameters,
        member,
    );
}

/// Mints the `MemberRef` a NON-empty vararg call site names (II.23.2.1): the parent is
/// the target's own `TypeDef` (a this-module member) or its `TypeRef` (external), and
/// the signature is the CALL-SITE form -- the fixed parameters, a sentinel, then each
/// variable argument's type. Keyed by fixed + `__arglist` marker + the extra types, so
/// same-shaped call sites share one row and emission finds it by identity. (An EMPTY
/// `__arglist()` call names the def token instead, exactly as csc lowers it.)
fn mint_vararg_site_ref(
    method: &lamella_binder::MethodReference,
    extras: &[lamella_binder::BoundExpr],
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
) {
    let key_params = crate::expr::vararg_lookup_params(&method.parameters, extras);
    if tokens
        .method(&method.declaring_type, &method.name, &key_params)
        .is_some()
    {
        return;
    }
    let parent = match tokens.type_token(&method.declaring_type) {
        Some(token) => token,
        None => {
            let Some((namespace, name)) = split_type_name(&method.declaring_type) else {
                return;
            };
            mint_named_type_token(&method.declaring_type, image, tokens);
            image.type_ref(&namespace, &name)
        }
    };
    for parameter in &method.parameters {
        mint_named_type_token(parameter, image, tokens);
    }
    let extra_symbols: Vec<TypeSymbol> =
        extras.iter().map(crate::expr::vararg_extra_symbol).collect();
    for extra in &extra_symbols {
        mint_named_type_token(extra, image, tokens);
    }
    let fixed_sigs: Result<Vec<TypeSig>, _> = method
        .parameters
        .iter()
        .map(|ty| type_sig(tokens, ty))
        .collect();
    let extra_sigs: Result<Vec<TypeSig>, _> =
        extra_symbols.iter().map(|ty| type_sig(tokens, ty)).collect();
    let (Ok(fixed_sigs), Ok(extra_sigs), Ok(return_sig)) = (
        fixed_sigs,
        extra_sigs,
        type_sig(tokens, &method.return_type),
    ) else {
        return;
    };
    let signature =
        vararg_call_site_signature(!method.is_static, &fixed_sigs, &extra_sigs, &return_sig);
    let member = image.member_ref(parent, &method.name, &signature);
    tokens.insert_method(&method.declaring_type, &method.name, &key_params, member);
}

/// Mints the two rows a generic CALL SITE needs and records the second one under the site key:
///
/// 1. the `MemberRef` naming the generic DEFINITION, whose signature is still open (`!!0`), and
/// 2. the `MethodSpec` (II.22.29) that closes it over the type arguments THIS site named.
///
/// **BOTH ROWS ARE REQUIRED AND NEITHER IS SUFFICIENT.** The definition alone is what a call
/// emitted against the open method names -- it binds, and `!!0` is never substituted. The
/// instantiation alone has nothing to instantiate. `emit_call` looks up only the site key, so a
/// site this function declines to mint is REFUSED rather than quietly emitted against the
/// definition (see the lookup there).
///
/// One row per distinct (definition, type arguments) shape: two sites naming `Id<int>` share a
/// row, and `Id<int>` and `Id<string>` never do. II.24.2.6 does not require `MethodSpec` to be
/// sorted or deduplicated, so this is a size choice rather than a correctness rule.
fn mint_generic_call_site(
    method: &lamella_binder::MethodReference,
    instantiation: &lamella_binder::MethodInstantiation,
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
) {
    let site_key =
        crate::expr::generic_site_lookup_params(&instantiation.parameters, &instantiation.arguments);
    if tokens
        .method(&method.declaring_type, &method.name, &site_key)
        .is_some()
    {
        return;
    }
    let Some(definition) = mint_generic_definition_ref(method, instantiation, image, tokens) else {
        return;
    };
    for argument in &instantiation.arguments {
        mint_named_type_token(argument, image, tokens);
    }
    let scope = body_scope(tokens);
    let arguments: Result<Vec<TypeSig>, _> = instantiation
        .arguments
        .iter()
        .map(|ty| open_type_sig(tokens, ty, scope))
        .collect();
    let Ok(arguments) = arguments else {
        return;
    };
    let spec = image.method_spec(definition, &method_spec_signature(&arguments));
    tokens.insert_method(&method.declaring_type, &method.name, &site_key, spec);
}

/// The token naming the generic DEFINITION a `MethodSpec` instantiates: a `MemberRef` carrying the
/// GENERIC calling convention, its `GenParamCount`, and `!!n` wherever the open signature mentions
/// one of the method's own type parameters. Recorded under the definition's OPEN parameter key, so
/// several sites over one method mint it once.
///
/// **A METHOD DECLARED IN THIS MODULE IS REFUSED HERE, ON PURPOSE.** The token pre-pass writes
/// every `MethodDef` with an ordinary DEFAULT signature and emits no `GenericParam` rows, because
/// `Feature::Generics` refuses a generic DECLARATION and there has never been one to write. Taking
/// that token would put a `MethodSpec` over a method whose own signature says it is not generic --
/// metadata that contradicts itself, and `generic_method_signature`'s doc records what that
/// produces (csc answers CS0308 at a call site that otherwise compiles). Refusing costs a refused
/// call, which is what the declaration gate already answers with anyway.
///
/// **Its precondition is that no `MethodDef` this module writes carries a GENERIC signature.** A
/// build that writes them -- with their `GenericParam` rows -- makes this guard wrong, and removing
/// it belongs to that change.
fn mint_generic_definition_ref(
    method: &lamella_binder::MethodReference,
    instantiation: &lamella_binder::MethodInstantiation,
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
) -> Option<Token> {
    if let Some(token) = tokens.type_token(&method.declaring_type)
        && token.table() == TYPE_DEF
    {
        return tokens.method(
            &method.declaring_type,
            &method.name,
            &instantiation.parameters,
        );
    }
    if method.is_vararg {
        return None;
    }
    if let Some(token) = tokens.method(
        &method.declaring_type,
        &method.name,
        &instantiation.parameters,
    ) {
        return Some(token);
    }
    if let Some(declaring) = method.declaring_instantiation.as_deref() {
        return mint_instantiated_generic_definition_ref(
            method,
            instantiation,
            declaring,
            image,
            tokens,
        );
    }
    let (namespace, name) = split_type_name(&method.declaring_type)?;
    mint_named_type_token(&method.declaring_type, image, tokens);
    for ty in instantiation
        .parameters
        .iter()
        .chain(core::iter::once(&instantiation.return_type))
    {
        if !mentions_type_parameter(ty, &instantiation.type_parameters) {
            mint_named_type_token(ty, image, tokens);
        }
    }
    let parameter_sigs: Result<Vec<TypeSig>, _> = instantiation
        .parameters
        .iter()
        .map(|ty| open_type_sig(tokens, ty, method_scope(instantiation)))
        .collect();
    let (Ok(parameter_sigs), Ok(return_sig)) = (
        parameter_sigs,
        open_type_sig(tokens, &instantiation.return_type, method_scope(instantiation)),
    ) else {
        return None;
    };
    let signature = generic_method_signature(
        !method.is_static,
        instantiation.type_parameters.len() as u32,
        &parameter_sigs,
        &return_sig,
    );
    let type_ref = image.type_ref(&namespace, &name);
    let member = image.member_ref(type_ref, &method.name, &signature);
    tokens.insert_method(
        &method.declaring_type,
        &method.name,
        &instantiation.parameters,
        member,
    );
    Some(member)
}

/// The `MemberRef` naming a generic METHOD declared on an INSTANTIATED generic type -- the
/// `Second<TM>` of a `Holder<int>` -- whose parent is the declaring instantiation's `TypeSpec` and
/// whose signature is the DEFINITION's, open over BOTH numbering spaces at once.
///
/// **THIS IS THE ONE SIGNATURE IN THE EMITTER WHERE `!n` AND `!!n` ARE BOTH LIVE**, and the two
/// sibling paths each carry exactly one of them: [`mint_instantiated_member_ref`] writes `!n` with
/// an empty method list, and the external arm of [`mint_generic_definition_ref`] writes `!!n`
/// through [`method_scope`], whose declaring half is empty *because* it was written when the only
/// generic call this stage emitted was on a non-generic type. Neither could name
/// `Box<TOuter> M<TMethod>(Box<TMethod>)`, and the failure is silent in the direction that matters:
/// a scope missing one list does not refuse, it numbers that space's names against the other list
/// or falls through to a class lookup -- a signature that decodes cleanly and names a different
/// type (`GenericScope`'s own doc gives csc's bytes for exactly this shape).
///
/// **THE OPEN SIGNATURE COMES FROM THE DECLARING INSTANTIATION, NOT FROM THE METHOD ONE.**
/// `MethodInstantiation::parameters` is open over the METHOD's parameters and already CLOSED over
/// the type's -- it is what the call site resolved against -- so writing it here would put `int`
/// where II.23.2.1 requires `!0`, on a `MemberRef` whose parent already carries that `int`. The
/// arguments would then be applied twice by any reader that substitutes.
fn mint_instantiated_generic_definition_ref(
    method: &lamella_binder::MethodReference,
    instantiation: &lamella_binder::MethodInstantiation,
    declaring: &lamella_binder::TypeInstantiation,
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
) -> Option<Token> {
    let TypeSymbol::Instantiation {
        definition,
        arguments,
    } = &method.declaring_type
    else {
        return None;
    };
    let parent = mint_type_spec(&method.declaring_type, definition, arguments, image, tokens)?;
    let scope = GenericScope {
        method: &instantiation.type_parameters,
        declaring: &declaring.type_parameters,
    };
    let open_names: Vec<Box<str>> = declaring
        .type_parameters
        .iter()
        .chain(instantiation.type_parameters.iter())
        .cloned()
        .collect();
    for ty in declaring
        .parameters
        .iter()
        .chain(core::iter::once(&declaring.return_type))
    {
        if !mentions_type_parameter(ty, &open_names) {
            mint_named_type_token(ty, image, tokens);
        }
    }
    let parameter_sigs: Result<Vec<TypeSig>, _> = declaring
        .parameters
        .iter()
        .map(|ty| open_type_sig(tokens, ty, scope))
        .collect();
    let (Ok(parameter_sigs), Ok(return_sig)) = (
        parameter_sigs,
        open_type_sig(tokens, &declaring.return_type, scope),
    ) else {
        return None;
    };
    let signature = generic_method_signature(
        !method.is_static,
        instantiation.type_parameters.len() as u32,
        &parameter_sigs,
        &return_sig,
    );
    let member = image.member_ref(parent, &method.name, &signature);
    tokens.insert_method(
        &method.declaring_type,
        &method.name,
        &instantiation.parameters,
        member,
    );
    Some(member)
}

/// Mints a `MemberRef` (a FieldRef) for a field on a type outside this module -- the
/// persistent REPL `__Repl` (a session variable) or a BCL field -- so emission can name
/// it. Mirrors [`mint_member_ref`]: the declaring type and the field's own type are
/// tokenized first (the latter so its signature encodes), then a `MemberRef` carrying a
/// FIELD signature is recorded under the field's identity. The declaring type's `TypeRef`
/// is reused as the member's parent. A no-op if the declaring type or the field type
/// cannot be tokenized.
fn mint_field_ref(field: &FieldReference, image: &mut ImageBuilder, tokens: &mut Tokens) {
    if let Some(declaring) = field.declaring_instantiation.as_deref() {
        mint_instantiated_field_ref(field, declaring, image, tokens);
        return;
    }
    mint_named_type_token(&field.declaring_type, image, tokens);
    mint_named_type_token(&field.ty, image, tokens);
    let Some(parent) = tokens.type_token(&field.declaring_type) else {
        return;
    };
    let Ok(field_sig) = type_sig(tokens, &field.ty) else {
        return;
    };
    let signature = field_signature(&field_sig);
    let member = image.member_ref(parent, &field.name, &signature);
    tokens.insert_field(&field.declaring_type, &field.name, member);
}

/// Whether `ty` must be named by a `TypeSpec` rather than by an ordinary type token: it IS, or it
/// structurally CONTAINS, a constructed generic type or a body type parameter.
///
/// **The question is about the leaf, not the wrapper.** `Box<T>[]`, `Box<int>[][]` and `T[]` all
/// need a spec; `int[]` does not, and keeping it out is what stops this from moving every
/// array-typed instruction operand in the corpus.
pub(crate) fn requires_type_spec(tokens: &Tokens, ty: &TypeSymbol) -> bool {
    match ty {
        TypeSymbol::Instantiation { .. } => true,
        TypeSymbol::Array { element, .. }
        | TypeSymbol::Pointer(element)
        | TypeSymbol::ByRef(element) => requires_type_spec(tokens, element),
        TypeSymbol::Named(_) => tokens.body_type_parameter(ty).is_some(),
        _ => false,
    }
}

/// Mints the `TypeSpec` row for a COMPOSITE over a constructed type or a type parameter --
/// `Box<T>[]` -- together with the ordinary rows its blob refers to.
///
/// **The element's own tokens are minted FIRST and that is load-bearing.** The outer blob encodes
/// the element inline, and a `Box<T>[]` whose `` Box`1 `` has no `TypeRef` row encodes a token
/// nothing resolves -- metadata that writes cleanly and fails at load.
fn mint_composite_type_spec(
    ty: &TypeSymbol,
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
) -> Option<Token> {
    match ty {
        TypeSymbol::Instantiation {
            definition,
            arguments,
        } => return mint_type_spec(ty, definition, arguments, image, tokens),
        TypeSymbol::Array { element, .. }
        | TypeSymbol::Pointer(element)
        | TypeSymbol::ByRef(element) => {
            if tokens.body_type_parameter(element).is_none() {
                mint_composite_type_spec(element, image, tokens);
            }
        }
        _ => {}
    }
    let blob = instantiation_blob(tokens, ty)?;
    if let Some(existing) = tokens.type_spec_for(&blob) {
        return Some(existing);
    }
    let token = image.type_spec(&blob);
    tokens.insert_type_spec_for(&blob, token);
    Some(token)
}

/// The `TypeSpec` row (II.23.2.14) naming a constructed generic type, minted once and shared by
/// every member named through it -- the parent both `mint_instantiated_member_ref` and
/// `mint_instantiated_field_ref` attach to.
///
/// The definition's own token and each argument's must exist before the `GENERICINST` blob can name
/// them, which is why the minting happens here rather than at the call sites.
///
/// **THE ARGUMENTS ARE ENCODED IN THE SCOPE OF THE BODY BEING WALKED, WHICH IS A POSITION AND NOT
/// A NAME.** `Box<int>`'s `<int>` is a closed type and encodes the same anywhere; a `Box<T>` is
/// `Box<!!0>` inside `T Unwrap<T>(Box<T>)` and `Box<!0>` inside a `class Outer<T>`, and the two are
/// different rows. The scope arrives ambiently on `tokens` ([`Tokens::body_scope`]) rather than as
/// a parameter, because this function is one of about twenty the minting walk reaches, and a policy
/// handed to twenty sites is a policy that arrives at some of them.
///
/// **A NAME-BASED GUARD -- refuse to mint a bare untokenized name -- IS NOT THE FIX**, and it is
/// worth saying so because it is the obvious one. It breaks a `T[]` local inside a generic type
/// (whose `T` encodes as `!0` and needs no token) and a REPL session's global-namespace type;
/// narrowing it to "not one of the DECLARING type's parameters" fails differently, because the
/// method's `T` and `` Box`1 ``'s `T` are DIFFERENT parameters with the SAME NAME, so a name test
/// encodes `Box<!0>` -- a second silent wrong answer in place of the first. **Only the POSITION
/// separates them.**
fn mint_type_spec(
    instantiation: &TypeSymbol,
    definition: &[Box<str>],
    arguments: &[TypeSymbol],
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
) -> Option<Token> {
    mint_named_type_token(&definition_symbol(definition, arguments.len()), image, tokens);
    for argument in arguments {
        mint_named_type_token(argument, image, tokens);
    }
    let blob = instantiation_blob(tokens, instantiation)?;
    if let Some(existing) = tokens.type_spec_for(&blob) {
        return Some(existing);
    }
    let token = image.type_spec(&blob);
    tokens.insert_type_spec_for(&blob, token);
    Some(token)
}

/// The `TypeSpec` signature bytes naming a constructed generic type in the scope of the body being
/// emitted -- the one encoder both [`mint_type_spec`] and [`instantiation_spec`] key through.
///
/// **THE MINT AND THE LOOK-UP MUST ENCODE IDENTICALLY OR THE MAP IS WRITE-ONLY**, and they are two
/// functions in two modules that run in two different walks. Sharing the encoder is what makes
/// "same type, same key" true by construction rather than by two sites agreeing; a lookup that
/// encoded in an empty scope would simply miss every open row and refuse, which reads as an
/// unimplemented feature rather than as a mismatch.
fn instantiation_blob(tokens: &Tokens, instantiation: &TypeSymbol) -> Option<Vec<u8>> {
    let signature = open_type_sig(tokens, instantiation, body_scope(tokens)).ok()?;
    Some(type_signature(&signature))
}

/// The `TypeSpec` row naming a constructed generic type where an INSTRUCTION operand needs a token
/// -- `newarr Box<!!0>`, `castclass Box<!0>`, `isinst`, `ldtoken`. The read half of
/// [`mint_type_spec`], which the minting walk has already run for the same body.
///
/// **AN INSTANTIATION HAS NO ORDINARY TOKEN AND MUST NOT BE GIVEN ONE.** [`Tokens::type_token`]
/// keys by a type's `Display`, and `Box<T>` written against a method's `T` and against its
/// declaring type's `T` are ONE display string and TWO rows (`Box<!!0>` and `Box<!0>`). Answering
/// an instruction from that map hands one of them the other's token -- metadata that decodes
/// cleanly and names a different type. So the answer comes from the blob-keyed map or not at all.
///
/// **AND A COMPOSITE OVER ONE IS THE SAME QUESTION.** Answering `Box<T>` here while `Box<T>[]`
/// reaches this map through nothing refuses `typeof(Box<T>[])` and `((Box<T>[])o).Length` while
/// the element type they are built from works. The shape
/// asked for is not "an instantiation" but "a type that cannot be named by an ordinary token",
/// which is what [`requires_type_spec`] decides.
///
/// A CLOSED array like `int[]` deliberately does NOT come here. It has an ordinary token today,
/// and routing it through the spec map would move the row for every array-typed operand in every
/// program -- a far wider change than the defect, and one whose blast radius is the whole corpus
/// rather than generic code.
///
/// Returns `None` for every other shape, so [`Tokens::instruction_type_token`] can chain it.
pub(crate) fn structural_type_spec(tokens: &Tokens, ty: &TypeSymbol) -> Option<Token> {
    if !requires_type_spec(tokens, ty) {
        return None;
    }
    tokens.type_spec_for(&instantiation_blob(tokens, ty)?)
}

/// The generic scope of the body being emitted, as [`GenericScope`] -- the one place the ambient
/// pair on `tokens` is turned into the scope a signature is written in, so no site can consult one
/// half of it.
fn body_scope(tokens: &Tokens) -> GenericScope<'_> {
    let (method, declaring) = tokens.body_scope();
    GenericScope { method, declaring }
}

/// Mints the `MemberRef` naming a FIELD of an INSTANTIATED generic type -- `Counter<int>.Total` --
/// with a `TypeSpec` parent carrying the arguments and the DEFINITION's field signature
/// (ECMA-335 4th ed II.23.2.1, and II.9.7 for why the parent decides storage).
///
/// **THIS ONE CORRUPTS DATA RATHER THAN METADATA, WHICH IS WHAT MAKES IT THE WORST OF THEM.**
/// A static field gets one copy PER INSTANTIATION. With the parent erased, `Counter<int>.Total` and
/// `Counter<string>.Total` name the definition's single `FieldDef` and share one cell: measured on
/// the interpreter tier as a program answering 503503 where 10507 is correct, with ZERO violations
/// reported -- because an erased use site is indistinguishable from non-generic code, so every
/// protection built for generics keys on a `TypeSpec` that is not there.
///
/// **A FIELD WHOSE TYPE NEVER MENTIONS `T` IS THE DANGEROUS CASE, NOT THE EASY ONE.**
/// `Counter<T> { static int Total; }` has a signature that is byte-identical open and closed, so
/// nothing in the SIGNATURE can tell you whether the instantiation was carried. Only the parent row
/// says, which is exactly why the defect ran silently.
fn mint_instantiated_field_ref(
    field: &FieldReference,
    declaring: &lamella_binder::FieldInstantiation,
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
) {
    let TypeSymbol::Instantiation {
        definition,
        arguments,
    } = &field.declaring_type
    else {
        return;
    };
    let Some(parent) = mint_type_spec(&field.declaring_type, definition, arguments, image, tokens)
    else {
        return;
    };
    if !mentions_type_parameter(&declaring.ty, &declaring.type_parameters) {
        mint_named_type_token(&declaring.ty, image, tokens);
    }
    let scope = GenericScope {
        method: &[],
        declaring: &declaring.type_parameters,
    };
    let Ok(field_sig) = open_type_sig(tokens, &declaring.ty, scope) else {
        return;
    };
    let signature = field_signature(&field_sig);
    let member = image.member_ref(parent, &field.name, &signature);
    tokens.insert_field(&field.declaring_type, &field.name, member);
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
    let address_sig =
        method_signature(true, &int_sigs, &TypeSig::ByRef(Box::new(element_sig.clone())));
    let address = image.member_ref(type_spec, "Address", &address_sig);
    tokens.insert_method(array_ty, "Address", &int_params, address);
    let mut set_sigs = int_sigs;
    set_sigs.push(element_sig);
    let set_sig = method_signature(true, &set_sigs, &TypeSig::Void);
    let set = image.member_ref(type_spec, "Set", &set_sig);
    let mut set_params = int_params;
    set_params.push((**element).clone());
    tokens.insert_method(array_ty, "Set", &set_params, set);
}

fn mint_type_token(image: &mut ImageBuilder, tokens: &mut Tokens, ty: &TypeSymbol) {
    if tokens.body_type_parameter(ty).is_some() {
        return;
    }
    let canonical = tokens.canonical(ty);
    let ty = &canonical;
    if requires_type_spec(tokens, ty) {
        mint_composite_type_spec(ty, image, tokens);
        return;
    }
    if tokens.type_token(ty).is_some() {
        return;
    }
    if crate::expr::is_typed_reference(ty) {
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
        TypeSymbol::ByRef(element) => {
            mint_type_token(image, tokens, element);
            None
        }
        TypeSymbol::Instantiation { .. } => None,
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
        SpecialType::Void => ("System", "Void"),
        SpecialType::Null => return None,
    })
}

/// Records where every external type LIVES, so the `TypeRef` the writer mints for it names the
/// right place: the assembly that defines it (System.Diagnostics for Trace, not mscorlib, which
/// resolves only what CoreLib defines or forwards), and -- for a nested type -- the type it is
/// nested in.
///
/// **BOTH FACTS ARE THE SAME WALK AND ARE RECORDED IN THE SAME PASS.** The enclosing name sits on
/// the same `TypeInfo` as the assembly name, and the writer needs it for exactly the same reason:
/// a `TypeRef` written without it resolves to something else or to nothing. Splitting them would
/// mean two walks of every model type that must agree on the key they record under.
fn register_external_type_scopes(model: &Model, image: &mut ImageBuilder) {
    let entries: Vec<(String, Box<str>, Option<Box<str>>)> = model
        .type_keys()
        .filter_map(|(namespace, name)| {
            let info = model.get_by_symbol(&named_symbol(namespace, name))?;
            let assembly = info.assembly.clone()?;
            let qualified = if namespace.is_empty() {
                String::from(name)
            } else {
                alloc::format!("{namespace}.{name}")
            };
            Some((qualified, assembly, info.enclosing.clone()))
        })
        .collect();
    for (qualified, assembly, enclosing) in entries {
        image.set_type_assembly(&qualified, &assembly);
        if let Some(enclosing) = enclosing {
            image.set_type_enclosing(&qualified, &enclosing);
        }
    }
}

/// Records each reference assembly's real identity (name -> version + full public key) in the
/// image, so an `AssemblyRef` emitted for it carries that identity rather than a
/// `Version=4.0.0.0, PublicKeyToken=null` default. Without this, csc consuming an lcsc-built
/// library alongside the same reference pack rejects it -- the `System.Runtime` reference names a
/// phantom assembly whose identity matches nothing it has (CS0012). The reference pack is the
/// single source of truth: we forward exactly what it declares.
fn register_assembly_identities(references: &[Assembly], image: &mut ImageBuilder) {
    for reference in references {
        if let Some(name) = reference.assembly_name() {
            image.set_assembly_identity(
                name,
                reference.assembly_version(),
                reference.assembly_public_key(),
            );
        }
    }
}

/// Marks every referenced struct/enum as a value type in `tokens`, so `type_sig` emits it as
/// `ValueType` rather than `Class` (a class reference to a value type is a load-time mismatch).
/// This-module structs/enums are already marked by the token pre-pass. The `System` built-ins
/// (which fold to their special forms) stay unmarked: they have dedicated primitive emission
/// (`ldind`/`stind`, `ldelem`/`stelem`), not the tokened value-type paths.
fn mark_external_value_types(model: &Model, tokens: &mut Tokens) {
    let value_types: Vec<(TypeSymbol, lamella_binder::TypeKind)> = model
        .type_keys()
        .map(|(namespace, name)| named_symbol(namespace, name))
        .filter(|symbol| !matches!(symbol, TypeSymbol::Special(_)))
        .filter_map(|symbol| {
            let info = model.get_by_symbol(&symbol)?;
            info.is_external.then_some((symbol, info.kind))
        })
        .collect();
    for (symbol, kind) in value_types {
        if is_reference_base_class(&symbol) {
            continue;
        }
        match kind {
            lamella_binder::TypeKind::Struct => tokens.insert_struct(&symbol),
            lamella_binder::TypeKind::Enum => {
                tokens.insert_enum(&symbol);
                tokens.insert_enum_underlying(&symbol, enum_underlying(model, &symbol));
            }
            _ => {}
        }
    }
}

fn mint_named_type_token(ty: &TypeSymbol, image: &mut ImageBuilder, tokens: &mut Tokens) {
    if tokens.body_type_parameter(ty).is_some() {
        return;
    }
    if matches!(ty, TypeSymbol::Instantiation { .. }) {
        mint_type_token(image, tokens, ty);
        return;
    }
    if let TypeSymbol::Array { element, .. }
    | TypeSymbol::Pointer(element)
    | TypeSymbol::ByRef(element) = ty
    {
        mint_named_type_token(element, image, tokens);
        return;
    }
    let needs_ref = matches!(
        ty,
        TypeSymbol::Named(_) | TypeSymbol::Special(SpecialType::Decimal)
    );
    if !needs_ref || tokens.type_token(ty).is_some() {
        return;
    }
    if let Some((namespace, name)) = split_type_name(ty) {
        let token = image.type_ref(&namespace, &name);
        tokens.insert_type(ty, token);
    }
}

/// Mints the external `TypeRef` for a syntactic signature type name -- resolved using-aware
/// through the binder (`Type` with `using System;` -> `System.Type`) -- and inserts it under
/// the SYNTACTIC key, so `type_sig` (which keys on `bind_type`) finds it. A this-module type
/// already has its TypeDef (the guard skips it); primitives/arrays/pointers `type_sig` builds
/// directly.
/// The type-parameter names in scope for a signature: the declaring type's and, for a method, its
/// own. A slice of slices so the two numbering spaces stay separate lists and neither has to be
/// copied to be searched together.
type ParameterScope<'a> = &'a [&'a [Box<str>]];

/// Whether `ty` IS one of the type parameters in scope -- a single-part name matching one of them.
///
/// **THE LEAF, NOT THE COMPOSITE, AND THAT DISTINCTION IS THE WHOLE OF IT.** `IEnumerator<T>`
/// MENTIONS `T` and is not itself a parameter: its definition is an ordinary imported type that
/// needs a `TypeRef` like any other, and only the argument is a position. Asking "does this
/// composite mention a parameter" and skipping the whole thing is what refused every generic class
/// implementing an imported generic interface -- the shape of every collection.
fn is_type_parameter(ty: &TypeSymbol, scope: ParameterScope<'_>) -> bool {
    let TypeSymbol::Named(parts) = ty else {
        return false;
    };
    matches!(&parts[..], [only] if scope.iter().any(|names| names.iter().any(|name| name == only)))
}

/// The type parameters declared by the ENCLOSING types of `ty`, walking the whole chain outward --
/// `T` for anything declared inside `class Box<T>`, however deeply nested.
///
/// A nested type sees these names lexically (they are in scope through its whole body) while
/// declaring none of them itself, so it is the one context where a name can be a type parameter of
/// the program and not a parameter of the declaration being emitted. Every caller wants that
/// distinction; none of them wants to walk the chain.
/// A type's parameter names in the order their `!n` positions count from: the ENCLOSING chain's
/// first, then the ones this declaration writes.
///
/// **ECMA-335 II.9.2: A NESTED TYPE HAS THE TYPE PARAMETERS OF EVERY TYPE IT IS NESTED IN.** So
/// `struct Cursor` inside `class Box<T>` is a generic type of arity 1 whose parameter is `T`, and
/// `T` written in one of its members is `!0` OF CURSOR. Numbering it against Cursor's own list --
/// which is empty -- finds no position for it at all.
///
/// The name keeps only the arity the type introduces itself (`Cursor`, `` Pair`1 ``, II.10.7.2);
/// the rest is read back off the enclosing parts by `definition_metadata_name`, which is what
/// makes the two spellings agree.
fn type_parameter_names(
    binder: &Binder,
    enclosing: &TypeSymbol,
    declaration: &TypeDecl,
) -> Vec<Box<str>> {
    enclosing_type_parameters(binder, enclosing)
        .into_iter()
        .chain(
            declaration
                .type_parameters
                .iter()
                .map(|parameter| parameter.name.clone()),
        )
        .collect()
}

fn enclosing_type_parameters(binder: &Binder, ty: &TypeSymbol) -> Vec<Box<str>> {
    let Some(enclosing) = binder
        .model()
        .get_by_symbol(ty)
        .and_then(|info| info.enclosing.clone())
    else {
        return Vec::new();
    };
    binder
        .model()
        .get_by_symbol(&type_symbol_from_dotted(&enclosing))
        .map(|info| info.type_parameters.to_vec())
        .unwrap_or_default()
}

fn mint_signature_type(
    binder: &Binder,
    syntactic: &TypeSymbol,
    scope: ParameterScope<'_>,
    inherited: &[Box<str>],
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
) -> Result<(), crate::EmitError> {
    if let TypeSymbol::Instantiation {
        definition,
        arguments,
    } = syntactic
    {
        let named = definition_symbol(definition, arguments.len());
        if tokens.type_token(&named).is_none()
            && let Some((namespace, name)) = split_type_name(&binder.resolve_type(&named))
        {
            let token = image.type_ref(&namespace, &name);
            tokens.insert_type(&named, token);
        }
        for argument in arguments {
            mint_signature_type(binder, argument, scope, inherited, image, tokens)?;
        }
        return Ok(());
    }
    if let TypeSymbol::Array { element, .. }
    | TypeSymbol::Pointer(element)
    | TypeSymbol::ByRef(element) = syntactic
    {
        mint_signature_type(binder, element, scope, inherited, image, tokens)?;
        return Ok(());
    }
    if is_type_parameter(syntactic, scope) {
        return Ok(());
    }
    if is_type_parameter(syntactic, &[inherited]) {
        return Err(crate::EmitError::Unsupported(
            "a nested type naming its enclosing type's type parameter",
        ));
    }
    let needs_ref = matches!(
        syntactic,
        TypeSymbol::Named(_) | TypeSymbol::Special(SpecialType::Decimal)
    );
    if !needs_ref || tokens.type_token(syntactic).is_some() {
        return Ok(());
    }
    let resolved = binder.resolve_type(syntactic);
    if let Some(defined) = tokens.type_token(&resolved) {
        tokens.insert_type(syntactic, defined);
        return Ok(());
    }
    if let Some((namespace, name)) = split_type_name(&resolved) {
        let token = image.type_ref(&namespace, &name);
        tokens.insert_type(syntactic, token);
    }
    Ok(())
}

/// Mints the external types named in a type's member SIGNATURES (field, method
/// parameter/return, property, event, operator, indexer types), so `type_sig` finds them
/// even when a type appears only in a signature and not in any body (which `mint_references`
/// would otherwise be the only thing to catch).
///
/// **`declaring_parameters` ARE SKIPPED, AND THE COST OF NOT SKIPPING THEM IS NOT BLOAT.** A `T`
/// is a POSITION, not a type: minting its token asks for a `TypeRef` to a type named `T` that no
/// assembly declares, and a `class Box<T> { T item; T Get(); }` emitted one. It is unreferenced --
/// every signature that mentions `T` encodes `!0` through the declaring scope, which matches by
/// NAME before it would ever consult a token -- so the rows decoded correctly and the assembly
/// merely carried a dangling reference csc never emits.
///
/// **The hazard is that it makes a REFUSAL stop refusing.** `type_sig` has no case for a bare `T`
/// and falls through to the named-type lookup, which is exactly the safety net a signature written
/// in the WRONG scope relies on. With a token registered under `T`, that lookup SUCCEEDS and
/// encodes `Class(TypeRef T)` instead. The row that is junk in one place is a silent wrong answer
/// in another -- measured as a `TypeSpec` for `Box<T>` naming a class called `T`.
///
/// **A METHOD'S OWN PARAMETERS COUNT TOO, AND THAT IS THE HALF THAT WAS MISSING.** The skip list
/// was the DECLARING type's alone, so `static T Unwrap<T>(Box<T> b)` on a NON-generic class had an
/// empty list, mentioned no declaring parameter, and minted the `T` this rule exists to refuse.
/// A skip list that names one of two numbering spaces is a skip list for programs that use one.
fn mint_member_signature_types(
    binder: &Binder,
    members: &[Member],
    declaring_parameters: &[Box<str>],
    inherited_parameters: &[Box<str>],
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
) -> Result<(), crate::EmitError> {
    let mint = |syntactic: &TypeSymbol,
                own: &[Box<str>],
                image: &mut ImageBuilder,
                tokens: &mut Tokens| {
        mint_signature_type(
            binder,
            syntactic,
            &[declaring_parameters, own],
            inherited_parameters,
            image,
            tokens,
        )
    };
    for member in members {
        match member {
            Member::Field { ty, .. }
            | Member::Property { ty, .. }
            | Member::EventField { ty, .. }
            | Member::Event { ty, .. } => {
                mint(&binder.canonicalize(&bind_type(ty)), &[], image, tokens)?;
            }
            Member::Indexer {
                ty, parameters, ..
            } => {
                mint(&binder.canonicalize(&bind_type(ty)), &[], image, tokens)?;
                for parameter in parameters {
                    mint(&binder.canonicalize(&bind_type(&parameter.ty)), &[], image, tokens)?;
                }
            }
            Member::Method {
                return_type,
                type_parameters,
                parameters,
                ..
            } => {
                let own: Vec<Box<str>> = type_parameters
                    .iter()
                    .map(|parameter| parameter.name.clone())
                    .collect();
                mint(&binder.canonicalize(&bind_type(return_type)), &own, image, tokens)?;
                for parameter in parameters {
                    mint(&binder.canonicalize(&bind_type(&parameter.ty)), &own, image, tokens)?;
                }
            }
            Member::Operator {
                return_type,
                parameters,
                ..
            } => {
                mint(&binder.canonicalize(&bind_type(return_type)), &[], image, tokens)?;
                for parameter in parameters {
                    mint(&binder.canonicalize(&bind_type(&parameter.ty)), &[], image, tokens)?;
                }
            }
            Member::ConversionOperator {
                target, parameters, ..
            } => {
                mint(&binder.canonicalize(&bind_type(target)), &[], image, tokens)?;
                for parameter in parameters {
                    mint(&binder.canonicalize(&bind_type(&parameter.ty)), &[], image, tokens)?;
                }
            }
            Member::Constructor { parameters, .. } => {
                for parameter in parameters {
                    mint(&binder.canonicalize(&bind_type(&parameter.ty)), &[], image, tokens)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Mints the metadata token a `box`/`unbox.any`/`initobj` names for the value type `ty`. A
/// module struct already has its `TypeDef` token (nothing to do); a primitive needs a
/// `System.*` `TypeRef`; a CONSTRUCTED GENERIC struct needs its blob-keyed `TypeSpec`.
///
/// **AN INSTANTIATION IS DELEGATED, AND THIS IS THE FOURTH MINTING ENTRY TO NEED THE CASE.**
/// [`mint_named_type_token`] says the same thing about its own arm, and the shape repeated exactly:
/// `Own<int>` boxed out of a LOCAL worked, because the local's declared type comes through that
/// function -- while the identical box out of a PARAMETER or a FIELD refused, because those have no
/// declaration in the body and the box's own minting reached only [`TypeSymbol::Special`]. One
/// construct, four minting entries, and the case landed in a subset of them again
/// (`a-rule-with-several-implementations-gains-a-new-case-in-none-of-them`).
///
/// Delegated rather than re-handled. Building the `GENERICINST` blob here would be a second
/// implementation of "the `TypeSpec` naming a constructed type", and the two would drift -- the
/// mistake [`mint_type_token`]'s own note records having made under a `Display` key, where
/// `Box<!!0>` and `Box<!0>` are one string and two rows.
fn mint_value_type_token(ty: &TypeSymbol, image: &mut ImageBuilder, tokens: &mut Tokens) {
    if matches!(ty, TypeSymbol::Instantiation { .. }) {
        mint_type_token(image, tokens, ty);
        return;
    }
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

/// Mints System.Decimal's `(int lo, int mid, int hi, bool isNegative, byte scale)` constructor --
/// how a decimal literal is built, since System.Decimal has no CIL constant form.
fn mint_decimal_ctor(image: &mut ImageBuilder, tokens: &mut Tokens) {
    let decimal_ty = TypeSymbol::Special(SpecialType::Decimal);
    let params = [
        TypeSymbol::Special(SpecialType::Int32),
        TypeSymbol::Special(SpecialType::Int32),
        TypeSymbol::Special(SpecialType::Int32),
        TypeSymbol::Special(SpecialType::Boolean),
        TypeSymbol::Special(SpecialType::Byte),
    ];
    if tokens.method(&decimal_ty, ".ctor", &params).is_some() {
        return;
    }
    mint_value_type_token(&decimal_ty, image, tokens);
    let Some(parent) = tokens.type_token(&decimal_ty) else {
        return;
    };
    let Ok(param_sigs) = params
        .iter()
        .map(|ty| type_sig(tokens, ty))
        .collect::<Result<Vec<_>, _>>()
    else {
        return;
    };
    let signature = method_signature(true, &param_sigs, &TypeSig::Void);
    let ctor = image.member_ref(parent, ".ctor", &signature);
    tokens.insert_method(&decimal_ty, ".ctor", &params, ctor);
}

/// Emits `[DecimalConstantAttribute(scale, sign, hi, mid, low)]` on a `const decimal` field, since a
/// decimal has no `Constant` metadata encoding -- the attribute is how reflection recovers the value.
fn emit_decimal_constant_attribute(
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
    binder: &Binder,
    enclosing: &TypeSymbol,
    name: &str,
    field: Token,
) {
    let Some(Literal::Decimal {
        lo,
        mid,
        hi,
        scale,
        negative,
    }) = binder
        .model()
        .get_by_symbol(enclosing)
        .and_then(|info| info.find_field(name).and_then(|f| f.constant.clone()))
    else {
        return;
    };
    emit_decimal_constant_blob(image, tokens, lo, mid, hi, scale, negative, field);
}

/// `[DecimalConstant]` on a `Param`, for an optional parameter whose default is a `decimal`.
///
/// A decimal has no `Constant`-table encoding at all (II.22.9 has no element type for it), so csc
/// writes `Optional` WITHOUT `HasDefault` and puts the value in this attribute -- the same trick a
/// `const decimal` FIELD uses, and measured on both.
fn emit_decimal_parameter_attribute(
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
    literal: &Literal,
    param: Token,
) {
    let Literal::Decimal {
        lo,
        mid,
        hi,
        scale,
        negative,
    } = literal
    else {
        return;
    };
    emit_decimal_constant_blob(image, tokens, *lo, *mid, *hi, *scale, *negative, param);
}

/// The shared half: mint `DecimalConstantAttribute`'s five-argument `.ctor` and attach its value
/// blob to `parent` -- a `Field` for a `const decimal`, a `Param` for an optional one.
#[allow(clippy::too_many_arguments)]
fn emit_decimal_constant_blob(
    image: &mut ImageBuilder,
    tokens: &mut Tokens,
    lo: u32,
    mid: u32,
    hi: u32,
    scale: u8,
    negative: bool,
    parent: Token,
) {
    let attr_ty = named_symbol("System.Runtime.CompilerServices", "DecimalConstantAttribute");
    let params = [
        TypeSymbol::Special(SpecialType::Byte),
        TypeSymbol::Special(SpecialType::Byte),
        TypeSymbol::Special(SpecialType::UInt32),
        TypeSymbol::Special(SpecialType::UInt32),
        TypeSymbol::Special(SpecialType::UInt32),
    ];
    let reference = lamella_binder::MethodReference {
        declaring_type: attr_ty.clone(),
        name: ".ctor".into(),
        parameters: params.to_vec(),
        return_type: TypeSymbol::Special(SpecialType::Void),
        is_static: false,
        is_vararg: false,
        instantiation: None,
        declaring_instantiation: None,
    };
    mint_member_ref(&reference, image, tokens);
    let Some(ctor) = tokens.method(&attr_ty, ".ctor", &params) else {
        return;
    };
    let mut blob = alloc::vec![0x01u8, 0x00, scale, u8::from(negative)];
    blob.extend_from_slice(&hi.to_le_bytes());
    blob.extend_from_slice(&mid.to_le_bytes());
    blob.extend_from_slice(&lo.to_le_bytes());
    blob.extend_from_slice(&[0x00, 0x00]);
    image.add_custom_attribute(parent, ctor, &blob);
}

/// Mints `System.Decimal.op_Increment`/`op_Decrement` (`decimal -> decimal`): a `decimal++`/`--`
/// steps through the operator method (there is no native CIL decimal add), so user_step_method can
/// find its token and the increment emits the call instead of a native `add 1`.
fn mint_decimal_step(name: &str, image: &mut ImageBuilder, tokens: &mut Tokens) {
    let decimal_ty = TypeSymbol::Special(SpecialType::Decimal);
    let params = [decimal_ty.clone()];
    if tokens.method(&decimal_ty, name, &params).is_some() {
        return;
    }
    mint_value_type_token(&decimal_ty, image, tokens);
    let Some(parent) = tokens.type_token(&decimal_ty) else {
        return;
    };
    let Ok(sig) = type_sig(tokens, &decimal_ty) else {
        return;
    };
    let signature = method_signature(false, &[sig.clone()], &sig);
    let method = image.member_ref(parent, name, &signature);
    tokens.insert_method(&decimal_ty, name, &params, method);
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


/// Synthesizes `this.<field> = <init>;` for each instance field initializer AND each instance
/// AUTO-PROPERTY INITIALIZER, in declaration order (17.11). They run before the base-constructor
/// call in every constructor that does not chain to `this(...)`, so a virtual method the base
/// constructor invokes observes them already assigned. Static and const fields are excluded here
/// (a const folds; static initializers run in the static constructor).
///
/// **THE TWO KINDS INTERLEAVE AND THE ORDER IS THE SOURCE'S**, which is why one walk over the
/// members produces both rather than one walk appending to another: `int A = 1; int P { get; } = 2;
/// int B = 3;` stores A, P's backing field and B in that order, measured on csc's own `.ctor`.
///
/// **AN AUTO-PROPERTY INITIALIZER TARGETS THE BACKING FIELD, NOT THE PROPERTY.** That is not an
/// optimization: a getter-only auto-property has no setter to call, and going through a VIRTUAL
/// setter would run a derived override before the derived constructor had. csc emits `stfld` here
/// and so do we, which is the whole reason the binder models the field at all.
fn field_initializer_statements(declaration: &TypeDecl) -> Vec<Stmt> {
    let mut statements = Vec::new();
    let mut push = |name: Box<str>, init: &Expr, span| {
        let target = Expr::new(
            ExprKind::MemberAccess {
                receiver: Box::new(Expr::new(ExprKind::This, span)),
                name,
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
    };
    for member in &declaration.members {
        match member {
            Member::Field {
                modifiers,
                declarators,
                ..
            } => {
                if modifiers
                    .iter()
                    .any(|m| matches!(m, Modifier::Static | Modifier::Const))
                {
                    continue;
                }
                for declarator in declarators {
                    if let Some(init) = &declarator.initializer {
                        push(declarator.name.clone(), init, declarator.span);
                    }
                }
            }
            Member::Property {
                modifiers,
                name,
                explicit_interface,
                initializer: Some(init),
                span,
                ..
            } if !modifiers.iter().any(|m| matches!(m, Modifier::Static)) => push(
                auto_property_backing_field_name(explicit_interface.as_ref(), name).into(),
                init,
                *span,
            ),
            _ => {}
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

/// One initializer that runs in the `.cctor`.
struct StaticInitializer<'a> {
    /// The bound type of the storage being written -- the decimal-const and default-value tests
    /// ask about it, and neither can be answered from the expression alone.
    field_ty: TypeSymbol,
    /// What the synthesized assignment names: a declared field's own name, or an auto-property's
    /// `<P>k__BackingField`.
    target: Box<str>,
    /// The initializer expression as written.
    init: &'a Expr,
    /// Where the store is reported and given its sequence point.
    span: Span,
}

/// The initializers that run in the `.cctor`, in declaration order: every `static` field, every
/// `const decimal` (which has no `Constant` form, so it initializes there as a static readonly),
/// and every `static` AUTO-PROPERTY INITIALIZER, which stores to the property's backing field.
/// Every other `const` is a compile-time literal inlined at use, and an instance initializer runs
/// in the instance constructor -- neither belongs here.
///
/// **ONE LIST FOR THE DECISION AND FOR THE BODY.** `needs_static_constructor` and
/// `static_field_initializer_statements` must range over the same set: a `.cctor` the decision
/// omits and the body would have filled loses its stores silently, and one the decision emits and
/// the body leaves empty is a row csc does not write.
fn static_initializers(declaration: &TypeDecl) -> Vec<StaticInitializer<'_>> {
    let mut found = Vec::new();
    for member in &declaration.members {
        match member {
            Member::Field {
                modifiers,
                ty,
                declarators,
                ..
            } => {
                let is_static = modifiers.iter().any(|m| matches!(m, Modifier::Static));
                let is_const = modifiers.iter().any(|m| matches!(m, Modifier::Const));
                let field_ty = bind_type(ty);
                if is_const {
                    if !matches!(field_ty, TypeSymbol::Special(SpecialType::Decimal)) {
                        continue;
                    }
                } else if !is_static {
                    continue;
                }
                for declarator in declarators {
                    if let Some(init) = &declarator.initializer {
                        found.push(StaticInitializer {
                            field_ty: field_ty.clone(),
                            target: declarator.name.clone(),
                            init,
                            span: declarator.span,
                        });
                    }
                }
            }
            Member::Property {
                modifiers,
                ty,
                name,
                explicit_interface,
                initializer: Some(init),
                span,
                ..
            } if modifiers.iter().any(|m| matches!(m, Modifier::Static)) => {
                found.push(StaticInitializer {
                    field_ty: bind_type(ty),
                    target: auto_property_backing_field_name(explicit_interface.as_ref(), name)
                        .into(),
                    init,
                    span: *span,
                });
            }
            _ => {}
        }
    }
    found
}

/// Synthesizes `<field> = <init>;` for each static initializer, in declaration order -- the
/// statements that run first in the static constructor.
fn static_field_initializer_statements(declaration: &TypeDecl) -> Vec<Stmt> {
    static_initializers(declaration)
        .into_iter()
        .map(|entry| {
            let assignment = Expr::new(
                ExprKind::Assignment {
                    operator: AssignmentOperator::Assign,
                    target: Box::new(Expr::name(entry.target, entry.span)),
                    value: Box::new(entry.init.clone()),
                },
                entry.span,
            );
            Stmt::new(StmtKind::Expression(assignment), entry.span)
        })
        .collect()
}

/// Whether a static field initializer assigns exactly the field type's default value -- so csc
/// leaves it to the runtime's zero-init and, when EVERY initializer is default (and no static
/// constructor is declared), omits the `.cctor` outright. Deliberately conservative: only a
/// default-valued LITERAL whose kind matches the field type (no boxing, no numeric conversion)
/// counts; a folded constant expression or any conversion returns false, so lcsc still emits the
/// `.cctor` there rather than risk dropping a live initializer.
fn is_default_valued_static_init(field_ty: &TypeSymbol, init: &Expr) -> bool {
    let ExprKind::Literal(literal) = &init.kind else {
        return false;
    };
    match literal {
        Literal::Null => true,
        Literal::Boolean(false) => matches!(field_ty, TypeSymbol::Special(SpecialType::Boolean)),
        Literal::Character(0) => matches!(field_ty, TypeSymbol::Special(SpecialType::Char)),
        Literal::Integer { value: 0, .. } => {
            matches!(field_ty, TypeSymbol::Special(special) if is_integer_special(*special))
        }
        Literal::Real { bits: 0, .. } => matches!(
            field_ty,
            TypeSymbol::Special(SpecialType::Single | SpecialType::Double)
        ),
        Literal::Decimal {
            lo: 0,
            mid: 0,
            hi: 0,
            scale: 0,
            negative: false,
        } => matches!(field_ty, TypeSymbol::Special(SpecialType::Decimal)),
        _ => false,
    }
}

/// The integer special types, for which a `0` literal is the field default (so its initializer
/// is a no-op the runtime's zero-init already covers).
fn is_integer_special(special: SpecialType) -> bool {
    matches!(
        special,
        SpecialType::SByte
            | SpecialType::Byte
            | SpecialType::Int16
            | SpecialType::UInt16
            | SpecialType::Int32
            | SpecialType::UInt32
            | SpecialType::Int64
            | SpecialType::UInt64
    )
}

/// Whether the type needs a static constructor `.cctor`: it declares a static constructor, or it
/// has a static field initializer that assigns a NON-default value. When every static (and
/// `const decimal`) initializer is the field type's default and no static constructor is declared,
/// csc omits the `.cctor` -- the fields already hold that default -- and lcsc now matches (it was
/// emitting a redundant `.cctor`, which also cost the declaration a spurious sequence point).
fn needs_static_constructor(declaration: &TypeDecl) -> bool {
    static_constructor_body(declaration).is_some()
        || static_initializers(declaration)
            .iter()
            .any(|entry| !is_default_valued_static_init(&entry.field_ty, entry.init))
}

/// Whether the type declares an INSTANCE constructor (a static constructor does not
/// suppress the implicit default instance one).
/// Whether ANY part of the type declares an instance constructor, so the implicit parameterless
/// one is not synthesized (17.10.4).
///
/// **THE QUESTION IS ABOUT THE TYPE AND THE PARTS ARE NOT INDEPENDENT.** A `partial class` whose
/// first part writes `W(int)` and whose second writes nothing has ONE constructor, not two, and
/// asking each declaration on its own emitted a parameterless `.ctor` for every part that happened
/// to declare none -- rows csc does not write, and the method tokens after them all shifted.
fn any_part_declares_instance_constructor(
    declaration: &TypeDecl,
    later: &[PartialPart<'_>],
) -> bool {
    declares_instance_constructor(declaration)
        || later
            .iter()
            .any(|part| declares_instance_constructor(part.declaration))
}

/// Every modifier written on ANY part of a partial type, as one list.
///
/// **A PARTIAL TYPE'S MODIFIERS ARE THE UNION OF ITS PARTS' (17.1.4), SO A QUESTION ABOUT ONE HAS
/// TO BE ASKED OF EVERY PART.** Asking the declaration in hand answers about one part and reads
/// exactly like an answer about the type, which is what makes it invisible at every bit at once:
/// `partial class Q { }` + `abstract partial class Q { }` yields a type that is not abstract, and
/// `partial class V { }` + `public partial class V { }` yields one that is not public -- so a type
/// the source declares public ships as `NotPublic` and every consumer outside the assembly sees
/// CS0122 or CS0246 for a type that is right there.
///
/// **THE RETURN IS A LIST, NOT A PREDICATE, BECAUSE THE CALLER ASKS SEVERAL QUESTIONS.** Visibility
/// alone is four modifiers wide (`nested_visibility` reads `public`/`protected`/`internal`/
/// `private` together), and a per-modifier predicate would have to be threaded through it; handing
/// over the merged list instead makes the union the DEFAULT at that site rather than something each
/// new bit has to remember to opt into.
///
/// `TypeInfo::merge` unions `is_sealed` and `is_abstract` on the binder's side and cites this same
/// clause -- the binder had the rule right the whole time, and only the emitter read one part.
fn merged_modifiers(declaration: &TypeDecl, later: &[PartialPart<'_>]) -> Vec<Modifier> {
    let mut merged = declaration.modifiers.clone();
    for part in later {
        for modifier in &part.declaration.modifiers {
            if !merged.contains(modifier) {
                merged.push(*modifier);
            }
        }
    }
    merged
}

/// Whether ANY part carries `static`, which suppresses the implicit constructor for the whole type.
fn any_part_is_static(declaration: &TypeDecl, later: &[PartialPart<'_>]) -> bool {
    declaration.modifiers.contains(&Modifier::Static)
        || later
            .iter()
            .any(|part| part.declaration.modifiers.contains(&Modifier::Static))
}

fn declares_instance_constructor(declaration: &TypeDecl) -> bool {
    declaration.members.iter().any(|member| {
        matches!(member, Member::Constructor { modifiers, .. } if !is_static_constructor(modifiers))
    })
}

/// Maps a bound type to its signature form. A named type resolves to the `Class`
/// of its `TypeDef` token; array types come later.
/// The generic DEFINITION `definition` instantiated with `arity` arguments, as the plain named
/// symbol the token table holds it under. One line, and it stays as a name here because that name
/// is what the three emission sites below read; the rule itself lives in the binder beside the
/// model lookups that have to agree with it.
fn definition_symbol(definition: &[Box<str>], arity: usize) -> TypeSymbol {
    lamella_binder::definition_symbol(definition, arity)
}

fn type_sig(tokens: &Tokens, ty: &TypeSymbol) -> Result<TypeSig, crate::EmitError> {
    let canonical = tokens.canonical(ty);
    let ty = &canonical;
    let special = match ty {
        TypeSymbol::Special(SpecialType::Decimal) => {
            return tokens.type_token(ty).map(TypeSig::ValueType).ok_or(
                crate::EmitError::Unsupported("System.Decimal has no metadata token in a signature"),
            );
        }
        TypeSymbol::Special(special) => special,
        TypeSymbol::Named(_) if crate::expr::is_typed_reference(ty) => {
            return Ok(TypeSig::TypedByRef);
        }
        TypeSymbol::Named(_) if crate::expr::is_native_int(ty) => return Ok(TypeSig::NativeInt),
        TypeSymbol::Named(_) if crate::expr::is_native_uint(ty) => return Ok(TypeSig::NativeUInt),
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
        TypeSymbol::Array { element, rank } => {
            let element = Box::new(type_sig(tokens, element)?);
            return Ok(if *rank == 1 {
                TypeSig::SzArray(element)
            } else {
                TypeSig::Array { element, rank: *rank as u32 }
            });
        }
        TypeSymbol::Pointer(element) => {
            return Ok(TypeSig::Pointer(Box::new(type_sig(tokens, element)?)));
        }
        TypeSymbol::ByRef(element) => {
            return Ok(TypeSig::ByRef(Box::new(type_sig(tokens, element)?)));
        }
        TypeSymbol::Instantiation {
            definition,
            arguments,
        } => {
            let named = definition_symbol(definition, arguments.len());
            let token = tokens.type_token(&named).ok_or(crate::EmitError::Unsupported(
                "a generic definition outside this module in a signature",
            ))?;
            let head = if tokens.is_struct(&named) || tokens.is_enum(&named) {
                TypeSig::ValueType(token)
            } else {
                TypeSig::Class(token)
            };
            let mut lowered = Vec::with_capacity(arguments.len());
            for argument in arguments {
                lowered.push(type_sig(tokens, argument)?);
            }
            return Ok(TypeSig::GenericInst {
                definition: Box::new(head),
                arguments: lowered,
            });
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

/// The type parameters in scope where a signature is written: the METHOD's own, and the DECLARING
/// TYPE's. Both are live at once inside a generic method of a generic type.
///
/// **THEY ARE SEPARATE NUMBERING SPACES AND `!0` IS NOT `!!0`.** `class Holder<TOuter>` with a
/// `TOuter Echo<TMethod>(TMethod)` encodes as `!0 Echo<!!0>(!!0)` -- csc's own bytes for exactly
/// that method are `30 01 01 13 00 1e 00`, a `VAR 0` return beside an `MVAR 0` parameter. Encoding
/// one as the other yields a signature that decodes without error and names a different type.
#[derive(Clone, Copy, Default)]
struct GenericScope<'a> {
    /// The enclosing METHOD's own parameters -- `!!n` ([`TypeSig::MVar`]). Empty for an ordinary
    /// method, which is every method a C# 1.0 program declares.
    method: &'a [Box<str>],
    /// The declaring TYPE's parameters -- `!n` ([`TypeSig::Var`]).
    declaring: &'a [Box<str>],
}

/// [`type_sig`] for a signature that is still OPEN over the parameters in `scope`: a mention of one
/// encodes as `!!n` or `!n` at its declaration position, everything else lowers exactly as a closed
/// type does.
///
/// **`!n` IS A POSITION, NOT A TOKEN.** The binder substitutes by NAME
/// (`MethodSymbol::instantiate`), metadata numbers instead, and this is the one place the two meet
/// -- so an index comes from a declaration order and from nothing else.
///
/// **The METHOD's parameters are consulted first**, so a method parameter shadows a type
/// parameter of the same name. C# forbids that spelling outright (CS0693), so the precedence only
/// ever settles a program that is already being refused; making it explicit keeps the two lists
/// from silently depending on iteration order.
///
/// Names are matched BEFORE canonicalization, so a type parameter shadows a real type of the same
/// name here. **That rule is stated once, on `lamella_binder::resolve::TypeTable::shadow`**, and
/// this site applies it rather than restating it: the binder's `enter_type_parameters` is its other
/// use site, scoping the name while a declaration is bound, and this one decides which index the
/// same name encodes as. Two sites, one rule -- if they disagree, a program binds against one type
/// and emits the other, which no diagnostic can catch.
fn open_type_sig(
    tokens: &Tokens,
    ty: &TypeSymbol,
    scope: GenericScope,
) -> Result<TypeSig, crate::EmitError> {
    if let TypeSymbol::Named(parts) = ty
        && let [only] = &parts[..]
    {
        if let Some(number) = scope.method.iter().position(|name| name == only) {
            return Ok(TypeSig::MVar(number as u32));
        }
        if let Some(number) = scope.declaring.iter().position(|name| name == only) {
            return Ok(TypeSig::Var(number as u32));
        }
    }
    match ty {
        TypeSymbol::Array { element, rank } => {
            let element = Box::new(open_type_sig(tokens, element, scope)?);
            Ok(if *rank == 1 {
                TypeSig::SzArray(element)
            } else {
                TypeSig::Array {
                    element,
                    rank: u32::from(*rank),
                }
            })
        }
        TypeSymbol::Pointer(element) => Ok(TypeSig::Pointer(Box::new(open_type_sig(tokens, element, scope)?))),
        TypeSymbol::ByRef(element) => Ok(TypeSig::ByRef(Box::new(open_type_sig(tokens, element, scope)?))),
        TypeSymbol::Instantiation {
            definition,
            arguments,
        } => {
            let named = definition_symbol(definition, arguments.len());
            let token = tokens.type_token(&named).ok_or(crate::EmitError::Unsupported(
                "a generic definition outside this module in a signature",
            ))?;
            let head = if tokens.is_struct(&named) || tokens.is_enum(&named) {
                TypeSig::ValueType(token)
            } else {
                TypeSig::Class(token)
            };
            let mut lowered = Vec::with_capacity(arguments.len());
            for argument in arguments {
                lowered.push(open_type_sig(tokens, argument, scope)?);
            }
            Ok(TypeSig::GenericInst {
                definition: Box::new(head),
                arguments: lowered,
            })
        }
        _ => type_sig(tokens, ty),
    }
}

/// The scope for a generic method's own OPEN signature, as a call site instantiates it.
///
/// The DECLARING half is empty because this scope belongs to the EXTERNAL arm of
/// `mint_generic_definition_ref`, whose parent is a `TypeRef` to a non-generic type: there is no
/// `!n` to name, and a non-empty list here would number a same-spelled method parameter against
/// the wrong space. **It is not a statement that a generic method on a generic type is out of
/// reach** -- that shape is `mint_instantiated_generic_definition_ref`, which parents on a
/// `TypeSpec` and passes both lists. The distinction is worth stating because an empty list reads
/// as "there is no such case" rather than as "this arm cannot reach it".
fn method_scope(instantiation: &lamella_binder::MethodInstantiation) -> GenericScope<'_> {
    GenericScope {
        method: &instantiation.type_parameters,
        declaring: &[],
    }
}

/// [`type_sig`] for a MEMBER signature of `declaring`: a mention of one of the declaring type's own
/// parameters encodes as `!n`, everything else exactly as before.
///
/// This is the form every member-signature site uses, and it takes the DECLARING TYPE rather than a
/// parameter list so the scope is derived from data the site already holds. A site writing an
/// EXTERNAL member passes that member's own declaring type, which this module did not declare and
/// which therefore has no parameters recorded -- so an imported signature can never pick up a `!n`
/// belonging to whatever type happened to be emitting. See `Tokens::type_parameters`.
fn member_type_sig(
    tokens: &Tokens,
    declaring: &TypeSymbol,
    ty: &TypeSymbol,
) -> Result<TypeSig, crate::EmitError> {
    open_type_sig(
        tokens,
        ty,
        GenericScope {
            method: &[],
            declaring: tokens.type_parameters(declaring),
        },
    )
}

/// Whether `ty` mentions one of the method's own type parameters at any depth -- so it has no
/// `TypeRef` to mint and asking for one would invent a type named `T`.
fn mentions_type_parameter(ty: &TypeSymbol, type_parameters: &[Box<str>]) -> bool {
    match ty {
        TypeSymbol::Named(parts) => matches!(
            &parts[..],
            [only] if type_parameters.iter().any(|name| name == only)
        ),
        TypeSymbol::Array { element, .. }
        | TypeSymbol::Pointer(element)
        | TypeSymbol::ByRef(element) => mentions_type_parameter(element, type_parameters),
        TypeSymbol::Instantiation { arguments, .. } => arguments
            .iter()
            .any(|argument| mentions_type_parameter(argument, type_parameters)),
        _ => false,
    }
}

/// Walks the units in emission order, assigning each method its `MethodDef` token
/// (`1..`) so a body can name a forward call (across files too). The order must match
/// the emission walk so the tokens line up with the rows `add_method` produces. `canon`
/// (from the binder model) is installed first so the single-part signature names this
/// records key the same as the binder's qualified ones -- set before any insert, so
/// look-ups (also canonicalized) agree.
/// One declaration of a partial type, with the per-file context its members were bound under.
///
/// Emission RE-RESOLVES the names in a member's signature and body, so a part declared in another
/// file has to be emitted under that file's `using` directives, `#define` set and debug document --
/// not under the file that happens to hold the first part.
struct PartialPart<'a> {
    declaration: &'a TypeDecl,
    usings: &'a [UsingDirective],
    defined_symbols: &'a alloc::collections::BTreeSet<Box<str>>,
    debug: Option<&'a DebugContext<'a>>,
}

/// Every declaration of each PARTIAL type in the compilation, in source order, keyed by the
/// declaration scope and the type's metadata name.
///
/// **THE PARTS OF ONE TYPE ARE EMITTED BACK TO BACK, AND THAT IS WHAT MAKES ONE `TypeDef` OUT OF
/// THEM.** A `TypeDef` row records the FIRST of its fields and methods (II.22.37) and its ranges
/// end where the next type's begin, so a type's rows have to be contiguous in the flat tables.
/// Emitting the second part where the walk happens to reach it wrote a SECOND `TypeDef` of the same
/// name, and the CLR refuses to load that: *BadImageFormatException: Duplicate type with name 'W'*.
/// Measured by building it.
///
/// The key's first half is the declaration SCOPE -- a namespace, or the enclosing type's full name
/// for a nested part -- and the second is the arity-mangled name (II.10.7.2), so
/// `partial class W<T>` and `partial class W<T,U>` index apart, as the two types they are.
type PartialIndex<'a> = alloc::collections::BTreeMap<(String, String), Vec<PartialPart<'a>>>;

/// Builds the [`PartialIndex`] for a whole compilation. Non-partial declarations are absent by
/// design: a declaration colliding with a partial one is CS0260, and emission runs only after an
/// error-free bind.
fn index_partial_types<'a>(
    units: &'a [CompilationUnit],
    contexts: &'a [DebugContext<'a>],
) -> PartialIndex<'a> {
    let mut index = PartialIndex::new();
    for (position, unit) in units.iter().enumerate() {
        index_partial_members(&mut index, &unit.members, "", unit, contexts.get(position));
    }
    index
}

fn index_partial_members<'a>(
    index: &mut PartialIndex<'a>,
    members: &'a [NamespaceMember],
    scope: &str,
    unit: &'a CompilationUnit,
    debug: Option<&'a DebugContext<'a>>,
) {
    for member in members {
        match member {
            NamespaceMember::Namespace(declaration) => {
                let inner = join_namespace(scope, &declaration.name);
                index_partial_members(index, &declaration.members, &inner, unit, debug);
            }
            NamespaceMember::Type(declaration) => {
                if declaration.modifiers.contains(&Modifier::Partial) {
                    index
                        .entry((String::from(scope), partial_key_name(declaration)))
                        .or_default()
                        .push(PartialPart {
                            declaration,
                            usings: &unit.usings,
                            defined_symbols: &unit.defined_symbols,
                            debug,
                        });
                }
                let enclosing = lamella_binder::declared_full_name(scope, declaration);
                for nested in &declaration.members {
                    if let Member::NestedType(nested) = nested {
                        index_partial_members(
                            index,
                            core::slice::from_ref(nested.as_ref()),
                            &enclosing,
                            unit,
                            debug,
                        );
                    }
                }
            }
            NamespaceMember::Enum(_) | NamespaceMember::Delegate(_) => {}
        }
    }
}

/// A partial declaration's key name: its arity-mangled metadata name (II.10.7.2).
fn partial_key_name(declaration: &TypeDecl) -> String {
    lamella_binder::metadata_type_name(&declaration.name, declaration.type_parameters.len())
}

/// The parts of `declaration` AFTER the first, or an empty slice when it is not a partial type or
/// is not the first part.
///
/// **A LATER PART ANSWERS EMPTY AND IS SKIPPED BY ITS CALLER**, which is what keeps the type from
/// being emitted twice: the whole type is emitted where its FIRST part stands.
fn continuation_parts<'a, 'b>(
    index: &'b PartialIndex<'a>,
    scope: &str,
    declaration: &TypeDecl,
) -> Option<&'b [PartialPart<'a>]> {
    let parts = index.get(&(String::from(scope), partial_key_name(declaration)))?;
    let first = parts.first()?;
    if !core::ptr::eq(first.declaration, declaration) {
        return None;
    }
    Some(&parts[1..])
}

/// Whether `declaration` is a partial part that some EARLIER declaration already emitted.
fn is_later_partial_part(index: &PartialIndex<'_>, scope: &str, declaration: &TypeDecl) -> bool {
    index
        .get(&(String::from(scope), partial_key_name(declaration)))
        .and_then(|parts| parts.first())
        .is_some_and(|first| !core::ptr::eq(first.declaration, declaration))
}

fn assign_tokens(
    units: &[CompilationUnit],
    binder: &mut Binder,
    partials: &PartialIndex<'_>,
) -> Tokens {
    let mut tokens = Tokens::new();
    tokens.set_canon(binder.model().signature_canon());
    let mut next_type = 1u32;
    let mut next_field = 0u32;
    let mut next_method = 0u32;
    for unit in units {
        collect_tokens(
            &mut tokens,
            binder,
            &mut next_type,
            &mut next_field,
            &mut next_method,
            &unit.usings,
            &unit.members,
            "",
            partials,
        );
    }
    tokens
}

/// Reserves the tokens ONE type's rows will take, in the order [`emit_type_inner`] writes them.
///
/// **`continuation` AND `later` MIRROR EMISSION EXACTLY, AND THE MIRROR IS THE POINT.** A member's
/// `Field`/`MethodDef` token is reserved here and used when its row is written; if the two walks
/// visit members in different orders, a token names another member's row and nothing about the
/// image looks wrong until it runs. So the parts of a partial type are gathered here in the same
/// back-to-back order emission gathers them, and a LATER part reserves no `TypeDef` row of its own.
#[allow(clippy::too_many_arguments)]
fn collect_type_tokens(
    tokens: &mut Tokens,
    binder: &mut Binder,
    next_type: &mut u32,
    next_field: &mut u32,
    next_method: &mut u32,
    namespace: &str,
    declaration: &TypeDecl,
    continuation: bool,
    later: &[PartialPart<'_>],
    partials: &PartialIndex<'_>,
) {
    let declaring = declared_type_symbol(namespace, declaration);
    let declaration_type_parameters = binder
        .enter_type_parameters(&declaration.type_parameters, &declaration.constraints);
    binder.enter_type(declaring.clone());
    if !continuation {
        *next_type += 1;
        tokens.insert_type(&declaring, Token::new(TYPE_DEF, *next_type));
    }
    tokens.insert_type_parameters(
        &declaring,
        type_parameter_names(binder, &declaring, declaration),
    );
    let is_struct = declaration.kind == TypeKind::Struct;
    let is_interface = declaration.kind == TypeKind::Interface;
    let is_cil_primitive =
        matches!(&declaring, TypeSymbol::Special(s) if *s != SpecialType::Decimal);
    if is_struct && !is_cil_primitive {
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
        if let Member::Property {
            modifiers,
            name,
            getter,
            setter,
            explicit_interface,
            ..
        } = member
        {
            if is_auto_property(
                modifiers,
                getter.as_ref(),
                setter.as_ref(),
                is_interface,
            ) {
                *next_field += 1;
                tokens.insert_field(
                    &declaring,
                    &auto_property_backing_field_name(explicit_interface.as_ref(), name),
                    Token::new(FIELD, *next_field),
                );
            }
        }
        if let Member::EventField {
            modifiers,
            declarators,
            ..
        } = member
        {
            if event_has_backing_field(modifiers, is_interface) {
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
    }
    if !is_struct
        && !is_interface
        && !continuation
        && !any_part_declares_instance_constructor(declaration, later)
        && !any_part_is_static(declaration, later)
    {
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
                modifiers,
                name,
                type_parameters,
                parameters,
                is_vararg,
                body,
                explicit_interface,
                attributes,
                ..
            } if body.is_some()
                || is_interface
                || modifiers.contains(&Modifier::Abstract)
                || find_dll_import(name, attributes).is_some() =>
            {
                *next_method += 1;
                let method_type_parameter_names: Vec<Box<str>> = type_parameters
                    .iter()
                    .map(|parameter| parameter.name.clone())
                    .collect();
                let method_type_parameters =
                    binder.enter_type_parameter_names(&method_type_parameter_names);
                let mut params: Vec<TypeSymbol> = parameters
                    .iter()
                    .map(|parameter| binder.canonicalize(&parameter_symbol(parameter)))
                    .collect();
                binder.exit_type_parameters(method_type_parameters);
                if *is_vararg {
                    params.push(crate::expr::arglist_marker_symbol());
                }
                let token = Token::new(METHOD_DEF, *next_method);
                match explicit_interface {
                    Some(interface) => tokens.insert_method(
                        &declaring,
                        &explicit_interface_member_name(interface, name),
                        &params,
                        token,
                    ),
                    None => {
                        tokens.insert_method(&declaring, name, &params, token);
                        if modifiers.contains(&Modifier::Virtual)
                            || modifiers.contains(&Modifier::Override)
                            || modifiers.contains(&Modifier::Abstract)
                        {
                            tokens.insert_virtual_method(&declaring, name, &params);
                        }
                    }
                }
            }
            Member::Operator {
                operator,
                parameters,
                ..
            } => {
                *next_method += 1;
                let params: Vec<TypeSymbol> = parameters
                    .iter()
                    .map(|parameter| binder.canonicalize(&parameter_symbol(parameter)))
                    .collect();
                tokens.insert_method(
                    &declaring,
                    operator.method_name(parameters.len()),
                    &params,
                    Token::new(METHOD_DEF, *next_method),
                );
            }
            Member::ConversionOperator {
                direction,
                target,
                parameters,
                ..
            } => {
                *next_method += 1;
                let params: Vec<TypeSymbol> = parameters
                    .iter()
                    .map(|parameter| binder.canonicalize(&parameter_symbol(parameter)))
                    .collect();
                let token = Token::new(METHOD_DEF, *next_method);
                tokens.insert_method(&declaring, direction.method_name(), &params, token);
                tokens.insert_method(
                    &declaring,
                    &crate::tokens::conversion_key_name(
                        direction.method_name(),
                        &binder.canonicalize(&bind_type(target)),
                    ),
                    &params,
                    token,
                );
            }
            Member::Constructor {
                modifiers,
                parameters,
                is_vararg,
                ..
            } if !is_static_constructor(modifiers) => {
                *next_method += 1;
                let mut params: Vec<TypeSymbol> = parameters
                    .iter()
                    .map(|parameter| binder.canonicalize(&parameter_symbol(parameter)))
                    .collect();
                if *is_vararg {
                    params.push(crate::expr::arglist_marker_symbol());
                }
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
            modifiers,
            ty,
            name,
            getter,
            setter,
            explicit_interface,
            ..
        } = member
        {
            let property_ty = binder.canonicalize(&bind_type(ty));
            let is_auto = is_auto_property(
                modifiers,
                getter.as_ref(),
                setter.as_ref(),
                is_interface,
            );
            if getter
                .as_ref()
                .is_some_and(|a| {
                    a.body.is_some()
                        || is_interface
                        || is_auto
                        || modifiers.contains(&Modifier::Abstract)
                })
            {
                *next_method += 1;
                tokens.insert_method(
                    &declaring,
                    &explicit_accessor_name(
                        explicit_interface.as_ref(),
                        &accessor_name("get_", name),
                    ),
                    &[],
                    Token::new(METHOD_DEF, *next_method),
                );
            }
            if setter
                .as_ref()
                .is_some_and(|a| {
                    a.body.is_some()
                        || is_interface
                        || is_auto
                        || modifiers.contains(&Modifier::Abstract)
                })
            {
                *next_method += 1;
                tokens.insert_method(
                    &declaring,
                    &explicit_accessor_name(
                        explicit_interface.as_ref(),
                        &accessor_name("set_", name),
                    ),
                    &[property_ty],
                    Token::new(METHOD_DEF, *next_method),
                );
            }
        }
        if let Member::Indexer {
            modifiers,
            ty,
            parameters,
            getter,
            setter,
            attributes,
            ..
        } = member
        {
            let emitted = |accessor: &Option<lamella_syntax::ast::Accessor>| {
                accessor.as_ref().is_some_and(|a| {
                    a.body.is_some()
                        || is_interface
                        || modifiers.contains(&Modifier::Abstract)
                })
            };
            let indices: Vec<TypeSymbol> = parameters
                .iter()
                .map(|parameter| binder.canonicalize(&parameter_symbol(parameter)))
                .collect();
            let accessor = indexer_name(attributes);
            if emitted(getter) {
                *next_method += 1;
                let token = Token::new(METHOD_DEF, *next_method);
                tokens.insert_method(&declaring, "get_Item", &indices, token);
                alias_renamed_accessor(tokens, &declaring, "get_", &accessor, &indices, token);
            }
            if emitted(setter) {
                *next_method += 1;
                let token = Token::new(METHOD_DEF, *next_method);
                let mut parameters = indices;
                parameters.push(binder.canonicalize(&bind_type(ty)));
                tokens.insert_method(&declaring, "set_Item", &parameters, token);
                alias_renamed_accessor(tokens, &declaring, "set_", &accessor, &parameters, token);
            }
        }
    }
    for member in &declaration.members {
        if let Member::EventField {
            ty, declarators, ..
        } = member
        {
            let event_ty = binder.canonicalize(&bind_type(ty));
            for declarator in declarators {
                for prefix in ["add_", "remove_"] {
                    *next_method += 1;
                    tokens.insert_method(
                        &declaring,
                        &accessor_name(prefix, &declarator.name),
                        &[event_ty.clone()],
                        Token::new(METHOD_DEF, *next_method),
                    );
                }
            }
        }
        if let Member::Event {
            ty,
            name,
            adder,
            remover,
            explicit_interface,
            ..
        } = member
        {
            let event_ty = binder.canonicalize(&bind_type(ty));
            for (prefix, present) in [("add_", adder.is_some()), ("remove_", remover.is_some())] {
                if present {
                    *next_method += 1;
                    tokens.insert_method(
                        &declaring,
                        &explicit_accessor_name(
                            explicit_interface.as_ref(),
                            &accessor_name(prefix, name),
                        ),
                        &[event_ty.clone()],
                        Token::new(METHOD_DEF, *next_method),
                    );
                }
            }
        }
    }
    binder.exit_type();
    binder.exit_type_parameters(declaration_type_parameters);
    for part in later {
        let part_scope = enter_part(binder, part);
        collect_type_tokens(
            tokens,
            binder,
            next_type,
            next_field,
            next_method,
            namespace,
            part.declaration,
            true,
            &[],
            partials,
        );
        leave_part(binder, part_scope);
    }
    if continuation {
        return;
    }
    let enclosing_full = lamella_binder::declared_full_name(namespace, declaration);
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
                    binder,
                    next_type,
                    next_field,
                    next_method,
                    &[],
                    core::slice::from_ref(nested.as_ref()),
                    &enclosing_full,
                    partials,
                );
            }
        }
    }
    for part in later {
        let part_scope = enter_part(binder, part);
        collect_nested_tokens(
            tokens,
            binder,
            next_type,
            next_field,
            next_method,
            namespace,
            part.declaration,
            partials,
        );
        leave_part(binder, part_scope);
    }
}

/// The nested-type half of [`collect_type_tokens`], for the parts after the first.
#[allow(clippy::too_many_arguments)]
fn collect_nested_tokens(
    tokens: &mut Tokens,
    binder: &mut Binder,
    next_type: &mut u32,
    next_field: &mut u32,
    next_method: &mut u32,
    namespace: &str,
    declaration: &TypeDecl,
    partials: &PartialIndex<'_>,
) {
    let enclosing_full = lamella_binder::declared_full_name(namespace, declaration);
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
                    binder,
                    next_type,
                    next_field,
                    next_method,
                    &[],
                    core::slice::from_ref(nested.as_ref()),
                    &enclosing_full,
                    partials,
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_tokens(
    tokens: &mut Tokens,
    binder: &mut Binder,
    next_type: &mut u32,
    next_field: &mut u32,
    next_method: &mut u32,
    usings: &[UsingDirective],
    members: &[NamespaceMember],
    namespace: &str,
    partials: &PartialIndex<'_>,
) {
    let scope = binder.import_scope();
    for using in usings {
        binder.import_using(&using.kind);
    }
    let mut prefix = String::new();
    for part in namespace.split('.').filter(|part| !part.is_empty()) {
        if !prefix.is_empty() {
            prefix.push('.');
        }
        prefix.push_str(part);
        binder.import_namespace(&prefix);
    }
    for member in members {
        match member {
            NamespaceMember::Type(declaration) => {
                if is_later_partial_part(partials, namespace, declaration) {
                    continue;
                }
                let later = continuation_parts(partials, namespace, declaration).unwrap_or(&[]);
                collect_type_tokens(
                    tokens,
                    binder,
                    next_type,
                    next_field,
                    next_method,
                    namespace,
                    declaration,
                    false,
                    later,
                    partials,
                );
            }
            NamespaceMember::Namespace(declaration) => {
                let inner = join_namespace(namespace, &declaration.name);
                collect_tokens(
                    tokens,
                    binder,
                    next_type,
                    next_field,
                    next_method,
                    &declaration.usings,
                    &declaration.members,
                    &inner,
                    partials,
                );
            }
            NamespaceMember::Enum(declaration) => {
                let enum_ty = named_symbol(namespace, &declaration.name);
                *next_type += 1;
                tokens.insert_type(&enum_ty, Token::new(TYPE_DEF, *next_type));
                tokens.insert_enum(&enum_ty);
                let underlying = declaration
                    .base
                    .as_ref()
                    .map(bind_type)
                    .unwrap_or(TypeSymbol::Special(SpecialType::Int32));
                if let TypeSymbol::Special(special) = underlying {
                    tokens.insert_enum_underlying(&enum_ty, special);
                }
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
                    .map(|parameter| binder.canonicalize(&parameter_symbol(parameter)))
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
    binder.restore_import_scope(scope);
}

/// A named-type symbol from a dotted full name (e.g. `"Outer"` or `"N.Outer"`), matching
/// how the type was keyed in the token table.
fn type_symbol_from_dotted(full: &str) -> TypeSymbol {
    TypeSymbol::Named(full.split('.').map(Box::<str>::from).collect()).fold_builtin()
}

/// The METADATA name of a declared type: `` Box`1 `` for `class Box<T>` (ECMA-335 II.10.7.2),
/// and the name unchanged for every non-generic declaration.
///
/// **THE ASSEMBLER AND THE BINDER'S MODEL MUST AGREE ON THIS OR THEY NAME DIFFERENT TYPES.**
/// `declaration.rs` already mangles a source-declared generic type when it collects it, and a
/// definition read from a reference assembly arrives mangled -- so those two meet in one key
/// space and the assembler was the odd one out. Emitting the BARE name puts a `TypeDef` called
/// `Box` in the image while every lookup (`type_sig`'s `definition_of`, the model's
/// `get_by_symbol`) asks for `` Box`1 ``, and the miss is silent: the base class and the enclosing
/// type simply come back `None`.
fn declared_type_name(declaration: &TypeDecl) -> String {
    lamella_binder::metadata_type_name(&declaration.name, declaration.type_parameters.len())
}

/// [`declared_type_name`] as the symbol the token table and the model are keyed by.
fn declared_type_symbol(namespace: &str, declaration: &TypeDecl) -> TypeSymbol {
    named_symbol(namespace, &declared_type_name(declaration))
}

fn named_symbol(namespace: &str, name: &str) -> TypeSymbol {
    let mut parts: Vec<Box<str>> = Vec::new();
    if !namespace.is_empty() {
        for part in namespace.split('.') {
            parts.push(part.into());
        }
    }
    parts.push(name.into());
    TypeSymbol::Named(parts.into_boxed_slice()).fold_builtin()
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

    /// The WRITER's nested `TypeRef` and the READER's nesting walk are one rule seen from two
    /// sides, and this is the only place both are in scope -- so it is the only place they can be
    /// shown to agree.
    ///
    /// **THE TWO HALVES ARE NOT THE SAME CODE AND ONE OF THEM IS REACHED BY NOTHING ELSE WE RUN.**
    /// `token_type_symbol` walks a `TypeDef`'s `NestedClass` row and a `TypeRef`'s
    /// `ResolutionScope` chain, and the whole `foreach`-over-`List<T>` population reaches only the
    /// first: a reference assembly names its OWN nested types by `TypeDef`. The `TypeRef` arm is
    /// what a nested type in a THIRD assembly takes -- including one we emitted ourselves -- and
    /// with it disabled every generics-parity row stays green. Round-tripping an image through
    /// both is what asks it a question it can get wrong.
    ///
    /// A flat control rides along: a top-level reference must still decode by its own namespace,
    /// so a walk that fired on everything would be caught here rather than by its absence.
    #[test]
    fn a_nested_type_ref_this_writer_emits_decodes_back_under_its_enclosing_type() {
        use lamella_pe::{ImageBuilder, TypeSig, method_signature};

        let mut image = ImageBuilder::new("test.dll", "test");
        image.set_type_assembly("N.Outer`1", "Other");
        image.set_type_assembly("N.Outer`1.Inner", "Other");
        image.set_type_enclosing("N.Outer`1.Inner", "N.Outer`1");
        let inner = image.type_ref("N.Outer`1", "Inner");
        let flat = image.type_ref("N", "Plain");

        let object = image.object_type();
        image.add_type("App", "Holder", object, 0x0000_0001);
        let signature = method_signature(
            false,
            &[TypeSig::Class(inner), TypeSig::Class(flat)],
            &TypeSig::Void,
        );
        image.add_method("M", &signature, &[0x2A], 0x0006 | 0x0010, 0x0000, &[]);
        let pe = image.finish(Token::new(0, 0), true);

        let assembly = lamella_metadata::Assembly::read(&pe).expect("valid assembly");
        let mut model = Model::new();
        lamella_binder::load_assembly(&mut model, &assembly, "");
        let holder = model.get("App", "Holder").expect("the emitted type");
        let method = holder
            .methods
            .iter()
            .find(|m| &*m.name == "M")
            .expect("the emitted method");

        assert_eq!(
            method.parameters[0],
            TypeSymbol::Named(["N".into(), "Outer`1".into(), "Inner".into()].into()),
            "a nested TypeRef is named by its enclosing type, not by its own empty namespace"
        );
        assert_eq!(
            method.parameters[1],
            TypeSymbol::Named(["N".into(), "Plain".into()].into()),
            "and a top-level TypeRef still reads its own namespace"
        );
    }

    /// A constructed generic type lowers to `GENERICINST` over its DEFINITION's token.
    ///
    /// **THE DEFINITION IS FOUND BY ITS ARITY-MANGLED NAME, AND THAT IS THE JOIN THIS TEST
    /// EXISTS FOR.** The token table is keyed by a type's `Display`; a generic definition is
    /// recorded as `` Box`1 `` (II.10.7.2) because that is the one spelling both a source-collected
    /// and a reference-read definition arrive with. A lowering that looked up the BARE `Box` finds
    /// nothing here -- and the negative row below is what says so, because a build where the table
    /// happened to hold both spellings would pass the positive case either way.
    #[test]
    fn a_constructed_generic_lowers_to_genericinst_over_the_mangled_definition() {
        let mangled = TypeSymbol::Named(["App".into(), "Box`1".into()].into());
        let mut tokens = Tokens::new();
        tokens.insert_type(&mangled, Token::new(0x02, 4));

        let boxed_int = TypeSymbol::Instantiation {
            definition: ["App".into(), "Box".into()].into(),
            arguments: [TypeSymbol::Special(SpecialType::Int32)].into(),
        };
        let sig = type_sig(&tokens, &boxed_int).expect("an instantiation lowers");
        match sig {
            TypeSig::GenericInst {
                definition,
                arguments,
            } => {
                assert_eq!(*definition, TypeSig::Class(Token::new(0x02, 4)));
                assert_eq!(arguments, vec![TypeSig::Int32]);
            }
            other => panic!("expected GENERICINST, got {other:?}"),
        }

        let nested = TypeSymbol::Instantiation {
            definition: ["App".into(), "Box".into()].into(),
            arguments: [boxed_int.clone()].into(),
        };
        match type_sig(&tokens, &nested).expect("a nested instantiation lowers") {
            TypeSig::GenericInst { arguments, .. } => {
                assert!(
                    matches!(arguments.as_slice(), [TypeSig::GenericInst { .. }]),
                    "the argument must itself be an instantiation: {arguments:?}"
                );
            }
            other => panic!("expected GENERICINST, got {other:?}"),
        }

        let wrong_arity = TypeSymbol::Instantiation {
            definition: ["App".into(), "Box".into()].into(),
            arguments: [
                TypeSymbol::Special(SpecialType::Int32),
                TypeSymbol::Special(SpecialType::Int32),
            ]
            .into(),
        };
        assert!(type_sig(&tokens, &wrong_arity).is_err(), "a wrong arity must not resolve");
    }

    /// An instantiation gets a `TypeSpec` minted from its `GENERICINST` signature -- the token
    /// `ldelema`/`ldobj` need for a generic STRUCT array.
    ///
    /// **THE SEPARATION IS THE CLAIM, NOT THE MINTING.** A `TypeSpec` is keyed by its signature
    /// BYTES, so `Box<int>` and `Box<string>` must come back as DIFFERENT tokens. That is exactly
    /// what a Display-keyed or definition-folded scheme would get wrong, and getting it wrong is
    /// the cast hole: one token for two types means an `isinst` that answers for the wrong one and
    /// a static field that exists twice. A test that only checked "a token appeared" would pass
    /// against every broken version of this.
    ///
    /// **IT ASKS THROUGH `instruction_type_token`, AND THAT IS THE POINT RATHER THAN A DETAIL.**
    /// `type_token` is the DISPLAY-keyed map and an instantiation is deliberately absent from it:
    /// `Box<T>` written against a method's `T` and against its declaring type's `T` render the same
    /// string and are two rows. The row below asserts that absence, so a change that "fixed" a
    /// lookup by putting instantiations back under their display string fails here.
    #[test]
    fn each_instantiation_mints_its_own_typespec() {
        let mut image = ImageBuilder::new("test.dll", "test");
        let mut tokens = Tokens::new();
        let definition = TypeSymbol::Named(["App".into(), "Box`1".into()].into());
        let definition_token = image.type_ref("App", "Box`1");
        tokens.insert_type(&definition, definition_token);

        let of = |argument: TypeSymbol| TypeSymbol::Instantiation {
            definition: ["App".into(), "Box".into()].into(),
            arguments: [argument].into(),
        };
        let int = of(TypeSymbol::Special(SpecialType::Int32));
        let string = of(TypeSymbol::Special(SpecialType::String));

        mint_type_token(&mut image, &mut tokens, &int);
        mint_type_token(&mut image, &mut tokens, &string);

        let int_token = tokens
            .instruction_type_token(&int)
            .expect("Box<int> is minted");
        let string_token = tokens
            .instruction_type_token(&string)
            .expect("Box<string> is minted");
        assert_eq!(int_token.table(), lamella_metadata::tables::table::TYPE_SPEC);
        assert_eq!(string_token.table(), lamella_metadata::tables::table::TYPE_SPEC);
        assert_ne!(
            int_token, string_token,
            "two instantiations of one definition must not share a token"
        );
        assert_ne!(int_token, definition_token);
        assert_ne!(string_token, definition_token);

        assert!(
            tokens.type_token(&int).is_none(),
            "an instantiation's token belongs to the blob-keyed map alone"
        );

        mint_type_token(&mut image, &mut tokens, &int);
        assert_eq!(tokens.instruction_type_token(&int), Some(int_token));
    }

    /// **ONE SYMBOL, TWO SCOPES, TWO ROWS -- AND THE MINT AND THE LOOK-UP MUST AGREE ON WHICH.**
    /// `Box<T>` inside `T Unwrap<T>(...)` is `Box<!!0>`; the same `Box<T>` inside a `class Holder<T>`
    /// is `Box<!0>`. Both render `Box<T>`, so the two are told apart by the ambient body scope and
    /// by nothing else.
    ///
    /// This is the pair `each_instantiation_mints_its_own_typespec` cannot supply: there the two
    /// types differ in their ARGUMENTS, which a display key separates too. Here they are one symbol,
    /// which only the blob separates.
    ///
    /// **The `is_none` rows are the ones that fail on a scope-blind LOOK-UP.** A lookup that
    /// encoded in an empty scope would answer `None` for both and the `assert_ne!` alone would still
    /// pass on `(Some, Some)` never being reached -- so each token is asserted present first.
    #[test]
    fn one_symbol_in_two_scopes_is_two_typespec_rows() {
        let mut image = ImageBuilder::new("test.dll", "test");
        let mut tokens = Tokens::new();
        let definition = TypeSymbol::Named(["App".into(), "Box`1".into()].into());
        tokens.insert_type(&definition, image.type_ref("App", "Box`1"));

        let boxed_t = TypeSymbol::Instantiation {
            definition: ["App".into(), "Box".into()].into(),
            arguments: [TypeSymbol::Named(["T".into()].into())].into(),
        };

        let saved = tokens.enter_body_scope(&["T".into()], &[]);
        mint_type_token(&mut image, &mut tokens, &boxed_t);
        let as_mvar = tokens
            .instruction_type_token(&boxed_t)
            .expect("Box<!!0> is minted and found in the method scope");
        tokens.restore_body_scope(saved);

        let saved = tokens.enter_body_scope(&[], &["T".into()]);
        mint_type_token(&mut image, &mut tokens, &boxed_t);
        let as_var = tokens
            .instruction_type_token(&boxed_t)
            .expect("Box<!0> is minted and found in the declaring scope");
        tokens.restore_body_scope(saved);

        assert_ne!(
            as_mvar, as_var,
            "Box<!!0> and Box<!0> are different types and must not share a TypeSpec"
        );

        assert!(
            tokens.instruction_type_token(&boxed_t).is_none(),
            "Box<T> outside any generic scope names no type"
        );
    }

    /// **VALUE-NESS FOLLOWS THE DEFINITION, TOKENS DO NOT -- AND THE SECOND HALF IS WHY THIS
    /// TEST HAS A NEGATIVE ROW.** `Pair<int>` is a value type exactly when `` Pair`1 `` is one, so
    /// the PREDICATE resolves through the definition; but `Box<int>` and `Box<string>` are
    /// different types, so the TOKEN must not. A change that made `type_token` follow the
    /// definition too would pass every positive assertion here and fold two types onto one token --
    /// the cast hole `generics-identity-and-sharing` s2 names.
    #[test]
    fn value_ness_follows_the_definition_but_a_token_does_not() {
        let definition = TypeSymbol::Named(["App".into(), "Pair`1".into()].into());
        let mut tokens = Tokens::new();
        tokens.insert_type(&definition, Token::new(0x02, 9));
        tokens.insert_struct(&definition);

        let of = |argument: TypeSymbol| TypeSymbol::Instantiation {
            definition: ["App".into(), "Pair".into()].into(),
            arguments: [argument].into(),
        };
        let int = of(TypeSymbol::Special(SpecialType::Int32));
        let string = of(TypeSymbol::Special(SpecialType::String));

        assert!(tokens.is_struct(&int), "Pair<int> is a value type because Pair`1 is");
        assert!(tokens.is_struct(&string), "and so is Pair<string>");

        assert!(
            tokens.type_token(&int).is_none(),
            "an instantiation must not inherit its definition's token"
        );
        assert!(tokens.type_token(&string).is_none());

        let plain = TypeSymbol::Named(["App".into(), "Widget".into()].into());
        assert!(!tokens.is_struct(&plain), "an unrecorded plain type is not a struct");
        tokens.insert_struct(&plain);
        assert!(tokens.is_struct(&plain));
    }


    const GENERIC_FIXTURE: &str = "generic-methods.dll";
    const NESTED_FIXTURE: &str = "generic-params.dll";

    /// A csc-built reference assembly from `lamella-metadata`'s fixture directory, read at RUN time.
    ///
    /// **A MISSING FIXTURE SKIPS ONLY WHERE SKIPPING IS RIGHT.** If the fixture DIRECTORY exists
    /// and the file does not, that is a real breakage and this panics; only when the directory
    /// itself is gone does it return `None`. Without that split, deleting a fixture would turn
    /// every row below green.
    ///
    fn reference_fixture(name: &str) -> Option<Vec<u8>> {
        let directory = format!(
            "{}/../lamella-metadata/tests/fixtures",
            env!("CARGO_MANIFEST_DIR")
        );
        if !std::path::Path::new(&directory).is_dir() {
            eprintln!("{directory} absent (a stripped drop); skipping");
            return None;
        }
        let path = format!("{directory}/{name}");
        Some(std::fs::read(&path).unwrap_or_else(|error| {
            panic!("the fixture directory exists but {path} does not read: {error}")
        }))
    }

    /// `expression` parsed inside a `Main` under C# 2 and bound against the generic-method fixture.
    /// `None` when the fixture is unavailable -- see [`reference_fixture`].
    fn bound_expression(expression: &str) -> Option<BoundExpr> {
        let reference = reference_fixture(GENERIC_FIXTURE)?;
        Some(bound_expression_against(&reference, expression))
    }

    /// `expression` parsed inside a `Main` under C# 2 and bound against `reference`.
    ///
    /// The DIALECT is load-bearing in the parser, not only in the binder: under the default C# 1.0
    /// the parser skips a type-argument list, so `Id<int>(1)` arrives as a chain of comparisons and
    /// every row below would measure a different program.
    fn bound_expression_against(reference: &[u8], expression: &str) -> BoundExpr {
        let source = format!("class C {{ static void Main() {{ {expression}; }} }}\n");
        let options = LexOptions {
            version: LanguageVersion::CSharp2,
            ..LexOptions::default()
        };
        let unit = parse_compilation_unit_with(&source, options).unit;
        let NamespaceMember::Type(declaration) = &unit.members[0] else {
            panic!("the fixture source declares a class");
        };
        let Member::Method { body: Some(body), .. } = &declaration.members[0] else {
            panic!("the fixture source declares a method with a body");
        };
        let StmtKind::Block(statements) = &body.kind else {
            panic!("a method body is a block");
        };
        let StmtKind::Expression(expr) = &statements[0].kind else {
            panic!("the body's one statement is an expression statement");
        };
        let assembly = Assembly::read(reference).expect("the fixture parses");
        let mut model = Model::new();
        load_assembly(&mut model, &assembly, "");
        Binder::with_model(model).bind_expression(expr)
    }

    /// The token operand of the `call`/`callvirt` the expression lowers to.
    fn emitted_call_token(
        expr: &BoundExpr,
        image: &mut ImageBuilder,
        tokens: &mut Tokens,
    ) -> Result<Token, crate::EmitError> {
        mint_in_expr(expr, image, tokens);
        let mut out = Vec::new();
        crate::expr::emit_expression(expr, &crate::frame::Frame::empty(), tokens, &mut out)?;
        let call = out
            .iter()
            .rev()
            .find(|instruction| {
                matches!(
                    instruction.opcode,
                    lamella_cil::Opcode::Call | lamella_cil::Opcode::Callvirt
                )
            })
            .expect("the expression lowers to a call");
        match call.operand {
            lamella_cil::Operand::Token(token) => Ok(token),
            ref other => panic!("a call names its target by token, got {other:?}"),
        }
    }

    /// **THE ROW THE EMIT STAGE EXISTS FOR, AND ITS EVIDENCE IS THE SEPARATION.** A generic call
    /// must name its own `MethodSpec`, never the definition's token -- because emitting the
    /// definition still produces a call that BINDS, to the open method, with `!!0` unsubstituted.
    /// That outcome throws no error and writes valid metadata, so nothing but this assertion
    /// distinguishes it. `lamella-pe`'s `ImageBuilder::method_spec` doc records the same trap from
    /// the writer's side.
    #[test]
    fn a_generic_call_names_its_own_method_spec_and_not_the_definition() {
        let Some(expr) = bound_expression("Fixture.Util.Id<int>(1)") else { return };
        let mut image = ImageBuilder::new("test.dll", "test");
        let mut tokens = Tokens::new();
        let site = emitted_call_token(&expr, &mut image, &mut tokens).expect("a generic call emits");

        assert_eq!(
            site.table(),
            lamella_metadata::tables::table::METHOD_SPEC,
            "the call must name a MethodSpec row, not the method it instantiates"
        );
        let definition = tokens
            .method(
                &named_symbol("Fixture", "Util"),
                "Id",
                &[TypeSymbol::Named(["T".into()].into())],
            )
            .expect("the open definition is minted under its OPEN parameter key");
        assert_eq!(definition.table(), lamella_metadata::tables::table::MEMBER_REF);
        assert_ne!(site, definition);
    }

    /// Two instantiations of one definition are two rows over one `MemberRef`, and the arguments
    /// survive the blob round trip.
    ///
    /// **THE `assert_ne!` IS THE LOAD-BEARING LINE.** An implementation that interned the row
    /// by (declaring type, name) -- the shape every other member in this table uses -- gives both
    /// sites one token and one argument, and passes every "a MethodSpec appeared" assertion
    /// perfectly. It is the same collapse a shared `TypeSpec` would be, reached through the method
    /// table instead of the type one.
    #[test]
    fn two_instantiations_of_one_method_are_two_rows_over_one_member_ref() {
        let Some(int_call) = bound_expression("Fixture.Util.Id<int>(1)") else { return };
        let Some(string_call) = bound_expression("Fixture.Util.Id<string>(\"x\")") else { return };
        let mut image = ImageBuilder::new("test.dll", "test");
        let mut tokens = Tokens::new();
        let int_site =
            emitted_call_token(&int_call, &mut image, &mut tokens).expect("Id<int> emits");
        let string_site =
            emitted_call_token(&string_call, &mut image, &mut tokens).expect("Id<string> emits");
        assert_ne!(int_site, string_site, "two instantiations are two rows");

        let pe = image.finish(Token::new(0, 0), true);
        let assembly = lamella_metadata::Assembly::read(&pe).expect("the emitted image parses");

        let definition = assembly
            .method_spec_method(int_site)
            .expect("a MethodSpec names the method it instantiates");
        assert_eq!(assembly.method_spec_method(string_site), Some(definition));

        assert_eq!(
            assembly.method_spec_instantiation(int_site),
            Some(alloc::vec![lamella_metadata::SigType::I4])
        );
        assert_eq!(
            assembly.method_spec_instantiation(string_site),
            Some(alloc::vec![lamella_metadata::SigType::String])
        );

        let member = assembly
            .member_ref(definition.row())
            .expect("the definition is a MemberRef");
        assert_eq!(member.name(), Some("Id"));
        assert_eq!(
            member.signature_blob(),
            &[0x10, 0x01, 0x01, 0x1E, 0x00, 0x1E, 0x00],
            "the open definition is GENERIC over one parameter and spelled with !!0"
        );
    }

    /// **TWO TYPE PARAMETERS, IN ORDER -- AT ARITY ONE EVERY NUMBERING SCHEME AGREES.** `!!0` is
    /// a POSITION, and the row above cannot tell a correct one from a hardcoded zero, from the
    /// declaring type's numbering, or from a reversed list. `T Two<T,U>(T,U)` separates all four in
    /// the signature AND in the instantiation blob, which are numbered from opposite ends of the
    /// same fact.
    #[test]
    fn a_second_type_parameter_is_numbered_by_its_own_position() {
        let Some(expr) = bound_expression("Fixture.Util.Two<int, string>(1, \"x\")") else { return };
        let mut image = ImageBuilder::new("test.dll", "test");
        let mut tokens = Tokens::new();
        let site = emitted_call_token(&expr, &mut image, &mut tokens).expect("Two<int,string> emits");

        let pe = image.finish(Token::new(0, 0), true);
        let assembly = lamella_metadata::Assembly::read(&pe).expect("the emitted image parses");

        assert_eq!(
            assembly.method_spec_instantiation(site),
            Some(alloc::vec![
                lamella_metadata::SigType::I4,
                lamella_metadata::SigType::String
            ])
        );
        let definition = assembly
            .method_spec_method(site)
            .expect("a MethodSpec names the method it instantiates");
        let member = assembly
            .member_ref(definition.row())
            .expect("the definition is a MemberRef");
        assert_eq!(member.name(), Some("Two"));
        assert_eq!(
            member.signature_blob(),
            &[0x10, 0x02, 0x02, 0x1E, 0x00, 0x1E, 0x00, 0x1E, 0x01],
            "the second parameter is !!1 -- its own declaration position, not the first's"
        );
    }

    /// **A GENERIC CALL WITH NO `MethodSpec` IS REFUSED, NEVER EMITTED AGAINST THE DEFINITION.**
    /// This is the control for the fallback that would otherwise be the natural thing to write --
    /// "look up the site, else the definition" -- and that fallback is not a degradation, it is the
    /// silent wrong program: the call binds to the open method and `!!0` is never substituted. The
    /// definition IS minted here and is a perfectly good token, so an emitter willing to use it
    /// would succeed and this row is what says it must not.
    #[test]
    fn a_generic_call_without_its_row_refuses_rather_than_naming_the_definition() {
        let Some(expr) = bound_expression("Fixture.Util.Id<int>(1)") else { return };
        let BoundExprKind::Call { method: Some(method), .. } = &expr.kind else {
            panic!("the expression bound to a call");
        };
        let instantiation = method
            .instantiation
            .as_deref()
            .expect("the binder recorded the call site's type arguments");

        let mut image = ImageBuilder::new("test.dll", "test");
        let mut tokens = Tokens::new();
        mint_generic_definition_ref(method, instantiation, &mut image, &mut tokens)
            .expect("the definition mints");

        let mut out = Vec::new();
        let emitted =
            crate::expr::emit_expression(&expr, &crate::frame::Frame::empty(), &tokens, &mut out);
        assert!(
            emitted.is_err(),
            "a generic call with no MethodSpec must refuse, not fall back to the definition"
        );
    }

    /// **A GENERIC CALL WHOSE DEFINITION HAS NO METHOD TOKEN IS REFUSED**, even when its declaring
    /// type is a this-module `TypeDef`.
    ///
    /// **A THIS-MODULE DECLARING TYPE IS NOT ITSELF A REFUSAL.** `MethodSpec.Method` is a
    /// `MethodDefOrRef` (II.22.29), so a generic method this module declares is named by its own
    /// `MethodDef` -- its signature carries the GENERIC convention and its `GenericParam` rows, so
    /// the spec and the definition agree. What this row measures is the REMAINING refusal: the
    /// definition's token has to EXIST, and here only the TYPE is registered.
    ///
    /// The positive half -- a this-module generic call that DOES mint -- is not expressible in this
    /// synthetic setup, because it needs the real pre-pass to have written the `MethodDef`. It is
    /// covered end to end instead: `lcsc /langversion:ISO-2` compiles `Id<string>("ok")` against a
    /// this-module `static T Id<T>(T x)`.
    #[test]
    fn a_generic_call_with_no_definition_token_is_refused_rather_than_wrongly_instantiated() {
        let Some(expr) = bound_expression("Fixture.Util.Id<int>(1)") else { return };
        let mut image = ImageBuilder::new("test.dll", "test");
        let mut tokens = Tokens::new();
        tokens.insert_type(&named_symbol("Fixture", "Util"), Token::new(TYPE_DEF, 2));

        mint_in_expr(&expr, &mut image, &mut tokens);
        let mut out = Vec::new();
        assert!(
            crate::expr::emit_expression(&expr, &crate::frame::Frame::empty(), &tokens, &mut out)
                .is_err(),
            "a MethodSpec over a non-generic MethodDef signature is refused, not written"
        );
    }

    /// **AN *IMPORTED* GENERIC METHOD ON A GENERIC TYPE IS STILL REFUSED, AND THE REASON MOVED.**
    /// The emitter could already write this shape for a THIS-MODULE declaration
    /// (`a_generic_method_on_a_generic_type_declared_here`); what blocked the IMPORTED spelling was
    /// one layer earlier and is not a generics-emission question at all. `reference::sigtype_to_symbol`
    /// must decode `ELEMENT_TYPE_VAR` as well as `ELEMENT_TYPE_MVAR`: with an arm for only one,
    /// `Fixture.Holder<TOuter>.Echo<TMethod>(TMethod) -> TOuter` arrives as
    /// `parameters: [Named(["TMethod"])]` with `return_type: Error` -- **one rule, two numbering
    /// spaces, the case written in one of them.**
    ///
    /// **BOTH HALVES ARE ASSERTED HERE BECAUSE ONLY THE PAIR SEPARATES THE TWO SPACES.** A decode
    /// that answered every numbered parameter from the METHOD's list would give this signature
    /// `[Named(["TMethod"])]` and `Named(["TMethod"])` -- one right and one wrong, and the parameter
    /// row alone cannot tell. `TOuter` in the return is what says `!0` was numbered against the
    /// DECLARING type's list.
    #[test]
    fn an_imported_generic_method_on_a_generic_type_decodes_both_numbering_spaces() {
        let Some(reference) = reference_fixture(NESTED_FIXTURE) else { return };
        let expr = bound_expression_against(
            &reference,
            "new Fixture.Holder<int>().Echo<string>(\"x\")",
        );
        let BoundExprKind::Call { method: Some(method), .. } = &expr.kind else {
            panic!("the expression bound to a call");
        };
        assert!(
            method.instantiation.is_some(),
            "the call site's own type argument is recorded"
        );
        let declaring = method
            .declaring_instantiation
            .as_deref()
            .expect("the DECLARING type's open form is recovered, or the `!n` half has no owner");
        assert_eq!(
            declaring.type_parameters,
            [Box::<str>::from("TOuter")],
            "the declaring type's parameter NAMES are what `!0` is numbered against"
        );
        assert_eq!(
            declaring.parameters,
            [TypeSymbol::Named([Box::from("TMethod")].into())],
            "MVAR 0 decodes to the METHOD's declared name"
        );
        assert_eq!(
            declaring.return_type,
            TypeSymbol::Named([Box::from("TOuter")].into()),
            "VAR 0 decodes to the DECLARING TYPE's declared name, not the method's and not Error"
        );

        let mut image = ImageBuilder::new("test.dll", "test");
        let mut tokens = Tokens::new();
        let spec = emitted_call_token(&expr, &mut image, &mut tokens)
            .expect("an imported generic method on a generic type emits");
        assert_eq!(spec.table(), lamella_metadata::tables::table::METHOD_SPEC);
        let built = image.finish(Token::new(0, 0), true);
        let assembly = Assembly::read(&built).expect("the image parses");
        let definition = assembly
            .method_spec_method(spec)
            .expect("a MethodSpec names the method it instantiates");
        let member = assembly
            .member_ref(definition.row())
            .expect("the MemberRef row is readable");
        assert_eq!(
            member.parent().table(),
            TYPE_SPEC,
            "the parent is the instantiation's TypeSpec, so `<int>` is not lost"
        );
        assert_eq!(
            member.signature_blob(),
            &[0x30, 0x01, 0x01, 0x13, 0x00, 0x1e, 0x00],
            "GENERIC | HAS_THIS, GenParamCount 1, 1 parameter, VAR 0 return, MVAR 0 parameter"
        );
    }

    /// **A GENERIC METHOD ON A GENERIC *TYPE* HAS BOTH NUMBERING SPACES LIVE IN ONE SIGNATURE**,
    /// and this is the smallest program that does. csc's own bytes for `Echo` are
    /// `30 01 01 13 00 1e 00`: the GENERIC convention, `GenParamCount` 1, one parameter, a `!0`
    /// (VAR 0) return beside an `!!0` (MVAR 0) parameter.
    ///
    /// **THE SIGNATURE BYTES ARE THE ASSERTION, BECAUSE EVERY WRONG ANSWER DECODES CLEANLY.** A
    /// `TypeRef` parent naming `` Holder`1 `` loses the `<int>`; `!!0` written where `!0` was meant
    /// names a different type; and under a `Holder<int>` whose argument is `int`, a run cannot tell
    /// the two apart at all when only one of them appears. `0x13` and `0x1E` are one byte apart.
    #[test]
    fn a_generic_method_on_a_generic_type_declared_here_names_both_numbering_spaces() {
        let image = image_of_gated_source(
            "namespace App {
                 public class Holder<TOuter> {
                     public TOuter Echo<TMethod>(TMethod m) { return default(TOuter); }
                 }
                 public class Program {
                     public static int Main() { return new Holder<int>().Echo<string>(\"x\"); }
                 }
             }\n",
        );
        let assembly = Assembly::read(&image).expect("the image parses");
        let echo = assembly
            .type_defs()
            .find(|t| t.name().is_some_and(|n| n.name == "Holder`1"))
            .expect("Holder`1 is in the image")
            .methods()
            .find(|m| m.name() == Some("Echo"))
            .expect("Holder`1 declares Echo");
        assert_eq!(
            echo.signature_blob(),
            &[0x30, 0x01, 0x01, 0x13, 0x00, 0x1e, 0x00],
            "GENERIC | HAS_THIS, GenParamCount 1, 1 parameter, VAR 0 return, MVAR 0 parameter"
        );

        let members = member_refs_of(&assembly);
        let echo_ref = members
            .iter()
            .find(|(_, name, _)| name == "Echo")
            .expect("the call site mints a MemberRef for Echo");
        assert_eq!(
            echo_ref.0, TYPE_SPEC,
            "the parent is the instantiation's TypeSpec, so `<int>` is not lost: {members:?}"
        );
        assert_eq!(
            echo_ref.2,
            [0x30, 0x01, 0x01, 0x13, 0x00, 0x1e, 0x00],
            "II.23.2.1 wants the DEFINITION's signature -- `!0`, never the substituted int32"
        );
    }

    /// **FOUR INSTRUCTIONS NAME A CONSTRUCTED GENERIC TYPE, AND THEY ASK ONE QUESTION.**
    /// `newarr`, `castclass`, `isinst` and `ldtoken` all ask `instruction_type_token` for
    /// an operand. Let an instantiation reach that question through no arm and each
    /// refused with a message about its own opcode and the single missing case read as four
    /// unrelated unimplemented features.
    ///
    /// **THE TABLE IS THE INSTRUMENT, NOT ANY ROW OF IT.** A fix written at one opcode passes that
    /// opcode's row and leaves the others exactly as they were, which is the shape
    /// `a-rule-with-several-implementations-gains-a-new-case-in-none-of-them` records. Each row
    /// asserts the BLOB, because a `castclass` naming the DEFINITION accepts every instantiation --
    /// which decodes cleanly and answers wrongly.
    ///
    /// **`ldtoken` IS THE FOURTH OPCODE AND IT IS NOT IN THIS TABLE**, because `typeof` lowers to
    /// a call of `System.Type::GetTypeFromHandle` and this helper compiles against no references at
    /// all. It is covered where a corlib exists: the generics-position probe compiles AND RUNS
    /// `typeof(Box<T>).FullName` against csc, which is the row that reads the type ARGUMENT back
    /// (`.Name` is `` Box`1 `` for every instantiation and cannot see it).
    #[test]
    fn every_instruction_naming_a_constructed_generic_gets_its_typespec() {
        let image = image_of_gated_source(
            "namespace App {
                 public class Box<T> { public T Value; }
                 public class Program {
                     public static int Array<T>() { Box<T>[] a = new Box<T>[3]; a[0] = null; return 0; }
                     public static T Cast<T>(object o) { return ((Box<T>)o).Value; }
                     public static bool Test<T>(object o) { return o is Box<T>; }
                     public static int Main() { return 0; }
                 }
             }\n",
        );
        let assembly = Assembly::read(&image).expect("the image parses");
        let program = assembly
            .type_defs()
            .find(|t| t.name().is_some_and(|n| n.name == "Program"))
            .expect("Program is in the image");
        let boxed = assembly
            .type_defs()
            .find(|t| t.name().is_some_and(|n| n.name == "Box`1"))
            .expect("Box`1 is in the image");
        let wanted = lamella_metadata::SigType::GenericInst {
            definition: alloc::boxed::Box::new(lamella_metadata::SigType::Class(boxed.token())),
            arguments: alloc::vec![lamella_metadata::SigType::MVar(0)],
        };

        for (method_name, opcode) in [
            ("Array", lamella_cil::Opcode::Newarr),
            ("Cast", lamella_cil::Opcode::Castclass),
            ("Test", lamella_cil::Opcode::Isinst),
        ] {
            let operand = program
                .methods()
                .find(|m| m.name() == Some(method_name))
                .unwrap_or_else(|| panic!("Program declares {method_name}"))
                .body()
                .unwrap_or_else(|| panic!("{method_name} has a body"))
                .code
                .iter()
                .find_map(|instruction| match (instruction.opcode, &instruction.operand) {
                    (found, lamella_cil::Operand::Token(token)) if found == opcode => Some(*token),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{method_name} lowers through {opcode:?}"));
            assert_eq!(
                operand.table(),
                TYPE_SPEC,
                "{opcode:?} names a TypeSpec; a TypeRef here is the phantom `T` again"
            );
            assert_eq!(
                assembly.type_spec_signature(operand),
                Some(wanted.clone()),
                "{opcode:?} in {method_name} must name `Box<!!0>`, not the definition and not `Box<!0>`"
            );
        }

        let refs: Vec<String> = assembly
            .type_refs()
            .filter_map(|r| r.name())
            .map(|n| String::from(n.name))
            .collect();
        assert!(
            !refs.iter().any(|name| name == "T"),
            "no TypeRef is minted for the type parameter: {refs:?}"
        );
    }

    /// Compiles `source` to an image AS IF the generics gate were already lifted.
    ///
    /// **THIS DOES NOT SUBVERT THE EMIT-TRUST BARRIER, IT STATES ITS PRECONDITION.** The barrier
    /// exists so a half-built feature cannot reach emit, and `Feature::Generics` is exactly such a
    /// feature -- so a generic declaration draws `LAM0001` and `compile_source` returns no image.
    /// The assertion below is the substitute: **the unit is bound FOR REAL and every diagnostic it
    /// draws must be that one gate.** A generic declaration that also drew CS0246, or a cascade, or
    /// anything else fails here rather than being emitted -- which is more than a clean bind proves,
    /// because a clean bind cannot happen at all yet.
    ///
    /// It is also the only configuration in which the declaration stage can be observed, and it
    /// disappears the day the gate lifts: this helper should then become an ordinary
    /// `compile_source` call, and the test that still passes is the one that was measuring the
    /// right thing.
    /// The `Param` rows of the single method named `name`, as `(sequence, flags, constant)` --
    /// the shape the optional-parameter parity harness compares against csc, asserted here so a
    /// regression is caught by `cargo test` and not only by a tool that needs an SDK beside it.
    fn param_rows_of(image: &[u8], name: &str) -> Vec<(u32, u32, Option<(u8, Vec<u8>)>)> {
        let assembly = lamella_metadata::Assembly::read(image).expect("the image parses");
        let method = assembly
            .type_defs()
            .flat_map(|t| t.methods().collect::<Vec<_>>())
            .find(|m| m.name() == Some(name))
            .expect("the method is in the image");
        method
            .params()
            .filter(|p| p.sequence() != 0)
            .map(|p| {
                let constant = p.constant().map(|c| (constant_tag(&c), constant_blob(&c)));
                (p.sequence(), p.flags(), constant)
            })
            .collect()
    }

    /// A `ConstantValue`'s element-type byte and little-endian blob, for the assertions below.
    /// Only the shapes those tests use; anything else is a test bug rather than a compiler one.
    fn constant_tag(value: &lamella_metadata::ConstantValue) -> u8 {
        match value {
            lamella_metadata::ConstantValue::I4(_) => 0x08,
            lamella_metadata::ConstantValue::String(_) => 0x0e,
            lamella_metadata::ConstantValue::Null => 0x12,
            other => panic!("unexpected constant shape in this test: {other:?}"),
        }
    }

    fn constant_blob(value: &lamella_metadata::ConstantValue) -> Vec<u8> {
        match value {
            lamella_metadata::ConstantValue::I4(n) => n.to_le_bytes().to_vec(),
            lamella_metadata::ConstantValue::String(units) => {
                units.iter().flat_map(|u| u.to_le_bytes()).collect()
            }
            lamella_metadata::ConstantValue::Null => alloc::vec![0u8; 4],
            other => panic!("unexpected constant shape in this test: {other:?}"),
        }
    }

    /// Compiles `source` at C# 4 -- the rung an optional parameter needs -- and returns the image.
    /// The C# 2 helper beside this one cannot serve: the parser gates `= expr` below 4.
    fn image_of_csharp4_source(source: &str) -> Vec<u8> {
        let options = LexOptions {
            version: LanguageVersion::CSharp4,
            ..LexOptions::default()
        };
        let parsed = parse_compilation_unit_with(source, options);
        assert!(
            parsed.diagnostics.iter().all(|d| d.severity() != Severity::Error),
            "the source must PARSE cleanly under C# 4: {:?}",
            parsed.diagnostics
        );
        let units = alloc::vec![parsed.unit];
        let references: Vec<Assembly> = Vec::new();
        let diagnostics = lamella_binder::bind_compilation_unit_with_dialect(
            &units[0],
            &references,
            false,
            LanguageVersion::CSharp4,
        );
        assert!(
            diagnostics.is_empty(),
            "the source must BIND cleanly at C# 4: {:?}",
            diagnostics.iter().map(|d| alloc::format!("{}", d.kind)).collect::<Vec<_>>()
        );
        let program = ValidatedProgram::from_clean_bind(&units, &references, false)
            .expect("the barrier is minted from the assertion above");
        let (image, _) = build_image(&program, "test.dll", "test", None, false, false)
            .expect("an optional parameter emits");
        image
    }

    /// AN OPTIONAL PARAMETER IS `Optional|HasDefault` PLUS A `Constant` ROW -- `0x1010`, which is
    /// what csc writes for every default that IS a constant (measured over 34 forms in
    /// the optional-parameter parity harness).
    ///
    /// **RED-PROOF: drop `PARAM_HAS_DEFAULT` from `emit_declared_parameter_metadata` and the flag
    /// assertion fails; drop the `add_constant` call and the row assertion fails.** Before this
    /// landed the whole declaration was refused by the feature gate, so both read nothing at all.
    #[test]
    fn an_optional_parameter_carries_its_flags_and_its_constant_row() {
        let image = image_of_csharp4_source(
            "public class P { public void M(int a, int b = 7, string s = \"hi\") { } }",
        );
        let rows = param_rows_of(&image, "M");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].1, 0x0000, "a required parameter carries no bits");
        assert_eq!(rows[0].2, None);
        assert_eq!(rows[1].1, 0x1010, "Optional | HasDefault");
        assert_eq!(rows[1].2, Some((0x08, alloc::vec![7, 0, 0, 0])));
        assert_eq!(rows[2].1, 0x1010);
        assert_eq!(
            rows[2].2,
            Some((0x0e, alloc::vec![b'h', 0, b'i', 0])),
            "a Constant-table string is raw UTF-16, not a length-prefixed SerString"
        );
    }

    /// csc SPELLS FOUR DIFFERENT DEFAULTS AS A NULL REFERENCE, and the fourth is the one no
    /// reading of the standard predicts: `default(S)` on a STRUCT.
    ///
    #[test]
    fn every_shape_csc_spells_as_a_null_reference_is_spelled_that_way_here() {
        let image = image_of_csharp4_source(
            "public struct S { public int N; } \
             public class P { public void M(string a = null, string b = default(string), \
             S d = default(S)) { } }",
        );
        let rows = param_rows_of(&image, "M");
        assert_eq!(rows.len(), 3);
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(row.1, 0x1010, "parameter {index}");
            assert_eq!(
                row.2,
                Some((0x12, alloc::vec![0u8; 4])),
                "parameter {index} must be a NullReference constant -- including the struct"
            );
        }
    }

    /// AN ENUM DEFAULT IS ITS UNDERLYING INTEGER'S CONSTANT, not the enum's -- and the value has
    /// to survive the model-aware fold that resolves `E.B` against another type.
    ///
    /// **RED-PROOF: remove the `MemberAccess` arm from `parameter_default_in_model` and the
    /// constant goes to `None` while the flags keep `Optional` -- exactly the half-emitted shape
    /// that would let a caller omit an argument this compiler cannot supply.**
    #[test]
    fn an_enum_member_default_folds_to_its_underlying_integer() {
        let image = image_of_csharp4_source(
            "public enum E { A, B, C } public class P { public void M(E e = E.C) { } }",
        );
        let rows = param_rows_of(&image, "M");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, 0x1010);
        assert_eq!(rows[0].2, Some((0x08, alloc::vec![2, 0, 0, 0])));
    }

    fn image_of_gated_source(source: &str) -> Vec<u8> {
        let options = LexOptions {
            version: LanguageVersion::CSharp2,
            ..LexOptions::default()
        };
        let parsed = parse_compilation_unit_with(source, options);
        assert!(
            parsed.diagnostics.iter().all(|d| d.severity() != Severity::Error),
            "the source must PARSE cleanly under C# 2: {:?}",
            parsed.diagnostics
        );
        let units = alloc::vec![parsed.unit];
        let references: Vec<Assembly> = Vec::new();
        for diagnostic in lamella_binder::bind_compilation_unit_with_dialect(
            &units[0],
            &references,
            false,
            LanguageVersion::CSharp2,
        ) {
            assert_eq!(
                diagnostic.code(),
                1,
                "the ONLY diagnostic may be the generics gate: {}",
                diagnostic.kind
            );
        }
        let program = ValidatedProgram::from_clean_bind(&units, &references, false)
            .expect("the barrier is minted from the assertion above, not from a bare `false`");
        let (image, _) = build_image(&program, "test.dll", "test", None, false, false)
            .expect("a generic declaration emits");
        image
    }

    /// A property reached through TWO different instantiations names TWO different accessors.
    ///
    /// **THE ASSERTION IS THAT THE TOKENS DIFFER, AND NOTHING WEAKER WOULD HAVE CAUGHT THIS.**
    /// Emitting `callvirt` at the SAME open `get_Item` MethodDef for both compiles, runs, and
    /// returns `!0` unsubstituted from one shared method. Every
    /// "does it compile" row was green against that, which is why the queue recorded the case as
    /// "refused rather than named wrongly": the refusal path existed but was unreachable, because
    /// with the declaring type erased the token lookup SUCCEEDED against the definition.
    ///
    /// A single instantiation cannot see it either -- one call to one method looks correct however
    /// the token was chosen. Two are the instrument.
    #[test]
    fn a_property_on_two_instantiations_names_two_accessors() {
        let image = image_of_gated_source(
            "public class B<T> { private T f; \
             public T Item { get { return f; } set { f = value; } } }\n\
             public class C { public static void M(B<int> bi, B<string> bs) { \
             int x = bi.Item; bi.Item = x; string s = bs.Item; bs.Item = s; } }\n",
        );
        let assembly = lamella_metadata::Assembly::read(&image).expect("the image parses");
        let calls: alloc::vec::Vec<lamella_token::Token> = assembly
            .type_defs()
            .filter(|t| t.name().is_some_and(|n| n.name == "C"))
            .flat_map(|t| t.methods().collect::<alloc::vec::Vec<_>>())
            .filter_map(|m| m.body())
            .flat_map(|body| {
                body.code
                    .iter()
                    .filter_map(|instruction| match instruction.operand {
                        lamella_cil::Operand::Token(token) => Some(token),
                        _ => None,
                    })
                    .collect::<alloc::vec::Vec<_>>()
            })
            .collect();
        let member_refs: alloc::collections::BTreeSet<u32> =
            calls.iter().filter(|t| t.table() == 0x0A).map(|t| t.row()).collect();
        assert!(
            member_refs.len() >= 4,
            "each instantiation's getter and setter needs its OWN MemberRef; got {member_refs:?} \
             from {calls:?}"
        );
        assert!(
            !calls.iter().any(|t| t.table() == 0x06),
            "no accessor may be called at the definition's own MethodDef -- that is the erased \
             form, identical for every instantiation: {calls:?}"
        );
    }

    /// [`image_of_gated_source`] with a csc-built REFERENCE assembly behind it, so an IMPORTED
    /// generic definition is in scope.
    ///
    /// **A SEPARATE HARNESS BECAUSE A SAME-MODULE GENERIC CANNOT EXERCISE THE PATH UNDER TEST.**
    /// A this-module `Box`1` already has a `TypeDef` in the token table, so `type_sig` finds it and
    /// no minting is needed -- which is exactly why the imported case went unnoticed. The reference
    /// is what makes the definition need a `TypeRef` that something has to mint.
    /// `None` when the fixture is unavailable -- see [`reference_fixture`], whose note explains why
    /// `include_bytes!` is the one thing this must not do.
    fn image_of_source_against_generic_fixture(source: &str) -> Option<Vec<u8>> {
        let fixture = reference_fixture(NESTED_FIXTURE)?;
        Some(image_of_source_against(&fixture, source))
    }

    /// Compiles `source` under C# 2 with no references and returns the image, like
    /// [`image_of_gated_source`] -- but named, so a reference's `[assembly: InternalsVisibleTo]`
    /// has something to match and a consumer can be compiled twice under two different names.
    fn named_image_of(assembly_name: &str, source: &str) -> Vec<u8> {
        let options = LexOptions {
            version: LanguageVersion::CSharp2,
            ..LexOptions::default()
        };
        let parsed = parse_compilation_unit_with(source, options);
        assert!(
            parsed.diagnostics.iter().all(|d| d.severity() != Severity::Error),
            "the source must PARSE cleanly under C# 2: {:?}",
            parsed.diagnostics
        );
        let compiled = crate::compile_unit_with_references(
            &parsed.unit,
            &alloc::format!("{assembly_name}.dll"),
            assembly_name,
            &[],
        );
        assert!(
            !compiled.diagnostics.iter().any(Diagnostic::is_error),
            "the library must COMPILE: {:?}",
            compiled.diagnostics
        );
        compiled.image.expect("a clean bind emits an image")
    }

    /// The diagnostic codes a consumer named `assembly_name` gets for `source` against `library`.
    fn consumer_codes(assembly_name: &str, library: &[u8], source: &str) -> Vec<u16> {
        let options = LexOptions {
            version: LanguageVersion::CSharp2,
            ..LexOptions::default()
        };
        let parsed = parse_compilation_unit_with(source, options);
        assert!(
            parsed.diagnostics.iter().all(|d| d.severity() != Severity::Error),
            "the consumer must PARSE cleanly: {:?}",
            parsed.diagnostics
        );
        let references = alloc::vec![Assembly::read(library).expect("the library parses")];
        let compiled = crate::compile_unit_with_references(
            &parsed.unit,
            &alloc::format!("{assembly_name}.dll"),
            assembly_name,
            &references,
        );
        compiled
            .diagnostics
            .iter()
            .filter(|d| d.is_error())
            .map(|d| d.code)
            .collect()
    }

    /// **A REFERENCED ASSEMBLY'S `internal` MEMBERS ARE NOT IMPORTED, SO NAMING ONE IS AN ABSENCE
    /// RATHER THAN A PROTECTION LEVEL.** csc reads a reference at `MetadataImportOptions.Public`
    /// -- public, `protected` and `protected internal` only -- so a cross-assembly `internal`
    /// constructor is `CS1729` (*no constructor takes 1 arguments*) and a cross-assembly `internal`
    /// method is `CS1061`, both measured. lcsc imported them and reported `CS0122` instead, which
    /// is the right family and the wrong sentence; and because nothing then STOPPED the call, an
    /// `internal` constructor reached across an assembly boundary compiled clean.
    ///
    /// **THIS REACHES REAL LIBRARIES, NOT ONLY CONTRIVED ONES.** A library in this tree compiles
    /// against such a constructor and csc refuses the same source, so the two disagree on whether
    /// the assembly builds at all -- which is what an A/B oracle against csc exists to catch.
    #[test]
    fn a_reference_s_internal_members_are_not_imported() {
        let library = named_image_of(
            "Lib",
            "namespace Lib {
                 public class Result {
                     internal Result(int n) { }
                     internal int Peek() { return 0; }
                     public int Open() { return 1; }
                 }
             }\n",
        );
        assert_eq!(
            consumer_codes(
                "Consumer",
                &library,
                "namespace App { class P { static void M() { object o = new Lib.Result(1); } } }\n",
            ),
            alloc::vec![1729],
            "an internal constructor of another assembly does not exist here"
        );
        assert_eq!(
            consumer_codes(
                "Consumer",
                &library,
                "namespace App { class P { static void M() { Lib.Result r = null; r.Peek(); } } }\n",
            ),
            alloc::vec![1061],
            "and neither does an internal method"
        );
        assert!(
            consumer_codes(
                "Consumer",
                &library,
                "namespace App { class P { static void M() { Lib.Result r = null; r.Open(); } } }\n",
            )
            .is_empty(),
            "THE CONTROL: the public member on the SAME type still binds, so this is a filter and \
             not a blanket refusal"
        );
    }

    /// **A VIRTUAL METHOD IS IMPORTED WHATEVER ITS ACCESS, AND THIS ROW IS ABOUT THE CONTRACT A
    /// DERIVED TYPE INHERITS RATHER THAN ABOUT WHAT THIS ONE MAY CALL.** An `internal abstract`
    /// member is exactly what a derived class in ANOTHER assembly cannot supply, so a filter that
    /// drops it makes `class D : ImportedBase { }` compile where csc reports `CS0534` -- a contract
    /// that does not fail but silently vanishes. Not reachable from the `internal`-member test
    /// above, which asks what a CALL sees; found by a hand-written control after it was green.
    #[test]
    fn an_internal_abstract_member_of_a_reference_is_still_owed_by_a_derived_class() {
        let library = named_image_of(
            "Lib",
            "namespace Lib {
                 public abstract class Owed { public abstract int Pub(); internal abstract int Hidden(); }
             }\n",
        );
        assert_eq!(
            consumer_codes(
                "Consumer",
                &library,
                "namespace App { class D : Lib.Owed { public override int Pub() { return 1; } } }\n",
            ),
            alloc::vec![534],
            "an internal abstract member is owed -- and cannot be supplied from another assembly"
        );
        assert!(
            consumer_codes(
                "Consumer",
                &library,
                "namespace App { abstract class D : Lib.Owed { public override int Pub() { return 1; } } }\n",
            )
            .is_empty(),
            "THE CONTROL: an ABSTRACT derived class owes nothing, so this is the inherited-contract \
             rule and not a blanket refusal to derive"
        );
    }

    /// **AND AN `[assembly: InternalsVisibleTo]` NAMING THE COMPILATION PUTS THEM BACK.** The grant
    /// is keyed on the ASSEMBLY NAME, not on the file: the identical consumer source compiled under
    /// a different `/out` name is refused again, which is the control that separates "the friend
    /// declaration was read" from "the filter stopped running".
    ///
    /// The attribute is declared in the library's own source rather than taken from a corlib
    /// fixture, so the test states its whole world and cannot be moved by a corlib rebuild.
    #[test]
    fn a_friend_declaration_restores_a_reference_s_internal_members_by_name() {
        let library = named_image_of(
            "Lib",
            "[assembly: System.Runtime.CompilerServices.InternalsVisibleTo(\"Friend\")]
             namespace System { public class Attribute { } }
             namespace System.Runtime.CompilerServices {
                 public sealed class InternalsVisibleToAttribute : System.Attribute {
                     public InternalsVisibleToAttribute(string assemblyName) { }
                 }
             }
             namespace Lib { public class Result { internal Result(int n) { } } }\n",
        );
        let use_it =
            "namespace App { class P { static void M() { object o = new Lib.Result(1); } } }\n";
        assert!(
            consumer_codes("Friend", &library, use_it).is_empty(),
            "the assembly the declaration NAMES reaches the internal constructor"
        );
        assert_eq!(
            consumer_codes("Stranger", &library, use_it),
            alloc::vec![1729],
            "and any other assembly does not -- the grant is a name, not a switch"
        );
    }

    /// **A MEMBER BESIDE AN EXPLICIT IMPLEMENTATION IMPLEMENTS NOTHING, AND WAS GIVEN THE
    /// INTERFACE'S SLOT ANYWAY.** 13.4.4 looks for an explicit implementation FIRST and only then
    /// for a public member, so when `int ILeft.Value()` is present the plain `public int Value()`
    /// beside it implements no interface member at all -- csc gives it no `virtual`, `newslot` or
    /// `final`, and this compiler gave it all three.
    ///
    /// ONE predicate serves every member kind, so a defect in it reaches all of them at once:
    /// a method, a property or indexer accessor, and both event accessors. Measured against csc
    /// on this exact source.
    ///
    /// The two controls are most of the value. `Plain` implements the same interface IMPLICITLY and
    /// its members MUST keep the sealed-virtual slot; `Lonely` implements nothing and must have
    /// none.
    #[test]
    fn a_member_beside_an_explicit_implementation_takes_no_interface_slot() {
        let image = named_image_of(
            "Ax",
            "namespace Ax {
                 public delegate void Handler();
                 public interface ILeft { int Value(); int Prop { get; } event Handler E; }
                 public interface IRight { int Value(); }
                 public class Both : ILeft, IRight {
                     int ILeft.Value() { return 0; }
                     int IRight.Value() { return 0; }
                     int ILeft.Prop { get { return 0; } }
                     event Handler ILeft.E { add { } remove { } }
                     public int Value() { return 0; }
                     public int Prop { get { return 0; } }
                     public event Handler E;
                     public void Poke() { if (E != null) { E(); } }
                 }
                 public class Plain : ILeft {
                     public int Value() { return 0; }
                     public int Prop { get { return 0; } }
                     public event Handler E;
                     public void Poke() { if (E != null) { E(); } }
                 }
                 public class Lonely {
                     public int Value() { return 0; }
                     public int Prop { get { return 0; } }
                     public event Handler E;
                     public void Poke() { if (E != null) { E(); } }
                 }
             }\n",
        );
        let assembly = Assembly::read(&image).expect("the library parses");
        let mut slots = alloc::collections::BTreeMap::new();
        for ty in assembly.type_defs() {
            let Some(name) = ty.name().map(|n| n.name) else { continue };
            for method in ty.methods() {
                let Some(member) = method.name() else { continue };
                let slot = method.flags() as u16
                    & (METHOD_VIRTUAL | METHOD_NEWSLOT | METHOD_FINAL | METHOD_ABSTRACT);
                slots.insert(alloc::format!("{name}::{member}"), slot);
            }
        }
        let sealed_virtual = METHOD_VIRTUAL | METHOD_NEWSLOT | METHOD_FINAL;

        for member in ["ILeft.Value", "IRight.Value", "ILeft.get_Prop", "ILeft.add_E", "ILeft.remove_E"] {
            assert_eq!(
                slots[&alloc::format!("Both::{member}")],
                sealed_virtual,
                "{member} is the explicit implementation and holds the slot"
            );
        }
        for member in ["Value", "get_Prop", "add_E", "remove_E"] {
            assert_eq!(
                slots[&alloc::format!("Both::{member}")],
                0,
                "Both::{member} sits beside an explicit implementation and implements nothing"
            );
        }

        for member in ["Value", "get_Prop", "add_E", "remove_E"] {
            assert_eq!(
                slots[&alloc::format!("Plain::{member}")],
                sealed_virtual,
                "Plain::{member} implements ILeft implicitly and MUST keep the slot"
            );
        }
        for member in ["Value", "get_Prop", "add_E", "remove_E", "Poke"] {
            assert_eq!(
                slots[&alloc::format!("Lonely::{member}")],
                0,
                "Lonely implements nothing"
            );
        }
        assert_eq!(slots["Both::Poke"], 0, "an ordinary method is never virtual");
    }

    /// **A TYPE'S VISIBILITY AND ITS NESTING FOLLOW ITS DECLARATION, AT EVERY TYPE KIND.** The rule
    /// is one statement, because a per-kind copy that writes a literal `Public` bit ships an
    /// `internal` interface, delegate or enum as PUBLIC surface -- the declared boundary of the
    /// assembly, visible to every consumer, with nothing anywhere saying so.
    ///
    /// **THE NESTING HALF IS WORSE THAN A WRONG BIT.** `emit_nested_types` hands the enclosing
    /// type's full name where a namespace goes, so a path that does not translate it into real
    /// nesting emits a nested interface or delegate as a TOP-LEVEL type whose metadata namespace is
    /// the literal string `Lib.Outer`, with no `NestedClass` row at all.
    /// **A consumer in another assembly then cannot name it at all**: `Outer.INested`, the spelling
    /// the source declares, is CS0426 when csc reads the emitted assembly, because that type has no
    /// such member. Not a shape difference -- an unreachable type.
    ///
    /// Every expected value here was measured against csc on the same source.
    #[test]
    fn a_type_s_visibility_and_nesting_follow_its_declaration_at_every_type_kind() {
        let image = named_image_of(
            "Lib",
            "namespace Lib {
                 public class Open { }
                 internal class Hidden { }
                 public interface IShown { void M(); }
                 internal interface IHidden { void M(); }
                 public delegate void ShownHandler();
                 internal delegate void HiddenHandler();
                 public enum ShownChoice { First }
                 internal enum HiddenChoice { First }
                 public class Outer {
                     public class Shown { }
                     private class Buried { }
                     internal interface INested { void M(); }
                     private delegate void NestedHandler();
                     protected internal enum NestedChoice { First }
                 }
             }\n",
        );
        let assembly = Assembly::read(&image).expect("the library parses");
        let mut seen = alloc::collections::BTreeMap::new();
        for ty in assembly.type_defs() {
            let Some(name) = ty.name() else { continue };
            if name.name == "<Module>" {
                continue;
            }
            let nested = ty.enclosing_type().is_some();
            seen.insert(
                alloc::string::String::from(name.name),
                (
                    alloc::string::String::from(name.namespace),
                    ty.flags() & 0x0000_0007,
                    nested,
                ),
            );
        }
        let top = |ns: &str, visibility: u32| {
            (alloc::string::String::from(ns), visibility, false)
        };
        let nested = |visibility: u32| (alloc::string::String::new(), visibility, true);

        assert_eq!(seen["Open"], top("Lib", 0x01), "a public class");
        assert_eq!(seen["Hidden"], top("Lib", 0x00), "an internal class");
        assert_eq!(seen["IShown"], top("Lib", 0x01), "a public interface");
        assert_eq!(seen["IHidden"], top("Lib", 0x00), "an INTERNAL INTERFACE, which shipped public");
        assert_eq!(seen["ShownHandler"], top("Lib", 0x01), "a public delegate");
        assert_eq!(seen["HiddenHandler"], top("Lib", 0x00), "an INTERNAL DELEGATE, which shipped public");
        assert_eq!(seen["ShownChoice"], top("Lib", 0x01), "a public enum");
        assert_eq!(seen["HiddenChoice"], top("Lib", 0x00), "an INTERNAL ENUM, which shipped public");

        assert_eq!(seen["Shown"], nested(0x02), "NestedPublic");
        assert_eq!(seen["Buried"], nested(0x03), "NestedPrivate");
        assert_eq!(
            seen["INested"],
            nested(0x05),
            "a nested internal interface is NestedAssembly -- it was a top-level type named `Lib.Outer`"
        );
        assert_eq!(
            seen["NestedHandler"],
            nested(0x03),
            "a nested private delegate is NestedPrivate -- it was a top-level type too"
        );
        assert_eq!(
            seen["NestedChoice"],
            nested(0x07),
            "a nested protected-internal enum is NestedFamORAssem, not NestedPublic"
        );
    }

    /// **AN EVENT'S ACCESSORS CARRY THE VTABLE SLOT ITS MODIFIERS ASK FOR, AND AN `abstract` ONE
    /// IS BODYLESS.** The slot rule (II.23.1.10) has one statement per member kind in this
    /// compiler, and an event form naming none of them ships a `virtual` or `abstract` event as a
    /// plain non-virtual one -- so the contract DISAPPEARS from the metadata instead of failing: `class C : Abs { }` against such a library compiles here, compiles under csc,
    /// links under NativeAOT and runs, with nothing anywhere saying the event can never be raised.
    ///
    /// The three assertions are three different failure modes, and only the first is visible to a
    /// diagnostic: the flags are what a consumer binds against, the missing backing field is what
    /// `event_has_backing_field` decides in two places at once, and RVA 0 is a CLI validity rule
    /// the runtime enforces at type load (`TypeLoadException: Abstract method with non-zero RVA`)
    /// from an image ilverify passes and a bit-for-bit flag comparison calls clean.
    #[test]
    fn an_event_s_accessors_carry_the_vtable_slot_its_modifiers_ask_for() {
        let image = named_image_of(
            "Lib",
            "namespace Lib {
                 public delegate void H();
                 public abstract class Abs {
                     public abstract event H AbsE;
                     public virtual event H VirtE;
                     public event H PlainE;
                     public virtual event H CustomE { add { } remove { } }
                 }
                 public class Mid : Abs {
                     public override event H AbsE;
                     public sealed override event H VirtE;
                 }
             }\n",
        );
        let assembly = Assembly::read(&image).expect("the library parses");
        let mut flags = alloc::collections::BTreeMap::new();
        let mut rvas = alloc::collections::BTreeMap::new();
        let mut fields = alloc::vec::Vec::new();
        for ty in assembly.type_defs() {
            let Some(type_name) = ty.name().map(|n| n.name) else { continue };
            for method in ty.methods() {
                let Some(name) = method.name() else { continue };
                flags.insert(alloc::format!("{type_name}::{name}"), method.flags() as u16);
                rvas.insert(alloc::format!("{type_name}::{name}"), method.rva());
            }
            for field in ty.fields() {
                if let Some(name) = field.name() {
                    fields.push(alloc::format!("{type_name}::{name}"));
                }
            }
        }
        let slot = |key: &str| flags[key] & (METHOD_VIRTUAL | METHOD_NEWSLOT | METHOD_ABSTRACT | METHOD_FINAL);

        assert_eq!(
            slot("Abs::add_AbsE"),
            METHOD_VIRTUAL | METHOD_NEWSLOT | METHOD_ABSTRACT,
            "an abstract event's accessors are abstract virtual newslot"
        );
        assert_eq!(
            slot("Abs::add_VirtE"),
            METHOD_VIRTUAL | METHOD_NEWSLOT,
            "a virtual field-like event's accessors open a slot"
        );
        assert_eq!(
            slot("Abs::add_CustomE"),
            METHOD_VIRTUAL | METHOD_NEWSLOT,
            "and so do a virtual custom-accessor event's"
        );
        assert_eq!(slot("Abs::add_PlainE"), 0, "a plain event has no slot");
        assert_eq!(
            slot("Mid::add_AbsE"),
            METHOD_VIRTUAL,
            "an override reuses the inherited slot rather than opening one"
        );
        assert_eq!(
            slot("Mid::add_VirtE"),
            METHOD_VIRTUAL | METHOD_FINAL,
            "and `sealed override` closes it -- the case NO member kind handled"
        );

        assert_eq!(rvas["Abs::add_AbsE"], 0, "an abstract accessor has no body");
        assert_eq!(rvas["Abs::remove_AbsE"], 0, "both of them");
        assert!(
            rvas["Abs::add_VirtE"] != 0,
            "a virtual one still has the synthesized combine body"
        );
        assert!(
            !fields.contains(&alloc::string::String::from("Abs::AbsE")),
            "an abstract event declares a contract, not a backing field"
        );
        assert!(
            fields.contains(&alloc::string::String::from("Abs::VirtE")),
            "a virtual one still has its backing field"
        );
    }

    /// [`image_of_source_against_generic_fixture`] for any reference assembly.
    ///
    /// Taken as a parameter rather than copied per fixture: the generic-METHOD rows below need
    /// `generic-methods.dll` where the rows above need `generic-params.dll`, and a second copy of
    /// this body is a second place for the bind settings to drift from.
    fn image_of_source_against(fixture: &[u8], source: &str) -> Vec<u8> {
        let options = LexOptions {
            version: LanguageVersion::CSharp2,
            ..LexOptions::default()
        };
        let parsed = parse_compilation_unit_with(source, options);
        assert!(
            parsed.diagnostics.iter().all(|d| d.severity() != Severity::Error),
            "the source must PARSE cleanly under C# 2: {:?}",
            parsed.diagnostics
        );
        let units = alloc::vec![parsed.unit];
        let references = alloc::vec![Assembly::read(fixture).expect("the fixture parses")];
        for diagnostic in lamella_binder::bind_compilation_unit_with_dialect(
            &units[0],
            &references,
            false,
            LanguageVersion::CSharp2,
        ) {
            assert!(
                diagnostic.severity() != Severity::Error,
                "the bind must produce no ERROR: {}",
                diagnostic.kind
            );
        }
        let program = ValidatedProgram::from_clean_bind(&units, &references, false)
            .expect("the barrier is minted from the assertion above");
        let (image, _) = build_image(&program, "test.dll", "test", None, false, false)
            .expect("an imported generic in a signature emits");
        image
    }

    /// An IMPORTED generic definition used in every signature position a type can occupy.
    ///
    /// **THE POSITIONS ARE THE TEST, AND ONE OF THEM FOUND A SECOND MISSING SITE.** Minting is
    /// spread over eighteen `mint_*` functions; adding the instantiation case to
    /// `mint_signature_type` fixed the field, the parameter and the return, and an UNUSED LOCAL
    /// still refused -- a local's declared type comes through `mint_named_type_token` instead, and
    /// a local that was USED had been minted by the expression path so it passed either way.
    /// One construct, three minting entries, and the case had landed in a subset of them.
    ///
    /// Each row is a whole compilation that must EMIT, so a refusal anywhere in the pipeline fails
    /// it. `build_image` returning is the assertion.
    #[test]
    fn an_imported_generic_emits_in_every_signature_position() {
        for (label, source) in [
            ("field", "class C { Fixture.Box<Fixture.Plain> f; }"),
            (
                "parameter",
                "class C { void M(Fixture.Box<Fixture.Plain> p) { } }",
            ),
            (
                "return",
                "class C { Fixture.Box<Fixture.Plain> M() { return null; } }",
            ),
            (
                "UNUSED local -- the row that found the second site",
                "class C { void M() { Fixture.Box<Fixture.Plain> x = null; } }",
            ),
            (
                "used local",
                "class C { Fixture.Box<Fixture.Plain> M() { \
                 Fixture.Box<Fixture.Plain> x = null; return x; } }",
            ),
            ("array", "class C { Fixture.Box<Fixture.Plain>[] a; }"),
            (
                "array local",
                "class C { void M() { Fixture.Box<Fixture.Plain>[] a = null; } }",
            ),
            (
                "two arguments",
                "class C { Fixture.Pair<Fixture.Plain, Fixture.Plain> p; }",
            ),
            (
                "nested argument",
                "class C { Fixture.Box<Fixture.Box<Fixture.Plain>> n; }",
            ),
            (
                "ref parameter",
                "class C { void M(ref Fixture.Box<Fixture.Plain> p) { } }",
            ),
        ] {
            let Some(image) = image_of_source_against_generic_fixture(source) else {
                return;
            };
            assert!(!image.is_empty(), "{label} must emit: {source}");
        }
    }

    /// A generic call whose TYPE ARGUMENT is itself a type parameter -- `Util.Id<T>()` written
    /// inside a generic body -- and the closed spelling beside it as the control.
    ///
    /// **THE OPEN ROW IS THE FIRST GENERIC CALL IN THIS TREE WHOSE ARGUMENT IS NOT A CLOSED TYPE.**
    /// Every other call site passes something that names itself, so the argument encoder was never
    /// asked a question it could not answer: it refused `T` as an unresolvable named type, dropped
    /// the `MethodSpec`, and the whole call refused with "a generic call whose instantiation could
    /// not be minted". The closed row passed throughout and is why nothing noticed.
    ///
    /// **THE BLOB IS THE ASSERTION, NOT THE EMIT.** A `MethodSpec` carrying the wrong argument
    /// emits perfectly -- and an argument silently encoded as a TypeRef to a type named `T` is the
    /// shape that compiles clean and throws `TypeLoadException` on entry, which no "it emitted"
    /// check can see. `Var(0)` is the declaring type's first parameter, by position.
    #[test]
    fn a_generic_call_can_pass_a_type_parameter_as_its_argument() {
        let Some(fixture) = reference_fixture(GENERIC_FIXTURE) else {
            return;
        };
        let measured: Vec<(&str, Vec<Vec<lamella_metadata::SigType>>)> = [
            (
                "OPEN -- the argument is the declaring type's parameter",
                "class Box<T> { public T Pass(T value) { return Fixture.Util.Id<T>(value); } }",
            ),
            (
                "CONTROL -- closed, and it passed before the open row existed",
                "class C { public int Pass(int value) { return Fixture.Util.Id<int>(value); } }",
            ),
        ]
        .into_iter()
        .map(|(label, source)| {
            let image = image_of_source_against(&fixture, source);
            let assembly = lamella_metadata::Assembly::read(&image)
                .unwrap_or_else(|_| panic!("{label}: the emitted image parses"));
            let rows = assembly
                .tables()
                .row_count(lamella_metadata::tables::table::METHOD_SPEC);
            let specs = (1..=rows)
                .filter_map(|row| {
                    assembly.method_spec_instantiation(Token::new(
                        lamella_metadata::tables::table::METHOD_SPEC,
                        row,
                    ))
                })
                .collect();
            (label, specs)
        })
        .collect();

        let expected: Vec<(&str, Vec<Vec<lamella_metadata::SigType>>)> = alloc::vec![
            (
                "OPEN -- the argument is the declaring type's parameter",
                alloc::vec![alloc::vec![lamella_metadata::SigType::Var(0)]],
            ),
            (
                "CONTROL -- closed, and it passed before the open row existed",
                alloc::vec![alloc::vec![lamella_metadata::SigType::I4]],
            ),
        ];
        assert_eq!(
            measured, expected,
            "each row emits exactly one MethodSpec carrying exactly its own argument"
        );
    }

    /// The CONTROL for the row above: an imported NON-generic in the same positions. It passed
    /// before the fix and must still pass, so a change that broke ordinary imported types would
    /// fail here rather than hiding behind the generic rows going green.
    #[test]
    fn an_imported_non_generic_still_emits_in_the_same_positions() {
        for source in [
            "class C { Fixture.Plain f; }",
            "class C { void M(Fixture.Plain p) { } }",
            "class C { void M() { Fixture.Plain x = null; } }",
            "class C { Fixture.Plain[] a; }",
        ] {
            let Some(image) = image_of_source_against_generic_fixture(source) else {
                return;
            };
            assert!(!image.is_empty(), "the control must emit: {source}");
        }
    }

    /// **A SOURCE-DECLARED GENERIC TYPE IS NAMED `` Box`1 `` IN METADATA AND CARRIES ITS
    /// `GenericParam` ROWS.** Both halves are load-bearing and they fail in different places: the
    /// arity-mangled NAME is the single key space the binder's model, a reference-assembly reader
    /// and `type_sig`'s definition lookup all meet in -- emit `Box` and every one of them misses,
    /// silently, returning `None` for the base class and the enclosing type. The ROWS are what give
    /// the parameters names at all, so a consumer decoding `!0` has nothing to call it.
    #[test]
    fn a_declared_generic_type_is_mangled_and_carries_its_parameter_rows() {
        let image = image_of_gated_source("namespace App { public class Box<T> { } }\n");
        let assembly = lamella_metadata::Assembly::read(&image).expect("the image parses");

        let boxed = assembly
            .type_defs()
            .find(|t| t.name().is_some_and(|n| n.name.starts_with("Box")))
            .expect("the declared type is in the image");
        assert_eq!(
            boxed.name().map(|n| n.name),
            Some("Box`1"),
            "the arity is part of the metadata name (II.10.7.2)"
        );

        let by_owner = assembly.type_parameter_names();
        assert_eq!(
            by_owner.get(&boxed.token().row()).map(|names| names.as_slice()),
            Some(["T"].as_slice()),
            "the type's parameter is named in metadata"
        );
    }

    /// The constraint FLAG WORD (II.23.1.7), read back per parameter, with an UNCONSTRAINED
    /// parameter beside every constrained one.
    ///
    /// **THE UNCONSTRAINED ROW IS THE INSTRUMENT.** A writer that set a fixed flag word on every
    /// parameter satisfies every positive assertion here; only the `Plain<T>` row separates that
    /// from a writer that reads what the source said. Every expected value was measured from csc on
    /// the same source, not derived from the flag table.
    #[test]
    fn a_type_parameters_constraint_flags_are_emitted_and_match_csc() {
        let image = image_of_gated_source(
            "public interface IFoo { }\n\
             public class RefC<T> where T : class { }\n\
             public class ValC<T> where T : struct { }\n\
             public class NewC<T> where T : new() { }\n\
             public class IfaceC<T> where T : IFoo { }\n\
             public class Plain<T> { }\n",
        );
        let assembly = lamella_metadata::Assembly::read(&image).expect("the image parses");
        let flags: alloc::collections::BTreeMap<&str, u32> = assembly
            .type_defs()
            .filter_map(|t| {
                let name = t.name()?.name;
                let row = t.token().row();
                let flags = assembly
                    .generic_params()
                    .find(|&(_, _, owner, _)| owner == row << 1)
                    .map(|(_, flags, _, _)| flags)?;
                Some((name, flags))
            })
            .collect();

        assert_eq!(flags.get("RefC`1"), Some(&0x0004), "class");
        assert_eq!(flags.get("ValC`1"), Some(&0x0018), "struct implies new()");
        assert_eq!(flags.get("NewC`1"), Some(&0x0010), "new()");
        assert_eq!(flags.get("IfaceC`1"), Some(&0x0000), "an interface constraint is not a flag");
        assert_eq!(flags.get("Plain`1"), Some(&0x0000), "the control: nothing written, nothing set");
    }

    /// A named constraint becomes a `GenericParamConstraint` ROW owned by the right parameter.
    ///
    /// **THE LOAD-BEARING ASSERTION IS THE OWNER, NOT THE COUNT.** `GenericParam` is
    /// required-sorted and the finalizer reorders it, while `GenericParamConstraint.Owner` is a ROW
    /// index into it -- so an emitter that sorts without remapping produces the RIGHT NUMBER of
    /// perfectly valid rows attached to the WRONG parameters. Nothing structural detects that.
    ///
    /// **THE FIXTURE PUTS A GENERIC METHOD INSIDE A GENERIC TYPE, AND THAT IS THE ENTIRE POINT
    /// OF THE ROW.** An earlier version used two sibling generic types and was GREEN against the
    /// unremapped sort -- because their rows were already in owner order, so the sort was a no-op
    /// and there was nothing to repoint. Owner is a `TypeOrMethodDef` coded index (`TypeDef` = row
    /// << 1, `MethodDef` = row << 1 | 1), so a method's parameter sorts BEFORE its declaring type's
    /// even though it is emitted after -- which is the only cheap way to make emission order differ
    /// from key order. Red-proved by restoring the plain sort: this row fails, and it did NOT fail
    /// before the fixture changed.
    #[test]
    fn a_named_constraint_is_a_row_owned_by_the_parameter_that_declared_it() {
        let image = image_of_gated_source(
            "public interface IFoo { }\n\
             public class Bas { }\n\
             public class Outer<T> where T : Bas { public void M<U>() where U : IFoo { } }\n",
        );
        let assembly = lamella_metadata::Assembly::read(&image).expect("the image parses");
        let params: alloc::vec::Vec<(u32, u32, u32, Option<&str>)> =
            assembly.generic_params().collect();
        let rows = assembly
            .tables()
            .row_count(lamella_metadata::tables::table::GENERIC_PARAM_CONSTRAINT);
        assert_eq!(rows, 2, "one row per NAMED constraint, and none for the parameters' flags");

        let mut pairs: alloc::vec::Vec<(&str, alloc::string::String)> = alloc::vec::Vec::new();
        for index in 1..=rows {
            let row = assembly
                .tables()
                .row(lamella_metadata::tables::table::GENERIC_PARAM_CONSTRAINT, index)
                .expect("the row is present");
            let owner = params
                .get(row.raw(0).saturating_sub(1) as usize)
                .and_then(|&(_, _, _, name)| name)
                .expect("the owner names a real GenericParam row");
            let coded = row.raw(1);
            let (tag, target) = (coded & 3, coded >> 2);
            let name = if tag == 0 {
                assembly
                    .type_defs()
                    .find(|t| t.token().row() == target)
                    .and_then(|t| t.name().map(|n| n.name))
                    .unwrap_or("<unknown TypeDef>")
            } else {
                "<external>"
            };
            pairs.push((owner, alloc::string::String::from(name)));
        }
        pairs.sort();
        assert_eq!(
            pairs,
            [
                ("T", alloc::string::String::from("Bas")),
                ("U", alloc::string::String::from("IFoo")),
            ],
            "each constraint must be attached to the parameter that DECLARED it -- a swap here is \
             a valid assembly stating the wrong constraints"
        );
    }

    /// The constraints survive a ROUND TRIP: emitted here, read back through the same reader
    /// `reference.rs` uses when it loads a referenced assembly.
    ///
    /// **THIS IS A CLOSED LOOP AND SAYS SO.** Both halves are ours, so it proves the writer and the
    /// reader AGREE -- not that either matches ECMA-335. The independent check is the csc
    /// differential run by hand: for one source csc emitted the same nine flag words
    /// byte for byte and the same six constraint rows, which is what established that `where T :
    /// struct` carries a `System.ValueType` row as well as its flag. A row here that asserted
    /// conformance rather than agreement would be the fixture-builds-its-own-input shape.
    #[test]
    fn emitted_constraints_read_back_through_the_reference_reader() {
        let image = image_of_gated_source(
            "public interface IFoo { }\n\
             public class Con<T, U> where T : class, IFoo where U : struct { }\n",
        );
        let assembly = lamella_metadata::Assembly::read(&image).expect("the image parses");
        let by_parameter = assembly.generic_param_constraints();

        let con = assembly
            .type_defs()
            .find(|t| t.name().is_some_and(|n| n.name.starts_with("Con")))
            .expect("the type is in the image");
        let owner = con.token().row() << 1;

        let (t_flags, t_types) = by_parameter.get(&(owner, 0)).expect("T's entry");
        assert_eq!(*t_flags & 0x001C, 0x0004, "T is `class` and nothing else");
        assert_eq!(t_types.len(), 1, "T's named constraint is one row (IFoo)");

        let (u_flags, u_types) = by_parameter.get(&(owner, 1)).expect("U's entry");
        assert_eq!(
            *u_flags & 0x001C,
            0x0018,
            "U is `struct`, which carries the constructor bit with it (II.10.1.7)"
        );
        assert_eq!(u_types.len(), 1, "the struct flag is accompanied by its ValueType row");
    }

    /// **TWO PARAMETERS, AND THE ASSERTION READS THE `Number` COLUMN RATHER THAN THE ORDER THE
    /// NAMES COME BACK IN.** That distinction is the whole test, and it is not theoretical: an
    /// earlier version of this row asserted the NAMES through `type_parameter_names` and **stayed
    /// green against a writer that numbered every parameter zero** -- the names arrive in row order,
    /// so the column that actually maps `!0` to `TKey` and `!1` to `TValue` was never looked at.
    /// A consumer resolving `!1` against that image finds nothing.
    ///
    /// And a non-generic sibling is beside them, because a change that put a row on EVERY type
    /// passes every positive assertion here.
    #[test]
    fn parameter_rows_are_numbered_by_position_and_only_generic_types_get_them() {
        let image = image_of_gated_source(
            "namespace App { public class Pair<TKey, TValue> { } public class Plain { } }\n",
        );
        let assembly = lamella_metadata::Assembly::read(&image).expect("the image parses");

        let owner_of = |name: &str| {
            let row = assembly
                .type_defs()
                .find(|t| t.name().is_some_and(|n| n.name == name))
                .unwrap_or_else(|| panic!("{name} is in the image"))
                .token()
                .row();
            row << 1
        };
        let rows: Vec<(u32, u32, Option<&str>)> = assembly
            .generic_params()
            .map(|(number, _flags, owner, name)| (number, owner, name))
            .collect();
        assert_eq!(
            rows,
            [
                (0, owner_of("Pair`2"), Some("TKey")),
                (1, owner_of("Pair`2"), Some("TValue")),
            ],
            "numbered by declaration position, owned by the TypeDef, and nothing for `Plain`"
        );
    }

    /// **A MEMBER THAT NAMES ITS TYPE'S PARAMETER ENCODES `!0`, AND THE ALTERNATIVE IS NOT A
    /// WORSE SIGNATURE BUT NO SIGNATURE AT ALL.** `type_sig` has no case for a bare `T`: it falls
    /// through to the named-type lookup, finds nothing, and refuses -- so before this, a generic
    /// type with any member mentioning `T` could not be emitted even with the gate lifted.
    ///
    /// Asserted on the SIGNATURE BYTES because that is where the distinction lives. `0x13` is
    /// `ELEMENT_TYPE_VAR` and `0x1E` is `ELEMENT_TYPE_MVAR` (II.23.1.16) -- one byte apart, both
    /// decode cleanly, and they name different types.
    #[test]
    fn a_member_naming_its_types_parameter_encodes_var_not_mvar() {
        let image = image_of_gated_source(
            "namespace App { public class Box<T> { public T Value; public T Get() { return Value; } } }\n",
        );
        let assembly = lamella_metadata::Assembly::read(&image).expect("the image parses");
        let boxed = assembly
            .type_defs()
            .find(|t| t.name().is_some_and(|n| n.name == "Box`1"))
            .expect("Box`1 is in the image");

        let field = boxed.fields().next().expect("Box declares a field");
        assert_eq!(field.name(), Some("Value"));
        assert_eq!(
            field.signature(),
            Some(lamella_metadata::SigType::Var(0)),
            "a field of the type's own parameter is `!0` -- not a class ref, and not `!!0`"
        );

        let get = boxed
            .methods()
            .find(|m| m.name() == Some("Get"))
            .expect("Box declares Get");
        assert_eq!(
            get.signature_blob(),
            &[0x20, 0x00, 0x13, 0x00],
            "an instance method returning the type's own parameter is HASTHIS + `!0`"
        );
    }

    /// **A DECLARED GENERIC METHOD CARRIES THE GENERIC CONVENTION, ITS `GenParamCount`, AND ROWS
    /// THE `MethodDef` OWNS -- AND THE THREE MUST AGREE.** II.23.2.1 is explicit that the SIGNATURE
    /// is what makes a method generic; `GenericParam` rows without the convention are metadata that
    /// contradicts itself, and csc answers CS0308 at a call site against such a method. The
    /// signature bytes are asserted directly because that is where the disagreement would live.
    ///
    /// `T Id<T>(T x)`, static: `0x10` GENERIC, `0x01` GenParamCount, `0x01` ParamCount,
    /// `1E 00` return `!!0`, `1E 00` parameter `!!0`.
    #[test]
    fn a_declared_generic_method_is_generic_in_its_signature_and_owns_its_rows() {
        let image = image_of_gated_source(
            "namespace App { public class Util { public static T Id<T>(T x) { return x; } } }
",
        );
        let assembly = lamella_metadata::Assembly::read(&image).expect("the image parses");
        let util = assembly
            .type_defs()
            .find(|t| t.name().is_some_and(|n| n.name == "Util"))
            .expect("Util is in the image");
        let id = util
            .methods()
            .find(|m| m.name() == Some("Id"))
            .expect("Util declares Id");

        assert_eq!(
            id.signature_blob(),
            &[0x10, 0x01, 0x01, 0x1E, 0x00, 0x1E, 0x00],
            "a declared `T Id<T>(T)` is GENERIC over one parameter, spelled with `!!0`"
        );

        let rows: Vec<(u32, u32, Option<&str>)> = assembly
            .generic_params()
            .map(|(number, _flags, owner, name)| (number, owner, name))
            .collect();
        assert_eq!(
            rows,
            [((0), (id.token().row() << 1) | 1, Some("T"))],
            "one row, numbered 0, owned by the MethodDef"
        );

        assert!(
            assembly.type_parameter_names().is_empty(),
            "a non-generic type owns no parameter, even when its method declares one"
        );
    }

    /// The `TypeSpec` table's index (II.22.39), as a `MemberRef` parent token's table byte.
    const TYPE_SPEC: u8 = lamella_metadata::tables::table::TYPE_SPEC;

    /// Every `MemberRef` in `image` as `(parent table, name, signature blob)`.
    fn member_refs_of(assembly: &Assembly<'_>) -> Vec<(u8, String, Vec<u8>)> {
        assembly
            .member_refs()
            .map(|member| {
                (
                    member.parent().table(),
                    String::from(member.name().unwrap_or_default()),
                    member.signature_blob().to_vec(),
                )
            })
            .collect()
    }

    /// **A MEMBER OF AN INSTANTIATED GENERIC TYPE IS NAMED BY A `MemberRef` WHOSE PARENT IS A
    /// `TypeSpec` CARRYING THE ARGUMENTS (II.23.2.1), AND EVERY WRONG ANSWER HERE DECODES
    /// CLEANLY.** On exactly this source, the wrong answers are: `Box<int>.Echo(41)` as a `MemberRef`
    /// parented on a `TypeRef` to this module's OWN `` Box`1 `` -- the `<int>` gone, the signature
    /// substituted -- and `b.Get()` as a `callvirt` straight at the open `MethodDef`. **Both
    /// compile, exit 0, and produce IL instruction-for-instruction identical to the non-generic
    /// control's.**
    ///
    /// **THE SIGNATURE IS ASSERTED AGAINST THE DEFINITION'S OWN `MethodDef` BLOB RATHER THAN
    /// AGAINST A LITERAL, AND THAT IS THE POINT OF THE ROW.** II.23.2.1 requires the `MemberRef` to
    /// carry the DEFINITION's signature -- `!0`, not the substituted `int32` -- so the definition
    /// already holds an independent copy of the right answer, written by the declaration path
    /// rather than by the minting path under test. A literal would be a second transcription of my
    /// own reading; this is two producers agreeing.
    ///
    /// The three shapes are here together because they took three different routes to the same
    /// defect: the ctor was REFUSED outright, the instance call named the open `MethodDef`, and the
    /// static call minted a wrong `MemberRef`. One fix, three symptoms, so one test.
    #[test]
    fn a_member_of_an_instantiated_generic_type_is_named_through_a_type_spec() {
        let image = image_of_gated_source(
            "namespace App {
                 public class Box<T> {
                     private T item;
                     public Box(T value) { item = value; }
                     public T Get() { return item; }
                     public static T Echo(T value) { return value; }
                 }
                 public class Program {
                     public static int Main() {
                         App.Box<int> b = new App.Box<int>(41);
                         return App.Box<int>.Echo(b.Get());
                     }
                 }
             }\n",
        );
        let assembly = Assembly::read(&image).expect("the image parses");
        let boxed = assembly
            .type_defs()
            .find(|t| t.name().is_some_and(|n| n.name == "Box`1"))
            .expect("Box`1 is in the image");
        let definition_sig = |name: &str| {
            boxed
                .methods()
                .find(|m| m.name() == Some(name))
                .unwrap_or_else(|| panic!("Box`1 declares {name}"))
                .signature_blob()
                .to_vec()
        };

        let members = member_refs_of(&assembly);
        for name in [".ctor", "Get", "Echo"] {
            let found = members
                .iter()
                .find(|(parent, member, _)| *parent == TYPE_SPEC && member == name)
                .unwrap_or_else(|| {
                    panic!("`{name}` on Box<int> is named through a TypeSpec, got {members:?}")
                });
            assert_eq!(
                found.2,
                definition_sig(name),
                "`{name}`'s MemberRef carries the DEFINITION's signature, byte for byte"
            );
        }

        let parent = assembly
            .member_refs()
            .find(|member| member.name() == Some(".ctor") && member.parent().table() == TYPE_SPEC)
            .expect("the constructor is TypeSpec-parented")
            .parent();
        let spec = assembly
            .type_spec_signature(parent)
            .expect("the parent TypeSpec's blob decodes");
        assert_eq!(
            spec,
            lamella_metadata::SigType::GenericInst {
                definition: alloc::boxed::Box::new(lamella_metadata::SigType::Class(
                    boxed.token()
                )),
                arguments: alloc::vec![lamella_metadata::SigType::I4],
            },
            "the parent is `Box`1<int32>` over this module's OWN TypeDef, not a TypeRef to it"
        );
    }

    /// **THE ROW WHERE ERASURE CORRUPTS DATA RATHER THAN METADATA.** ECMA-335 II.9.7 gives each
    /// instantiation of a generic type its own copy of each static field. With the use site erased,
    /// `Counter<int>.Total` and `Counter<string>.Total` both named the definition's single
    /// `FieldDef` and shared ONE cell -- measured on the interpreter tier as a program answering
    /// 503503 where 10507 is correct, **with zero violations reported**, because an erased use site
    /// is indistinguishable from non-generic code and so is never refused.
    ///
    /// **THE ASSERTION IS THAT THE TWO PARENTS DIFFER, WHICH IS THE PROPERTY STORAGE FOLLOWS.**
    /// Asserting each field merely HAS a `TypeSpec` parent would pass a writer that gave both the
    /// same one -- and that writer shares the cell exactly as the old one did, while looking
    /// correct in every other respect.
    ///
    /// `Total` is declared `int`, so its signature is byte-identical open and closed. Nothing
    /// about the SIGNATURE distinguishes the right answer from the wrong one here; only the parent
    /// row does. That is why the defect ran silently, and why a signature-only assertion is not a
    /// substitute for this one.
    ///
    /// **THERE ARE THREE PARENTS AND NOT TWO, AND THE THIRD IS THE ONE `Add`'S OWN BODY USES.**
    /// `Total = Total + n` inside `Counter<T>` names the type over its OWN parameter,
    /// `` Counter<!0> `` -- a definition reached in its own body is still an instantiation.
    /// Measured against csc on this exact program: three `MemberRef`s named `Total`, parented by
    /// `` Counter<!0> ``, `Counter<int>` and `Counter<string>`.
    ///
    /// **THE NON-GENERIC SIBLING IS STILL THE CONTROL, AND IT IS NOW ASSERTED BY NAME.** `Plain`
    /// is a this-module type whose static field the token pre-pass already registered, so it must
    /// mint no `MemberRef` at all. A count alone can no longer say so -- three is now the right
    /// answer for two different reasons -- so the parents are checked against `Plain`'s own row
    /// instead.
    #[test]
    fn a_static_field_of_two_instantiations_names_two_distinct_parents() {
        let image = image_of_gated_source(
            "namespace App {
                 public class Counter<T> {
                     public static int Total = 7;
                     public static void Add(int n) { Total = Total + n; }
                 }
                 public class Plain { public static int Total = 7; }
                 public class Program {
                     public static int Main() {
                         App.Counter<int>.Add(3);
                         App.Counter<string>.Add(500);
                         return App.Counter<int>.Total * 1000 + App.Counter<string>.Total + Plain.Total;
                     }
                 }
             }\n",
        );
        let assembly = Assembly::read(&image).expect("the image parses");
        let totals: Vec<Token> = assembly
            .member_refs()
            .filter(|member| member.is_field() && member.name() == Some("Total"))
            .map(|member| member.parent())
            .collect();
        assert!(
            totals.iter().all(|parent| parent.table() == TYPE_SPEC),
            "a static field of an instantiated type is named through a TypeSpec, and the \
             non-generic sibling is not named by a MemberRef at all: {totals:?}"
        );
        let argument_of = |parent: Token| match assembly.type_spec_signature(parent) {
            Some(lamella_metadata::SigType::GenericInst { arguments, .. }) => arguments,
            other => panic!("the parent is a GenericInst, got {other:?}"),
        };
        let mut arguments: Vec<Vec<lamella_metadata::SigType>> =
            totals.iter().map(|parent| argument_of(*parent)).collect();
        arguments.sort_by_key(|argument| alloc::format!("{argument:?}"));
        assert_eq!(
            arguments,
            alloc::vec![
                alloc::vec![lamella_metadata::SigType::I4],
                alloc::vec![lamella_metadata::SigType::String],
                alloc::vec![lamella_metadata::SigType::Var(0)],
            ],
            "csc emits three: Counter<!0> for the in-body access, then Counter<int> and \
             Counter<string> for the two use sites"
        );
    }

    /// **`typeof(Box<>)` NAMES THE DEFINITION'S OWN ROW AND `typeof(Box<int>)` NAMES A `TypeSpec`,
    /// AND THE ASSERTION IS THAT THEY DIFFER.** ECMA-334 4th ed 14.5.11: an `unbound-type-name`
    /// *resolves to the unbound generic type associated with the resulting constructed type*, and
    /// 25.5 adds that its `System.Type` *is not the same as* an instantiation's -- so an emitter
    /// that answered one row for both would make the probe's `open == closed` TRUE, which is the
    /// program's own refusal condition.
    ///
    /// **MEASURED AGAINST csc with `System.Reflection.Metadata` over csc's own output**:
    /// `typeof(List<>)` is `ldtoken` of the **TypeRef** `` List`1 ``, `typeof(Dictionary<,>)` of
    /// `` Dictionary`2 ``, and `typeof(List<int>)` beside them of a `TypeSpec`. lcsc emits the same
    /// three shapes. The unbound form names no `TypeSpec` at
    /// all, which is the half a `TypeSpec`-vs-`TypeSpec` comparison could not have caught.
    ///
    /// Declared in this module rather than imported, so the definition's row is a `TypeDef` and the
    /// test needs no reference assembly; against a referenced `List<T>` the same path yields the
    /// `TypeRef` csc emits, and `generics-parity`'s `typeof-open-generic-type` row is that case.
    ///
    /// The locals are `object` rather than `System.Type` for the helper's reason and not for
    /// style: this compiles against NO references, so naming `System.Type` in source is a CS0246
    /// and the helper admits no diagnostic but the generics gate. `ldtoken` is emitted from the
    /// operand either way, which is the whole of what this reads.
    #[test]
    fn an_unbound_generic_typeof_names_the_definition_and_a_constructed_one_names_a_type_spec() {
        let image = image_of_gated_source(
            "namespace App {
                 public class Box<T> { }
                 public class Pair<T, U> { }
                 public class Program {
                     public static int Main() {
                         object open = typeof(App.Box<>);
                         object closed = typeof(App.Box<int>);
                         object two = typeof(App.Pair<,>);
                         return (open == closed || open == two) ? 1 : 42;
                     }
                 }
             }\n",
        );
        let assembly = Assembly::read(&image).expect("the image parses");
        let main = assembly
            .find_type("App", "Program")
            .expect("the Program type")
            .methods()
            .find(|method| method.name() == Some("Main"))
            .expect("the Main method");
        let body = main.body().expect("Main has a method body");
        let reflected: Vec<Token> = body
            .code
            .iter()
            .filter(|instruction| instruction.opcode == lamella_cil::Opcode::Ldtoken)
            .filter_map(|instruction| match instruction.operand {
                lamella_cil::Operand::Token(token) => Some(token),
                _ => None,
            })
            .collect();
        assert_eq!(reflected.len(), 3, "three typeof operands: {reflected:?}");
        assert_eq!(
            reflected
                .iter()
                .map(|token| token.table())
                .collect::<Vec<u8>>(),
            alloc::vec![TYPE_DEF, TYPE_SPEC, TYPE_DEF],
            "the unbound forms name their definition's row and the constructed one a TypeSpec: \
             {reflected:?}"
        );
        assert_ne!(
            reflected[0], reflected[2],
            "Box<> and Pair<,> are two definitions, not one: {reflected:?}"
        );
        let named = |token: Token| {
            assembly
                .type_defs()
                .find(|ty| ty.token() == token)
                .and_then(|ty| ty.name().map(|name| alloc::format!("{}", name.name)))
        };
        assert_eq!(named(reflected[0]).as_deref(), Some("Box`1"));
        assert_eq!(named(reflected[2]).as_deref(), Some("Pair`2"));
    }

    /// **`beforefieldinit`'s ABSENCE IS A DEMAND, NOT A DEFAULT.** A type without the flag requires
    /// PRECISE initializer timing (II.10.5.3.3); with it, the initializer may run at or before
    /// first static field access. Omitting it for nothing says every type in the image needs a
    /// first-access check.
    ///
    /// Measured on the class library: **100% of trigger sites need a runtime check when the flag
    /// is omitted, against 4% under csc's on the same sources.** The flag is most of a lazy
    /// initializer's cost, decided before the mechanism is written.
    ///
    /// **THE RULE IS csc's, MEASURED ACROSS EVERY TYPE KIND RATHER THAN RECALLED -- AND TWO ROWS
    /// ARE NOT WHAT "unless it declares a static constructor" PREDICTS.** An enum and a delegate
    /// never carry it, which reasoning from the C# rule alone would have got wrong for two whole
    /// type kinds.
    ///
    /// The `StaticFieldInit` row is the one that decides the implementation: it HAS a `.cctor`
    /// **`sealed` IS TWO FAULTS WITH ONE SYMPTOM, SO THE TEST IS A ROUND TRIP.** The emitter never
    /// wrote `TYPE_SEALED` for a `sealed class` and the importer never read one back, and either
    /// alone is invisible: fixing the emit leaves lcsc unable to see its own output, and fixing the
    /// import leaves it nothing to see. So this compiles source, asserts the BYTES carry the flag,
    /// then loads those same bytes as a reference assembly and asserts the MODEL carries it.
    ///
    /// **THE OTHER KINDS ARE HERE BECAUSE THE IMPORT SIDE REACHES ALL OF THEM AT ONCE.** A struct,
    /// an enum and a delegate are sealed in metadata by their own emit paths, and deriving from any
    /// of them across an assembly boundary compiled before the flag was read -- so a test that
    /// covered only `sealed class` would credit one row of four.
    #[test]
    fn sealed_survives_the_assembly_boundary_in_both_directions() {
        let image = image_of_gated_source(
            "namespace Lib {
                 public sealed class Shut { public int A; }
                 public class Open { public int B; }
                 public struct Val { public int C; }
                 public enum Kind { A }
                 public delegate void Ping();
             }\n",
        );
        let assembly = Assembly::read(&image).expect("the image parses");
        let emitted_sealed = |name: &str| -> bool {
            assembly
                .type_defs()
                .find(|t| t.name().is_some_and(|n| n.name == name))
                .unwrap_or_else(|| panic!("{name} is in the image"))
                .flags()
                & TYPE_SEALED
                != 0
        };
        assert!(emitted_sealed("Shut"), "a `sealed class` emits II.23.1.15's sealed bit");
        assert!(!emitted_sealed("Open"), "and an unsealed one does not -- the control");
        let mut model = Model::new();
        lamella_binder::load_assembly(&mut model, &assembly, "");
        let modelled_sealed = |name: &str| -> bool {
            model
                .get("Lib", name)
                .unwrap_or_else(|| panic!("{name} loaded from the reference"))
                .is_sealed
        };
        assert!(modelled_sealed("Shut"), "the importer reads the flag back");
        assert!(!modelled_sealed("Open"), "and reads its absence -- the control");
        assert!(modelled_sealed("Val"), "a struct is sealed in metadata and now says so");
        assert!(modelled_sealed("Kind"), "so is an enum");
        assert!(modelled_sealed("Ping"), "so is a delegate");
    }

    /// **A PARTIAL TYPE'S FLAGS ARE THE UNION OF ITS PARTS' MODIFIERS (17.1.4), AND READING THE
    /// FIRST PART'S LIST ALONE IS AN ANSWER ABOUT ONE PART THAT LOOKS LIKE AN ANSWER ABOUT THE
    /// TYPE.** Every bit decided that way is wrong for the same reason.
    ///
    /// **VISIBILITY IS THE ONE THAT MATTERS MOST, AND IT IS NOT A FLAG NOBODY READS.**
    /// `partial class V { }` + `public partial class V { }` emitted `NotPublic`, so a type the
    /// source declares public was invisible outside its own assembly -- CS0122 or CS0246 at every
    /// consumer, for a type that is right there. The nested form emitted `NestedPrivate` for a
    /// `public` nested type, which is the same failure one level in. `abstract` dropped to an
    /// instantiable class, and `static` produced a type carrying neither flag while
    /// `any_part_is_static` had ALREADY suppressed its constructor from any part -- so two halves
    /// of one type disagreed inside one image.
    ///
    /// **THE FIRST-PART ROWS ARE THE CONTROL AND THEY WERE ALWAYS GREEN**, which is why nothing
    /// caught this: the matrix's `partial-type` probe splits MEMBERS across parts and not
    /// MODIFIERS, so every instrument in the tree wrote the modifier where the emitter looked.
    /// Every row measured from csc on this same source.
    #[test]
    fn a_partial_types_flags_are_the_union_of_its_parts_modifiers() {
        let image = image_of_gated_source(
            "namespace App {
                 public abstract partial class FirstAbstract { public int A; }
                 public partial class FirstAbstract { public int B; }
                 public partial class LaterAbstract { public int C; }
                 public abstract partial class LaterAbstract { public int D; }
                 public static partial class FirstStatic { public static int E() { return 1; } }
                 public partial class FirstStatic { public static int F() { return 2; } }
                 public partial class LaterStatic { public static int G() { return 1; } }
                 public static partial class LaterStatic { public static int H() { return 2; } }
                 public partial class FirstPublic { public int I; }
                 partial class FirstPublic { public int J; }
                 partial class LaterPublic { public int K; }
                 public partial class LaterPublic { public int L; }
                 public class Host {
                     partial class LaterNested { public int M; }
                     public partial class LaterNested { public int N; }
                 }
             }\n",
        );
        let assembly = Assembly::read(&image).expect("the image parses");
        let flags_of = |name: &str| -> u32 {
            assembly
                .type_defs()
                .find(|t| t.name().is_some_and(|n| n.name == name))
                .unwrap_or_else(|| panic!("{name} is in the image"))
                .flags()
        };
        let shape = |name: &str| -> (bool, bool) {
            let flags = flags_of(name);
            ((flags & TYPE_ABSTRACT) != 0, (flags & TYPE_SEALED) != 0)
        };
        let visibility = |name: &str| -> u32 { flags_of(name) & 0x0000_0007 };
        assert_eq!(shape("FirstAbstract"), (true, false), "abstract on the first part");
        assert_eq!(shape("FirstStatic"), (true, true), "static on the first part is abstract+sealed");
        assert_eq!(visibility("FirstPublic"), 0x01, "public on the first part");
        assert_eq!(
            shape("LaterAbstract"),
            (true, false),
            "an `abstract` written on a later part is the type's, not that part's"
        );
        assert_eq!(
            shape("LaterStatic"),
            (true, true),
            "a `static` written on a later part likewise -- and its constructor was already \
             suppressed from any part, so the flags were the half that disagreed"
        );
        assert_eq!(
            visibility("LaterPublic"),
            0x01,
            "a `public` written on a later part -- this one shipped NotPublic, so the type was \
             unreachable from any other assembly"
        );
        assert_eq!(
            visibility("LaterNested"),
            0x02,
            "and NestedPublic for the nested form, which shipped NestedPrivate"
        );
    }

    /// (synthesized for its initializers) and KEEPS the flag. Keying off the presence of a `.cctor`
    /// would clear it for nearly every type that has one -- the exact outcome this change exists to
    /// stop. Only an explicitly DECLARED `static C()` is the request for precise timing.
    #[test]
    fn before_field_init_matches_csc_for_every_type_kind() {
        let image = image_of_gated_source(
            "namespace App {
                 public class NoStatics { public int Y; }
                 public class StaticFieldInit { public static int X = 5; }
                 public class ExplicitCctor { public static int X; static ExplicitCctor() { X = 5; } }
                 public struct PlainStruct { public int X; }
                 public enum Color { A, B }
                 public interface IThing { void M(); }
                 public delegate void D();
             }\n",
        );
        let assembly = Assembly::read(&image).expect("the image parses");
        let flagged = |name: &str| -> bool {
            let type_def = assembly
                .type_defs()
                .find(|t| t.name().is_some_and(|n| n.name == name))
                .unwrap_or_else(|| panic!("{name} is in the image"));
            (type_def.flags() & TYPE_BEFORE_FIELD_INIT) != 0
        };
        assert!(flagged("NoStatics"), "a class with no statics carries it");
        assert!(
            flagged("StaticFieldInit"),
            "static field initializers get a .cctor and KEEP the flag -- they are not a request \
             for precise timing"
        );
        assert!(
            !flagged("ExplicitCctor"),
            "an explicitly declared `static C()` IS the request, and is the only thing that is"
        );
        assert!(flagged("PlainStruct"), "a struct carries it");
        assert!(flagged("IThing"), "an interface carries it");
        assert!(!flagged("Color"), "an enum never carries it, measured from csc");
        assert!(!flagged("D"), "a delegate never carries it, measured from csc");
    }

    /// **`default(T)` IS THE ONLY SPELLING OF A TYPE PARAMETER'S ZERO, AND ITS VALUE CANNOT BE
    /// DECIDED WHERE IT IS WRITTEN.** `T` may close over a reference type, where the answer is
    /// `null`, or a struct, where it is an all-zero value -- and one `default(T)` is both, per
    /// instantiation. So it lowers to the single form correct for either: `initobj` over a token
    /// naming `!0`, resolved against the instantiation in hand.
    ///
    /// **ASSERTED ON THE `TypeSpec` BLOB, NOT ON THE OPCODE.** That an `initobj` is emitted says
    /// nothing about WHICH type it initializes, and every wrong answer here decodes cleanly -- a
    /// `TypeRef` to an invented type called `T` (which nothing mints, precisely so this lookup
    /// cannot find one), or `!1` where `!0` was meant.
    ///
    /// csc emits the identical token for the identical source -- `initobj` over `TypeSpec` row 1 --
    /// measured against it rather than assumed.
    #[test]
    fn default_of_a_type_parameter_initobjs_over_a_var_type_spec() {
        let image = image_of_gated_source(
            "namespace App {
                 public class Box<T> { public T Zero() { return default(T); } }
             }\n",
        );
        let assembly = Assembly::read(&image).expect("the image parses");
        let boxed = assembly
            .type_defs()
            .find(|t| t.name().is_some_and(|n| n.name == "Box`1"))
            .expect("Box`1 is in the image");
        let zero = boxed
            .methods()
            .find(|m| m.name() == Some("Zero"))
            .expect("Box`1 declares Zero");
        let token = zero
            .body()
            .expect("Zero has a body")
            .code
            .iter()
            .find_map(|instruction| match (instruction.opcode, &instruction.operand) {
                (lamella_cil::Opcode::Initobj, lamella_cil::Operand::Token(token)) => Some(*token),
                _ => None,
            })
            .expect("default(T) lowers through initobj");
        assert_eq!(
            token.table(),
            TYPE_SPEC,
            "a bare `T` is named by a TypeSpec -- it deliberately has no TypeRef of its own"
        );
        assert_eq!(
            assembly.type_spec_signature(token),
            Some(lamella_metadata::SigType::Var(0)),
            "the blob is `!0`; `!1` or a class ref would decode just as cleanly and mean otherwise"
        );
        assert_eq!(
            zero.local_variables().first(),
            Some(&lamella_metadata::SigType::Var(0))
        );
    }

    /// The three non-generic shapes, because **`default` is not a generics feature** -- it is a
    /// C# 2.0 operator that applies to every type, and it was missing for every type (measured:
    /// `default(int)` and `default(string)` failed at the PARSER exactly as `default(T)` did). Each
    /// lowers differently, so an implementation covering one would still compile the others wrongly.
    #[test]
    fn default_of_an_ordinary_type_lowers_by_what_the_type_is() {
        let image = image_of_gated_source(
            "namespace App {
                 public struct Pt { public int X; }
                 public class C {
                     public int Zi() { return default(int); }
                     public string Zs() { return default(string); }
                     public Pt Zp() { return default(Pt); }
                 }
             }\n",
        );
        let assembly = Assembly::read(&image).expect("the image parses");
        let holder = assembly
            .type_defs()
            .find(|t| t.name().is_some_and(|n| n.name == "C"))
            .expect("C is in the image");
        let opcodes = |name: &str| -> Vec<lamella_cil::Opcode> {
            holder
                .methods()
                .find(|m| m.name() == Some(name))
                .unwrap_or_else(|| panic!("C declares {name}"))
                .body()
                .expect("the method has a body")
                .code
                .iter()
                .map(|instruction| instruction.opcode)
                .collect()
        };
        assert!(opcodes("Zi").contains(&lamella_cil::Opcode::LdcI4));
        assert!(
            opcodes("Zs").contains(&lamella_cil::Opcode::Ldnull),
            "a reference type's default is the null reference"
        );
        assert!(
            opcodes("Zp").contains(&lamella_cil::Opcode::Initobj),
            "a struct's default is its all-zero value through initobj"
        );
        assert!(
            !opcodes("Zp").contains(&lamella_cil::Opcode::Ldnull),
            "a struct's default is not a null reference"
        );
    }

    /// **THE EMITTER RE-BINDS EVERY BODY, AND IT WAS DOING SO WITHOUT THE TYPE'S PARAMETERS IN
    /// SCOPE.** A local `T x;` inside `class Box<T>` bound CLEAN -- the diagnostic pass wraps body
    /// binding in `enter_type_parameters` -- and then failed at emit with *"the error type has no
    /// signature"*, because this stage binds the same body a second time and had never entered that
    /// scope. `T` resolved in one phase and became the ERROR type in the other.
    ///
    /// **NOTHING REPORTED IT, BY DESIGN.** An emit-time diagnostic reaches no one (the constructor
    /// chain says so in as many words), so the CS0246 raised here was discarded and the only
    /// evidence was a signature refusal three steps downstream. What proved where it lived: a
    /// program with BOTH `Nope y;` and `T x;` reports CS0246 for `Nope` ALONE.
    ///
    /// **`new T[1]` WAS THE SAME DEFECT WEARING A DIFFERENT MESSAGE** -- *"array creation of a
    /// non-array type"* -- because `T[]` had become an array of the error type. Two symptoms with
    /// different names, one cause, and neither name pointed at it.
    ///
    /// **ASSERTED ON THE EMITTED LOCAL SIGNATURE, WHICH IS THE PHASE THAT WAS BROKEN.** A binder
    /// test cannot see this: binding was already correct. The slot must decode as `!0`
    /// (`ELEMENT_TYPE_VAR 0`), not as a class ref and not as the error type.
    #[test]
    fn a_local_of_the_types_own_parameter_emits_as_var_zero() {
        let image = image_of_gated_source(
            "namespace App {
                 public class Box<T> {
                     public T Keep(T v) { T x = v; return x; }
                     public T[] Make() { T[] a = new T[1]; return a; }
                 }
             }\n",
        );
        let assembly = Assembly::read(&image).expect("the image parses");
        let boxed = assembly
            .type_defs()
            .find(|t| t.name().is_some_and(|n| n.name == "Box`1"))
            .expect("Box`1 is in the image");
        let locals_of = |name: &str| {
            boxed
                .methods()
                .find(|m| m.name() == Some(name))
                .unwrap_or_else(|| panic!("Box`1 declares {name}"))
                .local_variables()
        };
        assert_eq!(
            locals_of("Keep").first(),
            Some(&lamella_metadata::SigType::Var(0)),
            "a local of the declaring type's own parameter is `!0`"
        );
        assert!(
            locals_of("Make")
                .iter()
                .any(|ty| matches!(ty, lamella_metadata::SigType::SzArray(element)
                    if **element == lamella_metadata::SigType::Var(0))),
            "an array of the type's own parameter is `!0[]`: {:?}",
            locals_of("Make")
        );
    }

    /// **`Box<T>` IS TWO ROWS AND ONE SYMBOL.** Encode a constructed type whose argument is a type
    /// parameter in an EMPTY scope and its `<T>` falls through to a named-type lookup, finding the
    /// phantom `TypeRef` the walk has just minted:
    /// `T Unwrap<T>(Box<T> b) { return b.Value; }` compiled CLEAN and threw `TypeLoadException:
    /// Could not load type 'T'` on entry. Seven programs did, over both numbering spaces.
    ///
    /// **THE TABLE IS OVER POSITIONS AND SPACES, NOT AN EXAMPLE.** The same spelling `Box<T>` is
    /// `Box<!0>` in a member of `` Holder`1 `` and `Box<!!0>` in a generic method, and the binder
    /// substitutes by NAME so both display alike. One image must carry BOTH rows: a `type_specs`
    /// map keyed by the display string answered the second with the first's token, which is
    /// metadata that decodes cleanly and names the other thing.
    ///
    /// **`Var(0)` ALONE IS THE THIRD ROW AND IT IS THE ONE WITH NO INSTANTIATION IN IT.**
    /// `TOuter[] a = new TOuter[3];` needs a token for `newarr`, and a bare parameter has none by
    /// design -- so that program threw too, from a `newarr` naming a class called `TOuter`. It is
    /// in this table because a fixture built only from constructed types cannot express it.
    #[test]
    fn a_constructed_type_over_a_parameter_encodes_its_position_in_both_spaces() {
        let image = image_of_gated_source(
            "namespace App {
                 public class Box<T> { public T Value; }
                 public class Holder<TOuter> {
                     public TOuter Same(Box<TOuter> b) { return b.Value; }
                     public int Count() { TOuter[] a = new TOuter[3]; return a == null ? 0 : 3; }
                 }
                 public class Program {
                     public static T Unwrap<T>(Box<T> b) { return b.Value; }
                     public static int Main() { return 0; }
                 }
             }\n",
        );
        let assembly = Assembly::read(&image).expect("the image parses");
        let boxed = assembly
            .type_defs()
            .find(|t| t.name().is_some_and(|n| n.name == "Box`1"))
            .expect("Box`1 is in the image");
        let specs: Vec<lamella_metadata::SigType> = (1..64)
            .filter_map(|row| assembly.type_spec_signature(Token::new(TYPE_SPEC, row)))
            .collect();
        let over = |argument: lamella_metadata::SigType| lamella_metadata::SigType::GenericInst {
            definition: alloc::boxed::Box::new(lamella_metadata::SigType::Class(boxed.token())),
            arguments: alloc::vec![argument],
        };
        for (what, wanted) in [
            ("Box<!0>, the declaring type's parameter", over(lamella_metadata::SigType::Var(0))),
            ("Box<!!0>, the method's own parameter", over(lamella_metadata::SigType::MVar(0))),
        ] {
            assert!(
                specs.contains(&wanted),
                "{what} is a TypeSpec row of its own; the table holds {specs:?}"
            );
        }

        let count = assembly
            .type_defs()
            .find(|t| t.name().is_some_and(|n| n.name == "Holder`1"))
            .expect("Holder`1 is in the image")
            .methods()
            .find(|m| m.name() == Some("Count"))
            .expect("Holder`1 declares Count");
        let element = count
            .body()
            .expect("Count has a body")
            .code
            .iter()
            .find_map(|instruction| match (instruction.opcode, &instruction.operand) {
                (lamella_cil::Opcode::Newarr, lamella_cil::Operand::Token(token)) => Some(*token),
                _ => None,
            })
            .expect("`new TOuter[3]` lowers through newarr");
        assert_eq!(
            element.table(),
            TYPE_SPEC,
            "`newarr` over a type parameter names a TypeSpec; a TypeRef here is the phantom `T`"
        );
        assert_eq!(
            assembly.type_spec_signature(element),
            Some(lamella_metadata::SigType::Var(0)),
            "the element blob is `!0` -- the position, not a class of that name"
        );
        let refs: Vec<String> = assembly
            .type_refs()
            .filter_map(|r| r.name())
            .map(|n| String::from(n.name))
            .collect();
        assert!(
            !refs.iter().any(|name| name == "T" || name == "TOuter"),
            "no TypeRef is minted for either type parameter: {refs:?}"
        );
    }

    /// **THE EMITTER RE-BINDS EVERY BODY, SO BOTH PARAMETER SCOPES HAVE TO BE OPEN AROUND IT.**
    /// `emit_type` opens the declaring type's; a method's OWN `T` needs its own, or a LOCAL whose
    /// type names it resolves to the ERROR type HERE AND NOWHERE ELSE -- the diagnostic pass binds
    /// the same body inside `enter_type_parameters` and reports nothing, and an emit-time
    /// diagnostic reaches no one.
    ///
    /// **ONLY ONE OF THE FOUR CELLS EXERCISES THE METHOD SCOPE**, which is why any single example
    /// would declare the area fine or the feature absent:
    ///
    ///     Box<T> local  in a generic METHOD    the method scope, and the only cell that needs it
    ///     Box<T> local  in a generic TYPE      the DECLARING scope
    ///     Box<int> local in a generic METHOD   closed, needs no scope
    ///     Box<T> parameter of a generic method a signature, not a body
    ///
    /// The last three are controls and none is optional: without them a fix that entered the wrong
    /// scope, or that entered one and leaked it into the next method, passes the first row alone.
    #[test]
    fn a_local_naming_the_methods_own_type_parameter_resolves_at_emit() {
        let image = image_of_gated_source(
            "namespace App {
                 public class Box<T> { public T Value; }
                 public class Holder<TOuter> {
                     public int InType() { Box<TOuter> x = null; return x == null ? 1 : 0; }
                 }
                 public class Program {
                     public static int InMethod<T>() { Box<T> x = null; return x == null ? 2 : 0; }
                     public static int Closed<T>() { Box<int> x = null; return x == null ? 3 : 0; }
                     public static int Parameter<T>(Box<T> b) { return b == null ? 4 : 0; }
                     public static int Main() { return 0; }
                 }
             }\n",
        );
        let assembly = Assembly::read(&image).expect("the image parses");
        let program = assembly
            .type_defs()
            .find(|t| t.name().is_some_and(|n| n.name == "Program"))
            .expect("Program is in the image");
        for name in ["InMethod", "Closed", "Parameter"] {
            assert!(
                program
                    .methods()
                    .find(|m| m.name() == Some(name))
                    .and_then(|m| m.body())
                    .is_some(),
                "`{name}` emitted a body"
            );
        }
    }

    /// **`T[]`'s ELEMENT ACCESS TAKES THE TOKEN-CARRYING `stelem`/`ldelem`, NOT `stelem.ref`.**
    /// `T[]` is `int[]` under one instantiation and `string[]` under another, so no width-specific
    /// opcode picked at compile time is right for both. `Count<int>` storing through `stelem.ref`
    /// writes an integer where a reference is expected and faults at the next READ rather than at
    /// the store, which is why this is asserted on the emitted opcode and not on a run.
    #[test]
    fn an_array_of_a_type_parameter_stores_through_the_token_form() {
        let image = image_of_gated_source(
            "namespace App {
                 public class Program {
                     public static int Fill<T>(T seed) { T[] a = new T[4]; a[0] = seed; return 5; }
                     public static int Main() { return 0; }
                 }
             }\n",
        );
        let assembly = Assembly::read(&image).expect("the image parses");
        let fill = assembly
            .type_defs()
            .find(|t| t.name().is_some_and(|n| n.name == "Program"))
            .expect("Program is in the image")
            .methods()
            .find(|m| m.name() == Some("Fill"))
            .expect("Program declares Fill");
        let body = fill.body().expect("Fill has a body");
        assert!(
            !body
                .code
                .iter()
                .any(|instruction| instruction.opcode == lamella_cil::Opcode::StelemRef),
            "a `T` element is not stored with `stelem.ref`: {:?}",
            body.code
        );
        let store = body
            .code
            .iter()
            .find_map(|instruction| match (instruction.opcode, &instruction.operand) {
                (lamella_cil::Opcode::Stelem, lamella_cil::Operand::Token(token)) => Some(*token),
                _ => None,
            })
            .expect("`a[0] = seed` lowers through the token-carrying `stelem`");
        assert_eq!(
            assembly.type_spec_signature(store),
            Some(lamella_metadata::SigType::MVar(0)),
            "the element blob is `!!0` -- the METHOD's parameter, not the declaring type's `!0`"
        );
    }

    /// **A TYPE PARAMETER IS A POSITION, NOT A TYPE, AND ASKING FOR ITS TOKEN INVENTED ONE.** A
    /// `class Box<T> { T item; T Get(); }` emitted a `TypeRef` with an empty namespace named `T` --
    /// a reference to a type no assembly declares, which csc never writes.
    ///
    /// **THE ROW WAS UNREFERENCED, AND THAT IS WHY IT SURVIVED.** Every signature mentioning `T`
    /// encodes `!0` through the declaring scope, which matches by NAME before it would consult a
    /// token, so the emitted assembly decoded correctly and nothing was visibly wrong. The reason
    /// it matters is elsewhere: `type_sig` refuses a bare `T` by failing its named-type lookup, and
    /// that refusal is what stops a signature written in the WRONG scope from being numbered
    /// against the wrong parameter list. A registered `T` turns that refusal into
    /// `Class(TypeRef T)`.
    ///
    /// The CONTROL is the second half and it is not optional: an EXTERNAL type mentioned only in a
    /// signature must STILL be minted, in the SAME compilation and through the same function. A
    /// change that skipped every signature type would remove the phantom and break every external
    /// type a member names -- and would pass an absence-only assertion perfectly. It needs a real
    /// reference assembly, which is why this compiles against the fixture rather than using
    /// `image_of_gated_source` (which passes none, so nothing external exists to mint).
    #[test]
    fn a_type_parameter_gets_no_type_ref_but_a_real_signature_type_still_does() {
        let Some(reference) = reference_fixture(NESTED_FIXTURE) else {
            return;
        };
        let reference = Assembly::read(&reference).expect("the fixture parses");
        let compilation = compile_source_with(
            "namespace App {
                 public class Box<T> {
                     private T item;
                     public Box(T value) { item = value; }
                     public T Get() { return item; }
                     public Fixture.Plain Only() { return null; }
                 }
             }\n",
            "test.cs",
            "test",
            "test",
            core::slice::from_ref(&reference),
            false,
            LexOptions {
                version: LanguageVersion::CSharp2,
                ..LexOptions::default()
            },
        );
        assert!(
            compilation.diagnostics.is_empty(),
            "the source compiles clean: {:?}",
            compilation.diagnostics
        );
        let image = compilation.image.expect("an image is emitted");
        let assembly = Assembly::read(&image).expect("the image parses");
        let refs: Vec<(String, String)> = assembly
            .type_refs()
            .filter_map(|r| r.name())
            .map(|n| (String::from(n.namespace), String::from(n.name)))
            .collect();
        assert!(
            !refs.iter().any(|(_, name)| name == "T"),
            "no TypeRef is minted for the type parameter `T`: {refs:?}"
        );
        assert!(
            refs.iter().any(|(_, name)| name == "Plain"),
            "an external type named only in a signature is still minted: {refs:?}"
        );
    }

    /// **THE SPELLING EVERY REAL PROGRAM USES FOR ITS OWN TYPES, AND IT DID NOT RESOLVE.** A
    /// `Box<int>` written inside the namespace that declares `` Box`1 `` was CS0246: an
    /// instantiation's definition was looked up by its dotted name EXACTLY, never through the
    /// in-scope search a plain name gets, and the model keys a generic type by its ARITY-MANGLED
    /// name -- so searching those scopes for `Box` would have found nothing either.
    ///
    /// **THE CONTROLS ARE BOTH HALVES AND NEITHER IS OPTIONAL.** The non-generic `Plain` proves the
    /// enclosing-namespace search works at all, so its absence for the generic is not a shared
    /// cause; the QUALIFIED `App.Box<int>` proves the type is in the model and the arity is right,
    /// so the failure is the unqualified lookup and nothing else. Without them a missing type and a
    /// missing lookup are the same CS0246.
    #[test]
    fn an_unqualified_generic_name_resolves_in_its_own_namespace() {
        let image = image_of_gated_source(
            "namespace App {
                 public class Box<T> { private T item; public Box(T v) { item = v; } public T Get() { return item; } }
                 public class Plain { private int item; public Plain(int v) { item = v; } public int Get() { return item; } }
                 public class Program {
                     static int Unqualified(Box<int> b) { return b.Get(); }
                     static int Qualified(App.Box<int> b) { return b.Get(); }
                     static int Control(Plain p) { return p.Get(); }
                     public static int Main() { return 0; }
                 }
             }\n",
        );
        let assembly = Assembly::read(&image).expect("the image parses");
        assert!(
            assembly
                .type_defs()
                .any(|t| t.name().is_some_and(|n| n.name == "Box`1")),
            "the generic type the three signatures name is in the image"
        );
    }

    /// The control, and it is what makes the row above mean something: a member of a NON-generic
    /// type in the very same image still takes a `TypeRef`/`TypeDef` parent and mints no
    /// `TypeSpec` at all. A change that parented EVERY `MemberRef` on a `TypeSpec` would pass
    /// every assertion above.
    #[test]
    fn a_member_of_a_non_generic_type_takes_no_type_spec_parent() {
        let image = image_of_gated_source(
            "namespace App {
                 public class Plain {
                     private int item;
                     public Plain(int value) { item = value; }
                     public int Get() { return item; }
                 }
                 public class Program {
                     public static int Main() { return new Plain(41).Get(); }
                 }
             }\n",
        );
        let assembly = Assembly::read(&image).expect("the image parses");
        let members = member_refs_of(&assembly);
        assert!(
            members.iter().all(|(parent, ..)| *parent != TYPE_SPEC),
            "no member of a non-generic type is parented on a TypeSpec: {members:?}"
        );
    }

    /// **THE HALF A "SIMPLIFICATION" WOULD TAKE AWAY.** The substituted signature is right there on
    /// the bound reference and it is the obvious thing to write; `instance void .ctor(int32)`
    /// decodes cleanly, describes a method `` Box`1 `` does not declare, and nothing downstream
    /// says so. This asserts the parameter is `ELEMENT_TYPE_VAR 0` (`0x13 0x00`) and NOT
    /// `ELEMENT_TYPE_I4` (`0x08`), on the bytes, because that is where the two differ.
    #[test]
    fn an_instantiated_members_signature_is_open_not_substituted() {
        let image = image_of_gated_source(
            "namespace App {
                 public class Box<T> {
                     private T item;
                     public Box(T value) { item = value; }
                     public T Get() { return item; }
                 }
                 public class Program {
                     public static int Main() { object o = new App.Box<int>(41); return o == null ? 1 : 0; }
                 }
             }\n",
        );
        let assembly = Assembly::read(&image).expect("the image parses");
        let ctor = member_refs_of(&assembly)
            .into_iter()
            .find(|(parent, name, _)| *parent == TYPE_SPEC && name == ".ctor")
            .expect("the constructor of Box<int> is minted");
        assert_eq!(
            ctor.2,
            &[0x20, 0x01, 0x01, 0x13, 0x00],
            "the parameter is `!0`; `0x08` there would be the substituted `int32`"
        );
    }

    /// The ordinary path is unchanged: a NON-generic call still names a plain `MemberRef` and
    /// mints no `MethodSpec` at all. Without this the branch above could have swallowed every call
    /// in the language and the generic rows would still pass.
    #[test]
    fn a_non_generic_call_still_names_a_member_ref() {
        let Some(expr) = bound_expression("Fixture.Util.Plain(1)") else { return };
        let mut image = ImageBuilder::new("test.dll", "test");
        let mut tokens = Tokens::new();
        let site = emitted_call_token(&expr, &mut image, &mut tokens).expect("a plain call emits");
        assert_eq!(site.table(), lamella_metadata::tables::table::MEMBER_REF);
    }

    /// A generic STRUCT instantiates to a VALUE type, not a reference type. The two are not
    /// interchangeable in a signature: emitting `Class` for a value type decodes cleanly and boxes
    /// where nothing should be boxed, which is the quiet half of the same family as the cast hole.
    #[test]
    fn an_instantiated_generic_struct_lowers_to_a_value_type() {
        let mangled = TypeSymbol::Named(["App".into(), "Pair`1".into()].into());
        let mut tokens = Tokens::new();
        tokens.insert_type(&mangled, Token::new(0x02, 9));
        tokens.insert_struct(&mangled);

        let sig = type_sig(
            &tokens,
            &TypeSymbol::Instantiation {
                definition: ["App".into(), "Pair".into()].into(),
                arguments: [TypeSymbol::Special(SpecialType::Int32)].into(),
            },
        )
        .expect("an instantiated struct lowers");
        match sig {
            TypeSig::GenericInst { definition, .. } => {
                assert_eq!(*definition, TypeSig::ValueType(Token::new(0x02, 9)));
            }
            other => panic!("expected GENERICINST, got {other:?}"),
        }
    }

    #[test]
    fn validated_program_is_minted_only_by_a_clean_bind() {
        let no_units: &[CompilationUnit] = &[];
        let no_refs: &[Assembly] = &[];
        assert!(ValidatedProgram::from_clean_bind(no_units, no_refs, false).is_some());
        assert!(ValidatedProgram::from_clean_bind(no_units, no_refs, true).is_none());
    }

    #[test]
    fn an_object_initializer_emits_a_store_per_member_and_leaves_the_object() {
        let unit = parse_compilation_unit(
            "namespace App { \
               public class Inner { public int G; } \
               public class C { \
                 public int F; \
                 private int p; \
                 public int P { get { return p; } set { p = value; } } \
                 public Inner N; \
                 public static C Make() { return new C { F = 1, P = 2, N = { G = 3 } }; } \
               } }",
        )
        .unit;

        let result = compile(
            &unit,
            "app.dll",
            "app",
            &[],
            None,
            false,
            false,
            false,
            LanguageVersion::CSharp3,
        );
        assert!(
            result.diagnostics.is_empty(),
            "an object initializer should compile clean at C# 3: {:?}",
            result.diagnostics
        );
        let with_initializer = body_of(&result.image.expect("an image"), "Make");

        let control_unit = parse_compilation_unit(
            "namespace App { \
               public class Inner { public int G; } \
               public class C { \
                 public int F; \
                 private int p; \
                 public int P { get { return p; } set { p = value; } } \
                 public Inner N; \
                 public static C Make() { return new C(); } \
               } }",
        )
        .unit;
        let control = compile(
            &control_unit,
            "app.dll",
            "app",
            &[],
            None,
            false,
            false,
            false,
            LanguageVersion::CSharp3,
        );
        let without = body_of(&control.image.expect("an image"), "Make");

        assert!(
            with_initializer > without,
            "the initializer must emit a store per member: body was {with_initializer} bytes \
             with it and {without} without -- equal means the members were dropped"
        );
    }

    /// The IL byte length of the named method in `image`, for comparing two emissions of one
    /// program shape.
    fn body_of(image: &[u8], method: &str) -> usize {
        let assembly = Assembly::read(image).expect("the reader parses the image");
        assembly
            .type_defs()
            .flat_map(|ty| ty.methods().collect::<Vec<_>>())
            .find(|m| m.name() == Some(method))
            .and_then(|m| m.body())
            .map(|body| body.code.len())
            .unwrap_or_else(|| panic!("no body for {method}"))
    }

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
    fn assembly_flags_algid_culture_are_consumed_into_the_assembly_row() {
        let unit = parse_compilation_unit(
            "[assembly: System.Reflection.AssemblyFlags(0x100u)] \
             [assembly: System.Reflection.AssemblyAlgorithmId(0x8004u)] \
             [assembly: System.Reflection.AssemblyCulture(\"en-US\")] \
             class C { static int Main() { return 0; } }",
        )
        .unit;
        let result = compile_unit(&unit, "attrs.dll", "attrs");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let image = result.image.expect("an image");
        let assembly = Assembly::read(&image).expect("the reader parses the image");

        assert_eq!(assembly.assembly_flags(), 0x100);
        assert_eq!(assembly.assembly_hash_algorithm(), 0x8004);
        assert_eq!(assembly.assembly_culture(), Some("en-US"));
    }

    #[test]
    fn referenced_exception_carries_its_base_chain() {
        let refs = parse_compilation_unit(
            "namespace System { \
                public class Exception { } \
                public class SystemException : Exception { } \
                public class ArithmeticException : SystemException { } \
                public class DivideByZeroException : ArithmeticException { } }",
        )
        .unit;
        let ref_image = compile_unit(&refs, "refs.dll", "refs")
            .image
            .expect("ref image");
        let reference = Assembly::read(&ref_image).expect("ref assembly");

        let program = parse_compilation_unit(
            "public class P { public object M() { \
                return new System.DivideByZeroException(); } }",
        )
        .unit;
        let compiled =
            compile_unit_with_references(&program, "p.dll", "p", core::slice::from_ref(&reference));
        assert!(compiled.diagnostics.is_empty(), "{:?}", compiled.diagnostics);
        let image = compiled
            .image
            .unwrap_or_else(|| panic!("program image; emit_error = {:?}", compiled.emit_error));
        let assembly = Assembly::read(&image).expect("program assembly");

        let token = (1..)
            .map_while(|index| assembly.type_ref(index).map(|t| (index, t)))
            .find(|(_, t)| {
                t.name()
                    .is_some_and(|n| n.namespace == "System" && n.name == "DivideByZeroException")
            })
            .map(|(index, _)| Token::new(TYPE_REF, index))
            .expect("a DivideByZeroException TypeRef");
        assert_eq!(
            assembly.exception_base_chain(token),
            Some(alloc::vec![
                exception_tag_for_name("System", "DivideByZeroException"),
                exception_tag_for_name("System", "ArithmeticException"),
                exception_tag_for_name("System", "SystemException"),
                exception_tag_for_name("System", "Exception"),
            ])
        );
    }

    #[test]
    fn synthesized_ctor_chains_to_a_referenced_base_not_object() {
        let refs = parse_compilation_unit(
            "namespace Lib { public class Base { public int X; public Base() { X = 42; } } }",
        )
        .unit;
        let ref_image = compile_unit(&refs, "lib.dll", "lib").image.expect("ref image");
        let reference = Assembly::read(&ref_image).expect("ref assembly");

        let program = parse_compilation_unit("public class Derived : Lib.Base { }").unit;
        let compiled =
            compile_unit_with_references(&program, "p.dll", "p", core::slice::from_ref(&reference));
        assert!(compiled.diagnostics.is_empty(), "{:?}", compiled.diagnostics);
        let image = compiled
            .image
            .unwrap_or_else(|| panic!("program image; emit_error = {:?}", compiled.emit_error));
        let assembly = Assembly::read(&image).expect("program assembly");

        let chains_to_base = assembly.member_refs().any(|member| {
            member.name() == Some(".ctor") && {
                let parent = member.parent();
                parent.table() == TYPE_REF
                    && assembly
                        .type_ref(parent.row())
                        .and_then(|type_ref| type_ref.name())
                        .is_some_and(|name| name.namespace == "Lib" && name.name == "Base")
            }
        });
        assert!(
            chains_to_base,
            "the synthesized constructor must chain to Lib.Base::.ctor, not System.Object::.ctor"
        );
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
    fn a_rectangular_array_local_slot_keeps_its_rank() {
        let unit = parse_compilation_unit(
            "class Program { static int Main() { int[,] m = new int[2, 3]; \
                m[0, 0] = 42; return m[0, 0]; } }",
        )
        .unit;
        let result = compile_unit(&unit, "arr2dlocal.dll", "arr2dlocal");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let image = result.image.expect("an image");
        let assembly = Assembly::read(&image).expect("the reader parses the image");
        let main = assembly
            .find_type("", "Program")
            .expect("the Program type")
            .methods()
            .find(|method| method.name() == Some("Main"))
            .expect("the Main method");
        let locals = main.local_variables();
        let slot = locals.first().expect("Main declares the m local");
        match slot {
            lamella_metadata::signature::SigType::Array { element, rank } => {
                assert_eq!(*rank, 2, "the `int[,]` local slot must be rank 2");
                assert!(
                    matches!(**element, lamella_metadata::signature::SigType::I4),
                    "element was {element:?}"
                );
            }
            other => panic!("expected the local slot to be SigType::Array {{ rank: 2 }}, got {other:?}"),
        }
    }

    /// The `fixed` statement's holder slot must be `pinned` in the emitted local-variable
    /// signature AND that constraint must survive the metadata READ -- the producer and the
    /// consumer, in one assertion, because a `pinned` byte the reader discards is worth exactly
    /// as much as one never written.
    ///
    /// Why the pin is the whole statement: `fixed` hands the program a `T*`, and an unmanaged
    /// pointer is NOT reported to the garbage collector (ECMA-335: `ELEMENT_TYPE_PTR` is not a
    /// GC-tracked type). So on a MOVING collector the only thing keeping that pointer valid is
    /// the array not moving, and the only thing that says so is this constraint.
    ///
    /// The pointer slot is asserted UNPINNED in the same breath: a fix that pins every slot in a
    /// method containing `fixed` would satisfy the first half and quietly defeat compaction.
    #[test]
    fn a_fixed_statements_holder_slot_is_pinned_and_the_pointer_slot_is_not() {
        use lamella_metadata::signature::SigType;
        let unit = parse_compilation_unit(
            "class Program { static unsafe int Main() { int[] arr = new int[4]; \
                 fixed (int* p = arr) { p[0] = 42; } return arr[0]; } }",
        )
        .unit;
        let result = compile_unit(&unit, "pinned.dll", "pinned");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let image = result.image.expect("an image");
        let assembly = Assembly::read(&image).expect("the reader parses the image");
        let main = assembly
            .find_type("", "Program")
            .expect("the Program type")
            .methods()
            .find(|method| method.name() == Some("Main"))
            .expect("the Main method");
        let slots = main.local_variable_slots();

        let pinned: Vec<_> = slots.iter().filter(|slot| slot.pinned).collect();
        assert!(
            !pinned.is_empty(),
            "the `fixed` holder slot must report `pinned`; slots were {slots:?}"
        );
        for slot in &pinned {
            assert!(
                matches!(slot.ty, SigType::SzArray(_) | SigType::ByRef(_)),
                "a pinned slot must hold something the collector can pin, got {:?}",
                slot.ty
            );
        }
        let pointer_slots: Vec<_> = slots
            .iter()
            .filter(|slot| matches!(slot.ty, SigType::Pointer(_)))
            .collect();
        assert!(
            !pointer_slots.is_empty(),
            "Main declares the `int* p` local; slots were {slots:?}"
        );
        for slot in pointer_slots {
            assert!(!slot.pinned, "the pointer slot must not be pinned: {slot:?}");
        }
    }

    /// TWO INSTANTIATIONS OF ONE NESTED TYPE ARE TWO TYPES, and neither converts to the other.
    ///
    /// **THIS IS THE HALF THAT MADE THE PARSE SAFE TO LAND, AND WITHOUT IT THE PARSE IS A
    /// MISCOMPILE.** Until `Box<int>.Cursor` could be written at all, a nested type was nameable
    /// only from INSIDE its enclosing type, where its arguments are never in question -- so no
    /// program could hold two instantiations of one nested type and assign between them. Accepting
    /// the name while the binder built the OPEN definition from it would have made both
    /// `` Box`1.Cursor ``, accepted the assignment, and stored a `Cursor<int>` into a
    /// `Cursor<string>` slot.
    ///
    /// The message is csc's, measured on this exact source at `/langversion:ISO-2`:
    /// *Cannot implicitly convert type 'Box<int>.Cursor' to 'Box<string>.Cursor'*. The QUOTED
    /// SPELLING is half the assertion: a symbol that spelled itself `` Box`1.Cursor<int> `` would
    /// refuse correctly and then name a type that appears in no source, and CS0029 exists to be
    /// read beside the program.
    /// A NESTED GENERIC TYPE NAMED BARE takes its enclosing type's arguments too -- `Pair<string>`
    /// written inside `Box<T>` is `Box<T>.Pair<string>`, arity 2, enclosing first (II.9.2).
    ///
    /// **ASSERTED THROUGH A REFUSAL, BECAUSE A CLEAN COMPILE CANNOT SEE THIS.** A resolution that
    /// dropped the enclosing argument still compiles -- it just names a different type, and every
    /// use inside the declaration agrees with itself. The witness is a caller in another
    /// instantiation: if `Make()` returned anything but `Box<int>.Pair<string>`, the assignment
    /// below would either be accepted or refused with a different pair of names.
    ///
    /// The message is csc's, measured on this source at `/langversion:ISO-2`.
    #[test]
    fn a_nested_generic_named_bare_takes_its_enclosing_type_s_arguments() {
        let options = LexOptions {
            version: LanguageVersion::CSharp2,
            ..LexOptions::default()
        };
        let source = "public class Box<T> { \
                 public class Pair<U> { public U b; } \
                 public Pair<string> Make() { return new Pair<string>(); } } \
             class Program { static int Main() { \
                 Box<INSTANTIATION>.Pair<string> q = new Box<int>().Make(); \
                 return q.b == null ? 0 : 1; } }";

        let unit = parse_compilation_unit_with(&source.replace("INSTANTIATION", "int"), options.clone())
            .unit;
        let result = compile(
            &unit,
            "bare.dll",
            "bare",
            &[],
            None,
            false,
            false,
            false,
            LanguageVersion::CSharp2,
        );
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some(), "{:?}", result.emit_error);

        let unit = parse_compilation_unit_with(&source.replace("INSTANTIATION", "long"), options).unit;
        let result = compile(
            &unit,
            "bare2.dll",
            "bare2",
            &[],
            None,
            false,
            false,
            false,
            LanguageVersion::CSharp2,
        );
        let errors: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.is_error())
            .collect();
        assert_eq!(errors.len(), 1, "{:?}", result.diagnostics);
        assert_eq!(errors[0].code, 29);
        assert_eq!(
            &*errors[0].message,
            "Cannot implicitly convert type 'Box<int>.Pair<string>' to 'Box<long>.Pair<string>'"
        );
    }

    #[test]
    fn two_instantiations_of_one_nested_type_do_not_convert() {
        let options = LexOptions {
            version: LanguageVersion::CSharp2,
            ..LexOptions::default()
        };
        let unit = parse_compilation_unit_with(
            "public class Box<T> { public struct Cursor { public int n; } }              class Program { static int Main() {                  Box<int>.Cursor a = new Box<int>.Cursor();                  Box<string>.Cursor b = a;                  return b.n; } }",
            options.clone(),
        )
        .unit;
        let result = compile(
            &unit,
            "id.dll",
            "id",
            &[],
            None,
            false,
            false,
            false,
            LanguageVersion::CSharp2,
        );
        let errors: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.is_error())
            .collect();
        assert_eq!(errors.len(), 1, "{:?}", result.diagnostics);
        assert_eq!(errors[0].code, 29);
        assert_eq!(
            &*errors[0].message,
            "Cannot implicitly convert type 'Box<int>.Cursor' to 'Box<string>.Cursor'"
        );
        assert!(result.image.is_none(), "an errored compilation emits nothing");

        let same = parse_compilation_unit_with(
            "public class Box<T> { public struct Cursor { public int n; } }              class Program { static int Main() {                  Box<int>.Cursor a = new Box<int>.Cursor();                  Box<int>.Cursor b = a;                  return b.n; } }",
            options,
        )
        .unit;
        let result = compile(
            &same,
            "id2.dll",
            "id2",
            &[],
            None,
            false,
            false,
            false,
            LanguageVersion::CSharp2,
        );
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.image.is_some(), "{:?}", result.emit_error);
    }

    #[test]
    fn a_nested_type_carries_its_enclosing_type_s_parameters() {
        let options = LexOptions {
            version: LanguageVersion::CSharp2,
            ..LexOptions::default()
        };
        let unit = parse_compilation_unit_with(
            "public class Box<T> { \
                 public struct Cursor { public T held; } \
                 public class Pair<U> { public T first; public U second; } \
                 public struct Ring { public struct Hub { public T deep; } } } \
             public class Plain { public struct Inner { public int n; } }",
            options,
        )
        .unit;
        let result = compile(
            &unit,
            "n.dll",
            "n",
            &[],
            None,
            false,
            false,
            false,
            LanguageVersion::CSharp2,
        );
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let image = result.image.expect("an image");
        let assembly = Assembly::read(&image).expect("the reader parses the image");
        let parameters_of = |name: &str| -> Vec<String> {
            let ty = assembly
                .find_type("", name)
                .unwrap_or_else(|| panic!("type {name} is missing"));
            let owner = ty.token().row() << 1;
            let mut found: Vec<(u32, String)> = assembly
                .generic_params()
                .filter(|(_, _, parameter_owner, _)| *parameter_owner == owner)
                .map(|(number, _, _, parameter_name)| {
                    (number, parameter_name.unwrap_or_default().to_string())
                })
                .collect();
            found.sort_by_key(|(number, _)| *number);
            found.into_iter().map(|(_, name)| name).collect()
        };
        assert_eq!(parameters_of("Box`1"), ["T"]);
        assert_eq!(parameters_of("Cursor"), ["T"]);
        assert_eq!(parameters_of("Pair`1"), ["T", "U"]);
        assert_eq!(parameters_of("Hub"), ["T"]);
        assert_eq!(parameters_of("Inner"), Vec::<String>::new());
    }

    #[test]
    fn a_property_accessor_takes_the_accessibility_the_language_gives_it() {
        let options = LexOptions {
            version: LanguageVersion::CSharp2,
            ..LexOptions::default()
        };
        let unit = parse_compilation_unit_with(
            "class C { int f; \
                 private int Priv { get { return f; } set { f = value; } } \
                 internal int Int { get { return f; } set { f = value; } } \
                 protected int Prot { get { return f; } set { f = value; } } \
                 public int Pub { get { return f; } set { f = value; } } \
                 public int Narrowed { get { return f; } private set { f = value; } } \
                 protected internal int Both { get { return f; } internal set { f = value; } } }",
            options,
        )
        .unit;
        let result = compile(
            &unit,
            "a.dll",
            "a",
            &[],
            None,
            false,
            false,
            false,
            LanguageVersion::CSharp2,
        );
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let image = result.image.expect("an image");
        let assembly = Assembly::read(&image).expect("the reader parses the image");
        let ty = assembly.find_type("", "C").expect("the C type");
        let access_of = |name: &str| -> u32 {
            ty.methods()
                .find(|method| method.name() == Some(name))
                .unwrap_or_else(|| panic!("method {name} is missing"))
                .flags()
                & 0x0007
        };
        const PRIVATE: u32 = 0x0001;
        const ASSEMBLY: u32 = 0x0003;
        const FAMILY: u32 = 0x0004;
        const FAM_OR_ASSEM: u32 = 0x0005;
        const PUBLIC: u32 = 0x0006;
        for (property, expected) in [
            ("Priv", PRIVATE),
            ("Int", ASSEMBLY),
            ("Prot", FAMILY),
            ("Pub", PUBLIC),
        ] {
            assert_eq!(access_of(&alloc::format!("get_{property}")), expected, "get_{property}");
            assert_eq!(access_of(&alloc::format!("set_{property}")), expected, "set_{property}");
        }
        assert_eq!(access_of("get_Narrowed"), PUBLIC);
        assert_eq!(access_of("set_Narrowed"), PRIVATE);
        assert_eq!(access_of("get_Both"), FAM_OR_ASSEM);
        assert_eq!(access_of("set_Both"), ASSEMBLY);
    }

    #[test]
    fn an_auto_property_gets_a_backing_field_and_two_synthesized_accessors() {
        use lamella_cil::Opcode;
        let unit = parse_compilation_unit(
            "class C { public int Value { get; set; } \
                 public static int Count { get; set; } \
                 int m; public int Manual { get { return m; } set { m = value; } } }",
        )
        .unit;
        let result = compile(
            &unit,
            "ap.dll",
            "ap",
            &[],
            None,
            false,
            false,
            false,
            LanguageVersion::CSharp3,
        );
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let image = result.image.expect("an image");
        let assembly = Assembly::read(&image).expect("the reader parses the image");
        let ty = assembly.find_type("", "C").expect("the C type");

        const GENERATED: &str = "System.Runtime.CompilerServices.CompilerGeneratedAttribute";
        const BROWSABLE: &str = "System.Diagnostics.DebuggerBrowsableAttribute";

        let field_named = |name: &str| {
            ty.fields()
                .find(|field| field.name() == Some(name))
                .unwrap_or_else(|| panic!("field {name} is missing"))
        };
        let value_field = field_named("<Value>k__BackingField");
        assert_eq!(value_field.flags() & 0x0007, u32::from(FIELD_PRIVATE));
        assert!(
            !value_field.is_static(),
            "an instance property's backing field is an instance one"
        );
        assert_eq!(
            attribute_names_on(&assembly, value_field.token()),
            [BROWSABLE, GENERATED]
        );
        assert!(
            field_named("<Count>k__BackingField").is_static(),
            "a static property's backing field is static"
        );
        assert!(
            ty.fields()
                .all(|field| field.name() != Some("<Manual>k__BackingField")),
            "a property with written accessors gets NO backing field"
        );

        let opcodes = |name: &str| -> Vec<Opcode> {
            ty.methods()
                .find(|method| method.name() == Some(name))
                .unwrap_or_else(|| panic!("method {name} is missing"))
                .body()
                .unwrap_or_else(|| panic!("method {name} has no body"))
                .code
                .iter()
                .map(|instruction| instruction.opcode)
                .collect()
        };
        assert_eq!(
            opcodes("get_Value"),
            [Opcode::Ldarg, Opcode::Ldfld, Opcode::Ret]
        );
        assert_eq!(
            opcodes("set_Value"),
            [Opcode::Ldarg, Opcode::Ldarg, Opcode::Stfld, Opcode::Ret]
        );
        assert_eq!(opcodes("get_Count"), [Opcode::Ldsfld, Opcode::Ret]);
        assert_eq!(
            opcodes("set_Count"),
            [Opcode::Ldarg, Opcode::Stsfld, Opcode::Ret]
        );

        for accessor in ["get_Value", "set_Value", "get_Count", "set_Count"] {
            let method = ty
                .methods()
                .find(|method| method.name() == Some(accessor))
                .expect("the accessor");
            assert_eq!(
                attribute_names_on(&assembly, method.token()),
                [GENERATED],
                "{accessor} is compiler-written and says so"
            );
            assert_ne!(
                method.flags() & u32::from(SPECIAL_NAME),
                0,
                "{accessor} is an accessor, so it is specialname"
            );
        }
        assert_eq!(
            attribute_names_on(
                &assembly,
                ty.methods()
                    .find(|method| method.name() == Some("get_Manual"))
                    .expect("get_Manual")
                    .token()
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn an_exception_filter_becomes_a_filter_clause_that_carries_no_type() {
        use lamella_cil::{EhKind, Opcode};
        let unit = parse_compilation_unit(
            "namespace System { public class Exception { } } \
             class Program { static int n; public static void Main() { \
                 try { n = 1; } \
                 catch (System.Exception e) when (e != null) { n = n + 2; } \
                 catch (System.Exception) { n = 3; } } }",
        )
        .unit;
        let result = compile(
            &unit,
            "ef.dll",
            "ef",
            &[],
            None,
            false,
            false,
            false,
            LanguageVersion::CSharp6,
        );
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let image = result.image.expect("an image");
        let assembly = Assembly::read(&image).expect("the reader parses the image");
        let main = assembly
            .find_type("", "Program")
            .expect("the Program type")
            .methods()
            .find(|method| method.name() == Some("Main"))
            .expect("the Main method");
        let body = main.body().expect("Main has a method body");
        assert_eq!(body.handlers.len(), 2, "one filtered clause and one typed one");

        let EhKind::Filter { filter_start } = body.handlers[0].kind else {
            panic!("the first clause is filtered, got {:?}", body.handlers[0].kind);
        };
        assert!(
            matches!(body.handlers[1].kind, EhKind::Catch(_)),
            "the control clause is an ordinary typed catch, got {:?}",
            body.handlers[1].kind
        );
        let handler_start = body.handlers[0].handler_range.start;
        assert!(
            filter_start < handler_start,
            "the filter runs before the handler it guards"
        );
        assert_eq!(
            body.code[handler_start as usize - 1].opcode,
            Opcode::Endfilter,
            "the instruction before the handler is the filter's verdict"
        );
        assert_eq!(body.code[filter_start as usize].opcode, Opcode::Isinst);
        assert_eq!(body.code[handler_start as usize].opcode, Opcode::Pop);
        assert_eq!(
            body.code[body.handlers[1].handler_range.start as usize].opcode,
            Opcode::Pop,
            "the control catch is unnamed, so it discards too -- see the named case in the gate"
        );
    }

    #[test]
    fn a_readonly_auto_property_gets_an_initonly_field_and_no_setter() {
        use lamella_cil::Opcode;
        let unit = parse_compilation_unit(
            "class C { public int Only { get; } \
                 public int Writable { get; set; } \
                 public static int Shared { get; } \
                 int m = 7; public int Manual { get { return m; } } }",
        )
        .unit;
        let result = compile(
            &unit,
            "ro.dll",
            "ro",
            &[],
            None,
            false,
            false,
            false,
            LanguageVersion::CSharp6,
        );
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let image = result.image.expect("an image");
        let assembly = Assembly::read(&image).expect("the reader parses the image");
        let ty = assembly.find_type("", "C").expect("the C type");
        let field_named = |name: &str| {
            ty.fields()
                .find(|field| field.name() == Some(name))
                .unwrap_or_else(|| panic!("field {name} is missing"))
        };
        let initonly = |name: &str| field_named(name).flags() & u32::from(FIELD_INITONLY) != 0;
        assert!(initonly("<Only>k__BackingField"), "`{{ get; }}` is write-once");
        assert!(
            initonly("<Shared>k__BackingField"),
            "and so is the static spelling of it"
        );
        assert!(
            !initonly("<Writable>k__BackingField"),
            "`{{ get; set; }}` has a setter, so its field is not initonly"
        );
        assert!(
            ty.fields()
                .all(|field| field.name() != Some("<Manual>k__BackingField")),
            "a getter-only property with a WRITTEN body is not an auto-property"
        );
        let method = |name: &str| ty.methods().find(|method| method.name() == Some(name));
        assert!(
            method("set_Only").is_none(),
            "there is no setter to synthesize, and emitting one would make the property writable"
        );
        assert!(method("set_Writable").is_some(), "the control still has one");
        assert_eq!(
            method("get_Only")
                .expect("get_Only")
                .body()
                .expect("a body")
                .code
                .iter()
                .map(|instruction| instruction.opcode)
                .collect::<Vec<_>>(),
            [Opcode::Ldarg, Opcode::Ldfld, Opcode::Ret]
        );
    }

    #[test]
    fn an_auto_property_initializer_stores_to_the_backing_field_in_declaration_order() {
        use lamella_cil::Opcode;
        let unit = parse_compilation_unit(
            "class C { public int A { get; set; } = 1; public int F = 2; \
                 public int B { get; } = 3; public static int S { get; } = 4; }",
        )
        .unit;
        let result = compile(
            &unit,
            "api.dll",
            "api",
            &[],
            None,
            false,
            false,
            false,
            LanguageVersion::CSharp6,
        );
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let image = result.image.expect("an image");
        let assembly = Assembly::read(&image).expect("the reader parses the image");
        let ty = assembly.find_type("", "C").expect("the C type");
        assert_eq!(
            ty.fields().filter_map(|field| field.name()).collect::<Vec<_>>(),
            [
                "<A>k__BackingField",
                "F",
                "<B>k__BackingField",
                "<S>k__BackingField",
            ]
        );
        let opcodes = |name: &str| -> Vec<Opcode> {
            ty.methods()
                .find(|method| method.name() == Some(name))
                .unwrap_or_else(|| panic!("method {name} is missing"))
                .body()
                .unwrap_or_else(|| panic!("method {name} has no body"))
                .code
                .iter()
                .map(|instruction| instruction.opcode)
                .collect()
        };
        assert_eq!(
            opcodes(".ctor"),
            [
                Opcode::Ldarg,
                Opcode::LdcI4,
                Opcode::Stfld,
                Opcode::Ldarg,
                Opcode::LdcI4,
                Opcode::Stfld,
                Opcode::Ldarg,
                Opcode::LdcI4,
                Opcode::Stfld,
                Opcode::Ldarg,
                Opcode::Call,
                Opcode::Ret,
            ]
        );
        assert_eq!(
            opcodes(".cctor"),
            [Opcode::LdcI4, Opcode::Stsfld, Opcode::Ret]
        );
    }

    #[test]
    fn a_generic_auto_property_reaches_its_field_through_a_type_spec() {
        use lamella_cil::Operand;
        let options = LexOptions {
            version: LanguageVersion::CSharp3,
            ..LexOptions::default()
        };
        let unit =
            parse_compilation_unit_with("class Bag<T> { public T Item { get; set; } }", options)
                .unit;
        let result = compile(
            &unit,
            "b.dll",
            "b",
            &[],
            None,
            false,
            false,
            false,
            LanguageVersion::CSharp3,
        );
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let image = result.image.expect("an image");
        let assembly = Assembly::read(&image).expect("the reader parses the image");
        let ty = assembly.find_type("", "Bag`1").expect("the Bag type");
        for accessor in ["get_Item", "set_Item"] {
            let code = ty
                .methods()
                .find(|method| method.name() == Some(accessor))
                .expect("the accessor")
                .body()
                .expect("a body")
                .code;
            let field = code
                .iter()
                .find_map(|instruction| match instruction.operand {
                    Operand::Token(token) => Some(token),
                    _ => None,
                })
                .expect("a field operand");
            assert_eq!(
                field.table(),
                lamella_metadata::tables::table::MEMBER_REF,
                "{accessor}'s field operand is a MemberRef over the type's own instantiation"
            );
        }
    }

    #[test]
    fn a_struct_constructor_chaining_to_this_zero_initializes_rather_than_calling() {
        use lamella_cil::Opcode;
        let unit = parse_compilation_unit(
            "struct Cell { public int A; public int B;                  public Cell(int a) : this() { A = a; }                  public Cell(int a, int b) : this(a) { B = b; } }              class Holder { public int N; public Holder(int n) : this() { N = n; } public Holder() { } }",
        )
        .unit;
        let result = compile_unit(&unit, "s.dll", "s");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let image = result.image.expect("an image");
        let assembly = Assembly::read(&image).expect("the reader parses the image");
        let opcodes = |type_name: &str, parameters: usize| -> Vec<Opcode> {
            assembly
                .find_type("", type_name)
                .expect("the type")
                .methods()
                .filter(|method| method.name() == Some(".ctor"))
                .find(|method| {
                    method
                        .signature()
                        .is_some_and(|signature| signature.parameters.len() == parameters)
                })
                .expect("the constructor")
                .body()
                .expect("a body")
                .code
                .iter()
                .map(|instruction| instruction.opcode)
                .collect()
        };
        assert_eq!(
            &opcodes("Cell", 1)[..2],
            [Opcode::Ldarg, Opcode::Initobj],
            "`: this()` on a struct zero-initializes"
        );
        assert_eq!(
            &opcodes("Cell", 2)[..3],
            [Opcode::Ldarg, Opcode::Ldarg, Opcode::Call],
            "`: this(args)` on a struct still calls the constructor it names"
        );
        assert_eq!(
            &opcodes("Holder", 1)[..2],
            [Opcode::Ldarg, Opcode::Call],
            "`: this()` on a CLASS calls the parameterless constructor"
        );
    }

    #[test]
    fn a_volatile_field_access_carries_the_volatile_prefix() {
        use lamella_cil::Opcode;
        let unit = parse_compilation_unit(
            "class C { volatile int v; int plain; \
                 public void SetV(int x) { v = x; } \
                 public int GetV() { return v; } \
                 public void SetPlain(int x) { plain = x; } \
                 public int GetPlain() { return plain; } \
                 static volatile int sv; \
                 public static void SetSV(int x) { sv = x; } \
                 public static int GetSV() { return sv; } }",
        )
        .unit;
        let result = compile_unit(&unit, "vol.dll", "vol");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let image = result.image.expect("an image");
        let assembly = Assembly::read(&image).expect("the reader parses the image");
        let ty = assembly.find_type("", "C").expect("the C type");
        let code_of = |name: &str| {
            ty.methods()
                .find(|method| method.name() == Some(name))
                .unwrap_or_else(|| panic!("method {name} is missing"))
                .body()
                .unwrap_or_else(|| panic!("method {name} has no body"))
                .code
        };
        let prefixes = |code: &[Instruction], op: Opcode| {
            code.windows(2)
                .any(|w| w[0].opcode == Opcode::Volatile && w[1].opcode == op)
        };
        let carries_volatile =
            |code: &[Instruction]| code.iter().any(|i| i.opcode == Opcode::Volatile);

        assert!(
            prefixes(&code_of("GetV"), Opcode::Ldfld),
            "an instance volatile read must be `volatile. ldfld`"
        );
        assert!(
            prefixes(&code_of("SetV"), Opcode::Stfld),
            "an instance volatile write must be `volatile. stfld`"
        );
        assert!(
            prefixes(&code_of("GetSV"), Opcode::Ldsfld),
            "a static volatile read must be `volatile. ldsfld`"
        );
        assert!(
            prefixes(&code_of("SetSV"), Opcode::Stsfld),
            "a static volatile write must be `volatile. stsfld`"
        );
        assert!(
            !carries_volatile(&code_of("GetPlain")),
            "a non-volatile read must not carry `volatile.`"
        );
        assert!(
            !carries_volatile(&code_of("SetPlain")),
            "a non-volatile write must not carry `volatile.`"
        );
    }

    #[test]
    fn an_interface_records_its_base_interfaces_as_interface_impls() {
        let unit = parse_compilation_unit(
            "interface IBase { int Base(); } interface IDerived : IBase { int Derived(); }",
        )
        .unit;
        let result = compile_unit(&unit, "ifaces.dll", "ifaces");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let image = result.image.expect("an image");
        let assembly = Assembly::read(&image).expect("the reader parses the image");
        let base = assembly.find_type("", "IBase").expect("the IBase type");
        let derived = assembly.find_type("", "IDerived").expect("the IDerived type");
        assert!(
            derived.interfaces().any(|token| token == base.token()),
            "IDerived must carry an InterfaceImpl row naming IBase"
        );
    }

    /// An explicit implementation's QUALIFIER is a written name and must be resolved before it
    /// becomes a `TypeRef` -- `int IThing.Get()` under `using N;` overrides `N.IThing::Get`.
    ///
    /// **THE INTERFACE MUST BE IMPORTED, WHICH IS THE WHOLE REASON THIS TEST COMPILES A REFERENCE
    /// FIRST.** Written against a this-module interface the same program is green either way: an
    /// in-module qualifier finds a tokenized `MethodDef` and never reaches the `MemberRef` arm
    /// where the defect lives. A test written that way passes with the fix reverted --
    /// proving nothing, the same way a resolver test using a GLOBAL-namespace definition never
    /// exercises the scope search.
    ///
    /// **AND THE ASSERTION IS THE `TypeRef`'s NAMESPACE, BECAUSE "IT EMITTED" CANNOT SEE THE
    /// FAILURE THAT MATTERS.** Taking the spelling as written mints a `TypeRef` with an EMPTY
    /// namespace: well-formed metadata that assembles, links and verifies, and then throws
    /// `TypeLoadException: Could not load type 'IEnumerable'` on the first use of the type, at run
    /// time, in a program that names no generics at all.
    /// A signature type that MENTIONS a type parameter still mints everything in it that is not
    /// one -- the skip belongs to the leaf, not to the composite that contains it.
    ///
    /// **THE TABLE IS THE INSTRUMENT, BECAUSE EITHER HALF ALONE LOOKS LIKE THE WHOLE RULE.**
    /// Skipping the composite passes every row where the parameter is the WHOLE type (`T`, `T[]`)
    /// and refuses every row where it is merely an argument; minting the composite passes those
    /// and puts a `TypeRef` to a type called `T` in the assembly, which is worse than a refusal
    /// because `type_sig`'s named-type fall-through then SUCCEEDS for a signature written in the
    /// wrong scope. So the rows are paired: what must be minted, and what must not.
    #[test]
    fn a_signature_mentioning_a_type_parameter_still_mints_the_types_that_are_not_one() {
        let at_csharp2 = |source: &str, module: &str, name: &str, refs: &[Assembly]| {
            let options = LexOptions {
                version: LanguageVersion::CSharp2,
                ..LexOptions::default()
            };
            let parsed = parse_compilation_unit_with(source, options);
            assert!(
                parsed.diagnostics.iter().all(|d| d.severity() != Severity::Error),
                "{source}: {:?}",
                parsed.diagnostics
            );
            compile(&parsed.unit, module, name, refs, None, false, false, false, LanguageVersion::CSharp2)
        };
        let built = at_csharp2(
            "namespace N { public interface IThing<T> { T Get(); } public class Plain { } }",
            "n.dll",
            "N",
            &[],
        );
        let library = built.image.unwrap_or_else(|| {
            panic!(
                "the reference assembly: diagnostics {:?}, emit {:?}",
                built.diagnostics, built.emit_error
            )
        });
        let reference = Assembly::read(&library).expect("the reader parses the reference");

        let type_refs = |source: &str| {
            let result = at_csharp2(source, "c.dll", "C", &[reference.clone()]);
            assert!(result.diagnostics.is_empty(), "{source}: {:?}", result.diagnostics);
            let image = result.image.expect("an image for: {source}");
            let assembly = Assembly::read(&image).expect("the reader parses the image");
            let mut names: Vec<String> = assembly
                .type_refs()
                .filter_map(|row| row.name())
                .map(|name| String::from(name.name))
                .collect();
            names.sort();
            names.dedup();
            names
        };

        let open = type_refs("using N; class Bag<T> { public IThing<T> Get() { return null; } }");
        assert!(
            open.iter().any(|name| name == "IThing`1"),
            "an open instantiation must still reference its definition, got {open:?}"
        );
        assert!(
            !open.iter().any(|name| name == "T"),
            "a type parameter must never become a TypeRef, got {open:?}"
        );

        let closed = type_refs("using N; class Bag { public IThing<Plain> Get() { return null; } }");
        assert!(
            closed.iter().any(|name| name == "IThing`1"),
            "a closed instantiation references its definition, got {closed:?}"
        );
        let bare = type_refs("using N; class Bag<T> { public T Get() { return default(T); } }");
        assert!(
            !bare.iter().any(|name| name == "T"),
            "a bare type parameter mints nothing, got {bare:?}"
        );
        let array = type_refs("using N; class Bag<T> { public T[] Get() { return null; } }");
        assert!(
            !array.iter().any(|name| name == "T"),
            "an array OF a type parameter mints nothing either, got {array:?}"
        );
    }

    /// A class implementing a CONSTRUCTED interface declares it, and declares it at the type
    /// parameter it was written with.
    ///
    /// **BOTH HALVES SHIP A LOADABLE IMAGE THAT CANNOT DISPATCH, WHICH IS WHY THE ASSERTIONS ARE
    /// ON THE METADATA AND NOT ON A RUN.** `InterfaceImpl` took its token from `type_token`, which
    /// by design never holds an instantiation (those are keyed by signature blob), so
    /// `if let Some(token)` dropped every generic interface and the type declared only the
    /// non-generic ones it inherits. And the minting walk tests a type parameter against the BODY
    /// scope, which no method has entered while a type header is being emitted, so `T` became a
    /// `TypeRef` to a class nobody declares and the row encoded `IEnumerable`1<class T>`. Either
    /// one assembles, links and verifies; the first call through the interface then fails to
    /// resolve at run time, which no compile-only check can see.
    #[test]
    fn a_constructed_interface_is_declared_as_a_type_spec_at_its_own_parameter() {
        let options = LexOptions {
            version: LanguageVersion::CSharp2,
            ..LexOptions::default()
        };
        let parsed = parse_compilation_unit_with(
            "interface IBox<T> { T Get(); } \
             class Bag<T> : IBox<T> { public T Get() { return default(T); } }",
            options,
        );
        assert!(
            parsed.diagnostics.iter().all(|d| d.severity() != Severity::Error),
            "{:?}",
            parsed.diagnostics
        );
        let result = compile(
            &parsed.unit, "c.dll", "C", &[], None, false, false, false, LanguageVersion::CSharp2,
        );
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let image = result.image.expect("an image");
        let assembly = Assembly::read(&image).expect("the reader parses the image");

        assert!(
            !assembly
                .type_refs()
                .filter_map(|row| row.name())
                .any(|name| name.name == "T"),
            "a type parameter must never become a TypeRef"
        );

        let bag = assembly.find_type("", "Bag`1").expect("the Bag`1 type");
        let interfaces: Vec<Token> = bag.interfaces().collect();
        assert_eq!(interfaces.len(), 1, "Bag`1 declares exactly IBox<T>");
        assert_eq!(
            interfaces[0].table(),
            0x1b,
            "a constructed interface is declared through a TypeSpec, got {:?}",
            interfaces[0]
        );
        let signature = assembly
            .type_spec_signature(interfaces[0])
            .expect("the InterfaceImpl's TypeSpec decodes");
        let lamella_metadata::SigType::GenericInst { arguments, .. } = signature else {
            panic!("the InterfaceImpl names a constructed type, got {signature:?}");
        };
        assert!(
            matches!(arguments.as_slice(), [lamella_metadata::SigType::Var(0)]),
            "IBox<T> at the class's own parameter encodes !0, got {arguments:?}"
        );
    }

    #[test]
    fn an_explicit_implementation_overrides_the_resolved_interface_not_the_written_name() {
        let library = parse_compilation_unit("namespace N { public interface IThing { int Get(); } }").unit;
        let library = compile_unit(&library, "n.dll", "N")
            .image
            .expect("the reference assembly");
        let reference = Assembly::read(&library).expect("the reader parses the reference");

        let qualifier_type_ref = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let result = compile_unit_with_references(&unit, "c.dll", "C", &[reference.clone()]);
            assert!(result.diagnostics.is_empty(), "{source}: {:?}", result.diagnostics);
            let image = result.image.expect("an image");
            let assembly = Assembly::read(&image).expect("the reader parses the image");
            let class = assembly.find_type("", "C").expect("the C type");
            let impls: Vec<(Token, Token)> = class.method_impls().collect();
            assert_eq!(impls.len(), 1, "one MethodImpl for: {source}");
            let member = assembly
                .member_ref(impls[0].1.row())
                .expect("the declaration is a MemberRef into the reference");
            let parent = assembly
                .type_ref(member.parent().row())
                .expect("the MemberRef's parent TypeRef");
            let name = parent.name().expect("the TypeRef's name");
            (String::from(name.namespace), String::from(name.name))
        };

        let through_using = qualifier_type_ref(
            "using N; class C : N.IThing { int IThing.Get() { return 1; } }",
        );
        let qualified = qualifier_type_ref(
            "using N; class C : N.IThing { int N.IThing.Get() { return 1; } }",
        );
        assert_eq!(
            through_using,
            (String::from("N"), String::from("IThing")),
            "an unqualified qualifier must resolve to N.IThing, not to a TypeRef in no namespace"
        );
        assert_eq!(
            qualified,
            (String::from("N"), String::from("IThing")),
            "the fully-qualified control"
        );
    }

    #[test]
    fn each_catch_handler_stores_into_a_slot_of_its_own_exception_type() {
        let unit = parse_compilation_unit(
            "namespace System { \
                public class Exception { } \
                public class InvalidOperationException : Exception { } \
                public class DivideByZeroException : Exception { } \
            } \
            class Program { static int Main() { \
                try { throw new System.InvalidOperationException(); } \
                catch (System.DivideByZeroException e) { return 1; } \
                catch (System.InvalidOperationException e) { return 42; } } }",
        )
        .unit;
        let result = compile_unit(&unit, "ehmc.dll", "ehmc");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let image = result.image.expect("an image");
        let assembly = Assembly::read(&image).expect("the reader parses the image");
        let main = assembly
            .find_type("", "Program")
            .expect("the Program type")
            .methods()
            .find(|method| method.name() == Some("Main"))
            .expect("the Main method");
        let locals = main.local_variables();
        let clauses = main.exception_clauses();
        let body = main.body().expect("Main has a method body");
        assert_eq!(clauses.len(), 2, "two catch clauses: {clauses:?}");
        assert_eq!(body.handlers.len(), 2, "two handler regions");

        let mut slots = Vec::new();
        let mut tags = Vec::new();
        for (clause, handler) in clauses.iter().zip(body.handlers.iter()) {
            let lamella_metadata::ExceptionHandlerKind::Catch(catch_token) = clause.kind else {
                panic!("both clauses are typed catches, got {:?}", clause.kind);
            };
            let opener = &body.code[handler.handler_range.start as usize];
            assert_eq!(
                opener.opcode,
                lamella_cil::Opcode::Stloc,
                "a named handler opens by storing its exception, got {:?}",
                opener.opcode
            );
            let &lamella_cil::Operand::Variable(slot) = &opener.operand else {
                panic!("stloc carries a variable slot, got {:?}", opener.operand);
            };
            let slot_type = match &locals[slot as usize] {
                lamella_metadata::signature::SigType::Class(token) => *token,
                other => panic!("the exception slot is a class type, got {other:?}"),
            };
            let catch_tag = assembly.exception_tag(catch_token);
            assert_ne!(catch_tag, 0, "a known exception type has a nonzero tag");
            assert_eq!(
                assembly.exception_tag(slot_type),
                catch_tag,
                "a handler must store its exception into a slot of that same type",
            );
            slots.push(slot);
            tags.push(catch_tag);
        }
        assert_ne!(slots[0], slots[1], "the two `e` clauses occupy distinct slots");
        assert_ne!(tags[0], tags[1], "the two clauses catch different types");
    }

    #[test]
    fn only_the_literal_zero_converts_to_an_enum() {
        let emits = |body: &str| {
            let src = format!("enum E {{ A }} class P {{ static int M() {{ {body} }} }}");
            compile_unit(&parse_compilation_unit(&src).unit, "z.dll", "z")
                .image
                .is_some()
        };
        assert!(emits("E e = 0; return (int)e;"), "the literal 0 converts");
        assert!(!emits("E e = 0.0; return 0;"), "a floating zero does not convert");
        assert!(!emits("E e = 0.0m; return 0;"), "a decimal zero does not convert");
    }

    #[test]
    fn same_named_locals_in_sibling_scopes_keep_distinct_slots() {
        let unit = parse_compilation_unit(
            "struct A { public int X; } struct B { public int X; } \
             class Program { static int M(bool c) { \
                 if (c) { A v; v.X = 11; return v.X; } \
                 else { B v; v.X = 22; return v.X; } } }",
        )
        .unit;
        let result = compile_unit(&unit, "sib.dll", "sib");
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let image = result.image.expect("an image");
        let assembly = Assembly::read(&image).expect("the reader parses the image");
        let method = assembly
            .find_type("", "Program")
            .expect("the Program type")
            .methods()
            .find(|method| method.name() == Some("M"))
            .expect("the M method");
        let locals = method.local_variables();
        let body = method.body().expect("M has a method body");
        let value_slots: Vec<usize> = locals
            .iter()
            .enumerate()
            .filter(|(_, ty)| matches!(ty, lamella_metadata::signature::SigType::ValueType(_)))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(value_slots.len(), 2, "two value-type locals (A v, B v): {locals:?}");
        for slot in value_slots {
            let referenced = body.code.iter().any(|instr| {
                matches!(instr.operand, lamella_cil::Operand::Variable(v) if v as usize == slot)
            });
            assert!(
                referenced,
                "value-type local slot {slot} is declared but never referenced -- the name \
                 collision collapsed both `v`s onto one slot"
            );
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
    fn synthesized_locals_are_not_named_in_the_pdb() {
        let source = "class P { static int Run() { int u = 5; try { return u; } finally { } } }";
        let unit = parse_compilation_unit(source).unit;
        let pdb_bytes = compile_unit_with_debug(&unit, "p.dll", "p", &[], source, "p.cs")
            .pdb
            .expect("a pdb");
        let pdb = lamella_metadata::PortablePdb::read(&pdb_bytes).expect("read the pdb");
        let names: Vec<&str> = (1..=3)
            .flat_map(|rid| pdb.local_variables(rid))
            .map(|local| local.name)
            .collect();
        assert!(names.contains(&"u"), "the user local `u` is named: {names:?}");
        assert!(
            names
                .iter()
                .all(|n| !n.is_empty() && !n.starts_with('<') && !n.starts_with('$')),
            "no synthesized local (empty / `<`- / `$`-led) is named: {names:?}"
        );
    }

    #[test]
    fn a_const_decimal_initializes_in_the_cctor() {
        let source =
            "class C\n{\n    const decimal Pi = 3.14m;\n    static int Main() { return (int)Pi; }\n}\n";
        let unit = parse_compilation_unit(source).unit;
        let pdb_bytes = compile_unit_with_debug(&unit, "c.dll", "c", &[], source, "c.cs")
            .pdb
            .expect("a pdb");
        let pdb = lamella_metadata::PortablePdb::read(&pdb_bytes).expect("read the pdb");
        assert!(
            (1..=4).any(|rid| pdb.sequence_points(rid).iter().any(|p| p.start_line == 3)),
            "the const-decimal declaration (line 3) is covered by its .cctor init"
        );
    }

    #[test]
    fn all_default_static_initializers_omit_the_cctor() {
        let omit = "class C { static int a = 0; static bool b = false; static string s = null; static double d = 0.0; }";
        let keep = "class D { static int a = 0; static int b = 5; }";
        let boxed = "class E { static object o = 0; }";
        for (source, ty, expect_cctor) in [(omit, "C", false), (keep, "D", true), (boxed, "E", true)] {
            let unit = parse_compilation_unit(source).unit;
            let image = compile_unit(&unit, "t.dll", "t").image.expect("an image");
            let assembly = Assembly::read(&image).expect("the image reads back");
            let has_cctor = assembly
                .find_type("", ty)
                .expect("the type is present")
                .methods()
                .any(|method| method.name() == Some(".cctor"));
            assert_eq!(has_cctor, expect_cctor, "type {ty}: .cctor presence mismatch");
        }
    }

    #[test]
    fn an_unreachable_constant_switch_section_gets_hidden_points() {
        let source = "class P\n{\n    static int Main()\n    {\n        int v;\n        switch (3)\n        {\n            case 1: v = 42; break;\n            case 2: break;\n            case 3: goto case 1;\n        }\n        return v;\n    }\n}\n";
        let unit = parse_compilation_unit(source).unit;
        let pdb_bytes = compile_unit_with_debug(&unit, "s.dll", "s", &[], source, "s.cs")
            .pdb
            .expect("a pdb");
        let pdb = lamella_metadata::PortablePdb::read(&pdb_bytes).expect("read the pdb");
        let lines: Vec<u32> = (1..=pdb.method_count())
            .flat_map(|rid| {
                pdb.sequence_points(rid)
                    .into_iter()
                    .map(|point| point.start_line)
            })
            .collect();
        assert!(
            !lines.contains(&9),
            "case 2 (line 9) is unreachable -- it must carry no visible point, got {lines:?}"
        );
        assert!(lines.contains(&8), "case 1 (line 8, reached via goto) stays visible");
        assert!(lines.contains(&10), "case 3 (line 10, selected) stays visible");
    }

    #[test]
    fn a_constructor_points_its_implicit_base_call_before_the_body() {
        let source = "class C\n{\n    int f;\n    public C(int a)\n    {\n        f = a;\n    }\n}\n";
        let unit = parse_compilation_unit(source).unit;
        let pdb_bytes = compile_unit_with_debug(&unit, "c.dll", "c", &[], source, "c.cs")
            .pdb
            .expect("a pdb");
        let pdb = lamella_metadata::PortablePdb::read(&pdb_bytes).expect("read the pdb");
        let ctor = (1..=4)
            .map(|rid| pdb.sequence_points(rid))
            .find(|points| points.iter().any(|p| p.start_line == 4))
            .expect("a method covers the constructor signature (line 4)");
        let base = ctor.iter().find(|p| p.start_line == 4).expect("base-call point");
        let brace = ctor.iter().find(|p| p.start_line == 5).expect("body brace point");
        assert!(
            base.il_offset < brace.il_offset,
            "the base call precedes the body brace"
        );
    }

    #[test]
    fn a_constructor_carries_its_custom_attributes() {
        let source = "class MarkAttribute { }\nclass C\n{\n    [Mark] public C() { }\n    static void Main() { }\n}\n";
        let unit = parse_compilation_unit(source).unit;
        let image = compile_unit(&unit, "c.dll", "c").image.expect("an image");
        let assembly = Assembly::read(&image).expect("the image reads back");
        let ctor = assembly
            .find_type("", "C")
            .expect("type C is present")
            .methods()
            .find(|method| method.name() == Some(".ctor"))
            .expect("the constructor is present");
        assert_eq!(
            ctor.custom_attributes().count(),
            1,
            "the [Mark] attribute is emitted on the constructor row"
        );
    }

    /// The attribute names on one metadata row, so a test can state placement rather than a count.
    fn attribute_names_on(assembly: &Assembly, parent: lamella_token::Token) -> Vec<String> {
        let mut names: Vec<String> = assembly
            .custom_attributes(parent)
            .filter_map(|attribute| {
                assembly
                    .resolve_method(attribute.constructor)
                    .and_then(|ctor| ctor.declaring_type)
                    .map(|declaring| alloc::format!("{}.{}", declaring.namespace, declaring.name))
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn required_members_are_marked_and_their_constructors_guarded() {
        let options = LexOptions {
            version: LanguageVersion::CSharp11,
            ..LexOptions::default()
        };
        let source = "namespace System.Diagnostics.CodeAnalysis { public class SetsRequiredMembersAttribute { } } \
             namespace App { \
               public class Base { public required int F; public Base() { } \
                 [System.Diagnostics.CodeAnalysis.SetsRequiredMembers] public Base(int f) { F = f; } } \
               public class Derived : Base { public Derived() { } } \
               public class Plain { public int G; public Plain() { } } \
             }";
        let unit = parse_compilation_unit_with(source, options).unit;
        let result = compile(
            &unit,
            "app.dll",
            "app",
            &[],
            None,
            false,
            false,
            false,
            LanguageVersion::CSharp11,
        );
        assert!(
            result.diagnostics.is_empty(),
            "the program should compile clean at C# 11: {:?}",
            result.diagnostics
        );
        let image = result.image.expect("an image");
        let assembly = Assembly::read(&image).expect("reads back");

        const REQUIRED: &str = "System.Runtime.CompilerServices.RequiredMemberAttribute";
        const FEATURE: &str = "System.Runtime.CompilerServices.CompilerFeatureRequiredAttribute";
        const OBSOLETE: &str = "System.ObsoleteAttribute";
        const SETS: &str = "System.Diagnostics.CodeAnalysis.SetsRequiredMembersAttribute";

        let base = assembly.find_type("App", "Base").expect("App.Base");
        assert_eq!(attribute_names_on(&assembly, base.token()), [REQUIRED]);
        let field = base.fields().find(|f| f.name() == Some("F")).expect("F");
        assert_eq!(attribute_names_on(&assembly, field.token()), [REQUIRED]);

        let mut guarded = 0;
        let mut exempt = 0;
        for ctor in base.methods().filter(|m| m.name() == Some(".ctor")) {
            let names = attribute_names_on(&assembly, ctor.token());
            if names == [SETS] {
                exempt += 1;
            } else if names == [OBSOLETE, FEATURE] || names == [FEATURE, OBSOLETE] {
                guarded += 1;
            } else {
                panic!("unexpected attributes on a constructor: {names:?}");
            }
        }
        assert_eq!((guarded, exempt), (1, 1), "one guarded ctor and one exempt");

        let derived = assembly.find_type("App", "Derived").expect("App.Derived");
        assert_eq!(
            attribute_names_on(&assembly, derived.token()),
            Vec::<String>::new(),
            "a derived type declaring nothing required carries no type-level marker"
        );
        let derived_ctor = derived
            .methods()
            .find(|m| m.name() == Some(".ctor"))
            .expect("Derived..ctor");
        assert_eq!(
            attribute_names_on(&assembly, derived_ctor.token()),
            [OBSOLETE, FEATURE],
            "an inherited requirement still guards the derived constructor"
        );

        let plain = assembly.find_type("App", "Plain").expect("App.Plain");
        assert_eq!(attribute_names_on(&assembly, plain.token()), Vec::<String>::new());
        let plain_field = plain.fields().find(|f| f.name() == Some("G")).expect("G");
        assert_eq!(attribute_names_on(&assembly, plain_field.token()), Vec::<String>::new());
        let plain_ctor = plain
            .methods()
            .find(|m| m.name() == Some(".ctor"))
            .expect("Plain..ctor");
        assert_eq!(attribute_names_on(&assembly, plain_ctor.token()), Vec::<String>::new());
    }

    #[test]
    fn a_required_member_is_required_across_an_assembly_boundary() {
        let options = LexOptions {
            version: LanguageVersion::CSharp11,
            ..LexOptions::default()
        };
        let build_library = |source: &str| {
            let unit = parse_compilation_unit_with(source, options.clone()).unit;
            let result = compile(
                &unit,
                "lib.dll",
                "lib",
                &[],
                None,
                false,
                false,
                false,
                LanguageVersion::CSharp11,
            );
            assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
            result.image.expect("library image")
        };
        let consumer_codes = |reference: &Assembly| {
            let program =
                parse_compilation_unit_with("public class P { public object M() { return new Lib.C(); } }", options.clone())
                    .unit;
            let compiled = compile(
                &program,
                "p.dll",
                "p",
                core::slice::from_ref(reference),
                None,
                false,
                false,
                false,
                LanguageVersion::CSharp11,
            );
            compiled
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.code)
                .collect::<Vec<u16>>()
        };

        let required_image =
            build_library("namespace Lib { public class C { public required int F; public C() { } } }");
        let required = Assembly::read(&required_image).expect("library assembly");
        assert_eq!(
            consumer_codes(&required),
            [9035],
            "an IMPORTED required member must still be required at the creation site"
        );

        let plain_image =
            build_library("namespace Lib { public class C { public int F; public C() { } } }");
        let plain = Assembly::read(&plain_image).expect("library assembly");
        assert_eq!(consumer_codes(&plain), Vec::<u16>::new());

        let exempt_image = build_library(
            "namespace System.Diagnostics.CodeAnalysis { public class SetsRequiredMembersAttribute { } } \
             namespace Lib { public class C { public required int F; \
               [System.Diagnostics.CodeAnalysis.SetsRequiredMembers] public C() { F = 1; } } }",
        );
        let exempt = Assembly::read(&exempt_image).expect("library assembly");
        assert_eq!(
            consumer_codes(&exempt),
            Vec::<u16>::new(),
            "an imported [SetsRequiredMembers] constructor exempts the creation"
        );
    }

    #[test]
    fn a_driver_option_reaches_a_two_file_compilation_exactly_as_it_reaches_a_one_file_one() {
        struct Row {
            /// The feature, and the version that introduced it.
            feature: &'static str,
            /// A program using it, as two independent top-level declarations. No BCL member
            /// appears: this compilation has NO references, and a row needing corlib would fail in
            /// BOTH spreads and agree its way to green while saying nothing about the option.
            first: &'static str,
            second: &'static str,
            /// A dialect that permits it, and one that does not. Per row rather than fixed, so the
            /// table proves the SELECTED version arrives -- not merely that something above C# 1 does.
            admits: LanguageVersion,
            refuses: LanguageVersion,
        }
        let rows = [
            Row {
                feature: "generics",
                first: "class G { public static T Id<T>(T x) { return x; } }",
                second: "class P { static int Main() { return G.Id<int>(1); } }",
                admits: LanguageVersion::CSharp2,
                refuses: LanguageVersion::CSharp1,
            },
            Row {
                feature: "the default operator",
                first: "class D { public static int Zero() { return default(int); } }",
                second: "class P { static int Main() { return D.Zero(); } }",
                admits: LanguageVersion::CSharp2,
                refuses: LanguageVersion::CSharp1,
            },
            Row {
                feature: "static classes",
                first: "static class S { public static int One() { return 1; } }",
                second: "class P { static int Main() { return S.One(); } }",
                admits: LanguageVersion::CSharp2,
                refuses: LanguageVersion::CSharp1,
            },
            Row {
                feature: "binary literals",
                first: "class B { public static int Five() { return 0b101; } }",
                second: "class P { static int Main() { return B.Five(); } }",
                admits: LanguageVersion::CSharp7,
                refuses: LanguageVersion::CSharp2,
            },
        ];
        let codes = |diagnostics: &[Diagnostic]| -> Vec<u16> {
            let mut codes: Vec<u16> = diagnostics
                .iter()
                .filter(|d| d.is_error())
                .map(|d| d.code)
                .collect();
            codes.sort_unstable();
            codes
        };
        for Row {
            feature,
            first,
            second,
            admits,
            refuses,
        } in rows
        {
            for (version, admitted) in [
                (admits, true),
                (refuses, false),
            ] {
                let options = LexOptions {
                    version,
                    ..LexOptions::default()
                };
                let joined = alloc::format!("{first}\n{second}\n");
                let one = compile_source_with(
                    &joined,
                    "both.cs",
                    "p.dll",
                    "p",
                    &[],
                    false,
                    options.clone(),
                );
                let two = compile_sources_with(
                    &[(first, "a.cs"), (second, "b.cs")],
                    "p.dll",
                    "p",
                    &[],
                    false,
                    options,
                );
                let two_codes = codes(&two.diagnostics.concat());
                assert_eq!(
                    codes(&one.diagnostics),
                    two_codes,
                    "{feature} at {version:?}: one file and two files must agree"
                );
                assert_eq!(
                    two_codes.is_empty(),
                    admitted,
                    "{feature} at {version:?}: two files should be {}",
                    if admitted { "admitted" } else { "refused" }
                );
            }
        }
    }

    /// A STRUCT HAS NO PREDEFINED `==`, AND SAYING IT DID WAS A WRONG ANSWER RATHER THAN A LOOSE
    /// ONE (14.9).
    ///
    /// **THE REFUSAL IS THE FIX FOR A MISCOMPILE, WHICH IS WHY IT IS ASSERTED AS ONE.** Before it,
    /// `a == b` on a struct declaring no `operator ==` compiled and compared THE FIRST FIELD: two
    /// structs differing only in a later field answered EQUAL, and two `System.Nullable<int>`
    /// holding 40 and 2 answered EQUAL because what was compared was `HasValue`. csc refuses every
    /// one of these programs, so there was no oracle to notice until one was RUN.
    ///
    /// The accepting rows are the half that decides: an enum, a string, a class and a struct that
    /// DOES declare the operator all keep their comparison, and each is a rule this could take away
    /// without a single refusing row noticing.
    #[test]
    fn a_struct_has_no_predefined_equality() {
        let cases: [(&str, &[u16], &str); 8] = [
            (
                "struct Plain { public int v; } \
                 class P { static bool M(Plain a, Plain b) { return a == b; } }",
                &[19],
                "Operator '==' cannot be applied to operands of type 'Plain' and 'Plain'",
            ),
            (
                "struct Pair<T> { public int v; } \
                 class P { static bool M(Pair<string> a, Pair<string> b) { return a == b; } }",
                &[19],
                "Operator '==' cannot be applied to operands of type 'Pair<string>' and \
                 'Pair<string>'",
            ),
            (
                "struct Pair<T> { public int v; } \
                 class P { static bool M(Pair<string> a) { return a == null; } }",
                &[19],
                "Operator '==' cannot be applied to operands of type 'Pair<string>' and '<null>'",
            ),
            (
                "struct Pair<T> { public int v; } \
                 class P { static bool M(Pair<string> a, Pair<string> b) { return a != b; } }",
                &[19],
                "Operator '!=' cannot be applied to operands of type 'Pair<string>' and \
                 'Pair<string>'",
            ),
            ("enum E { A, B } class P { static bool M(E a, E b) { return a == b; } }", &[], ""),
            ("class P { static bool M(string a, string b) { return a == b; } }", &[], ""),
            ("class C { } class P { static bool M(C a, C b) { return a == b; } }", &[], ""),
            (
                "struct Money { public int v; \
                     public static bool operator ==(Money a, Money b) { return a.v == b.v; } \
                     public static bool operator !=(Money a, Money b) { return a.v != b.v; } \
                     public override bool Equals(object o) { return true; } \
                     public override int GetHashCode() { return v; } } \
                 class P { static bool M(Money a, Money b) { return a == b; } }",
                &[],
                "",
            ),
        ];
        for (source, expected, message) in cases {
            let result = compile_source_with(
                source,
                "e.cs",
                "e.dll",
                "e",
                &[],
                false,
                LexOptions {
                    version: LanguageVersion::CSharp2,
                    ..LexOptions::default()
                },
            );
            let errors: Vec<&Diagnostic> = result
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.is_error())
                .collect();
            let codes: Vec<u16> = errors.iter().map(|diagnostic| diagnostic.code).collect();
            assert_eq!(codes, expected, "for {source}");
            if let Some(first) = errors.first() {
                assert_eq!(&*first.message, message, "for {source}");
            }
        }
    }

    /// THE SEVEN WAYS PARTS OF ONE TYPE CAN CONTRADICT EACH OTHER, each measured against csc on the
    /// same source at `/langversion:ISO-2` -- code and sentence.
    ///
    /// **THE ACCEPTING ROWS ARE HALF THE TABLE AND THEY ARE THE HALF THAT DECIDES.** A check that
    /// refuses too much passes every error row: a `where` clause on ONE part, a lone `partial`
    /// declaration with no second part, and a part that states no accessibility are all LEGAL, and
    /// each is a rule this could get backwards without a single error row noticing.
    #[test]
    fn the_parts_of_a_partial_type_must_agree() {
        let cases: [(&str, &[u16], &str); 11] = [
            (
                "public partial class W { } public class W { }",
                &[260],
                "Missing partial modifier on declaration of type 'W'; another partial declaration \
                 of this type exists",
            ),
            (
                "public partial class W { } public partial struct W { }",
                &[261],
                "Partial declarations of 'W' must be all classes, all record classes, all structs, \
                 all unions, all record structs, or all interfaces",
            ),
            (
                "public partial class W { } internal partial class W { }",
                &[262],
                "Partial declarations of 'W' have conflicting accessibility modifiers",
            ),
            (
                "public class A { } public class B { } \
                 public partial class W : A { } public partial class W : B { }",
                &[263],
                "Partial declarations of 'W' must not specify different base classes",
            ),
            (
                "public partial class W<T> { } public partial class W<U> { }",
                &[264],
                "Partial declarations of 'W<T>' must have the same type parameter names in the \
                 same order",
            ),
            (
                "public partial class W<T> where T : class { } \
                 public partial class W<T> where T : struct { }",
                &[265],
                "Partial declarations of 'W<T>' have inconsistent constraints for type parameter \
                 'T'",
            ),
            (
                "partial public class W { }",
                &[267],
                "The 'partial' modifier can only appear immediately before 'class', 'record', \
                 'struct', 'interface', 'event', an instance constructor name, or a method or \
                 property return type",
            ),
            (
                "public partial class W { int a; } public partial class W { int a; }",
                &[102],
                "The type 'W' already contains a definition for 'a'",
            ),
            (
                "public partial class W { public W() { } } public partial class W { public W() { } }",
                &[111],
                "Type 'W' already defines a member called 'W' with the same parameter types",
            ),
            (
                "public partial class W<T> where T : class { } public partial class W<T> { }",
                &[],
                "",
            ),
            (
                "public partial class W { } partial class W { }",
                &[],
                "",
            ),
        ];
        for (source, expected, message) in cases {
            let result = compile_source_with(
                source,
                "p.cs",
                "p.dll",
                "p",
                &[],
                false,
                LexOptions {
                    version: LanguageVersion::CSharp2,
                    ..LexOptions::default()
                },
            );
            let errors: Vec<&Diagnostic> = result
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.is_error())
                .collect();
            let codes: Vec<u16> = errors.iter().map(|diagnostic| diagnostic.code).collect();
            assert_eq!(codes, expected, "for {source}");
            if let Some(first) = errors.first() {
                assert_eq!(&*first.message, message, "for {source}");
            }
        }
    }

    /// A PARTIAL TYPE SPLIT ACROSS FILES IS ONE `TypeDef` WITH BOTH FILES' MEMBERS, AND EACH PART
    /// BINDS UNDER ITS OWN FILE'S `using` DIRECTIVES.
    ///
    /// **THE `TypeDef` COUNT IS THE ASSERTION THAT MATTERS AND A CLEAN COMPILE DOES NOT MAKE IT.**
    /// Emitting each part where the walk reaches it produces an image that assembles: it is the CLR
    /// that refuses it, with `BadImageFormatException: Duplicate type with name 'Widget'`. A test
    /// that only checked for diagnostics passed under exactly that defect.
    ///
    /// Each file imports a namespace the other does not and uses it, so a merge that emitted both
    /// parts under one file's scope would fail to resolve one of them. No BCL type appears: this
    /// compilation has no references, and the point is the merge rather than the corlib.
    #[test]
    fn a_partial_type_split_across_files_is_one_typedef() {
        let a = "using N; \
                 namespace N { public class Ten { public static int Get() { return 10; } } } \
                 public partial class Widget { \
                     private int a; \
                     public void SetA(int v) { a = v + Ten.Get(); } \
                     public int A() { return a; } }";
        let b = "using M; \
                 namespace M { public class Two { public static int Get() { return 2; } } } \
                 public partial class Widget { \
                     private int b; \
                     public void SetB(int v) { b = v + Two.Get(); } \
                     public int Sum() { return a + b; } }";
        let sources = [(a, "a.cs"), (b, "b.cs")];
        let options = LexOptions {
            version: LanguageVersion::CSharp2,
            ..LexOptions::default()
        };
        let result = compile_sources_with(&sources, "w.dll", "w", &[], false, options);
        assert!(
            result.diagnostics.iter().all(|per_file| per_file.is_empty()),
            "{:?}",
            result.diagnostics
        );
        let image = result.image.expect("an image");
        let assembly = Assembly::read(&image).expect("the reader parses the image");
        let widgets = assembly
            .types()
            .filter(|ty| ty.name == "Widget")
            .count();
        assert_eq!(widgets, 1, "one TypeDef per type, not one per part");
        let widget = assembly.find_type("", "Widget").expect("the Widget type");
        let mut fields: Vec<&str> = widget.fields().filter_map(|field| field.name()).collect();
        fields.sort_unstable();
        assert_eq!(fields, ["a", "b"], "both files' fields land in the one type");
        let mut methods: Vec<&str> = widget.methods().filter_map(|method| method.name()).collect();
        methods.sort_unstable();
        assert_eq!(methods, [".ctor", "A", "SetA", "SetB", "Sum"]);
    }

    /// A part that names something only ANOTHER part's file imported does not resolve, and the
    /// diagnostic lands on the file that wrote it.
    ///
    /// **THE CONTROL FOR THE TEST ABOVE.** Merging the parts into one file's scope would make this
    /// program compile, and nothing else in the suite would notice: it is a program that must be
    /// REFUSED, and a merge is exactly the change that would start accepting it.
    #[test]
    fn a_partial_part_does_not_inherit_another_file_s_usings() {
        let a = "using N; \
                 namespace N { public class Ten { public static int Get() { return 10; } } } \
                 public partial class Widget { public int A() { return Ten.Get(); } }";
        let b = "public partial class Widget { public int B() { return Ten.Get(); } }";
        let sources = [(a, "a.cs"), (b, "b.cs")];
        let options = LexOptions {
            version: LanguageVersion::CSharp2,
            ..LexOptions::default()
        };
        let result = compile_sources_with(&sources, "w.dll", "w", &[], false, options);
        assert!(
            result.diagnostics[0].is_empty(),
            "a.cs imported N: {:?}",
            result.diagnostics[0]
        );
        let codes: Vec<u16> = result.diagnostics[1]
            .iter()
            .map(|diagnostic| diagnostic.code)
            .collect();
        assert_eq!(codes, [103], "b.cs did not: {:?}", result.diagnostics[1]);
        assert!(result.image.is_none());
    }

    #[test]
    fn multi_document_pdb_attributes_each_method_to_its_own_file() {
        let a = "class A { static int Alpha() { int ax = 1; return ax; } }";
        let b = "class B { static int Beta() { int bx = 2; return bx; } }";
        let sources = [(a, "a.cs"), (b, "b.cs")];
        let result =
            compile_sources_with(&sources, "lib.dll", "lib", &[], true, LexOptions::default());
        assert!(
            result.diagnostics.iter().all(|d| d.is_empty()),
            "{:?}",
            result.diagnostics
        );
        assert!(result.image.is_some(), "{:?}", result.emit_error);
        let pdb_bytes = result.pdb.expect("a multi-source pdb");
        let pdb = lamella_metadata::PortablePdb::read(&pdb_bytes).expect("read the pdb");

        assert_eq!(pdb.document_count(), 2);

        let mut docs: Vec<String> = (1..=pdb.method_count())
            .filter(|&rid| !pdb.sequence_points(rid).is_empty())
            .filter_map(|rid| pdb.method_document(rid))
            .collect();
        docs.sort();
        docs.dedup();
        assert!(docs.iter().any(|d| d.contains("a.cs")), "a.cs unattributed: {docs:?}");
        assert!(docs.iter().any(|d| d.contains("b.cs")), "b.cs unattributed: {docs:?}");

        let (rid, _il) = pdb.resolve_breakpoint("b.cs", 1).expect("a breakpoint in b.cs");
        assert!(pdb.method_document(rid).unwrap().contains("b.cs"));
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
            [4, 5, 6, 7]
        );

        assert_eq!(pdb.source_location(2, 0).unwrap().start_line, 4);
        let line6 = points.iter().find(|p| p.start_line == 6).expect("a point on line 6");
        assert_eq!(
            pdb.source_location(2, line6.il_offset).unwrap().start_line,
            6
        );
        assert!(pdb.method_document(2).unwrap().contains("app.cs"));

        assert_eq!(
            pdb.resolve_breakpoint("app.cs", 6),
            Some((2, line6.il_offset))
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
    fn a_call_taking_a_single_part_imported_type_resolves_at_emit() {
        let unit = parse_compilation_unit(
            "using N; namespace N { class Foo { } } \
             class Program { static void F(Foo f) { } \
             static int Main() { F(new Foo()); return 0; } }",
        )
        .unit;
        let result = compile_unit(&unit, "app.dll", "app");
        assert!(
            result.diagnostics.iter().all(|d| !d.is_error()),
            "unexpected diagnostics: {:?}",
            result.diagnostics
        );
        assert!(
            result.image.is_some(),
            "the call must resolve and emit (emit_error: {:?})",
            result.emit_error
        );
    }

    /// **A CONVERSION WHOSE TARGET IS REACHED BY A PREDEFINED REFERENCE CONVERSION MUST NOT CALL A
    /// CONVERSION OPERATOR**, because 17.9.3 forbids declaring one for such a pair -- to or from
    /// `object` or an interface-type, or between a type and its base type -- *"since a conversion
    /// would then already exist"*.
    ///
    /// The selection step accepts an operator whose return type merely CONVERTS to the target, and
    /// every type converts to `object`, so without the guard each of the first three rows here
    /// binds `op_Implicit` and calls it. **Against the real BCL that is an emit refusal, and
    /// `(object)aString` alone reaches it from every string cast in a program; from source it is
    /// worse than a refusal -- `(Base)d` returns an `Other`, so a virtual call through `b` answers
    /// from the wrong object with no diagnostic anywhere.**
    ///
    /// **`ToOther` IS THE INSTRUMENT, NOT A COURTESY ROW.** `(Other)t` is the case where the
    /// operator genuinely applies; a build whose search returned nothing at all satisfies every
    /// other assertion in this test. Only the row that must CONTAIN a call separates the fix from
    /// the sledgehammer.
    #[test]
    fn a_reference_conversion_never_calls_a_conversion_operator() {
        let result = compile_source(
            "interface IThing { }\n\
             class Base { }\n\
             class Other : Base, IThing { }\n\
             class Thing { public static implicit operator Other(Thing t) { return null; } }\n\
             class Derived : Base { public static implicit operator Other(Derived d) { return null; } }\n\
             class Impl : IThing { public static implicit operator Other(Impl i) { return null; } }\n\
             class Program {\n\
                 static object ToObject(Thing t) { return (object)t; }\n\
                 static Base ToBase(Derived d) { return (Base)d; }\n\
                 static IThing ToInterface(Impl i) { return (IThing)i; }\n\
                 static Other ToOther(Thing t) { return (Other)t; }\n\
                 static int Main() { return 0; }\n\
             }\n",
            "app.cs",
            "app.dll",
            "app",
            &[],
            false,
        );
        assert!(
            result.diagnostics.iter().all(|d| !d.is_error()),
            "the source must compile: {:?}",
            result.diagnostics
        );
        let image = result
            .image
            .unwrap_or_else(|| panic!("the program must emit (emit_error: {:?})", result.emit_error));
        let assembly = lamella_metadata::Assembly::read(&image).expect("the image parses");

        let calls = |name: &str| {
            let method = assembly
                .type_defs()
                .find(|t| t.name().is_some_and(|n| n.name == "Program"))
                .expect("Program is in the image")
                .methods()
                .find(|m| m.name() == Some(name))
                .unwrap_or_else(|| panic!("{name} is in the image"));
            method
                .body()
                .unwrap_or_else(|| panic!("{name} has a body"))
                .code
                .iter()
                .any(|instruction| instruction.opcode == lamella_cil::Opcode::Call)
        };

        assert!(!calls("ToObject"), "(object)t must box, not call op_Implicit");
        assert!(!calls("ToBase"), "(Base)d must upcast, not call op_Implicit");
        assert!(
            !calls("ToInterface"),
            "(IThing)i must be a reference conversion, not a call: 17.9.3 exists so that \
             \"no user-defined transformations occur when converting to an interface-type\""
        );
        assert!(
            calls("ToOther"),
            "the control row: (Other)t IS the operator's own conversion and must still call it"
        );
    }

    /// EVERY DECLARATION POSITION CARRIES THE METADATA FLAGS csc EMITS, ONE ROW EACH.
    ///
    /// **A MISSING FLAG IS INVISIBLE TO EVERY OTHER CHECK IN THIS TREE.** An emitter that does
    /// not write one and a binder that does not require it agree with each other, and only a
    /// consumer OUTSIDE the compilation disagrees -- so nothing inside can report it.
    ///
    /// **THE POSITIONS ARE THE TEST, NOT THE TOTAL.** A flag is rarely absent everywhere:
    /// `HideBySig` reaches constructors, virtuals, abstracts and event accessors while missing the
    /// rest, so a single method passes while three positions stay broken.
    #[test]
    fn every_declaration_position_carries_the_metadata_flags_csc_emits() {
        const HIDE_BY_SIG: u32 = 0x0080;
        const SPECIAL_NAME: u32 = 0x0800;
        const INIT_ONLY: u32 = 0x0020;
        const PARAM_OUT: u32 = 0x0002;

        let image = image_of_gated_source(
            "public class P {
                 public readonly int Fixed;
                 public int Mutable;
                 public int Instance() { return 1; }
                 public static int Static() { return 2; }
                 public int Prop { get { return 3; } }
                 public virtual int Virt() { return 4; }
                 public static P operator +(P a, P b) { return a; }
                 public static void Split(int v, out int lo, ref int hi) { lo = v; hi = v; }
                 public P() { }
             }",
        );
        let assembly = lamella_metadata::Assembly::read(&image).expect("the image parses");
        let ty = assembly
            .type_defs()
            .find(|def| def.name().is_some_and(|n| n.name == "P"))
            .expect("P is a TypeDef");

        let method = |name: &str| {
            ty.methods()
                .find(|m| m.name() == Some(name))
                .unwrap_or_else(|| panic!("P has no method {name}"))
        };
        for name in [
            "Instance", "Static", "get_Prop", "op_Addition", "Split", "Virt", ".ctor",
        ] {
            assert!(
                method(name).flags() & HIDE_BY_SIG != 0,
                "{name} must carry HideBySig"
            );
        }
        assert!(
            method("op_Addition").flags() & SPECIAL_NAME != 0,
            "an operator must carry SpecialName"
        );
        assert!(
            method("Instance").flags() & SPECIAL_NAME == 0,
            "an ordinary method must NOT carry SpecialName"
        );

        let field = |name: &str| {
            ty.fields()
                .find(|f| f.name() == Some(name))
                .unwrap_or_else(|| panic!("P has no field {name}"))
        };
        assert!(
            field("Fixed").flags() & INIT_ONLY != 0,
            "a readonly field must carry InitOnly"
        );
        assert!(
            field("Mutable").flags() & INIT_ONLY == 0,
            "a writable field must NOT carry InitOnly"
        );

        let split: alloc::vec::Vec<(Option<&str>, u32)> =
            method("Split").params().map(|p| (p.name(), p.flags())).collect();
        assert_eq!(
            split
                .iter()
                .find(|(name, _)| *name == Some("lo"))
                .map(|(_, flags)| flags & PARAM_OUT),
            Some(PARAM_OUT),
            "an `out` parameter must carry Out: {split:?}"
        );
        assert_eq!(
            split
                .iter()
                .find(|(name, _)| *name == Some("hi"))
                .map(|(_, flags)| flags & PARAM_OUT),
            Some(0),
            "a `ref` parameter must NOT carry Out: {split:?}"
        );
        assert_eq!(
            split
                .iter()
                .find(|(name, _)| *name == Some("v"))
                .map(|(_, flags)| flags & PARAM_OUT),
            Some(0),
            "a by-value parameter must NOT carry Out: {split:?}"
        );
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

    /// A `params` method declared in ONE assembly and called in expanded form from ANOTHER.
    ///
    /// **THE SECOND ASSEMBLY IS THE ENTIRE TEST.** In a single compilation the binder still has
    /// the source, so `Add(1, 2, 3)` binds from the syntax and a lost `[ParamArrayAttribute]`
    /// shows up nowhere. Across the boundary there is only metadata: with no marker the callee's
    /// signature reads as a plain `int[]` parameter, the expanded call finds no applicable
    /// overload, and the consumer fails with CS1501. That is the shape every real program has --
    /// corlib is a separate assembly -- and it is what this test was written against, having first
    /// been measured as a failure.
    #[test]
    fn a_params_method_binds_expanded_across_an_assembly_boundary() {
        let library = compile_source(
            "public class Sum { public static int Add(params int[] values) { return 0; } }",
            "lib.cs",
            "lib.dll",
            "lib",
            &[],
            false,
        );
        let library_image = library.image.expect("the library emits");
        let reference = Assembly::read(&library_image).expect("the emitted library parses");
        let consumer = compile_source(
            "class Program { static int Main() { return Sum.Add(1, 2, 3); } }",
            "app.cs",
            "app.dll",
            "app",
            &[reference],
            false,
        );
        assert!(
            consumer.diagnostics.is_empty(),
            "an expanded call on an imported params method must bind: {:?}",
            consumer.diagnostics
        );
        assert!(consumer.image.is_some(), "the consumer emits");
    }

    /// Every member kind that can DECLARE a parameter array marks it in metadata.
    ///
    /// **ENUMERATED BECAUSE THE MARKING IS SPREAD OVER SIX EMIT PATHS AND LANDED IN FIVE.** A
    /// concrete method, an abstract one, an interface method, a delegate's `Invoke`, an indexer
    /// accessor, a constructor and a P/Invoke each reach a different `add_*_method`, and the first
    /// pass wired every path but the ordinary concrete method -- the one nearly every program
    /// uses. A test naming one kind would have passed. The set below is compared WHOLE, so a new
    /// member kind that forgets the marker fails here rather than at a user's assembly boundary.
    ///
    /// The expected names come from reading the source beside them; the actual ones are read back
    /// out of the emitted image through `param_array_params` -- the same function the importer
    /// uses, so this measures what a consumer would actually see.
    #[test]
    fn every_member_kind_that_can_declare_a_params_array_marks_it() {
        let result = compile_source(
            "public delegate void Sink(params int[] items);
             public interface IAccept { int Take(params int[] values); }
             public abstract class Shape
             {
                 public abstract int Abstract(params int[] rest);
                 public int Concrete(params int[] rest) { return 0; }
                 public int this[params int[] index] { get { return 0; } }
                 public Shape(params int[] seed) { }
                 public int Unmarked(int plain) { return plain; }
             }",
            "kinds.cs",
            "kinds.dll",
            "kinds",
            &[],
            false,
        );
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let Some(image) = result.image else {
            panic!("the declarations emit: {:?}", result.emit_error)
        };
        let assembly = Assembly::read(&image).expect("the emitted image parses");
        let marked_rows = assembly.param_array_params();
        let mut marked: Vec<String> = Vec::new();
        for type_def in assembly.type_defs() {
            for method in type_def.methods() {
                let Some(name) = method.name() else { continue };
                if method
                    .params()
                    .any(|parameter| marked_rows.contains(&parameter.token().row()))
                {
                    marked.push(name.into());
                }
            }
        }
        marked.sort();
        assert_eq!(
            marked,
            alloc::vec![
                ".ctor".to_string(),
                "Abstract".to_string(),
                "Concrete".to_string(),
                "Invoke".to_string(),
                "Take".to_string(),
                "get_Item".to_string(),
            ],
            "the marked members must be exactly the ones declaring a parameter array"
        );
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

    /// The self-`TypeRef` mechanism the corlib ratchet counts, reduced to one struct: a member
    /// written inside its own namespace names its neighbour BARE (`S Create()`), the signature
    /// minter's SYNTACTIC key misses the token table -- which registered the full name -- and
    /// the old fall-through minted a `TypeRef` into the default scope for a type defined forty
    /// rows up. The fix re-asks with the RESOLVED symbol and aliases the syntactic key to the
    /// def token; this holds it at the one-struct scale the ratchet cannot localize.
    /// The emit half of that defect: a simple name shared by two namespaces must serialize as the
    /// SCOPE-LOCAL type's TypeDef, never as a raw simple name. The raw name missed the (fully
    /// keyed) enum registry, so the field serialized CLASS where the enum is a VALUETYPE, and
    /// the image failed to LOAD on .NET with a value-type mismatch -- a build that succeeds and
    /// an artifact that cannot run.
    #[test]
    fn a_simple_name_shared_by_two_namespaces_emits_the_scope_local_identity() {
        let compilation = compile_source(
            "namespace Some.Where { public enum Marker { A = 0 } } \
             namespace Other.Place { public enum Marker { B = 5 } \
             public class P { private static Marker _f; \
                 static int Main() { _f = Marker.B; return (int)_f; } } }",
            "test.cs",
            "test.dll",
            "test",
            &[],
            false,
        );
        assert!(
            compilation.diagnostics.iter().all(|d| !d.is_error()),
            "unexpected diagnostics: {:?}",
            compilation.diagnostics
        );
        let image = compilation.image.expect("the colliding-name program compiles to an image");
        let assembly = lamella_metadata::Assembly::read(&image).expect("the image parses");
        let marker_refs: alloc::vec::Vec<String> = assembly
            .type_refs()
            .filter_map(|r| r.name())
            .filter(|n| n.name == "Marker")
            .map(|n| alloc::format!("{}.{}", n.namespace, n.name))
            .collect();
        assert!(
            marker_refs.is_empty(),
            "a local enum leaked into a TypeRef: {marker_refs:?}"
        );
        let field_sig = assembly
            .type_defs()
            .find(|def| def.name().is_some_and(|n| n.name == "P"))
            .and_then(|def| def.fields().find(|f| f.name() == Some("_f")))
            .and_then(|f| f.signature())
            .expect("P._f has a readable signature");
        assert!(
            matches!(field_sig, lamella_metadata::SigType::ValueType(_)),
            "an enum field must serialize VALUETYPE, got {field_sig:?}"
        );
    }

    #[test]
    fn a_bare_mention_inside_its_own_namespace_mints_no_self_type_ref() {
        let compilation = compile_source(
            "namespace N { public struct S { private int _x;
                 public static S Create() { S v = new S(); v._x = 1; return v; }
                 public S Twin() { return Create(); } } }",
            "test.cs",
            "test.dll",
            "test",
            &[],
            false,
        );
        assert!(
            compilation.diagnostics.iter().all(|d| !d.is_error()),
            "unexpected diagnostics: {:?}",
            compilation.diagnostics
        );
        let image = compilation.image.expect("the struct compiles to an image");
        let assembly = lamella_metadata::Assembly::read(&image).expect("the image parses");
        let defined: alloc::vec::Vec<(String, String)> = assembly
            .type_defs()
            .filter_map(|def| def.name())
            .map(|n| (n.namespace.to_string(), n.name.to_string()))
            .collect();
        let self_refs: alloc::vec::Vec<String> = assembly
            .type_refs()
            .filter_map(|r| r.name())
            .filter(|n| defined.iter().any(|(ns, name)| ns == n.namespace && name == n.name))
            .map(|n| format!("{}.{}", n.namespace, n.name))
            .collect();
        assert!(
            self_refs.is_empty(),
            "the image references types it defines: {self_refs:?}"
        );
    }

    /// The other three self-reference mechanisms, at one-file scale -- each one reached by a
    /// DIFFERENT door, which is why the corlib ratchet counted three rows after the struct-`.ctor`
    /// fix closed the first two.
    ///
    /// **An operator lowering names its BCL member outright, so it must not mint unconditionally.**
    /// `a + b` is `String.Concat`, `typeof(T)` is `Type.GetTypeFromHandle`; in a module that
    /// DEFINES `System.String` and `System.Type` those are this module's own `MethodDef`s, and the
    /// reference gave each type a second identity scoped to an `mscorlib` `AssemblyRef` the image
    /// is not. Measured on the corlib: 108 `Concat` rows and 2 `GetTypeFromHandle` rows, every one
    /// of them resolving correctly here (every resolver in this compiler keys on (namespace,
    /// name)) and none of them
    /// resolving for csc, which refused the artifact as a core library with CS0518.
    ///
    /// **A renamed indexer arrives through a NAME, not through a missing guard.** `s[i]` lowers to
    /// a hard-named `get_Chars`, while the token pre-pass reserves an indexer's accessor under the
    /// model's synthetic `get_Item` -- so the guard that WAS there asked a question the table could
    /// not answer, and minted.
    ///
    /// **A constructor with an explicit chain must not pay for the implicit one.** Computing the
    /// base call before consulting the initializer MINTS a reference to the base's PARAMETERLESS
    /// constructor -- which a base like `Handle(bool, bool)` does not have, so the rows would name
    /// a member nothing declares and nothing calls.
    ///
    /// Both halves are asserted, because they fail independently: no `TypeRef` may name a type
    /// this module defines, AND no `MemberRef` may name one of these members however its parent is
    /// spelled (a `MemberRef` may hang off a `TypeDef`, which the first assertion cannot see).
    #[test]
    fn a_module_defining_the_bcl_types_its_lowerings_name_mints_no_reference_to_them() {
        let compilation = compile_source(
            "namespace System { \
                 public class Attribute { } \
                 public struct RuntimeTypeHandle { } \
                 public sealed class String { \
                     [System.Runtime.CompilerServices.IndexerName(\"Chars\")] \
                     public char this[int index] { get { return 'x'; } } \
                     public static string Concat(string a, string b) { return b; } } \
                 public class Type { \
                     public static Type GetTypeFromHandle(RuntimeTypeHandle handle) { return null; } } } \
             namespace System.Runtime.CompilerServices { \
                 public sealed class IndexerNameAttribute : System.Attribute { \
                     public IndexerNameAttribute(string indexerName) { } } } \
             namespace P { \
                 public abstract class Handle { internal Handle(bool a, bool b) { } } \
                 public sealed class Event : Handle { public Event(bool state) : base(state, true) { } } \
                 public class Q { \
                     public static string Join(string a, string b) { return a + b; } \
                     public static char First(string s) { return s[0]; } \
                     public static object Reflect() { return typeof(Q); } } }",
            "test.cs",
            "test.dll",
            "test",
            &[],
            false,
        );
        assert!(
            compilation.diagnostics.iter().all(|d| !d.is_error()),
            "unexpected diagnostics: {:?}",
            compilation.diagnostics
        );
        let image = compilation.image.expect("the corlib-shaped program compiles to an image");
        let assembly = lamella_metadata::Assembly::read(&image).expect("the image parses");
        let defined: alloc::vec::Vec<(String, String)> = assembly
            .type_defs()
            .filter_map(|def| def.name())
            .map(|n| (n.namespace.to_string(), n.name.to_string()))
            .collect();
        let self_refs: alloc::vec::Vec<String> = assembly
            .type_refs()
            .filter_map(|r| r.name())
            .filter(|n| defined.iter().any(|(ns, name)| ns == n.namespace && name == n.name))
            .map(|n| format!("{}.{}", n.namespace, n.name))
            .collect();
        assert!(
            self_refs.is_empty(),
            "the image references types it defines: {self_refs:?}"
        );
        let defined_tokens: alloc::vec::Vec<Token> =
            assembly.type_defs().map(|def| def.token()).collect();
        let referenced: alloc::vec::Vec<String> = assembly
            .member_refs()
            .filter(|member| {
                let parent = member.parent();
                match parent.table() {
                    TYPE_DEF => defined_tokens.contains(&parent),
                    TYPE_REF => assembly
                        .type_ref(parent.row())
                        .and_then(|r| r.name())
                        .is_some_and(|n| {
                            defined.iter().any(|(ns, name)| ns == n.namespace && name == n.name)
                        }),
                    _ => false,
                }
            })
            .map(|member| member.name().unwrap_or("<unnamed>").to_string())
            .collect();
        assert!(
            referenced.is_empty(),
            "a member of a type this module defines was referenced rather than called: {referenced:?}"
        );
    }
}
