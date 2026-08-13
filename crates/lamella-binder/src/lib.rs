#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Semantic analysis for C# 1.0 (ECMA-334 1st edition).

extern crate alloc;

pub mod bind;
pub mod bound;
pub mod complete;
pub mod conversion;
pub mod declaration;
pub mod diagnostic;
pub mod flow;
mod infer;
pub mod program;
pub mod reference;
pub mod resolve;
pub mod special;
pub mod statement;
pub mod symbols;
pub mod types;

pub use bind::{bind_type, parameter_symbol};
pub use complete::{
    Completion, CompletionKind, Hover, SignatureHelp, complete, hover, signature_help,
};
pub use bound::{
    Binder, BoundExpr, BoundExprKind, BoundInitializer, BoundInitializerTarget,
    BoundMemberInitializer, BoundMemberInitializerValue, ConversionKind, DeclaredField,
    FieldInstantiation, FieldReference, ConvertingStep, MethodInstantiation, MethodReference,
    SubmissionBinding, TypeInstantiation, bind_expression, literal_int_value,
};
pub use conversion::has_implicit_conversion;
pub use declaration::{
    collect_into, collect_model, collect_types, constraints_by_parameter, resolve_constants,
};
pub use diagnostic::{CodeNamespace, Diagnostic, DiagnosticKind};
pub use flow::{always_exits, check_definite_assignment, switch_section_reachability};
pub use program::{
    BindOptions, bind_compilation_unit, bind_compilation_unit_with_model, bind_compilation_unit_with_references, bind_compilation_unit_with_dialect, bind_compilation_unit_with_options, bind_compilation_unit_with_references_and_options,
    bind_compilation_units_with_options, bind_compilation_units_with_references, bind_compilation_units_with_references_and_options,
};
pub use reference::load_assembly;
pub use resolve::{TypeTable, resolve_type};
pub use special::SpecialType;
pub use statement::{
    BoundCatch, BoundDeclarator, BoundStmt, BoundStmtKind, BoundSwitchLabel, BoundSwitchSection,
};
pub use symbols::{
    Accessibility, EventSymbol, FieldSymbol, MethodSymbol, Model, PropertySymbol, SignatureCanon,
    TypeInfo, TypeKind, definition_metadata_name, definition_symbol, mangled_arity,
    metadata_type_name,
};
pub use types::TypeSymbol;
