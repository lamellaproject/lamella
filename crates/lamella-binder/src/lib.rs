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
pub mod program;
pub mod reference;
pub mod resolve;
pub mod special;
pub mod statement;
pub mod symbols;
pub mod types;

pub use bind::bind_type;
pub use complete::{
    Completion, CompletionKind, Hover, SignatureHelp, complete, hover, signature_help,
};
pub use bound::{
    Binder, BoundExpr, BoundExprKind, ConversionKind, FieldReference, MethodReference,
    bind_expression,
};
pub use conversion::has_implicit_conversion;
pub use declaration::{collect_into, collect_model, collect_types};
pub use diagnostic::{Diagnostic, DiagnosticKind};
pub use flow::{always_exits, check_definite_assignment};
pub use program::{
    bind_compilation_unit, bind_compilation_unit_with_model, bind_compilation_unit_with_references,
};
pub use reference::load_assembly;
pub use resolve::{TypeTable, resolve_type};
pub use special::SpecialType;
pub use statement::{
    BoundCatch, BoundDeclarator, BoundStmt, BoundStmtKind, BoundSwitchLabel, BoundSwitchSection,
};
pub use symbols::{FieldSymbol, MethodSymbol, Model, PropertySymbol, TypeInfo, TypeKind};
pub use types::TypeSymbol;
