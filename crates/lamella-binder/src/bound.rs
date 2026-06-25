//! The bound expression tree and the expression binder (ECMA-334 1st ed,
//! clause 14).

use crate::bind::bind_type;
use crate::conversion::{can_cast, converts};
use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::resolve::{TypeTable, resolve_type};
use crate::special::SpecialType;
use crate::symbols::{Accessibility, EventSymbol, MethodSymbol, Model, TypeInfo, TypeKind};
use crate::types::TypeSymbol;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use lamella_syntax::ast::{
    AssignmentOperator, BinaryOperator, Expr, ExprKind, Literal, PostfixOperator, TypeRef,
    TypeTestOperation, UnaryOperator,
};
use lamella_syntax::span::Span;
use lamella_syntax::token::{IntegerSuffix, RealSuffix};

/// A bound expression: its kind and its resolved type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundExpr {
    /// What the expression is, after binding.
    pub kind: BoundExprKind,
    /// The expression's type (`TypeSymbol::Error` when binding failed).
    pub ty: TypeSymbol,
}

/// The field an access resolved to, recorded so emission can name it with a
/// metadata token and choose `ldfld`/`stfld` versus `ldsfld`/`stsfld`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldReference {
    /// The type that declares the field.
    pub declaring_type: TypeSymbol,
    /// The field name.
    pub name: Box<str>,
    /// The field's type.
    pub ty: TypeSymbol,
    /// Whether the field is `static`.
    pub is_static: bool,
    /// The field's accessibility.
    pub accessibility: Accessibility,
    /// The compile-time constant value of a `const` field or enum member, so emission folds
    /// the access to a constant load (not an `ldsfld`). `None` for an ordinary field.
    pub constant: Option<Literal>,
}

/// The method an invocation resolved to, recorded so emission can name it with a
/// metadata token and choose `call` versus `callvirt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodReference {
    /// The type through which the method is named.
    pub declaring_type: TypeSymbol,
    /// The method name.
    pub name: Box<str>,
    /// The parameter types, in order.
    pub parameters: Vec<TypeSymbol>,
    /// The return type.
    pub return_type: TypeSymbol,
    /// Whether the method is `static`.
    pub is_static: bool,
}

/// What an inserted [`BoundExprKind::Conversion`] does at emit time (13.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversionKind {
    /// A widening numeric conversion: emit `conv.*` to the target type.
    ImplicitNumeric,
    /// A value type to `object`: emit `box`.
    Boxing,
    /// A reference upcast (derived to base or interface): a no-op in CIL.
    ImplicitReference,
}

/// The kind of a [`BoundExpr`]. Grows as the binder learns more expression forms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundExprKind {
    /// A constant literal, retyped from the syntax (9.4.4).
    Literal(Literal),
    /// A reference to a local variable or parameter (14.5.2).
    Local(Box<str>),
    /// A `ref`/`out` argument (17.5.1): the address of the inner variable, passed to a
    /// byref parameter. Its type is the variable's type. `out` assigns the variable (so
    /// it need not be assigned beforehand); `ref` requires it already assigned.
    Ref {
        /// `true` for `out`, `false` for `ref`.
        out: bool,
        /// The variable whose address is passed.
        operand: Box<BoundExpr>,
    },
    /// The `this` access (14.5.7); its type is the enclosing type.
    This,
    /// A `base` access (14.5.8); its type is the enclosing type's base class, used
    /// as the receiver of a non-virtual `base.member`.
    Base,
    /// A type name used as the receiver of a static member access (14.5.4). Its
    /// type is the named type so member lookup reaches the type's members.
    TypeReference(TypeSymbol),
    /// A namespace name used as the receiver of a qualified name (10.8). Not a
    /// value; only a step in resolving a qualified type name.
    NamespaceReference(Box<str>),
    /// Access to an instance or static field through a receiver (14.5.4); the
    /// expression's type is the field's.
    FieldAccess {
        /// The receiver the field is read from.
        receiver: Box<BoundExpr>,
        /// The field name.
        name: Box<str>,
        /// The resolved field, recorded for emission.
        field: Option<FieldReference>,
    },
    /// Access to a property through a receiver (14.5.4); the expression's type is
    /// the property's.
    PropertyAccess {
        /// The receiver the property is read from.
        receiver: Box<BoundExpr>,
        /// The type that declares the property (a base, for an inherited one) -- the
        /// `get_`/`set_` accessor is named on it, not the receiver's static type.
        declaring_type: TypeSymbol,
        /// The property name.
        name: Box<str>,
    },
    /// A method group named through a receiver (14.5.4) -- not a value on its own,
    /// only the target of an invocation, so its type is the error type.
    MethodGroup {
        /// The receiver the method is called on.
        receiver: Box<BoundExpr>,
        /// The method name.
        name: Box<str>,
    },
    /// A method call (14.5.5); its type is the chosen overload's return type.
    Call {
        /// The callee (a method group).
        callee: Box<BoundExpr>,
        /// The bound arguments, in order.
        arguments: Vec<BoundExpr>,
        /// The method overload resolution chose, when it succeeded.
        method: Option<MethodReference>,
    },
    /// An element access `receiver[indices]` (14.5.6); its type is the array's
    /// element type.
    ElementAccess {
        /// The indexed receiver.
        receiver: Box<BoundExpr>,
        /// The index arguments.
        indices: Vec<BoundExpr>,
    },
    /// An array creation `new T[...]` (14.5.10.2); its type is the array type.
    ArrayCreation {
        /// The dimension-length expressions (empty when the size comes from
        /// `elements`).
        lengths: Vec<BoundExpr>,
        /// The `{ ... }` initializer elements, converted to the element type; empty for
        /// a sized-but-uninitialized array.
        elements: Vec<BoundExpr>,
    },
    /// An object creation `new T(args)` (14.5.10.1); its type is the created type.
    ObjectCreation {
        /// The constructor arguments.
        arguments: Vec<BoundExpr>,
        /// The constructor overload resolution chose, when it succeeded.
        constructor: Option<MethodReference>,
    },
    /// A delegate creation `new D(methodGroup)` (14.5.10.3): a method group converts to
    /// the delegate `D`. Its type is `D`. Emits `ldftn target` (with the receiver, or
    /// `ldnull` for a static target) then `newobj D::.ctor`.
    DelegateCreation {
        /// The delegate type being created.
        delegate_type: TypeSymbol,
        /// The method the delegate targets.
        target: MethodReference,
        /// The receiver for an instance target; `None` for a static one.
        receiver: Option<Box<BoundExpr>>,
    },
    /// A binary operation on two bound operands (14.7-14.12).
    Binary {
        /// The operator.
        operator: BinaryOperator,
        /// The left operand.
        left: Box<BoundExpr>,
        /// The right operand.
        right: Box<BoundExpr>,
        /// Whether the operation is in a `checked` context, so emission uses the
        /// overflow-checking `add.ovf`/`sub.ovf`/`mul.ovf` (14.5.12).
        checked: bool,
    },
    /// A prefix unary operation (14.6).
    Unary {
        /// The operator.
        operator: UnaryOperator,
        /// The operand.
        operand: Box<BoundExpr>,
    },
    /// A postfix increment or decrement (14.5.9).
    Postfix {
        /// The operator.
        operator: PostfixOperator,
        /// The operand.
        operand: Box<BoundExpr>,
    },
    /// A cast to the expression's type (14.6.6).
    Cast {
        /// The operand being cast.
        operand: Box<BoundExpr>,
        /// Whether the cast is in a `checked` context, so a narrowing integer
        /// conversion uses `conv.ovf.*` (14.5.12).
        checked: bool,
    },
    /// An implicit conversion the binder inserts so emission knows to widen a
    /// numeric, box a value type, or treat a reference upcast as a no-op (13.1).
    /// The expression's type is the conversion's target.
    Conversion {
        /// The value being converted.
        operand: Box<BoundExpr>,
        /// What kind of conversion to perform.
        conversion: ConversionKind,
    },
    /// An `is`/`as` type test (14.9.9, 14.9.10); the tested type is the result
    /// type for `as` and `bool` for `is`.
    TypeTest {
        /// Whether this is `is` or `as`.
        operation: TypeTestOperation,
        /// The operand.
        operand: Box<BoundExpr>,
        /// The type tested against (`isinst` names it).
        target: TypeSymbol,
    },
    /// An assignment, simple or compound (14.14); its type is the target's.
    Assignment {
        /// The assignment operator.
        operator: AssignmentOperator,
        /// The assignment target (an lvalue).
        target: Box<BoundExpr>,
        /// The assigned value.
        value: Box<BoundExpr>,
    },
    /// A conditional expression `c ? a : b` (14.13).
    Conditional {
        /// The condition.
        condition: Box<BoundExpr>,
        /// The value when true.
        when_true: Box<BoundExpr>,
        /// The value when false.
        when_false: Box<BoundExpr>,
    },
    /// A `typeof` expression (14.5.11), naming the type it reflects; its type is
    /// `System.Type`.
    TypeOf(TypeSymbol),
    /// A `sizeof(T)` (III.4.25): the byte size of `T` as an `int`.
    SizeOf(TypeSymbol),
    /// A `stackalloc T[count]` (unsafe): a `T*` to `count * sizeof(T)` stack bytes.
    StackAlloc {
        /// The element type.
        element: TypeSymbol,
        /// The element count.
        count: Box<BoundExpr>,
    },
    /// A pointer indirection `*operand` (unsafe): reads/writes the element the pointer
    /// addresses. An lvalue when it is an assignment target.
    Dereference {
        /// The pointer being dereferenced.
        operand: Box<BoundExpr>,
    },
    /// A `checked` expression (14.5.12); the type is the operand's.
    Checked(Box<BoundExpr>),
    /// An `unchecked` expression (14.5.12); the type is the operand's.
    Unchecked(Box<BoundExpr>),
    /// An expression that could not be bound (yet), for recovery.
    Error,
}

/// The method currently being bound: its name (for `CS0127`) and declared return
/// type (for checking `return`).
#[derive(Debug, Clone)]
struct MethodContext {
    name: Box<str>,
    return_type: TypeSymbol,
}

/// The result of binding one REPL submission ([`Binder::bind_submission`]): the bound
/// `Submit$N` body, its return type, and the session variables it introduced.
#[derive(Debug, Clone)]
pub struct SubmissionBinding {
    /// The bound submission body -- a block of the session-field stores and statements,
    /// ending in a boxed `return` for a trailing display expression.
    pub body: crate::statement::BoundStmt,
    /// The `Submit$N` method's return type: `object` for a display expression, else `void`.
    pub return_type: TypeSymbol,
    /// The session variables this submission introduced, in declaration order, each with
    /// its stable `__Repl` field name -- the caller commits them so later submissions see
    /// them (and rebinds the source name on a redefinition). A field the runtime cannot
    /// resolve against the loaded `__Repl` it adds (inference).
    pub new_fields: Vec<DeclaredField>,
}

/// A session variable a submission introduced: its source name, the stable `__Repl` field
/// name it was assigned (the source name, or `x$2` on a redefinition), and its type.
#[derive(Debug, Clone)]
pub struct DeclaredField {
    /// The C# name the user wrote.
    pub source: Box<str>,
    /// The stable `__Repl` field name (the source name, or a fresh `x$2` on redefinition).
    pub stable: Box<str>,
    /// The field's type.
    pub ty: TypeSymbol,
}

/// Binds expressions, accumulating the semantic diagnostics found. Holds a stack
/// of local-variable scopes for name resolution.
#[derive(Debug, Default)]
pub struct Binder {
    diagnostics: Vec<Diagnostic>,
    scopes: Vec<BTreeMap<String, TypeSymbol>>,
    world: TypeTable,
    model: Model,
    current_type: Option<TypeSymbol>,
    current_method: Option<MethodContext>,
    imported_namespaces: Vec<Box<str>>,
    /// `using X = N.T;` aliases in scope, each the alias name and its target type, so an
    /// unqualified `X` resolves to the target (16.4.1). Scoped per namespace block alongside
    /// `imported_namespaces`.
    aliases: Vec<(Box<str>, TypeSymbol)>,
    /// Locals referenced in `switch` case-label expressions of the method being
    /// bound -- they are folded out of the bound tree, so the unused-local check
    /// (`CS0168`/`CS0219`) is seeded with them to avoid a false warning. Reset per
    /// method.
    case_label_uses: alloc::collections::BTreeSet<Box<str>>,
    /// Whether expressions are currently bound in a `checked` context (14.5.12),
    /// tracked as the binder descends so each arithmetic/cast node records whether
    /// emission should use the overflow-checking form. C# 1.0 defaults to unchecked.
    checked_context: bool,
    /// In REPL session mode, the name of the parameter standing in for the persistent
    /// `__Repl` instance (`s`). When set, an unqualified name that resolves to a member
    /// of the enclosing type reads through `s` (a parameter) instead of `this` -- the
    /// submission method is a static `Submit$N(__Repl s)`, so session locals are fields
    /// of `s`, not of a non-existent `this`. `None` in ordinary binding.
    session_receiver: Option<Box<str>>,
    /// In REPL session mode, each session variable's source name -> (its stable `__Repl`
    /// field name, its type). An unqualified name found here reads `s.<stable>` (14.5.2
    /// through `s`). It is keyed by SOURCE name and maps to the STABLE field name so a
    /// type-changing redefinition -- which adds a fresh field `x$2` and rebinds source `x`
    /// to it -- resolves correctly. Empty (so a no-op) in ordinary binding.
    session_fields: BTreeMap<String, (Box<str>, TypeSymbol)>,
    /// How many enclosing loops (`for`/`while`/`do`/`foreach`) the binder is inside, so a
    /// `break`/`continue` with no enclosing loop is `CS0139`. Reset per method.
    loop_depth: u32,
    /// How many enclosing `switch` statements the binder is inside (a `break` is also valid
    /// in a switch). Reset per method.
    switch_depth: u32,
    /// The preprocessor symbols defined for this compilation (from `#define`), so a call to a
    /// `[Conditional("X")]` method with no `X` here is omitted (24.4.2). Empty by default.
    defined_symbols: alloc::collections::BTreeSet<Box<str>>,
}

impl Binder {
    /// A fresh binder with an empty reference world.
    #[must_use]
    pub fn new() -> Binder {
        Binder::default()
    }

    /// A binder that resolves named types against `world` (existence only; member
    /// lookup needs [`Binder::with_model`]).
    #[must_use]
    pub fn with_world(world: TypeTable) -> Binder {
        Binder {
            world,
            ..Binder::default()
        }
    }

    /// A binder that resolves type names and looks members up against `model`.
    #[must_use]
    pub fn with_model(model: Model) -> Binder {
        Binder {
            world: model.type_table(),
            model,
            ..Binder::default()
        }
    }

    /// The binder's type model, for the assembling step (base classes, member kinds).
    #[must_use]
    pub fn model(&self) -> &Model {
        &self.model
    }

    /// Sets the `#define`d preprocessor symbols for this compilation, so a call to a
    /// `[Conditional("X")]` method with no `X` here is omitted (24.4.2).
    pub fn set_defined_symbols(&mut self, symbols: alloc::collections::BTreeSet<Box<str>>) {
        self.defined_symbols = symbols;
    }

    /// Records a diagnostic.
    pub(crate) fn report(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
    }

    /// Records the locals a `switch` case-label expression references, so the
    /// unused-local check (`CS0168`/`CS0219`) is not misled by a label folded out of
    /// the bound tree.
    pub(crate) fn record_case_label_uses(&mut self, expr: &BoundExpr) {
        crate::flow::collect_uses(expr, &mut self.case_label_uses);
    }

    /// Resolves a syntactic type name to its canonical type via the namespaces and aliases
    /// in scope (e.g. `Type` with `using System;` -> `System.Type`), for the emitter to mint
    /// an external type's `TypeRef` in a signature. Resolution-only; reports no diagnostic.
    #[must_use]
    pub fn resolve_type(&self, ty: &TypeSymbol) -> TypeSymbol {
        if let TypeSymbol::Named(parts) = ty {
            if parts.len() == 1 {
                let name: &str = &parts[0];
                if let Some(target) = self.alias_target(name) {
                    return target;
                }
                let hits = self.type_namespaces_containing(name);
                if hits.len() == 1 {
                    return type_symbol_in(&hits[0], name);
                }
            }
        }
        ty.clone()
    }

    /// Resolves a type against the reference world, reporting `CS0246` if unknown.
    pub(crate) fn resolve_named_type(&mut self, ty: &TypeSymbol, span: Span) -> TypeSymbol {
        if let TypeSymbol::Named(parts) = ty {
            if parts.len() == 1 {
                let name: &str = &parts[0];
                if let Some(target) = self.alias_target(name) {
                    return target;
                }
                let hits = self.type_namespaces_containing(name);
                if hits.len() == 1 {
                    return type_symbol_in(&hits[0], name);
                }
            }
        }
        resolve_type(&self.world, ty, &mut self.diagnostics, span)
    }

    /// Rewrites a single-part named type to its canonical fully-qualified symbol when that
    /// simple name is unambiguous in the model, so a body-bound type (e.g. a method's declared
    /// return type, structurally bound from syntax) is the SAME [`TypeSymbol`] as the qualified
    /// form a `new`/cast produces. Mirrors [`Model::canonicalize_signatures`] for the types the
    /// body re-binds from syntax; non-reporting (an unresolved name stays as is for the normal
    /// resolver to diagnose). Arrays and pointers canonicalize their element type.
    pub(crate) fn canonicalize(&self, ty: &TypeSymbol) -> TypeSymbol {
        match ty {
            TypeSymbol::Named(parts) if parts.len() == 1 => self
                .model
                .type_with_simple_name(&parts[0])
                .unwrap_or_else(|| ty.clone()),
            TypeSymbol::Array { element, rank } => TypeSymbol::Array {
                element: Box::new(self.canonicalize(element)),
                rank: *rank,
            },
            TypeSymbol::Pointer(inner) => TypeSymbol::Pointer(Box::new(self.canonicalize(inner))),
            _ => ty.clone(),
        }
    }

    /// Whether `from` implicitly converts to `to`, including reference conversions
    /// that walk the model's inheritance graph (13.1).
    pub(crate) fn converts(&self, from: &TypeSymbol, to: &TypeSymbol) -> bool {
        converts(&self.model, from, to)
            || self.user_conversion(from, to, "op_Implicit").is_some()
    }

    /// Whether `value` is assignable to `target`: it implicitly converts by type, or it
    /// is a constant whose value fits a narrower integral `target` (13.1.7). Use this at
    /// an assignment context that has the value expression, not just its type.
    pub(crate) fn assignable(&self, value: &BoundExpr, target: &TypeSymbol) -> bool {
        self.converts(&value.ty, target) || implicit_constant_conversion(value, target)
    }

    /// Reports a failed conversion (`CS0266`/`CS0029`) at an assignment context unless
    /// `value` is assignable to `target` (including the constant-expression rule). Error
    /// types are skipped so a prior failure does not cascade.
    pub(crate) fn check_assignable(&mut self, value: &BoundExpr, target: &TypeSymbol, span: Span) {
        if target.is_error() {
            return;
        }
        if let BoundExprKind::MethodGroup { name, .. } = &value.kind {
            let to_delegate = self
                .type_info_of(target)
                .is_some_and(|info| info.kind == TypeKind::Delegate);
            if !to_delegate {
                self.report(Diagnostic::new(
                    DiagnosticKind::MethodGroupToNonDelegate {
                        method: name.clone(),
                        target: target.to_string().into(),
                    },
                    span,
                ));
            }
            return;
        }
        if value.ty.is_error() {
            return;
        }
        if !self.assignable(value, target) {
            self.report_no_implicit_conversion(&value.ty, target, span);
        }
    }

    /// A user-defined conversion method (`op_Implicit`/`op_Explicit`) taking `from` and
    /// returning `to`, declared on either the source or target type (17.9.3). The static
    /// call a `from -> to` conversion lowers to.
    fn user_conversion(
        &self,
        from: &TypeSymbol,
        to: &TypeSymbol,
        name: &str,
    ) -> Option<MethodReference> {
        for owner in [from, to] {
            for method in self.methods_in_chain(owner, name) {
                if method.parameters.len() == 1
                    && &method.parameters[0] == from
                    && &method.return_type == to
                {
                    let declaring_type =
                        self.declaring_type_in_chain(owner, name, &method.parameters);
                    return Some(MethodReference {
                        declaring_type,
                        name: name.into(),
                        parameters: method.parameters,
                        return_type: method.return_type,
                        is_static: true,
                    });
                }
            }
        }
        None
    }

    /// Reports a failed implicit conversion at `span`: `CS0266` when an explicit
    /// conversion (a cast) exists, otherwise `CS0029`. Use this at every assignment
    /// context (initializer, assignment, return, field initializer); a context with
    /// no cast escape (a non-`bool` condition) reports `CS0029` directly.
    pub(crate) fn report_no_implicit_conversion(
        &mut self,
        from: &TypeSymbol,
        to: &TypeSymbol,
        span: Span,
    ) {
        let kind = if can_cast(&self.model, from, to) {
            DiagnosticKind::ExplicitConversionExists {
                from: from.to_string().into(),
                to: to.to_string().into(),
            }
        } else {
            DiagnosticKind::NoImplicitConversion {
                from: from.to_string().into(),
                to: to.to_string().into(),
            }
        };
        self.report(Diagnostic::new(kind, span));
    }

    /// Wraps `expr` in the implicit conversion to `target` so emission widens,
    /// boxes, or upcasts as needed (13.1). Returns `expr` unchanged when the types
    /// match or no implicit conversion applies (the site reports any error).
    pub(crate) fn convert(&self, expr: BoundExpr, target: &TypeSymbol) -> BoundExpr {
        if matches!(expr.kind, BoundExprKind::MethodGroup { .. })
            && self
                .type_info_of(target)
                .is_some_and(|info| info.kind == TypeKind::Delegate)
        {
            return self.bind_delegate_creation(target, &[expr], Span::empty_at(0));
        }
        if matches!(target, TypeSymbol::ByRef(_))
            && matches!(expr.kind, BoundExprKind::Ref { .. })
        {
            return expr;
        }
        if expr.ty == *target || expr.ty.is_error() || target.is_error() {
            return expr;
        }
        if converts(&self.model, &expr.ty, target) {
            let conversion = self.conversion_kind(&expr.ty, target);
            return BoundExpr {
                kind: BoundExprKind::Conversion {
                    operand: Box::new(expr),
                    conversion,
                },
                ty: target.clone(),
            };
        }
        if implicit_constant_conversion(&expr, target) {
            return BoundExpr {
                kind: BoundExprKind::Conversion {
                    operand: Box::new(expr),
                    conversion: ConversionKind::ImplicitNumeric,
                },
                ty: target.clone(),
            };
        }
        if let Some(method) = self.user_conversion(&expr.ty, target, "op_Implicit") {
            return BoundExpr {
                ty: target.clone(),
                kind: BoundExprKind::Call {
                    callee: Box::new(error_expr()),
                    arguments: alloc::vec![expr],
                    method: Some(method),
                },
            };
        }
        expr
    }

    /// Binds an array initializer `{ e, ... }` against `array_ty`, converting each
    /// element to the array's element type. Used for `new T[]{...}` and `T[] a = {...}`.
    pub(crate) fn bind_array_initializer(
        &mut self,
        init: &Expr,
        array_ty: &TypeSymbol,
    ) -> Vec<BoundExpr> {
        let ExprKind::ArrayInitializer(elements) = &init.kind else {
            return Vec::new();
        };
        let element_ty = match array_ty {
            TypeSymbol::Array { element, .. } => (**element).clone(),
            _ => TypeSymbol::Error,
        };
        elements
            .iter()
            .map(|element| {
                let bound = self.bind_expression(element);
                self.convert(bound, &element_ty)
            })
            .collect()
    }

    /// Converts `value` to the enclosing method's return type, for a `return`.
    pub(crate) fn convert_to_return_type(&self, value: BoundExpr) -> BoundExpr {
        match &self.current_method {
            Some(method) => {
                let target = method.return_type.clone();
                self.convert(value, &target)
            }
            None => value,
        }
    }

    fn conversion_kind(&self, from: &TypeSymbol, to: &TypeSymbol) -> ConversionKind {
        if as_special(from).is_some_and(SpecialType::is_numeric)
            && as_special(to).is_some_and(SpecialType::is_numeric)
        {
            ConversionKind::ImplicitNumeric
        } else if matches!(to, TypeSymbol::Special(SpecialType::Object)) && self.is_value_type(from)
        {
            ConversionKind::Boxing
        } else {
            ConversionKind::ImplicitReference
        }
    }

    /// Whether a type is a value type (boxed when converted to `object`).
    fn is_value_type(&self, ty: &TypeSymbol) -> bool {
        match ty {
            TypeSymbol::Special(
                SpecialType::Object | SpecialType::String | SpecialType::Null,
            ) => false,
            TypeSymbol::Special(_) => true,
            TypeSymbol::Named(_) => matches!(
                self.type_info_of(ty).map(|info| info.kind),
                Some(TypeKind::Struct | TypeKind::Enum)
            ),
            TypeSymbol::Array { .. }
            | TypeSymbol::Pointer(_)
            | TypeSymbol::ByRef(_)
            | TypeSymbol::Error => false,
        }
    }

    /// The result of `==`/`!=` when one operand is the null literal (14.9.6): the null type
    /// is reference-comparable with any reference type (and with the null type itself),
    /// giving `bool`. It is not comparable with a value type -- which has no null -- so that
    /// returns `None` and falls through to the not-applicable diagnostic. `None` also when
    /// neither operand is the null type.
    fn null_equality_result(
        &self,
        operator: BinaryOperator,
        left: &TypeSymbol,
        right: &TypeSymbol,
    ) -> Option<TypeSymbol> {
        if !matches!(operator, BinaryOperator::Equal | BinaryOperator::NotEqual) {
            return None;
        }
        let null = TypeSymbol::Special(SpecialType::Null);
        let other = if *left == null {
            right
        } else if *right == null {
            left
        } else {
            return None;
        };
        (!self.is_value_type(other)).then(|| TypeSymbol::Special(SpecialType::Boolean))
    }

    /// The diagnostics gathered so far.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Consumes the binder, returning its diagnostics.
    #[must_use]
    pub fn into_diagnostics(self) -> Vec<Diagnostic> {
        self.diagnostics
    }

    /// Sets the enclosing type whose members an unqualified name and `this`
    /// resolve against, for binding that type's method bodies.
    pub fn enter_type(&mut self, ty: TypeSymbol) {
        self.current_type = Some(ty);
    }

    /// Clears the enclosing type.
    pub fn exit_type(&mut self) {
        self.current_type = None;
    }

    /// Brings a namespace into scope for unqualified type-name resolution (a
    /// `using` directive, 16.3).
    pub fn import_namespace(&mut self, namespace: &str) {
        self.imported_namespaces.push(namespace.into());
    }

    /// Brings a `using X = N.T;` alias into scope: an unqualified `X` resolves to `target`
    /// (16.4.1).
    pub fn import_alias(&mut self, name: &str, target: TypeSymbol) {
        self.aliases.push((name.into(), target));
    }

    /// The target type of an in-scope alias `name`, if any (the most recent wins).
    fn alias_target(&self, name: &str) -> Option<TypeSymbol> {
        self.aliases
            .iter()
            .rev()
            .find(|(alias, _)| &**alias == name)
            .map(|(_, target)| target.clone())
    }

    /// A marker for the current set of imported namespaces and aliases, to scope the usings
    /// of a namespace block: snapshot before, restore after.
    #[must_use]
    pub fn import_scope(&self) -> (usize, usize) {
        (self.imported_namespaces.len(), self.aliases.len())
    }

    /// Restores the imported namespaces and aliases to an earlier [`Binder::import_scope`].
    pub fn restore_import_scope(&mut self, scope: (usize, usize)) {
        self.imported_namespaces.truncate(scope.0);
        self.aliases.truncate(scope.1);
    }

    /// The current type's namespace, if any, for unqualified type resolution.
    fn current_namespace(&self) -> Option<Box<str>> {
        match &self.current_type {
            Some(TypeSymbol::Named(parts)) if parts.len() > 1 => {
                Some(parts[..parts.len() - 1].join(".").into())
            }
            _ => None,
        }
    }

    /// The distinct in-scope namespaces (current, global, and imported) that hold
    /// a type with this name.
    fn type_namespaces_containing(&self, name: &str) -> Vec<Box<str>> {
        let mut search: Vec<Box<str>> = Vec::new();
        if let Some(TypeSymbol::Named(parts)) = &self.current_type {
            let mut enclosing = String::new();
            for part in parts.iter() {
                if !enclosing.is_empty() {
                    enclosing.push('.');
                }
                enclosing.push_str(part);
            }
            search.push(enclosing.into());
        }
        if let Some(current) = self.current_namespace() {
            search.push(current);
        }
        search.push(Box::from(""));
        search.extend(self.imported_namespaces.iter().cloned());
        let mut hits: Vec<Box<str>> = Vec::new();
        for namespace in search {
            if self.model.get(&namespace, name).is_some() && !hits.contains(&namespace) {
                hits.push(namespace);
            }
        }
        hits
    }

    /// Binds a method body end to end: the enclosing type is in scope for `this`
    /// and unqualified names, the parameters are declared as locals, and `return`
    /// statements are checked against `return_type` (15.9.4). Returns the bound
    /// body.
    pub fn bind_method(
        &mut self,
        enclosing_type: Option<TypeSymbol>,
        name: &str,
        return_type: TypeSymbol,
        parameters: &[(Box<str>, TypeSymbol)],
        body: &lamella_syntax::ast::Stmt,
    ) -> crate::statement::BoundStmt {
        let return_type = self.canonicalize(&return_type);
        let returns_value = !return_type.is_void();
        let body_span = body.span;
        self.current_type = enclosing_type;
        self.current_method = Some(MethodContext {
            name: name.into(),
            return_type,
        });
        self.enter_scope();
        self.case_label_uses.clear();
        self.loop_depth = 0;
        self.switch_depth = 0;
        for (parameter, ty) in parameters {
            self.declare_local(parameter, self.canonicalize(ty));
        }
        let bound = self.bind_statement(body);
        self.exit_scope();
        if returns_value && !crate::flow::always_exits(&bound) {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::NotAllPathsReturn {
                    method: name.into(),
                },
                body_span,
            ));
        }
        let parameter_names: Vec<Box<str>> = parameters
            .iter()
            .map(|(parameter, _)| parameter.clone())
            .collect();
        let unassigned =
            crate::flow::check_definite_assignment(&bound, &parameter_names, &self.model);
        self.diagnostics.extend(unassigned);
        self.diagnostics.extend(crate::flow::check_unused_locals(
            &bound,
            &self.case_label_uses,
        ));
        self.diagnostics
            .extend(crate::flow::check_unreachable(&bound));
        self.diagnostics.extend(crate::flow::check_labels(&bound));
        self.current_method = None;
        self.current_type = None;
        bound
    }

    /// Binds one REPL submission as the body of a `Submit$N(__Repl s)` method, in
    /// session mode: the enclosing type is `__Repl` and the implicit receiver is the
    /// parameter `receiver` (`s`), so an unqualified session variable reads/writes a
    /// field of `s` (14.5.2 against `s` rather than `this`). `initial_fields` maps each
    /// PRIOR session variable's source name to its stable field name + type (for reads);
    /// `occurrences` counts how many times each source name has already been declared (so
    /// a redefinition picks a fresh `x$2`). The introduced variables come back in
    /// [`SubmissionBinding::new_fields`] for the caller to commit.
    ///
    /// A TOP-LEVEL local declaration is not a real local: it is a persistent field, so
    /// `T x = init;` lowers to the field store `s.<stable> = init` (a declarator with no
    /// initializer just registers the field, which keeps its zero default), and a
    /// redefinition `T x = ...;` adds a fresh field `x$2` and rebinds source `x` to it.
    /// Every other statement -- including a declaration nested inside a block, an ordinary
    /// local of that block -- is bound normally. Diagnostics accumulate as usual; the
    /// caller drains them with [`Binder::into_diagnostics`].
    pub fn bind_submission(
        &mut self,
        repl_type: TypeSymbol,
        receiver: &str,
        statements: &[lamella_syntax::ast::Stmt],
        trailing: Option<&Expr>,
        initial_fields: BTreeMap<String, (Box<str>, TypeSymbol)>,
        mut occurrences: BTreeMap<String, u32>,
    ) -> SubmissionBinding {
        use crate::statement::{BoundStmt, BoundStmtKind};
        use lamella_syntax::ast::StmtKind;

        let body_span = statements
            .first()
            .map(|statement| statement.span)
            .or_else(|| trailing.map(|expr| expr.span))
            .unwrap_or(Span::empty_at(0));
        self.current_type = Some(repl_type.clone());
        self.session_receiver = Some(receiver.into());
        self.session_fields = initial_fields;
        self.current_method = Some(MethodContext {
            name: "Submit".into(),
            return_type: TypeSymbol::Special(SpecialType::Void),
        });
        self.enter_scope();
        self.case_label_uses.clear();
        self.loop_depth = 0;
        self.switch_depth = 0;

        let mut bound = Vec::new();
        let mut new_fields = Vec::new();
        for statement in statements {
            match &statement.kind {
                StmtKind::LocalDeclaration { ty, declarators } => {
                    let field_ty = self.resolve_named_type(&bind_type(ty), ty.span);
                    for declarator in declarators {
                        let source: &str = &declarator.name;
                        let value = declarator.initializer.as_ref().map(|initializer| {
                            let value = self.bind_expression(initializer);
                            self.check_assignable(&value, &field_ty, declarator.span);
                            self.convert(value, &field_ty)
                        });
                        let count = occurrences.get(source).copied().unwrap_or(0);
                        let stable: Box<str> = if count == 0 {
                            source.into()
                        } else {
                            format!("{source}${}", count + 1).into()
                        };
                        occurrences.insert(source.into(), count + 1);
                        self.session_fields
                            .insert(source.into(), (stable.clone(), field_ty.clone()));
                        new_fields.push(DeclaredField {
                            source: source.into(),
                            stable: stable.clone(),
                            ty: field_ty.clone(),
                        });
                        if let Some(value) = value {
                            let target =
                                self.session_field_access(receiver, &repl_type, &stable, &field_ty);
                            let assignment = BoundExpr {
                                ty: field_ty.clone(),
                                kind: BoundExprKind::Assignment {
                                    operator: AssignmentOperator::Assign,
                                    target: Box::new(target),
                                    value: Box::new(value),
                                },
                            };
                            bound.push(BoundStmt {
                                kind: BoundStmtKind::Expression(assignment),
                                span: declarator.span,
                            });
                        }
                    }
                }
                _ => bound.push(self.bind_statement(statement)),
            }
        }

        let mut return_type = TypeSymbol::Special(SpecialType::Void);
        if let Some(expr) = trailing {
            let value = self.bind_expression(expr);
            if value.ty.is_void() || value.ty.is_error() {
                bound.push(BoundStmt {
                    kind: BoundStmtKind::Expression(value),
                    span: expr.span,
                });
            } else {
                let object = TypeSymbol::Special(SpecialType::Object);
                let display = self.convert(value, &object);
                bound.push(BoundStmt {
                    kind: BoundStmtKind::Return(Some(display)),
                    span: expr.span,
                });
                return_type = object;
            }
        }

        self.exit_scope();
        self.session_receiver = None;
        self.session_fields = BTreeMap::new();
        self.current_type = None;
        self.current_method = None;
        SubmissionBinding {
            body: BoundStmt {
                kind: BoundStmtKind::Block(bound),
                span: body_span,
            },
            return_type,
            new_fields,
        }
    }

    /// A read/write of session field `name` of type `ty`, declared on `repl_type` and
    /// reached through the session receiver parameter `receiver` (`s`). The field is a
    /// public instance field of `__Repl`, so emission lowers it to `ldarg.0` (the `s`
    /// instance) + `ldfld`/`stfld` of the `<repl>.__Repl::name` reference.
    fn session_field_access(
        &self,
        receiver: &str,
        repl_type: &TypeSymbol,
        name: &str,
        ty: &TypeSymbol,
    ) -> BoundExpr {
        BoundExpr {
            ty: ty.clone(),
            kind: BoundExprKind::FieldAccess {
                receiver: Box::new(BoundExpr {
                    kind: BoundExprKind::Local(receiver.into()),
                    ty: repl_type.clone(),
                }),
                name: name.into(),
                field: Some(FieldReference {
                    declaring_type: repl_type.clone(),
                    name: name.into(),
                    ty: ty.clone(),
                    is_static: false,
                    accessibility: Accessibility::Public,
                    constant: None,
                }),
            },
        }
    }

    /// Binds a constructor initializer `: this(args)` / `: base(args)`: the arguments are
    /// bound in a scope with the constructor's parameters, then matched to a constructor
    /// of the sibling (`this`) or base type. Returns the target `.ctor` reference and the
    /// bound arguments, or `None` if it does not resolve.
    pub fn bind_constructor_chain(
        &mut self,
        enclosing: &TypeSymbol,
        parameters: &[(Box<str>, TypeSymbol)],
        initializer: &lamella_syntax::ast::ConstructorInitializer,
    ) -> Option<(MethodReference, Vec<BoundExpr>)> {
        self.current_type = Some(enclosing.clone());
        self.enter_scope();
        for (name, ty) in parameters {
            self.declare_local(name, ty.clone());
        }
        let arguments: Vec<BoundExpr> = initializer
            .arguments
            .iter()
            .map(|argument| self.bind_expression(argument))
            .collect();
        self.exit_scope();
        self.current_type = None;
        let target = match initializer.kind {
            lamella_syntax::ast::ConstructorInitializerKind::This => enclosing.clone(),
            lamella_syntax::ast::ConstructorInitializerKind::Base => {
                self.type_info_of(enclosing)?.base.clone()?
            }
        };
        let constructors = self.type_info_of(&target)?.constructors.clone();
        let argument_types: Vec<TypeSymbol> =
            arguments.iter().map(|argument| argument.ty.clone()).collect();
        let arg_constants: Vec<Option<i64>> =
            arguments.iter().map(constant_int_value).collect();
        let chosen =
            match resolve_overload(&self.model, &constructors, &argument_types, &arg_constants) {
            OverloadResult::Resolved(method) => method,
            _ => return None,
        };
        Some((
            MethodReference {
                declaring_type: target,
                name: ".ctor".into(),
                parameters: chosen.parameters,
                return_type: TypeSymbol::Special(SpecialType::Void),
                is_static: false,
            },
            arguments,
        ))
    }

    /// Binds a field initializer in `enclosing`'s context and checks it converts
    /// to the field's type (`CS0029`).
    pub fn bind_field_initializer(
        &mut self,
        enclosing: TypeSymbol,
        field_type: &TypeSymbol,
        initializer: &Expr,
    ) {
        self.current_type = Some(enclosing);
        self.enter_scope();
        let value = self.bind_expression(initializer);
        self.check_assignable(&value, field_type, initializer.span);
        self.exit_scope();
        self.current_type = None;
    }

    /// Checks a `return` statement against the enclosing method's return type
    /// (15.9.4): `CS0127` for a value in a `void` method, `CS0126` for a missing
    /// value, `CS0029` for a value that does not convert.
    pub(crate) fn check_return(&mut self, value: Option<&BoundExpr>, span: Span) {
        let Some(method) = self.current_method.clone() else {
            return;
        };
        if method.return_type.is_void() {
            if value.is_some_and(|expr| !expr.ty.is_error()) {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::ReturnValueInVoidMethod {
                        method: method.name,
                    },
                    span,
                ));
            }
        } else {
            match value {
                None => self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::ReturnValueRequired {
                        ty: method.return_type.to_string().into(),
                    },
                    span,
                )),
                Some(expr) => self.check_assignable(expr, &method.return_type, span),
            }
        }
    }

    /// Opens a nested scope (a block or method body).
    pub fn enter_scope(&mut self) {
        self.scopes.push(BTreeMap::new());
    }

    /// Closes the innermost scope.
    pub fn exit_scope(&mut self) {
        self.scopes.pop();
    }

    /// Enters / leaves a loop body, so `break`/`continue` know they have an enclosing loop.
    pub(crate) fn enter_loop(&mut self) {
        self.loop_depth += 1;
    }
    pub(crate) fn exit_loop(&mut self) {
        self.loop_depth = self.loop_depth.saturating_sub(1);
    }
    /// Enters / leaves a `switch`, so `break` knows it has an enclosing switch.
    pub(crate) fn enter_switch(&mut self) {
        self.switch_depth += 1;
    }
    pub(crate) fn exit_switch(&mut self) {
        self.switch_depth = self.switch_depth.saturating_sub(1);
    }
    /// Whether a `continue` is valid here (inside a loop).
    pub(crate) fn in_loop(&self) -> bool {
        self.loop_depth > 0
    }
    /// Whether a `break` is valid here (inside a loop or a switch).
    pub(crate) fn in_loop_or_switch(&self) -> bool {
        self.loop_depth > 0 || self.switch_depth > 0
    }

    /// Declares a local variable or parameter in the innermost scope.
    pub fn declare_local(&mut self, name: &str, ty: TypeSymbol) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.into(), ty);
        }
    }

    /// Looks a name up through the scope stack, innermost first.
    fn lookup_local(&self, name: &str) -> Option<&TypeSymbol> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    /// Whether a local of this name is already declared in the innermost scope: a
    /// redeclaration (CS0128).
    pub(crate) fn local_in_current_scope(&self, name: &str) -> bool {
        self.scopes
            .last()
            .is_some_and(|scope| scope.contains_key(name))
    }

    /// Whether a local of this name is declared in an enclosing (not innermost)
    /// scope, which a new local would shadow (CS0136).
    pub(crate) fn local_in_enclosing_scope(&self, name: &str) -> bool {
        let innermost = self.scopes.len().saturating_sub(1);
        self.scopes[..innermost]
            .iter()
            .any(|scope| scope.contains_key(name))
    }

    /// Binds an expression (14).
    pub fn bind_expression(&mut self, expr: &Expr) -> BoundExpr {
        match &expr.kind {
            ExprKind::Literal(literal) => BoundExpr {
                kind: BoundExprKind::Literal(literal.clone()),
                ty: literal_type(literal),
            },
            ExprKind::Name(name) => self.bind_name(name, expr.span),
            ExprKind::This => self.this_expr(),
            ExprKind::Base => self.base_expr(),
            ExprKind::MemberAccess { receiver, name } => {
                self.bind_member_access(receiver, name, expr.span)
            }
            ExprKind::Invocation {
                receiver,
                arguments,
            } => self.bind_invocation(receiver, arguments, expr.span),
            ExprKind::ElementAccess {
                receiver,
                arguments,
            } => self.bind_element_access(receiver, arguments, expr.span),
            ExprKind::ObjectCreation { target, arguments } => {
                self.bind_object_creation(target, arguments, expr.span)
            }
            ExprKind::ArrayCreation {
                element,
                lengths,
                rank,
                extra_ranks,
                initializer,
            } => {
                let lengths = lengths
                    .iter()
                    .map(|length| self.bind_expression(length))
                    .collect();
                let mut ty = self.resolve_named_type(&bind_type(element), element.span);
                if !ty.is_error() {
                    for &extra in extra_ranks.iter().rev() {
                        ty = ty.into_array(extra);
                    }
                    ty = ty.into_array(*rank);
                }
                let elements = initializer
                    .as_ref()
                    .map(|init| self.bind_array_initializer(init, &ty))
                    .unwrap_or_default();
                BoundExpr {
                    kind: BoundExprKind::ArrayCreation { lengths, elements },
                    ty,
                }
            }
            ExprKind::Binary {
                operator,
                left,
                right,
            } => self.bind_binary(*operator, left, right, expr.span),
            ExprKind::Unary { operator, operand } => self.bind_unary(*operator, operand, expr.span),
            ExprKind::RefArgument { out, operand } => {
                let operand = self.bind_expression(operand);
                let ty = operand.ty.clone();
                BoundExpr {
                    kind: BoundExprKind::Ref {
                        out: *out,
                        operand: Box::new(operand),
                    },
                    ty,
                }
            }
            ExprKind::PostfixUnary { operator, operand } => {
                self.bind_postfix(*operator, operand, expr.span)
            }
            ExprKind::Cast { target, operand } => {
                let operand = self.bind_expression(operand);
                let ty = self.resolve_named_type(&bind_type(target), target.span);
                if !operand.ty.is_error() && !ty.is_error() {
                    if let Some(method) = self
                        .user_conversion(&operand.ty, &ty, "op_Explicit")
                        .or_else(|| self.user_conversion(&operand.ty, &ty, "op_Implicit"))
                    {
                        return BoundExpr {
                            ty,
                            kind: BoundExprKind::Call {
                                callee: Box::new(error_expr()),
                                arguments: alloc::vec![operand],
                                method: Some(method),
                            },
                        };
                    }
                    if !can_cast(&self.model, &operand.ty, &ty) {
                        self.diagnostics.push(Diagnostic::new(
                            DiagnosticKind::CannotCast {
                                from: operand.ty.to_string().into(),
                                to: ty.to_string().into(),
                            },
                            target.span,
                        ));
                    }
                }
                BoundExpr {
                    kind: BoundExprKind::Cast {
                        operand: Box::new(operand),
                        checked: self.checked_context,
                    },
                    ty,
                }
            }
            ExprKind::TypeTest {
                operation,
                operand,
                target,
            } => {
                let operand = self.bind_expression(operand);
                let target = self.resolve_named_type(&bind_type(target), target.span);
                let ty = match operation {
                    TypeTestOperation::Is => TypeSymbol::Special(SpecialType::Boolean),
                    TypeTestOperation::As => target.clone(),
                };
                BoundExpr {
                    kind: BoundExprKind::TypeTest {
                        operation: *operation,
                        operand: Box::new(operand),
                        target,
                    },
                    ty,
                }
            }
            ExprKind::TypeOf(target) => {
                let target_ty = self.resolve_named_type(&bind_type(target), target.span);
                BoundExpr {
                    kind: BoundExprKind::TypeOf(target_ty),
                    ty: system_type(),
                }
            }
            ExprKind::SizeOf(target) => {
                let target_ty = self.resolve_named_type(&bind_type(target), target.span);
                BoundExpr {
                    kind: BoundExprKind::SizeOf(target_ty),
                    ty: TypeSymbol::Special(SpecialType::Int32),
                }
            }
            ExprKind::StackAlloc { element, count } => {
                let element_ty = self.resolve_named_type(&bind_type(element), element.span);
                let count = self.bind_expression(count);
                BoundExpr {
                    ty: TypeSymbol::Pointer(Box::new(element_ty.clone())),
                    kind: BoundExprKind::StackAlloc {
                        element: element_ty,
                        count: Box::new(count),
                    },
                }
            }
            ExprKind::Dereference(operand) => {
                let pointer = self.bind_expression(operand);
                let ty = match &pointer.ty {
                    TypeSymbol::Pointer(element) => (**element).clone(),
                    _ => TypeSymbol::Error,
                };
                BoundExpr {
                    kind: BoundExprKind::Dereference {
                        operand: Box::new(pointer),
                    },
                    ty,
                }
            }
            ExprKind::Checked(inner) => {
                let saved = self.checked_context;
                self.checked_context = true;
                let inner = self.bind_expression(inner);
                self.checked_context = saved;
                let ty = inner.ty.clone();
                BoundExpr {
                    kind: BoundExprKind::Checked(Box::new(inner)),
                    ty,
                }
            }
            ExprKind::Unchecked(inner) => {
                let saved = self.checked_context;
                self.checked_context = false;
                let inner = self.bind_expression(inner);
                self.checked_context = saved;
                let ty = inner.ty.clone();
                BoundExpr {
                    kind: BoundExprKind::Unchecked(Box::new(inner)),
                    ty,
                }
            }
            ExprKind::Conditional {
                condition,
                when_true,
                when_false,
            } => self.bind_conditional(condition, when_true, when_false),
            ExprKind::Assignment {
                operator,
                target,
                value,
            } => self.bind_assignment(*operator, target, value, expr.span),
            ExprKind::PredefinedType(predefined) => {
                let ty = TypeSymbol::Special(SpecialType::from_predefined(*predefined));
                BoundExpr {
                    kind: BoundExprKind::TypeReference(ty.clone()),
                    ty,
                }
            }
            ExprKind::Parenthesized(inner) => self.bind_expression(inner),
            _ => BoundExpr {
                kind: BoundExprKind::Error,
                ty: TypeSymbol::Error,
            },
        }
    }

    fn bind_binary(
        &mut self,
        operator: BinaryOperator,
        left_expr: &Expr,
        right_expr: &Expr,
        span: Span,
    ) -> BoundExpr {
        let left = self.bind_expression(left_expr);
        let right = self.bind_expression(right_expr);
        if matches!(operator, BinaryOperator::Equal | BinaryOperator::NotEqual)
            && !left.ty.is_error()
            && !right.ty.is_error()
            && self
                .type_info_of(&left.ty)
                .is_some_and(|info| info.kind == TypeKind::Struct)
        {
            if let Some(call) = self.bind_user_binary_operator(operator, &left, &right) {
                return call;
            }
        }
        let ty = if left.ty.is_error() || right.ty.is_error() {
            TypeSymbol::Error
        } else if let Some(result) = self.enum_binary_result(operator, &left.ty, &right.ty) {
            result
        } else if let Some(result) = pointer_binary_result(operator, &left.ty, &right.ty) {
            result
        } else if let Some(result) = self.null_equality_result(operator, &left.ty, &right.ty) {
            result
        } else if let Some(result) = binary_result_type(operator, &left.ty, &right.ty) {
            result
        } else {
            if let Some(call) = self.bind_user_binary_operator(operator, &left, &right) {
                return call;
            }
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::OperatorNotApplicable {
                    operator: operator_symbol(operator).into(),
                    left: left.ty.to_string().into(),
                    right: right.ty.to_string().into(),
                },
                span,
            ));
            TypeSymbol::Error
        };
        let (left, right) = if matches!(operator, BinaryOperator::Add)
            && matches!(ty, TypeSymbol::Special(SpecialType::String))
        {
            (self.to_concat_operand(left), self.to_concat_operand(right))
        } else {
            (left, right)
        };
        BoundExpr {
            kind: BoundExprKind::Binary {
                operator,
                left: Box::new(left),
                right: Box::new(right),
                checked: self.checked_context,
            },
            ty,
        }
    }

    /// A string-concatenation operand in `String.Concat` argument form: a string stays a
    /// string; any other type becomes `object` (a value type boxes), so concatenation
    /// uses the `Concat(object, object)` overload and calls the operand's `ToString`.
    fn to_concat_operand(&self, operand: BoundExpr) -> BoundExpr {
        if matches!(operand.ty, TypeSymbol::Special(SpecialType::String)) {
            operand
        } else {
            self.convert(operand, &TypeSymbol::Special(SpecialType::Object))
        }
    }

    /// Resolves a binary operator to a user-defined `op_*` method on either operand's
    /// type, as a static call -- the lowering of `a + b` for overloaded operators.
    fn bind_user_binary_operator(
        &mut self,
        operator: BinaryOperator,
        left: &BoundExpr,
        right: &BoundExpr,
    ) -> Option<BoundExpr> {
        let name = operator.overload_method_name()?;
        let argument_types = [left.ty.clone(), right.ty.clone()];
        for owner in [&left.ty, &right.ty] {
            let candidates = self.methods_in_chain(owner, name);
            if let OverloadResult::Resolved(method) =
                resolve_overload(&self.model, &candidates, &argument_types, &[])
            {
                let declaring_type = self.declaring_type_in_chain(owner, name, &method.parameters);
                return Some(BoundExpr {
                    ty: method.return_type.clone(),
                    kind: BoundExprKind::Call {
                        callee: Box::new(error_expr()),
                        arguments: alloc::vec![left.clone(), right.clone()],
                        method: Some(MethodReference {
                            declaring_type,
                            name: name.into(),
                            parameters: method.parameters,
                            return_type: method.return_type,
                            is_static: true,
                        }),
                    },
                });
            }
        }
        None
    }

    /// Whether `ty` is an enum type declared in the model.
    fn is_enum_type(&self, ty: &TypeSymbol) -> bool {
        self.type_info_of(ty)
            .is_some_and(|info| info.kind == TypeKind::Enum)
    }

    /// The result of a binary operator on enum operands of the same type (14.7):
    /// the bitwise operators yield the enum; the relational operators yield `bool`.
    /// `==`/`!=` fall through to the general path. `None` if it does not apply.
    fn is_enum_type_pair(&self, left: &TypeSymbol, right: &TypeSymbol) -> bool {
        left == right && self.is_enum_type(left)
    }

    fn enum_binary_result(
        &self,
        operator: BinaryOperator,
        left: &TypeSymbol,
        right: &TypeSymbol,
    ) -> Option<TypeSymbol> {
        use BinaryOperator as Op;
        if !self.is_enum_type_pair(left, right) {
            return None;
        }
        match operator {
            Op::BitwiseAnd | Op::BitwiseOr | Op::BitwiseXor => Some(left.clone()),
            Op::LessThan | Op::GreaterThan | Op::LessThanOrEqual | Op::GreaterThanOrEqual => {
                Some(TypeSymbol::Special(SpecialType::Boolean))
            }
            _ => None,
        }
    }

    fn bind_unary(
        &mut self,
        operator: UnaryOperator,
        operand_expr: &Expr,
        span: Span,
    ) -> BoundExpr {
        let operand = self.bind_expression(operand_expr);
        let ty = if operand.ty.is_error() {
            TypeSymbol::Error
        } else if operator == UnaryOperator::Complement && self.is_enum_type(&operand.ty) {
            operand.ty.clone()
        } else if let Some(result) = unary_result_type(operator, &operand.ty) {
            result
        } else if matches!(
            operator,
            UnaryOperator::PreIncrement | UnaryOperator::PreDecrement
        ) && self.has_step_operator(
            &operand.ty,
            if operator == UnaryOperator::PreIncrement {
                "op_Increment"
            } else {
                "op_Decrement"
            },
        ) {
            operand.ty.clone()
        } else {
            if let Some(call) = self.bind_user_unary_operator(operator, &operand) {
                return call;
            }
            self.report_unary(unary_operator_symbol(operator), &operand.ty, span);
            TypeSymbol::Error
        };
        BoundExpr {
            kind: BoundExprKind::Unary {
                operator,
                operand: Box::new(operand),
            },
            ty,
        }
    }

    /// Resolves a unary operator to a user-defined `op_*` method on the operand's type,
    /// as a static call.
    fn bind_user_unary_operator(
        &mut self,
        operator: UnaryOperator,
        operand: &BoundExpr,
    ) -> Option<BoundExpr> {
        let name = operator.overload_method_name()?;
        let argument_types = [operand.ty.clone()];
        let candidates = self.methods_in_chain(&operand.ty, name);
        if let OverloadResult::Resolved(method) =
            resolve_overload(&self.model, &candidates, &argument_types, &[])
        {
            let declaring_type = self.declaring_type_in_chain(&operand.ty, name, &method.parameters);
            return Some(BoundExpr {
                ty: method.return_type.clone(),
                kind: BoundExprKind::Call {
                    callee: Box::new(error_expr()),
                    arguments: alloc::vec![operand.clone()],
                    method: Some(MethodReference {
                        declaring_type,
                        name: name.into(),
                        parameters: method.parameters,
                        return_type: method.return_type,
                        is_static: true,
                    }),
                },
            });
        }
        None
    }

    fn bind_postfix(
        &mut self,
        operator: PostfixOperator,
        operand_expr: &Expr,
        span: Span,
    ) -> BoundExpr {
        let operand = self.bind_expression(operand_expr);
        let step = match operator {
            PostfixOperator::Increment => "op_Increment",
            PostfixOperator::Decrement => "op_Decrement",
        };
        let ty = if operand.ty.is_error() {
            TypeSymbol::Error
        } else if as_special(&operand.ty).is_some_and(SpecialType::is_numeric)
            || self.has_step_operator(&operand.ty, step)
        {
            operand.ty.clone()
        } else {
            let symbol = match operator {
                PostfixOperator::Increment => "++",
                PostfixOperator::Decrement => "--",
            };
            self.report_unary(symbol, &operand.ty, span);
            TypeSymbol::Error
        };
        BoundExpr {
            kind: BoundExprKind::Postfix {
                operator,
                operand: Box::new(operand),
            },
            ty,
        }
    }

    /// Whether `ty` declares a user-defined `op_Increment`/`op_Decrement` (named by
    /// `step`) -- a static method taking and returning `ty`.
    fn has_step_operator(&self, ty: &TypeSymbol, step: &str) -> bool {
        self.methods_in_chain(ty, step).iter().any(|method| {
            method.parameters.len() == 1
                && &method.parameters[0] == ty
                && &method.return_type == ty
        })
    }

    fn report_unary(&mut self, operator: &str, operand: &TypeSymbol, span: Span) {
        self.diagnostics.push(Diagnostic::new(
            DiagnosticKind::UnaryOperatorNotApplicable {
                operator: operator.into(),
                operand: operand.to_string().into(),
            },
            span,
        ));
    }

    fn bind_conditional(
        &mut self,
        condition: &Expr,
        when_true: &Expr,
        when_false: &Expr,
    ) -> BoundExpr {
        let condition_span = condition.span;
        let condition = self.bind_expression(condition);
        let boolean = TypeSymbol::Special(SpecialType::Boolean);
        if !condition.ty.is_error() && !self.converts(&condition.ty, &boolean) {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::NoImplicitConversion {
                    from: condition.ty.to_string().into(),
                    to: "bool".into(),
                },
                condition_span,
            ));
        }
        let span = when_false.span;
        let when_true = self.bind_expression(when_true);
        let when_false = self.bind_expression(when_false);
        let ty = if when_true.ty.is_error() || when_false.ty.is_error() {
            TypeSymbol::Error
        } else if let Some(common) =
            conditional_result_type(&self.model, &when_true.ty, &when_false.ty)
        {
            common
        } else {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::ConditionalTypeMismatch {
                    left: when_true.ty.to_string().into(),
                    right: when_false.ty.to_string().into(),
                },
                span,
            ));
            TypeSymbol::Error
        };
        BoundExpr {
            kind: BoundExprKind::Conditional {
                condition: Box::new(condition),
                when_true: Box::new(when_true),
                when_false: Box::new(when_false),
            },
            ty,
        }
    }

    /// Lowers an event subscription `receiver.E += h` (or `-=`) from outside the declaring
    /// type to a call of the event's `add_E`/`remove_E` accessor (17.7), the handler
    /// converted to the event's delegate type.
    fn bind_event_subscription(
        &mut self,
        receiver: BoundExpr,
        event: &EventSymbol,
        declaring: &TypeSymbol,
        operator: AssignmentOperator,
        value_expr: &Expr,
    ) -> BoundExpr {
        let value = self.bind_expression(value_expr);
        let handler = self.convert(value, &event.ty);
        let prefix = if matches!(operator, AssignmentOperator::Add) {
            "add_"
        } else {
            "remove_"
        };
        let mut accessor = String::from(prefix);
        accessor.push_str(&event.name);
        let void = TypeSymbol::Special(SpecialType::Void);
        let method = MethodReference {
            declaring_type: declaring.clone(),
            name: accessor.clone().into(),
            parameters: alloc::vec![event.ty.clone()],
            return_type: void.clone(),
            is_static: event.is_static,
        };
        let callee = BoundExpr {
            ty: TypeSymbol::Error,
            kind: BoundExprKind::MethodGroup {
                receiver: Box::new(receiver),
                name: accessor.into(),
            },
        };
        BoundExpr {
            kind: BoundExprKind::Call {
                callee: Box::new(callee),
                arguments: alloc::vec![handler],
                method: Some(method),
            },
            ty: void,
        }
    }

    fn bind_assignment(
        &mut self,
        operator: AssignmentOperator,
        target_expr: &Expr,
        value_expr: &Expr,
        span: Span,
    ) -> BoundExpr {
        let target_span = target_expr.span;
        if operator == AssignmentOperator::Assign {
            if let ExprKind::ElementAccess {
                receiver,
                arguments,
            } = &target_expr.kind
            {
                let bound_receiver = self.bind_expression(receiver);
                let is_indexer = !bound_receiver.ty.is_error()
                    && !matches!(
                        bound_receiver.ty,
                        TypeSymbol::Array { .. } | TypeSymbol::Special(SpecialType::String)
                    )
                    && !self
                        .methods_in_chain(&bound_receiver.ty, "set_Item")
                        .is_empty();
                if is_indexer {
                    let mut args: Vec<BoundExpr> = arguments
                        .iter()
                        .map(|argument| self.bind_expression(argument))
                        .collect();
                    args.push(self.bind_expression(value_expr));
                    return self
                        .bind_indexer_call(bound_receiver, "set_Item", args, span)
                        .unwrap_or_else(error_expr);
                }
            }
        }
        if matches!(
            operator,
            AssignmentOperator::Add | AssignmentOperator::Subtract
        ) {
            if let ExprKind::MemberAccess { receiver, name } = &target_expr.kind {
                let bound_receiver = self.bind_expression(receiver);
                if let Some((event, declaring)) = self.event_declaration(&bound_receiver.ty, name) {
                    if self.outside_event_declarer(&declaring) {
                        return self.bind_event_subscription(
                            bound_receiver,
                            &event,
                            &declaring,
                            operator,
                            value_expr,
                        );
                    }
                }
            }
        }
        let target = self.bind_expression(target_expr);
        if let BoundExprKind::FieldAccess {
            field: Some(field), ..
        } = &target.kind
        {
            let in_constructor = matches!(
                self.current_method.as_ref(),
                Some(context) if &*context.name == ".ctor" || &*context.name == ".cctor"
            );
            if !in_constructor && self.field_is_readonly(&field.declaring_type, &field.name) {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::ReadonlyAssignment {
                        field: field.name.clone(),
                    },
                    target_span,
                ));
            }
        }
        let mut value = self.bind_expression(value_expr);
        if matches!(
            operator,
            AssignmentOperator::Add | AssignmentOperator::Subtract
        ) && self
            .type_info_of(&target.ty)
            .is_some_and(|info| info.kind == TypeKind::Delegate)
            && (matches!(value.kind, BoundExprKind::MethodGroup { .. })
                || self
                    .type_info_of(&value.ty)
                    .is_some_and(|info| info.kind == TypeKind::Delegate))
        {
            let delegate_ty = target.ty.clone();
            let delegate_base =
                TypeSymbol::Named([Box::from("System"), Box::from("Delegate")].into());
            let accessor = if matches!(operator, AssignmentOperator::Add) {
                "Combine"
            } else {
                "Remove"
            };
            let operand = self.convert(value, &delegate_ty);
            let method = MethodReference {
                declaring_type: delegate_base.clone(),
                name: accessor.into(),
                parameters: alloc::vec![delegate_base.clone(), delegate_base.clone()],
                return_type: delegate_base.clone(),
                is_static: true,
            };
            let callee = BoundExpr {
                ty: TypeSymbol::Error,
                kind: BoundExprKind::MethodGroup {
                    receiver: Box::new(BoundExpr {
                        ty: delegate_base.clone(),
                        kind: BoundExprKind::TypeReference(delegate_base.clone()),
                    }),
                    name: accessor.into(),
                },
            };
            let combine = BoundExpr {
                kind: BoundExprKind::Call {
                    callee: Box::new(callee),
                    arguments: alloc::vec![target.clone(), operand],
                    method: Some(method),
                },
                ty: delegate_base,
            };
            let cast = BoundExpr {
                kind: BoundExprKind::Cast {
                    operand: Box::new(combine),
                    checked: false,
                },
                ty: delegate_ty.clone(),
            };
            return BoundExpr {
                kind: BoundExprKind::Assignment {
                    operator: AssignmentOperator::Assign,
                    target: Box::new(target),
                    value: Box::new(cast),
                },
                ty: delegate_ty,
            };
        }
        if !target.ty.is_error() && !is_lvalue(&target) {
            self.diagnostics
                .push(Diagnostic::new(DiagnosticKind::NotAssignable, target_span));
        } else if !target.ty.is_error() && !value.ty.is_error() {
            self.check_assignment(operator, &target.ty, &value, span);
            if matches!(operator, AssignmentOperator::Add)
                && matches!(target.ty, TypeSymbol::Special(SpecialType::String))
            {
                value = self.to_concat_operand(value);
            }
        }
        if !target.ty.is_error() && matches!(operator, AssignmentOperator::Assign) {
            value = self.convert(value, &target.ty);
        }
        let ty = target.ty.clone();
        BoundExpr {
            kind: BoundExprKind::Assignment {
                operator,
                target: Box::new(target),
                value: Box::new(value),
            },
            ty,
        }
    }

    fn check_assignment(
        &mut self,
        operator: AssignmentOperator,
        target: &TypeSymbol,
        value: &BoundExpr,
        span: Span,
    ) {
        match compound_binary_operator(operator) {
            None => {
                self.check_assignable(value, target, span);
            }
            Some(binary) => {
                if binary_result_type(binary, target, &value.ty).is_none() {
                    self.diagnostics.push(Diagnostic::new(
                        DiagnosticKind::OperatorNotApplicable {
                            operator: assignment_symbol(operator).into(),
                            left: target.to_string().into(),
                            right: value.ty.to_string().into(),
                        },
                        span,
                    ));
                }
            }
        }
    }

    fn bind_member_access(&mut self, receiver_expr: &Expr, name: &str, span: Span) -> BoundExpr {
        let receiver = self.bind_expression(receiver_expr);
        if let BoundExprKind::NamespaceReference(namespace) = &receiver.kind {
            let namespace = namespace.clone();
            return self.bind_qualified_name(&namespace, name, span);
        }
        if receiver.ty.is_error() {
            return error_expr();
        }
        if let BoundExprKind::TypeReference(TypeSymbol::Special(special)) = &receiver.kind {
            let special = *special;
            if let Some(constant) = predefined_constant(special, name) {
                let ty = TypeSymbol::Special(special);
                return BoundExpr {
                    kind: BoundExprKind::FieldAccess {
                        receiver: Box::new(receiver),
                        name: name.into(),
                        field: Some(FieldReference {
                            declaring_type: ty.clone(),
                            name: name.into(),
                            ty: ty.clone(),
                            is_static: true,
                            accessibility: Accessibility::Public,
                            constant: Some(integer_literal(constant)),
                        }),
                    },
                    ty,
                };
            }
        }
        if let Some((_, declaring)) = self.event_declaration(&receiver.ty, name) {
            if self.outside_event_declarer(&declaring) {
                self.report(Diagnostic::new(
                    DiagnosticKind::EventOutsideAddRemove { event: name.into() },
                    span,
                ));
                return error_expr();
            }
        }
        let receiver_kind = receiver_category(&receiver);
        match self.resolve_member(&receiver.ty, name) {
            MemberResolution::Field(field) => {
                self.check_accessible(&field.declaring_type, field.accessibility, name, span);
                self.check_static_instance(
                    receiver_kind,
                    field.is_static,
                    &field.declaring_type,
                    name,
                    span,
                );
                BoundExpr {
                    ty: field.ty.clone(),
                    kind: BoundExprKind::FieldAccess {
                        receiver: Box::new(receiver),
                        name: name.into(),
                        field: Some(field),
                    },
                }
            }
            MemberResolution::Property {
                declaring_type,
                ty,
                accessibility,
                is_static,
            } => {
                self.check_accessible(&declaring_type, accessibility, name, span);
                self.check_static_instance(receiver_kind, is_static, &declaring_type, name, span);
                BoundExpr {
                    kind: BoundExprKind::PropertyAccess {
                        receiver: Box::new(receiver),
                        declaring_type,
                        name: name.into(),
                    },
                    ty,
                }
            }
            MemberResolution::MethodGroup => BoundExpr {
                kind: BoundExprKind::MethodGroup {
                    receiver: Box::new(receiver),
                    name: name.into(),
                },
                ty: TypeSymbol::Error,
            },
            MemberResolution::NoSuchMember(type_name) => {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::MemberNotFound {
                        type_name: type_name.into(),
                        member: name.into(),
                    },
                    span,
                ));
                error_expr()
            }
            MemberResolution::Unknown => error_expr(),
        }
    }

    fn bind_invocation(
        &mut self,
        receiver_expr: &Expr,
        argument_exprs: &[Expr],
        span: Span,
    ) -> BoundExpr {
        let callee = self.bind_expression(receiver_expr);
        let callee = if self.is_delegate_value(&callee) {
            BoundExpr {
                ty: TypeSymbol::Error,
                kind: BoundExprKind::MethodGroup {
                    receiver: Box::new(callee),
                    name: "Invoke".into(),
                },
            }
        } else {
            callee
        };
        let arguments: Vec<BoundExpr> = argument_exprs
            .iter()
            .map(|argument| self.bind_expression(argument))
            .collect();
        let group = match &callee.kind {
            BoundExprKind::MethodGroup { receiver, name } => {
                Some((receiver.ty.clone(), name.clone()))
            }
            _ => None,
        };
        let receiver_kind = match &callee.kind {
            BoundExprKind::MethodGroup { receiver, .. } => Some(receiver_category(receiver)),
            _ => None,
        };
        let mut params_method = false;
        let has_method_group = arguments
            .iter()
            .any(|argument| matches!(argument.kind, BoundExprKind::MethodGroup { .. }));
        let real_error = arguments.iter().any(|argument| {
            argument.ty.is_error() && !matches!(argument.kind, BoundExprKind::MethodGroup { .. })
        });
        let mut resolved = match group {
            Some((receiver_ty, name)) if !real_error => {
                let candidates = self.methods_in_chain(&receiver_ty, &name);
                let chosen = if has_method_group {
                    self.resolve_with_method_groups(&name, &receiver_ty, &candidates, &arguments, span)
                } else {
                    let argument_types: Vec<TypeSymbol> = arguments
                        .iter()
                        .map(|argument| argument.ty.clone())
                        .collect();
                    let arg_constants: Vec<Option<i64>> =
                        arguments.iter().map(constant_int_value).collect();
                    self.resolve_call(
                        &name,
                        &receiver_ty,
                        &candidates,
                        &argument_types,
                        &arg_constants,
                        span,
                    )
                };
                chosen.map(|method| {
                    params_method = method.is_params;
                    let declaring_type =
                        self.declaring_type_in_chain(&receiver_ty, &method.name, &method.parameters);
                    MethodReference {
                        declaring_type,
                        name: method.name,
                        parameters: method.parameters,
                        return_type: method.return_type,
                        is_static: method.is_static,
                    }
                })
            }
            _ => None,
        };
        if resolved.is_none() && !arguments.iter().any(|argument| argument.ty.is_error()) {
            if let Some(invoke) = self
                .type_info_of(&callee.ty)
                .filter(|info| info.kind == TypeKind::Delegate)
                .and_then(|info| info.methods.iter().find(|m| &*m.name == "Invoke").cloned())
            {
                resolved = Some(MethodReference {
                    declaring_type: callee.ty.clone(),
                    name: "Invoke".into(),
                    parameters: invoke.parameters,
                    return_type: invoke.return_type,
                    is_static: false,
                });
            }
        }
        if let (Some(kind), Some(method)) = (receiver_kind, &resolved) {
            self.check_static_instance(
                kind,
                method.is_static,
                &method.declaring_type,
                &method.name,
                span,
            );
        }
        let arguments = match resolved.as_ref() {
            Some(method) if params_method => self.bind_params_arguments(method, arguments),
            Some(method) if method.parameters.len() == arguments.len() => arguments
                .into_iter()
                .zip(method.parameters.iter())
                .map(|(argument, parameter)| self.convert(argument, parameter))
                .collect(),
            _ => arguments,
        };
        let ty = resolved
            .as_ref()
            .map_or(TypeSymbol::Error, |method| method.return_type.clone());
        BoundExpr {
            kind: BoundExprKind::Call {
                callee: Box::new(callee),
                arguments,
                method: resolved,
            },
            ty,
        }
    }

    /// Binds the arguments of a call to a `params` method: an array supplied directly
    /// (normal form) converts 1:1; otherwise the trailing arguments are wrapped into a
    /// new array of the element type (expanded form).
    fn bind_params_arguments(
        &mut self,
        method: &MethodReference,
        arguments: Vec<BoundExpr>,
    ) -> Vec<BoundExpr> {
        let param_count = method.parameters.len();
        let fixed = param_count.saturating_sub(1);
        let array_ty = method.parameters[fixed].clone();
        if arguments.len() == param_count && self.converts(&arguments[fixed].ty, &array_ty) {
            return arguments
                .into_iter()
                .zip(method.parameters.iter())
                .map(|(argument, parameter)| self.convert(argument, parameter))
                .collect();
        }
        let element_ty = match &array_ty {
            TypeSymbol::Array { element, .. } => (**element).clone(),
            _ => TypeSymbol::Error,
        };
        let mut bound = Vec::with_capacity(param_count);
        let mut remaining = arguments.into_iter();
        for parameter in &method.parameters[..fixed] {
            if let Some(argument) = remaining.next() {
                bound.push(self.convert(argument, parameter));
            }
        }
        let elements: Vec<BoundExpr> = remaining
            .map(|argument| self.convert(argument, &element_ty))
            .collect();
        bound.push(BoundExpr {
            kind: BoundExprKind::ArrayCreation {
                lengths: Vec::new(),
                elements,
            },
            ty: array_ty,
        });
        bound
    }

    /// Resolves a call to a method group by overload resolution (14.4.2),
    /// reporting the appropriate diagnostic and returning the chosen method.
    fn resolve_call(
        &mut self,
        name: &str,
        declaring: &TypeSymbol,
        candidates: &[MethodSymbol],
        argument_types: &[TypeSymbol],
        arg_constants: &[Option<i64>],
        span: Span,
    ) -> Option<MethodSymbol> {
        match resolve_overload(&self.model, candidates, argument_types, arg_constants) {
            OverloadResult::Resolved(method) => {
                self.check_accessible(declaring, method.accessibility, name, span);
                Some(method)
            }
            OverloadResult::Ambiguous => {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::AmbiguousCall {
                        method: name.into(),
                    },
                    span,
                ));
                None
            }
            OverloadResult::WrongArgumentCount => {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::NoOverloadForArgumentCount {
                        method: name.into(),
                        count: argument_types.len() as u32,
                    },
                    span,
                ));
                None
            }
            OverloadResult::BadArgument { index, from, to } => {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::ArgumentConversion {
                        index: index as u32 + 1,
                        from: from.to_string().into(),
                        to: to.to_string().into(),
                    },
                    span,
                ));
                None
            }
        }
    }

    /// Resolves a call/constructor whose arguments include a method group (which has no
    /// type on its own, so it is excluded from the type-based [`resolve_call`]). A candidate
    /// applies when its parameter count matches, each ordinary argument is assignable to its
    /// parameter, and each method-group argument's parameter is a DELEGATE type -- the group
    /// converts to it (15.4), and `convert` builds the delegate at the call site. Returns the
    /// unique applicable method (reporting CS0122 if inaccessible, CS0121 if two apply),
    /// else `None`.
    fn resolve_with_method_groups(
        &mut self,
        name: &str,
        declaring: &TypeSymbol,
        candidates: &[MethodSymbol],
        arguments: &[BoundExpr],
        span: Span,
    ) -> Option<MethodSymbol> {
        let applicable: Vec<MethodSymbol> = candidates
            .iter()
            .filter(|candidate| {
                candidate.parameters.len() == arguments.len()
                    && arguments.iter().zip(&candidate.parameters).all(
                        |(argument, parameter)| {
                            if matches!(argument.kind, BoundExprKind::MethodGroup { .. }) {
                                self.type_info_of(parameter)
                                    .is_some_and(|info| info.kind == TypeKind::Delegate)
                            } else {
                                self.assignable(argument, parameter)
                            }
                        },
                    )
            })
            .cloned()
            .collect();
        match applicable.as_slice() {
            [method] => {
                self.check_accessible(declaring, method.accessibility, name, span);
                Some(method.clone())
            }
            [] => None,
            _ => {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::AmbiguousCall { method: name.into() },
                    span,
                ));
                None
            }
        }
    }

    fn bind_element_access(
        &mut self,
        receiver_expr: &Expr,
        argument_exprs: &[Expr],
        span: Span,
    ) -> BoundExpr {
        let receiver = self.bind_expression(receiver_expr);
        let indices: Vec<BoundExpr> = argument_exprs
            .iter()
            .map(|argument| self.bind_expression(argument))
            .collect();
        let element = match &receiver.ty {
            TypeSymbol::Array { element, .. } => Some((**element).clone()),
            TypeSymbol::Special(SpecialType::String) if indices.len() == 1 => {
                Some(TypeSymbol::Special(SpecialType::Char))
            }
            TypeSymbol::Pointer(element) if indices.len() == 1 => Some((**element).clone()),
            _ => None,
        };
        if let Some(ty) = element {
            return BoundExpr {
                kind: BoundExprKind::ElementAccess {
                    receiver: Box::new(receiver),
                    indices,
                },
                ty,
            };
        }
        if receiver.ty.is_error() {
            return error_expr();
        }
        if self.methods_in_chain(&receiver.ty, "get_Item").is_empty() {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::CannotIndex {
                    type_name: receiver.ty.to_string().into(),
                },
                span,
            ));
            return error_expr();
        }
        self.bind_indexer_call(receiver, "get_Item", indices, span)
            .unwrap_or_else(error_expr)
    }

    /// Binds an indexer access `obj[args]` as a call to its accessor: `get_Item` for a
    /// read, `set_Item` for a write (with the value appended). Resolves the overload,
    /// converts the arguments to the accessor's parameter types (boxing/upcasting), and
    /// builds the call. `None` when no overload matches (the resolver reports the error).
    fn bind_indexer_call(
        &mut self,
        receiver: BoundExpr,
        accessor: &str,
        arguments: Vec<BoundExpr>,
        span: Span,
    ) -> Option<BoundExpr> {
        if receiver.ty.is_error() || arguments.iter().any(|argument| argument.ty.is_error()) {
            return None;
        }
        let candidates = self.methods_in_chain(&receiver.ty, accessor);
        let argument_types: Vec<TypeSymbol> =
            arguments.iter().map(|argument| argument.ty.clone()).collect();
        let arg_constants: Vec<Option<i64>> =
            arguments.iter().map(constant_int_value).collect();
        let method = self.resolve_call(
            accessor,
            &receiver.ty,
            &candidates,
            &argument_types,
            &arg_constants,
            span,
        )?;
        let declaring_type =
            self.declaring_type_in_chain(&receiver.ty, &method.name, &method.parameters);
        let method_ref = MethodReference {
            declaring_type,
            name: method.name,
            parameters: method.parameters,
            return_type: method.return_type,
            is_static: false,
        };
        let arguments = if method_ref.parameters.len() == arguments.len() {
            arguments
                .into_iter()
                .zip(method_ref.parameters.iter())
                .map(|(argument, parameter)| self.convert(argument, parameter))
                .collect()
        } else {
            arguments
        };
        let ty = method_ref.return_type.clone();
        let callee = BoundExpr {
            ty: TypeSymbol::Error,
            kind: BoundExprKind::MethodGroup {
                receiver: Box::new(receiver),
                name: accessor.into(),
            },
        };
        Some(BoundExpr {
            kind: BoundExprKind::Call {
                callee: Box::new(callee),
                arguments,
                method: Some(method_ref),
            },
            ty,
        })
    }

    fn bind_object_creation(
        &mut self,
        target: &TypeRef,
        argument_exprs: &[Expr],
        span: Span,
    ) -> BoundExpr {
        let target_ty = self.resolve_named_type(&bind_type(target), target.span);
        let arguments: Vec<BoundExpr> = argument_exprs
            .iter()
            .map(|argument| self.bind_expression(argument))
            .collect();
        if self
            .type_info_of(&target_ty)
            .is_some_and(|info| info.kind == TypeKind::Delegate)
        {
            return self.bind_delegate_creation(&target_ty, &arguments, span);
        }
        let has_method_group = arguments
            .iter()
            .any(|argument| matches!(argument.kind, BoundExprKind::MethodGroup { .. }));
        let real_error = arguments.iter().any(|argument| {
            argument.ty.is_error() && !matches!(argument.kind, BoundExprKind::MethodGroup { .. })
        });
        let mut constructor = None;
        let mut arguments = arguments;
        let ty = if target_ty.is_error() {
            TypeSymbol::Error
        } else {
            if !real_error {
                if let Some(constructors) = self
                    .type_info_of(&target_ty)
                    .map(|info| info.constructors.clone())
                {
                    let chosen = if has_method_group {
                        self.resolve_with_method_groups(
                            ".ctor",
                            &target_ty,
                            &constructors,
                            &arguments,
                            span,
                        )
                    } else {
                        let argument_types: Vec<TypeSymbol> =
                            arguments.iter().map(|argument| argument.ty.clone()).collect();
                        let arg_constants: Vec<Option<i64>> =
                            arguments.iter().map(constant_int_value).collect();
                        self.check_constructor(
                            &target_ty,
                            &constructors,
                            &argument_types,
                            &arg_constants,
                            span,
                        )
                    };
                    if let Some(chosen) = chosen {
                        if chosen.parameters.len() == arguments.len() {
                            arguments = core::mem::take(&mut arguments)
                                .into_iter()
                                .zip(chosen.parameters.iter())
                                .map(|(argument, parameter)| self.convert(argument, parameter))
                                .collect();
                        }
                        constructor = Some(MethodReference {
                            declaring_type: target_ty.clone(),
                            name: ".ctor".into(),
                            parameters: chosen.parameters,
                            return_type: TypeSymbol::Special(SpecialType::Void),
                            is_static: false,
                        });
                    }
                }
            }
            target_ty
        };
        BoundExpr {
            kind: BoundExprKind::ObjectCreation {
                arguments,
                constructor,
            },
            ty,
        }
    }

    /// Binds `new D(methodGroup)`: the method group converts to delegate `D` when a
    /// method named in it matches `D`'s `Invoke` signature (same parameters and return).
    /// A static target carries no receiver; an instance target keeps its receiver.
    fn bind_delegate_creation(
        &self,
        delegate_ty: &TypeSymbol,
        arguments: &[BoundExpr],
        _span: Span,
    ) -> BoundExpr {
        let recover = BoundExpr {
            kind: BoundExprKind::ObjectCreation {
                arguments: Vec::new(),
                constructor: None,
            },
            ty: delegate_ty.clone(),
        };
        let Some(invoke) = self
            .type_info_of(delegate_ty)
            .and_then(|info| info.methods.iter().find(|m| &*m.name == "Invoke").cloned())
        else {
            return recover;
        };
        let [argument] = arguments else {
            return recover;
        };
        let BoundExprKind::MethodGroup { receiver, name } = &argument.kind else {
            return recover;
        };
        let receiver_ty = receiver.ty.clone();
        let Some(target) = self
            .methods_in_chain(&receiver_ty, name)
            .into_iter()
            .find(|m| m.parameters == invoke.parameters && m.return_type == invoke.return_type)
        else {
            return recover;
        };
        let declaring = self.declaring_type_in_chain(&receiver_ty, name, &target.parameters);
        let bound_receiver = if target.is_static {
            None
        } else {
            Some(receiver.clone())
        };
        BoundExpr {
            kind: BoundExprKind::DelegateCreation {
                delegate_type: delegate_ty.clone(),
                target: MethodReference {
                    declaring_type: declaring,
                    name: target.name.clone(),
                    parameters: target.parameters.clone(),
                    return_type: target.return_type,
                    is_static: target.is_static,
                },
                receiver: bound_receiver,
            },
            ty: delegate_ty.clone(),
        }
    }

    /// Resolves `new T(args)` against `T`'s constructors, reporting the diagnostic
    /// for a failed resolution. The created type is the result regardless.
    fn check_constructor(
        &mut self,
        target: &TypeSymbol,
        constructors: &[MethodSymbol],
        argument_types: &[TypeSymbol],
        arg_constants: &[Option<i64>],
        span: Span,
    ) -> Option<MethodSymbol> {
        match resolve_overload(&self.model, constructors, argument_types, arg_constants) {
            OverloadResult::Resolved(constructor) => return Some(constructor),
            OverloadResult::WrongArgumentCount => self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::NoConstructor {
                    type_name: target.to_string().into(),
                    count: argument_types.len() as u32,
                },
                span,
            )),
            OverloadResult::BadArgument { index, from, to } => {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::ArgumentConversion {
                        index: index as u32 + 1,
                        from: from.to_string().into(),
                        to: to.to_string().into(),
                    },
                    span,
                ));
            }
            OverloadResult::Ambiguous => self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::AmbiguousCall {
                    method: target.to_string().into(),
                },
                span,
            )),
        }
        None
    }

    /// Whether a member of `declaring` with this accessibility is reachable from the current
    /// context (10.5.1). `private` is the declaring type and any type nested in it;
    /// `protected` adds the types derived from the declaring type. `internal` and
    /// `protected internal` are treated as accessible: a same-assembly access IS allowed,
    /// and lcsc does not distinguish a reference assembly's internal members (rarely named)
    /// -- cross-assembly internal enforcement is a known gap.
    fn is_accessible(&self, declaring: &TypeSymbol, accessibility: Accessibility) -> bool {
        match accessibility {
            Accessibility::Public => true,
            Accessibility::Internal => !self.type_is_external(declaring),
            Accessibility::ProtectedInternal => {
                !self.type_is_external(declaring) || self.current_derives_from(declaring)
            }
            Accessibility::Private => self.in_private_scope_of(declaring),
            Accessibility::Protected => {
                self.in_private_scope_of(declaring) || self.current_derives_from(declaring)
            }
        }
    }

    /// Whether `ty` comes from a referenced assembly (so its `internal` members are not
    /// accessible from the unit being compiled).
    fn type_is_external(&self, ty: &TypeSymbol) -> bool {
        self.type_info_of(ty).is_some_and(|info| info.is_external)
    }

    /// Whether the current type is `declaring` or a type nested (at any depth) within it --
    /// the scope a `private` member is accessible from (10.5.1).
    fn in_private_scope_of(&self, declaring: &TypeSymbol) -> bool {
        let Some(current) = &self.current_type else {
            return false;
        };
        if current == declaring {
            return true;
        }
        let declaring_name = declaring.to_string();
        let mut info = self.type_info_of(current);
        while let Some(type_info) = info {
            match type_info.enclosing.as_deref() {
                None => return false,
                Some(enclosing) if enclosing == declaring_name => return true,
                Some(enclosing) => info = self.type_info_of(&named_symbol_from_dotted(enclosing)),
            }
        }
        false
    }

    /// Whether the current type derives from `declaring` -- the extra scope a `protected`
    /// member adds (a simplification of 10.5.3; the access-through-an-instance-of-the-derived-
    /// type rule is not enforced).
    fn current_derives_from(&self, declaring: &TypeSymbol) -> bool {
        let Some(current) = &self.current_type else {
            return false;
        };
        let mut info = self.type_info_of(current);
        while let Some(base) = info.and_then(|type_info| type_info.base.clone()) {
            if &base == declaring {
                return true;
            }
            info = self.type_info_of(&base);
        }
        false
    }

    /// Reports `CS0122` when a member is not accessible from the current context.
    fn check_accessible(
        &mut self,
        declaring: &TypeSymbol,
        accessibility: Accessibility,
        member: &str,
        span: Span,
    ) {
        if !self.is_accessible(declaring, accessibility) {
            let mut qualified = declaring.to_string();
            qualified.push('.');
            qualified.push_str(member);
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::Inaccessible {
                    member: qualified.into(),
                },
                span,
            ));
        }
    }

    /// Reports the static/instance mismatch of accessing a member through `receiver`
    /// (`CS0120` for an instance member named through a type, `CS0176` for a static
    /// member through an instance). An access through `this`/`base` is exempt.
    fn check_static_instance(
        &mut self,
        receiver: Receiver,
        is_static: bool,
        declaring: &TypeSymbol,
        member: &str,
        span: Span,
    ) {
        let kind = match receiver {
            Receiver::ViaType if !is_static => DiagnosticKind::ObjectReferenceRequired {
                member: qualified_member(declaring, member),
            },
            Receiver::Instance if is_static => DiagnosticKind::StaticMemberViaInstance {
                member: qualified_member(declaring, member),
            },
            _ => return,
        };
        self.diagnostics.push(Diagnostic::new(kind, span));
    }

    /// Looks a member up on a type, walking the base-class chain (14.3, 14.5.4).
    /// If `name` is a field-like event reachable on `ty` (itself or a base), returns the
    /// event and the symbol of the type that declares it. `+=`/`-=` route through its
    /// accessors from outside that type (17.7), and any other use there is CS0070.
    fn event_declaration(&self, ty: &TypeSymbol, name: &str) -> Option<(EventSymbol, TypeSymbol)> {
        let lookup = member_lookup_type(ty);
        let mut current = self.type_info_of(&lookup);
        while let Some(info) = current {
            if let Some(event) = info.find_event(name) {
                return Some((event.clone(), type_symbol_in(&info.namespace, &info.name)));
            }
            current = info.base.as_ref().and_then(|base| self.type_info_of(base));
        }
        None
    }

    /// Whether code currently being bound is outside the type that declares `event_owner`
    /// (so `+=`/`-=` must route through accessors and other uses are CS0070).
    fn outside_event_declarer(&self, declaring: &TypeSymbol) -> bool {
        !self.in_private_scope_of(declaring)
    }

    fn resolve_member(&self, ty: &TypeSymbol, name: &str) -> MemberResolution {
        let lookup = member_lookup_type(ty);
        if self.type_info_of(&lookup).is_none() {
            return MemberResolution::Unknown;
        }
        let mut visited: Vec<TypeSymbol> = Vec::new();
        let mut pending = alloc::vec![lookup.clone()];
        while let Some(current_ty) = pending.pop() {
            if visited.contains(&current_ty) {
                continue;
            }
            visited.push(current_ty.clone());
            let Some(info) = self.type_info_of(&current_ty) else {
                continue;
            };
            if let Some(field) = info.find_field(name) {
                return MemberResolution::Field(FieldReference {
                    declaring_type: type_symbol_in(&info.namespace, &info.name),
                    name: field.name.clone(),
                    ty: self.resolve_type(&field.ty),
                    is_static: field.is_static,
                    accessibility: field.accessibility,
                    constant: field.constant.clone(),
                });
            }
            if let Some(property) = info.find_property(name) {
                return MemberResolution::Property {
                    declaring_type: type_symbol_in(&info.namespace, &info.name),
                    ty: self.resolve_type(&property.ty),
                    accessibility: property.accessibility,
                    is_static: property.is_static,
                };
            }
            if info.methods_named(name).next().is_some() {
                return MemberResolution::MethodGroup;
            }
            for base in &info.bases {
                pending.push(base.clone());
            }
            if let Some(base) = &info.base {
                pending.push(base.clone());
            }
        }
        MemberResolution::NoSuchMember(ty.to_string())
    }

    /// The model entry for a named type, if any.
    fn type_info_of(&self, ty: &TypeSymbol) -> Option<&TypeInfo> {
        self.model.get_by_symbol(ty)
    }

    /// Whether `expr` is a delegate-typed VALUE (so `expr(args)` means `expr.Invoke(args)`)
    /// rather than a method group, type, or namespace.
    fn is_delegate_value(&self, expr: &BoundExpr) -> bool {
        !matches!(
            expr.kind,
            BoundExprKind::MethodGroup { .. }
                | BoundExprKind::TypeReference(_)
                | BoundExprKind::NamespaceReference(_)
        ) && self
            .type_info_of(&expr.ty)
            .is_some_and(|info| info.kind == TypeKind::Delegate)
    }

    /// Whether the field `name` declared by `declaring` is `readonly` (CS0191).
    fn field_is_readonly(&self, declaring: &TypeSymbol, name: &str) -> bool {
        self.type_info_of(declaring)
            .and_then(|info| info.fields.iter().find(|field| &*field.name == name))
            .is_some_and(|field| field.is_readonly)
    }

    /// Reports `CS0535` for each interface member a concrete class/struct does not
    /// implement. An abstract class (or an interface/enum) is exempt.
    pub(crate) fn check_interface_implementations(
        &mut self,
        class_ty: &TypeSymbol,
        declaration: &lamella_syntax::ast::TypeDecl,
    ) {
        if declaration
            .modifiers
            .iter()
            .any(|modifier| matches!(modifier, lamella_syntax::ast::Modifier::Abstract))
        {
            return;
        }
        let concrete = self
            .model
            .get_by_symbol(class_ty)
            .is_some_and(|info| matches!(info.kind, TypeKind::Class | TypeKind::Struct));
        if !concrete {
            return;
        }
        for interface in self.transitive_interfaces(class_ty) {
            let members = match self.model.get_by_symbol(&interface) {
                Some(info) => info.methods.clone(),
                None => continue,
            };
            let interface_name = dotted_type_name(&interface);
            for member in &members {
                if self.implements_interface_member(class_ty, member) {
                    continue;
                }
                let mut member_name = interface_name.clone();
                member_name.push('.');
                member_name.push_str(&member.name);
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::InterfaceMemberNotImplemented {
                        type_name: declaration.name.clone(),
                        member: member_name.into(),
                    },
                    declaration.span,
                ));
            }
        }
    }

    /// The interfaces a type transitively implements: its own interface bases, plus those
    /// interfaces' base interfaces.
    fn transitive_interfaces(&self, ty: &TypeSymbol) -> Vec<TypeSymbol> {
        let mut result: Vec<TypeSymbol> = Vec::new();
        let mut stack: Vec<TypeSymbol> = alloc::vec![ty.clone()];
        while let Some(current) = stack.pop() {
            let Some(info) = self.model.get_by_symbol(&current) else {
                continue;
            };
            for base in &info.bases {
                let is_interface = self
                    .model
                    .get_by_symbol(base)
                    .is_some_and(|base_info| base_info.kind == TypeKind::Interface);
                if is_interface && !result.contains(base) {
                    result.push(base.clone());
                    stack.push(base.clone());
                }
            }
        }
        result
    }

    /// Whether `class_ty` implements interface `member` -- implicitly (a method with the
    /// same name + parameter types anywhere in the class chain) or explicitly (a method
    /// registered under a mangled `<interface>.<member>` name). The explicit check is
    /// lenient (any `.<member>` impl) so a real explicit impl is never falsely flagged.
    fn implements_interface_member(&self, class_ty: &TypeSymbol, member: &MethodSymbol) -> bool {
        if self
            .methods_in_chain(class_ty, &member.name)
            .iter()
            .any(|candidate| candidate.parameters == member.parameters)
        {
            return true;
        }
        let mut suffix = String::from(".");
        suffix.push_str(&member.name);
        self.model
            .get_by_symbol(class_ty)
            .is_some_and(|info| info.methods.iter().any(|m| m.name.ends_with(&suffix)))
    }

    /// Reports `CS0146` if `class_ty`'s base-class chain is circular (A : B, B : A). The
    /// chain walk is bounded by a visited set, so a cycle is detected, not looped on.
    pub(crate) fn check_base_cycle(
        &mut self,
        class_ty: &TypeSymbol,
        declaration: &lamella_syntax::ast::TypeDecl,
    ) {
        let mut visited: Vec<TypeSymbol> = Vec::new();
        let mut current = Some(class_ty.clone());
        while let Some(ty) = current.take() {
            if visited.contains(&ty) {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::CircularBase {
                        type_name: declaration.name.clone(),
                    },
                    declaration.span,
                ));
                return;
            }
            visited.push(ty.clone());
            current = self
                .model
                .get_by_symbol(&ty)
                .and_then(|info| info.base.clone());
        }
    }

    /// Every method named `name` on `ty` or any of its base classes -- the method
    /// group an invocation resolves over (most-derived first).
    /// Resolves a no-argument instance method `name` on `receiver_ty` to a reference -- for
    /// the compiler-synthesized calls of a `foreach` enumerator pattern (GetEnumerator on the
    /// collection, MoveNext/get_Current on the enumerator). `None` when the type has no such
    /// method (so the collection is not enumerable), without reporting a diagnostic.
    pub(crate) fn resolve_instance_method(
        &mut self,
        receiver_ty: &TypeSymbol,
        name: &str,
        span: Span,
    ) -> Option<MethodReference> {
        let candidates = self.methods_in_chain(receiver_ty, name);
        if candidates.is_empty() {
            return None;
        }
        let chosen = self.resolve_call(name, receiver_ty, &candidates, &[], &[], span)?;
        let declaring_type =
            self.declaring_type_in_chain(receiver_ty, &chosen.name, &chosen.parameters);
        Some(MethodReference {
            declaring_type,
            name: chosen.name,
            parameters: chosen.parameters,
            return_type: chosen.return_type,
            is_static: chosen.is_static,
        })
    }

    /// Whether a bound call is to a `[Conditional("X")]` method none of whose symbols are
    /// defined here -- so the call statement is omitted whole (24.4.2), arguments and all. The
    /// method's `conditional` is recovered from the model by the resolved overload.
    pub(crate) fn conditional_call_omitted(&self, expr: &BoundExpr) -> bool {
        let BoundExprKind::Call {
            method: Some(method),
            ..
        } = &expr.kind
        else {
            return false;
        };
        let conditional = self
            .methods_in_chain(&method.declaring_type, &method.name)
            .into_iter()
            .find(|candidate| candidate.parameters == method.parameters)
            .map(|candidate| candidate.conditional)
            .unwrap_or_default();
        !conditional.is_empty()
            && !conditional
                .iter()
                .any(|symbol| self.defined_symbols.contains(symbol))
    }

    fn methods_in_chain(&self, ty: &TypeSymbol, name: &str) -> Vec<MethodSymbol> {
        let mut methods: Vec<MethodSymbol> = Vec::new();
        let mut visited: Vec<TypeSymbol> = Vec::new();
        let mut pending = alloc::vec![member_lookup_type(ty)];
        while let Some(current_ty) = pending.pop() {
            if visited.contains(&current_ty) {
                continue;
            }
            visited.push(current_ty.clone());
            let Some(info) = self.type_info_of(&current_ty) else {
                continue;
            };
            for method in info.methods_named(name) {
                if !methods.iter().any(|kept| kept.parameters == method.parameters) {
                    methods.push(method.clone());
                }
            }
            for base in &info.bases {
                pending.push(base.clone());
            }
            if let Some(base) = &info.base {
                pending.push(base.clone());
            }
        }
        methods
    }

    /// The type a resolved method reference should name: the most-derived type from
    /// `ty` up its base chain that declares the method `name(parameters)`. An override
    /// names the deriving type; a method only inherited names the base that declares
    /// it (so the emitted token resolves there, not on the receiver's type).
    fn declaring_type_in_chain(
        &self,
        ty: &TypeSymbol,
        name: &str,
        parameters: &[TypeSymbol],
    ) -> TypeSymbol {
        let lookup = member_lookup_type(ty);
        let mut visited: Vec<TypeSymbol> = Vec::new();
        let mut pending = alloc::vec![lookup.clone()];
        while let Some(current_ty) = pending.pop() {
            if visited.contains(&current_ty) {
                continue;
            }
            visited.push(current_ty.clone());
            let Some(info) = self.type_info_of(&current_ty) else {
                continue;
            };
            if info
                .methods_named(name)
                .any(|method| method.parameters.as_slice() == parameters)
            {
                return type_symbol_in(&info.namespace, &info.name);
            }
            for base in &info.bases {
                pending.push(base.clone());
            }
            if let Some(base) = &info.base {
                pending.push(base.clone());
            }
        }
        lookup
    }

    /// Binds a simple name (14.5.2). For now a name resolves only to a local
    /// variable or parameter; anything else is `CS0103` (field, type, and
    /// namespace lookup arrive with the declaration model).
    fn bind_name(&mut self, name: &str, span: Span) -> BoundExpr {
        if let Some(ty) = self.lookup_local(name) {
            return BoundExpr {
                kind: BoundExprKind::Local(name.into()),
                ty: ty.clone(),
            };
        }
        if let Some(receiver) = self.session_receiver.clone() {
            if let Some((stable, ty)) = self.session_fields.get(name).cloned() {
                let repl_type = self.current_type.clone().unwrap_or(TypeSymbol::Error);
                return self.session_field_access(&receiver, &repl_type, &stable, &ty);
            }
        }
        if let Some(current) = self.current_type.clone() {
            match self.resolve_member(&current, name) {
                MemberResolution::Field(field) => {
                    return BoundExpr {
                        ty: field.ty.clone(),
                        kind: BoundExprKind::FieldAccess {
                            receiver: Box::new(self.implicit_receiver()),
                            name: name.into(),
                            field: Some(field),
                        },
                    };
                }
                MemberResolution::Property {
                    declaring_type, ty, ..
                } => {
                    return BoundExpr {
                        kind: BoundExprKind::PropertyAccess {
                            receiver: Box::new(self.implicit_receiver()),
                            declaring_type,
                            name: name.into(),
                        },
                        ty,
                    };
                }
                MemberResolution::MethodGroup => {
                    return BoundExpr {
                        kind: BoundExprKind::MethodGroup {
                            receiver: Box::new(self.implicit_receiver()),
                            name: name.into(),
                        },
                        ty: TypeSymbol::Error,
                    };
                }
                MemberResolution::NoSuchMember(_) | MemberResolution::Unknown => {}
            }
        }
        if let Some(target) = self.alias_target(name) {
            return BoundExpr {
                kind: BoundExprKind::TypeReference(target.clone()),
                ty: target,
            };
        }
        let hits = self.type_namespaces_containing(name);
        if hits.len() == 1 {
            let ty = type_symbol_in(&hits[0], name);
            return BoundExpr {
                kind: BoundExprKind::TypeReference(ty.clone()),
                ty,
            };
        }
        if hits.len() >= 2 {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::AmbiguousReference {
                    name: name.into(),
                    first: full_type_name(&hits[0], name),
                    second: full_type_name(&hits[1], name),
                },
                span,
            ));
            return error_expr();
        }
        if self.model.is_namespace(name) {
            return BoundExpr {
                kind: BoundExprKind::NamespaceReference(name.into()),
                ty: TypeSymbol::Error,
            };
        }
        self.diagnostics.push(Diagnostic::new(
            DiagnosticKind::NameNotFound { name: name.into() },
            span,
        ));
        error_expr()
    }

    /// Resolves `namespace.name`: a nested namespace, a type, or `CS0234`.
    fn bind_qualified_name(&mut self, namespace: &str, name: &str, span: Span) -> BoundExpr {
        if self.model.get(namespace, name).is_some() {
            let ty = qualified_type_symbol(namespace, name);
            return BoundExpr {
                kind: BoundExprKind::TypeReference(ty.clone()),
                ty,
            };
        }
        let mut nested = String::from(namespace);
        nested.push('.');
        nested.push_str(name);
        if self.model.is_namespace(&nested) {
            return BoundExpr {
                kind: BoundExprKind::NamespaceReference(nested.into()),
                ty: TypeSymbol::Error,
            };
        }
        self.diagnostics.push(Diagnostic::new(
            DiagnosticKind::NamespaceMemberNotFound {
                namespace: namespace.into(),
                name: name.into(),
            },
            span,
        ));
        error_expr()
    }

    /// The `this` access, typed as the enclosing type (the error type when there
    /// is none, for recovery).
    fn this_expr(&self) -> BoundExpr {
        BoundExpr {
            kind: BoundExprKind::This,
            ty: self.current_type.clone().unwrap_or(TypeSymbol::Error),
        }
    }

    /// The receiver an implicit member access reads through: `this` normally, or the
    /// `s: __Repl` parameter in REPL session mode. A submission's `Submit$N` is a
    /// static method, so its session locals -- modeled as fields of the enclosing
    /// `__Repl` -- are reached through the parameter `s` (`ldarg.0; ldfld`), not a
    /// `this` it does not have. Both carry the enclosing type, so member lookup is
    /// identical; only the emitted receiver differs.
    fn implicit_receiver(&self) -> BoundExpr {
        match &self.session_receiver {
            Some(name) => BoundExpr {
                kind: BoundExprKind::Local(name.clone()),
                ty: self.current_type.clone().unwrap_or(TypeSymbol::Error),
            },
            None => self.this_expr(),
        }
    }

    /// The `base` access, typed as the enclosing type's base class (the error type
    /// when there is no enclosing type or it has no base, for recovery).
    fn base_expr(&self) -> BoundExpr {
        let base = self
            .current_type
            .as_ref()
            .and_then(|ty| self.type_info_of(ty))
            .and_then(|info| info.base.clone());
        BoundExpr {
            kind: BoundExprKind::Base,
            ty: base.unwrap_or(TypeSymbol::Error),
        }
    }
}

/// Binds a single expression and discards the diagnostics, for callers that only
/// want the typed tree.
#[must_use]
pub fn bind_expression(expr: &Expr) -> BoundExpr {
    let mut binder = Binder::new();
    binder.bind_expression(expr)
}

/// The result type of pointer arithmetic (18.5.6, unsafe): `p + n` / `n + p` / `p - n`
/// (a `T*`, the integer scaled by `sizeof(T)`) and `p - q` (a `long`, the element-count
/// difference). `None` when neither operand is a pointer (a plain numeric op handles it).
fn pointer_binary_result(
    operator: BinaryOperator,
    left: &TypeSymbol,
    right: &TypeSymbol,
) -> Option<TypeSymbol> {
    use BinaryOperator::{Add, Subtract};
    let integral = |ty: &TypeSymbol| matches!(ty, TypeSymbol::Special(special) if special.is_integral());
    match (operator, left, right) {
        (Add, TypeSymbol::Pointer(_), other) | (Add, other, TypeSymbol::Pointer(_))
            if integral(other) =>
        {
            Some(if matches!(left, TypeSymbol::Pointer(_)) {
                left.clone()
            } else {
                right.clone()
            })
        }
        (Subtract, TypeSymbol::Pointer(_), other) if integral(other) => Some(left.clone()),
        (Subtract, TypeSymbol::Pointer(a), TypeSymbol::Pointer(b)) if a == b => {
            Some(TypeSymbol::Special(SpecialType::Int64))
        }
        _ => None,
    }
}

/// The result type of a binary operator on operand types, or `None` if the
/// operator does not apply (14.7-14.12).
fn binary_result_type(
    operator: BinaryOperator,
    left: &TypeSymbol,
    right: &TypeSymbol,
) -> Option<TypeSymbol> {
    use BinaryOperator as Op;
    let bool_type = TypeSymbol::Special(SpecialType::Boolean);
    let left_special = as_special(left);
    let right_special = as_special(right);
    match operator {
        Op::Add
            if (left_special == Some(SpecialType::String)
                || right_special == Some(SpecialType::String))
                && !left.is_void()
                && !right.is_void() =>
        {
            Some(TypeSymbol::Special(SpecialType::String))
        }
        Op::Multiply | Op::Divide | Op::Modulo | Op::Add | Op::Subtract => {
            binary_numeric_promotion(left_special?, right_special?).map(TypeSymbol::Special)
        }
        Op::LessThan | Op::GreaterThan | Op::LessThanOrEqual | Op::GreaterThanOrEqual => {
            binary_numeric_promotion(left_special?, right_special?).map(|_| bool_type)
        }
        Op::Equal | Op::NotEqual => equality_comparable(left, right).then_some(bool_type),
        Op::LogicalAnd | Op::LogicalOr => {
            let boolean = Some(SpecialType::Boolean);
            (left_special == boolean && right_special == boolean).then_some(bool_type)
        }
        Op::BitwiseAnd | Op::BitwiseOr | Op::BitwiseXor => {
            let boolean = Some(SpecialType::Boolean);
            if left_special == boolean && right_special == boolean {
                Some(bool_type)
            } else {
                let (left, right) = (left_special?, right_special?);
                (is_integral(left) && is_integral(right))
                    .then_some(binary_numeric_promotion(left, right).map(TypeSymbol::Special))
                    .flatten()
            }
        }
        Op::LeftShift | Op::RightShift => {
            let (left, right) = (left_special?, right_special?);
            (is_integral(left) && is_integral(right))
                .then_some(TypeSymbol::Special(shift_result(left)))
        }
    }
}

/// The outcome of looking a member up on a type.
/// How a member-access receiver was written, for the static/instance check.
#[derive(Clone, Copy)]
enum Receiver {
    /// Through a type name, e.g. `Type.Member`.
    ViaType,
    /// Through `this`/`base` (implicit or explicit): no static/instance error.
    ImplicitThis,
    /// Through an instance value, e.g. `obj.Member`.
    Instance,
}

/// Categorizes a bound receiver for the static/instance check (CS0120/CS0176).
fn receiver_category(receiver: &BoundExpr) -> Receiver {
    match &receiver.kind {
        BoundExprKind::TypeReference(_) => Receiver::ViaType,
        BoundExprKind::This | BoundExprKind::Base => Receiver::ImplicitThis,
        _ => Receiver::Instance,
    }
}

/// `Type.member`, for a diagnostic message.
fn qualified_member(declaring: &TypeSymbol, member: &str) -> Box<str> {
    let mut qualified = declaring.to_string();
    qualified.push('.');
    qualified.push_str(member);
    qualified.into()
}

enum MemberResolution {
    /// A field, with its resolved reference.
    Field(FieldReference),
    /// A property, with its declaring type, type, accessibility, and staticness.
    Property {
        /// The type that declares the property.
        declaring_type: TypeSymbol,
        /// The property's type.
        ty: TypeSymbol,
        /// The property's accessibility.
        accessibility: Accessibility,
        /// Whether the property is `static`.
        is_static: bool,
    },
    /// One or more methods of that name (a method group).
    MethodGroup,
    /// The type is known but has no such member; carries the type's display name.
    NoSuchMember(String),
    /// The type is not in the model, so members cannot be resolved.
    Unknown,
}

/// A named-type symbol from a (non-empty, dotted) namespace and a simple name.
fn qualified_type_symbol(namespace: &str, name: &str) -> TypeSymbol {
    let mut parts: Vec<Box<str>> = namespace.split('.').map(Box::from).collect();
    parts.push(Box::from(name));
    TypeSymbol::Named(parts.into_boxed_slice())
}

/// A named-type symbol from a full dotted name (e.g. `Outer` or `Ns.Outer`), as a type's
/// `enclosing` is stored -- the inverse of [`TypeSymbol`]'s `Display`.
fn named_symbol_from_dotted(dotted: &str) -> TypeSymbol {
    TypeSymbol::Named(dotted.split('.').map(Box::from).collect())
}

/// A named-type symbol from a namespace (possibly empty) and a simple name.
fn type_symbol_in(namespace: &str, name: &str) -> TypeSymbol {
    if namespace.is_empty() {
        TypeSymbol::Named([Box::from(name)].into())
    } else {
        qualified_type_symbol(namespace, name)
    }
}

/// The full dotted name of a type in a namespace (the bare name when global).
fn full_type_name(namespace: &str, name: &str) -> Box<str> {
    if namespace.is_empty() {
        Box::from(name)
    } else {
        let mut full = String::from(namespace);
        full.push('.');
        full.push_str(name);
        full.into()
    }
}

/// An error placeholder expression, used for recovery.
fn error_expr() -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Error,
        ty: TypeSymbol::Error,
    }
}

/// The compile-time constant value of a predefined integral type's `MaxValue` or
/// `MinValue` member (4.1.5), as a two's-complement `i64` so an `ldc.i4`/`ldc.i8`
/// reproduces the right bits (`uint.MaxValue` -> -1 as `i32`, `ulong.MaxValue` -> -1
/// as `i64`). `None` for any other type or member name.
fn predefined_constant(special: SpecialType, member: &str) -> Option<i64> {
    use SpecialType as S;
    let (min, max): (i64, i64) = match special {
        S::SByte => (i8::MIN as i64, i8::MAX as i64),
        S::Byte => (0, u8::MAX as i64),
        S::Int16 => (i16::MIN as i64, i16::MAX as i64),
        S::UInt16 => (0, u16::MAX as i64),
        S::Char => (0, u16::MAX as i64),
        S::Int32 => (i32::MIN as i64, i32::MAX as i64),
        S::UInt32 => (0, u32::MAX as i64),
        S::Int64 => (i64::MIN, i64::MAX),
        S::UInt64 => (0, u64::MAX as i64),
        _ => return None,
    };
    match member {
        "MaxValue" => Some(max),
        "MinValue" => Some(min),
        _ => None,
    }
}

/// Whether `expr` is a constant of type `int`/`long` whose value fits the integral
/// `target` -- the implicit constant expression conversion (13.1.7), which lets
/// `byte b = 10` and `b[0] = 10` compile without a cast.
fn implicit_constant_conversion(expr: &BoundExpr, target: &TypeSymbol) -> bool {
    let (TypeSymbol::Special(source), TypeSymbol::Special(target)) = (&expr.ty, target) else {
        return false;
    };
    if !matches!(source, SpecialType::Int32 | SpecialType::Int64) {
        return false;
    }
    match constant_int_value(expr) {
        Some(value) => constant_fits(value, *target),
        None => false,
    }
}

/// The compile-time value of a constant integer expression: an integer literal, or a
/// member access that folded to a constant (a `const` field, an enum member, a
/// predefined `MaxValue`/`MinValue`). `None` for a non-constant expression.
fn constant_int_value(expr: &BoundExpr) -> Option<i64> {
    match &expr.kind {
        BoundExprKind::Literal(Literal::Integer { value, .. }) => i64::try_from(*value).ok(),
        BoundExprKind::FieldAccess {
            field: Some(field), ..
        } => field.constant.as_ref().and_then(literal_int_value),
        BoundExprKind::Unary { operator, operand } => match operator {
            UnaryOperator::Plus => constant_int_value(operand),
            UnaryOperator::Minus => constant_int_value(operand)?.checked_neg(),
            _ => None,
        },
        _ => None,
    }
}

/// An integer constant literal of the given value (the form a folded enum member /
/// `const` field / predefined `MaxValue` takes); its `i64` round-trips via `value as u64`.
pub(crate) fn integer_literal(value: i64) -> Literal {
    Literal::Integer {
        value: value as u64,
        suffix: lamella_syntax::token::IntegerSuffix::None,
    }
}

/// The `i64` value of an integral constant literal -- an integer (its two's-complement
/// bits), a `char`, or a `bool` -- the form case labels, constant-conversion checks, and the
/// enum-member value table compare. `None` for a real, string, or null literal.
pub fn literal_int_value(literal: &Literal) -> Option<i64> {
    match literal {
        Literal::Integer { value, .. } => Some(*value as i64),
        Literal::Character(unit) => Some(i64::from(*unit)),
        Literal::Boolean(b) => Some(i64::from(*b)),
        Literal::Real { .. } | Literal::String(_) | Literal::Null => None,
    }
}

/// Whether the constant `value` is in range of the integral `target` (13.1.7). `char`
/// is excluded: an int constant needs an explicit cast to `char`.
fn constant_fits(value: i64, target: SpecialType) -> bool {
    use SpecialType as S;
    match target {
        S::SByte => i8::try_from(value).is_ok(),
        S::Byte => u8::try_from(value).is_ok(),
        S::Int16 => i16::try_from(value).is_ok(),
        S::UInt16 => u16::try_from(value).is_ok(),
        S::UInt32 => u32::try_from(value).is_ok(),
        S::UInt64 => value >= 0,
        _ => false,
    }
}

/// The outcome of overload resolution over a method group (14.4.2).
enum OverloadResult {
    /// A unique best overload.
    Resolved(MethodSymbol),
    /// Two or more applicable overloads with no unique best.
    Ambiguous,
    /// No overload accepts this number of arguments.
    WrongArgumentCount,
    /// A count matches but an argument does not convert to the parameter.
    BadArgument {
        /// The 0-based argument position.
        index: usize,
        /// The argument type.
        from: TypeSymbol,
        /// The parameter type.
        to: TypeSymbol,
    },
}

/// Whether an argument -- its type `arg_ty`, plus `arg_const` (its compile-time integer value
/// when it is a constant) -- is applicable to parameter `param`: a standard implicit conversion,
/// or the implicit constant-expression conversion (13.1.7) for an `int`/`long` constant whose
/// value fits an integral parameter (so `Set(0x518, m)` binds when `Set` takes `uint`).
fn arg_applicable(
    model: &Model,
    arg_ty: &TypeSymbol,
    arg_const: Option<i64>,
    param: &TypeSymbol,
) -> bool {
    if converts(model, arg_ty, param) {
        return true;
    }
    matches!(
        arg_ty,
        TypeSymbol::Special(SpecialType::Int32 | SpecialType::Int64)
    ) && match param {
        TypeSymbol::Special(target) => arg_const.is_some_and(|value| constant_fits(value, *target)),
        _ => false,
    }
}

/// Chooses the overload for a call (14.4.2): the unique best among the applicable candidates,
/// or the diagnostic-bearing outcome otherwise. Conversions resolve against `model` so a
/// derived argument matches a base parameter; `arg_constants` carries each argument's
/// compile-time integer value (or `None`) to enable the constant conversion (13.1.7) in
/// applicability. An empty `arg_constants` means "no constants" (e.g. the operator paths).
fn resolve_overload(
    model: &Model,
    candidates: &[MethodSymbol],
    arguments: &[TypeSymbol],
    arg_constants: &[Option<i64>],
) -> OverloadResult {
    let applicable: Vec<&MethodSymbol> = candidates
        .iter()
        .filter(|candidate| is_applicable(model, candidate, arguments, arg_constants))
        .collect();
    if let Some(best) = best_candidate(model, &applicable, arguments, arg_constants) {
        return OverloadResult::Resolved(best.clone());
    }
    if !applicable.is_empty() {
        return OverloadResult::Ambiguous;
    }
    let Some(count_matched) = candidates
        .iter()
        .find(|candidate| candidate.parameters.len() == arguments.len())
    else {
        return OverloadResult::WrongArgumentCount;
    };
    for (index, (argument, parameter)) in
        arguments.iter().zip(&count_matched.parameters).enumerate()
    {
        if !arg_applicable(model, argument, arg_constants.get(index).copied().flatten(), parameter)
        {
            return OverloadResult::BadArgument {
                index,
                from: argument.clone(),
                to: parameter.clone(),
            };
        }
    }
    OverloadResult::WrongArgumentCount
}

/// Whether a method is applicable to the arguments: in normal form the counts match
/// and every argument converts to its parameter (14.4.2.1); a `params` method is also
/// applicable in expanded form, where the trailing arguments fill its array.
fn is_applicable(
    model: &Model,
    method: &MethodSymbol,
    arguments: &[TypeSymbol],
    arg_constants: &[Option<i64>],
) -> bool {
    is_normal_applicable(model, method, arguments, arg_constants)
        || (method.is_params && is_applicable_expanded(model, method, arguments, arg_constants))
}

/// Whether a method applies in NORMAL form: the counts match and every argument converts
/// to its parameter (14.4.2.1). (Expanded `params` form is [`is_applicable_expanded`].)
fn is_normal_applicable(
    model: &Model,
    method: &MethodSymbol,
    arguments: &[TypeSymbol],
    arg_constants: &[Option<i64>],
) -> bool {
    method.parameters.len() == arguments.len()
        && arguments
            .iter()
            .zip(&method.parameters)
            .enumerate()
            .all(|(i, (argument, parameter))| {
                arg_applicable(model, argument, arg_constants.get(i).copied().flatten(), parameter)
            })
}

/// Whether a `params` method applies in expanded form (14.4.2.1): the leading fixed
/// parameters convert, and every trailing argument converts to the array's element type.
fn is_applicable_expanded(
    model: &Model,
    method: &MethodSymbol,
    arguments: &[TypeSymbol],
    arg_constants: &[Option<i64>],
) -> bool {
    let fixed = method.parameters.len().saturating_sub(1);
    if arguments.len() < fixed {
        return false;
    }
    if !arguments[..fixed]
        .iter()
        .zip(&method.parameters[..fixed])
        .enumerate()
        .all(|(i, (argument, parameter))| {
            arg_applicable(model, argument, arg_constants.get(i).copied().flatten(), parameter)
        })
    {
        return false;
    }
    let TypeSymbol::Array { element, .. } = &method.parameters[fixed] else {
        return false;
    };
    arguments[fixed..].iter().enumerate().all(|(offset, argument)| {
        arg_applicable(
            model,
            argument,
            arg_constants.get(fixed + offset).copied().flatten(),
            element,
        )
    })
}

/// The single applicable candidate better than every other, or `None` when none
/// is uniquely best.
fn best_candidate<'a>(
    model: &Model,
    applicable: &[&'a MethodSymbol],
    arguments: &[TypeSymbol],
    arg_constants: &[Option<i64>],
) -> Option<&'a MethodSymbol> {
    applicable.iter().copied().find(|&candidate| {
        applicable.iter().all(|&other| {
            core::ptr::eq(candidate, other)
                || is_better(model, candidate, other, arguments, arg_constants)
        })
    })
}

/// Whether `c1` is a better function member than `c2` for the arguments: no worse
/// a parameter for every argument and strictly better for at least one, using the
/// better-conversion-target rule (14.4.2.2, 14.4.2.3 simplified).
fn is_better(
    model: &Model,
    c1: &MethodSymbol,
    c2: &MethodSymbol,
    arguments: &[TypeSymbol],
    arg_constants: &[Option<i64>],
) -> bool {
    let c1_normal = is_normal_applicable(model, c1, arguments, arg_constants);
    let c2_normal = is_normal_applicable(model, c2, arguments, arg_constants);
    if c1_normal != c2_normal {
        return c1_normal;
    }
    let mut strictly_better_somewhere = false;
    let compared = arguments
        .len()
        .min(c1.parameters.len())
        .min(c2.parameters.len());
    for index in 0..compared {
        let (p1, p2) = (&c1.parameters[index], &c2.parameters[index]);
        if p1 == p2 {
            continue;
        }
        let arg = &arguments[index];
        let (std1, std2) = (converts(model, arg, p1), converts(model, arg, p2));
        if std1 != std2 {
            if std1 {
                strictly_better_somewhere = true;
                continue;
            }
            return false;
        }
        if converts(model, p1, p2) || signed_preferred(p1, p2) {
            strictly_better_somewhere = true;
        } else {
            return false;
        }
    }
    strictly_better_somewhere
}

/// The signed/unsigned better-conversion special cases (14.4.2.3): a signed integral
/// target is better than a wider-or-equal unsigned one when neither converts to the
/// other (`sbyte` over byte/ushort/uint/ulong; `short` over ushort/uint/ulong; `int`
/// over uint/ulong; `long` over ulong). This is what makes `Console.WriteLine(byte)`
/// resolve to the `int` overload rather than report a spurious CS0121.
fn signed_preferred(p1: &TypeSymbol, p2: &TypeSymbol) -> bool {
    use SpecialType as S;
    let (Some(a), Some(b)) = (as_special(p1), as_special(p2)) else {
        return false;
    };
    matches!(
        (a, b),
        (S::SByte, S::Byte | S::UInt16 | S::UInt32 | S::UInt64)
            | (S::Int16, S::UInt16 | S::UInt32 | S::UInt64)
            | (S::Int32, S::UInt32 | S::UInt64)
            | (S::Int64, S::UInt64)
    )
}

/// The special type of `ty`, if it is one.
fn as_special(ty: &TypeSymbol) -> Option<SpecialType> {
    match ty {
        TypeSymbol::Special(special) => Some(*special),
        _ => None,
    }
}

/// A named type's dotted name (`["NS", "I"]` -> "NS.I"); empty for a non-named type.
fn dotted_type_name(ty: &TypeSymbol) -> String {
    match ty {
        TypeSymbol::Named(parts) => {
            let mut name = String::new();
            for part in parts.iter() {
                if !name.is_empty() {
                    name.push('.');
                }
                name.push_str(part);
            }
            name
        }
        _ => String::new(),
    }
}

/// The `System.Type` named type, the result of a `typeof` expression (14.5.11).
fn system_type() -> TypeSymbol {
    TypeSymbol::Named([Box::from("System"), Box::from("Type")].into())
}

/// `System.Array`, whose members (Length, GetLength, ...) an array's member access
/// resolves against.
fn system_array() -> TypeSymbol {
    TypeSymbol::Named([Box::from("System"), Box::from("Array")].into())
}

/// The type whose members a receiver of type `ty` resolves against: `System.Array`
/// for an array (its members live there), otherwise `ty` itself.
fn member_lookup_type(ty: &TypeSymbol) -> TypeSymbol {
    if matches!(ty, TypeSymbol::Array { .. }) {
        system_array()
    } else {
        ty.clone()
    }
}

/// The type of a conditional expression from its branch types (14.13): the branch
/// type the other implicitly converts to, or `None` (`CS0173`) when there is no
/// one-way conversion between them.
fn conditional_result_type(
    model: &Model,
    when_true: &TypeSymbol,
    when_false: &TypeSymbol,
) -> Option<TypeSymbol> {
    if when_true == when_false {
        return Some(when_true.clone());
    }
    match (
        converts(model, when_true, when_false),
        converts(model, when_false, when_true),
    ) {
        (true, false) => Some(when_false.clone()),
        (false, true) => Some(when_true.clone()),
        _ => None,
    }
}

/// Whether a bound expression denotes something assignable: a local or parameter,
/// a field or (writable) property, or an array element. A read-only property's
/// missing setter is a finer check left for later.
fn is_lvalue(expr: &BoundExpr) -> bool {
    matches!(
        expr.kind,
        BoundExprKind::Local(_)
            | BoundExprKind::FieldAccess { .. }
            | BoundExprKind::PropertyAccess { .. }
            | BoundExprKind::ElementAccess { .. }
            | BoundExprKind::Dereference { .. }
    )
}

/// The binary operator underlying a compound assignment, or `None` for simple `=`.
fn compound_binary_operator(operator: AssignmentOperator) -> Option<BinaryOperator> {
    use AssignmentOperator as A;
    Some(match operator {
        A::Assign => return None,
        A::Add => BinaryOperator::Add,
        A::Subtract => BinaryOperator::Subtract,
        A::Multiply => BinaryOperator::Multiply,
        A::Divide => BinaryOperator::Divide,
        A::Modulo => BinaryOperator::Modulo,
        A::And => BinaryOperator::BitwiseAnd,
        A::Or => BinaryOperator::BitwiseOr,
        A::Xor => BinaryOperator::BitwiseXor,
        A::LeftShift => BinaryOperator::LeftShift,
        A::RightShift => BinaryOperator::RightShift,
    })
}

/// The source symbol of an assignment operator, for diagnostics.
fn assignment_symbol(operator: AssignmentOperator) -> &'static str {
    use AssignmentOperator as A;
    match operator {
        A::Assign => "=",
        A::Add => "+=",
        A::Subtract => "-=",
        A::Multiply => "*=",
        A::Divide => "/=",
        A::Modulo => "%=",
        A::And => "&=",
        A::Or => "|=",
        A::Xor => "^=",
        A::LeftShift => "<<=",
        A::RightShift => ">>=",
    }
}

/// Whether two types may be compared with `==`/`!=`. Numeric pairs that promote,
/// `bool` pairs, identical types, and anything against `object` qualify; the
/// stricter reference-equality rules arrive with the type hierarchy.
fn equality_comparable(left: &TypeSymbol, right: &TypeSymbol) -> bool {
    if let (Some(left), Some(right)) = (as_special(left), as_special(right)) {
        if left.is_numeric() && right.is_numeric() {
            return binary_numeric_promotion(left, right).is_some();
        }
        if left == SpecialType::Boolean && right == SpecialType::Boolean {
            return true;
        }
    }
    left == right
        || matches!(left, TypeSymbol::Special(SpecialType::Object))
        || matches!(right, TypeSymbol::Special(SpecialType::Object))
}

/// Binary numeric promotion (14.2.6.2): the common type of two numeric operands,
/// or `None` if either is not numeric (or `decimal` is mixed with floating point).
fn binary_numeric_promotion(left: SpecialType, right: SpecialType) -> Option<SpecialType> {
    use SpecialType::{Decimal, Double, Int16, Int32, Int64, SByte, Single, UInt32, UInt64};
    if !left.is_numeric() || !right.is_numeric() {
        return None;
    }
    let has = |special: SpecialType| left == special || right == special;
    Some(if has(Decimal) {
        if has(Double) || has(Single) {
            return None;
        }
        Decimal
    } else if has(Double) {
        Double
    } else if has(Single) {
        Single
    } else if has(UInt64) {
        UInt64
    } else if has(Int64) {
        Int64
    } else if has(UInt32) {
        if matches!(left, SByte | Int16 | Int32) || matches!(right, SByte | Int16 | Int32) {
            Int64
        } else {
            UInt32
        }
    } else {
        Int32
    })
}

/// Whether a special type is one of the integral types (14.8 shift, bitwise).
fn is_integral(special: SpecialType) -> bool {
    use SpecialType::{Byte, Char, Int16, Int32, Int64, SByte, UInt16, UInt32, UInt64};
    matches!(
        special,
        SByte | Byte | Int16 | UInt16 | Int32 | UInt32 | Int64 | UInt64 | Char
    )
}

/// The result type of a shift, i.e. the unary-numeric-promoted left operand:
/// `int`, `uint`, `long`, or `ulong` (14.8).
fn shift_result(left: SpecialType) -> SpecialType {
    match left {
        SpecialType::Int32 | SpecialType::UInt32 | SpecialType::Int64 | SpecialType::UInt64 => left,
        _ => SpecialType::Int32,
    }
}

/// The result type of a prefix unary operator, or `None` if it does not apply
/// (14.6). The `++`/`--` cases keep the operand type; their lvalue requirement is
/// checked once name resolution lands.
fn unary_result_type(operator: UnaryOperator, operand: &TypeSymbol) -> Option<TypeSymbol> {
    use SpecialType::{Boolean, Int64, UInt32, UInt64};
    let special = as_special(operand)?;
    match operator {
        UnaryOperator::Plus => special
            .is_numeric()
            .then_some(TypeSymbol::Special(unary_numeric_promote(special))),
        UnaryOperator::Minus => match special {
            UInt64 => None,
            UInt32 => Some(TypeSymbol::Special(Int64)),
            other if other.is_numeric() => Some(TypeSymbol::Special(unary_numeric_promote(other))),
            _ => None,
        },
        UnaryOperator::Not => (special == Boolean).then_some(TypeSymbol::Special(Boolean)),
        UnaryOperator::Complement => {
            is_integral(special).then_some(TypeSymbol::Special(unary_numeric_promote(special)))
        }
        UnaryOperator::PreIncrement | UnaryOperator::PreDecrement => {
            special.is_numeric().then_some(operand.clone())
        }
    }
}

/// Unary numeric promotion (14.2.6.1): the smaller integral types and `char`
/// promote to `int`; every other numeric type is unchanged.
fn unary_numeric_promote(special: SpecialType) -> SpecialType {
    use SpecialType::{Byte, Char, Int16, Int32, SByte, UInt16};
    match special {
        SByte | Byte | Int16 | UInt16 | Char => Int32,
        other => other,
    }
}

/// The source symbol of a prefix unary operator, for diagnostics.
fn unary_operator_symbol(operator: UnaryOperator) -> &'static str {
    match operator {
        UnaryOperator::Plus => "+",
        UnaryOperator::Minus => "-",
        UnaryOperator::Not => "!",
        UnaryOperator::Complement => "~",
        UnaryOperator::PreIncrement => "++",
        UnaryOperator::PreDecrement => "--",
    }
}

/// The type of a literal (9.4.4).
fn literal_type(literal: &Literal) -> TypeSymbol {
    let special = match literal {
        Literal::Integer { value, suffix } => integer_literal_type(*value, *suffix),
        Literal::Real { suffix, .. } => match suffix {
            RealSuffix::Float => SpecialType::Single,
            RealSuffix::Decimal => SpecialType::Decimal,
            RealSuffix::Double | RealSuffix::None => SpecialType::Double,
        },
        Literal::Character(_) => SpecialType::Char,
        Literal::String(_) => SpecialType::String,
        Literal::Boolean(_) => SpecialType::Boolean,
        Literal::Null => SpecialType::Null,
    };
    TypeSymbol::Special(special)
}

/// The type of an integer literal (9.4.4.2): the first type in the
/// suffix-determined list whose range holds the value.
fn integer_literal_type(value: u64, suffix: IntegerSuffix) -> SpecialType {
    let i32_max = i32::MAX as u64;
    let u32_max = u32::MAX as u64;
    let i64_max = i64::MAX as u64;
    match suffix {
        IntegerSuffix::None => {
            if value <= i32_max {
                SpecialType::Int32
            } else if value <= u32_max {
                SpecialType::UInt32
            } else if value <= i64_max {
                SpecialType::Int64
            } else {
                SpecialType::UInt64
            }
        }
        IntegerSuffix::Unsigned => {
            if value <= u32_max {
                SpecialType::UInt32
            } else {
                SpecialType::UInt64
            }
        }
        IntegerSuffix::Long => {
            if value <= i64_max {
                SpecialType::Int64
            } else {
                SpecialType::UInt64
            }
        }
        IntegerSuffix::UnsignedLong => SpecialType::UInt64,
    }
}

/// The source symbol of a binary operator, for diagnostics.
fn operator_symbol(operator: BinaryOperator) -> &'static str {
    use BinaryOperator as Op;
    match operator {
        Op::Multiply => "*",
        Op::Divide => "/",
        Op::Modulo => "%",
        Op::Add => "+",
        Op::Subtract => "-",
        Op::LeftShift => "<<",
        Op::RightShift => ">>",
        Op::LessThan => "<",
        Op::GreaterThan => ">",
        Op::LessThanOrEqual => "<=",
        Op::GreaterThanOrEqual => ">=",
        Op::Equal => "==",
        Op::NotEqual => "!=",
        Op::BitwiseAnd => "&",
        Op::BitwiseXor => "^",
        Op::BitwiseOr => "|",
        Op::LogicalAnd => "&&",
        Op::LogicalOr => "||",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_syntax::parser::parse_expression;

    fn bound_type(source: &str) -> TypeSymbol {
        bind_expression(&parse_expression(source).expr).ty
    }

    fn codes(source: &str) -> Vec<u16> {
        let mut binder = Binder::new();
        binder.bind_expression(&parse_expression(source).expr);
        binder
            .into_diagnostics()
            .iter()
            .map(Diagnostic::code)
            .collect()
    }

    fn special(source: &str) -> SpecialType {
        match bound_type(source) {
            TypeSymbol::Special(special) => special,
            other => panic!("expected a special type, got {other:?}"),
        }
    }

    #[test]
    fn integer_literal_types_follow_the_value_and_suffix() {
        assert_eq!(special("42"), SpecialType::Int32);
        assert_eq!(special("2147483648"), SpecialType::UInt32);
        assert_eq!(special("10000000000000000000"), SpecialType::UInt64);
        assert_eq!(special("1u"), SpecialType::UInt32);
        assert_eq!(special("1L"), SpecialType::Int64);
    }

    #[test]
    fn arithmetic_uses_binary_numeric_promotion() {
        assert_eq!(special("1 + 2"), SpecialType::Int32);
        assert_eq!(special("1 + 2L"), SpecialType::Int64);
        assert_eq!(special("1 + 2.0"), SpecialType::Double);
        assert_eq!(special("1 * 2.0f"), SpecialType::Single);
        assert_eq!(special("'a' + 1"), SpecialType::Int32);
    }

    #[test]
    fn relational_equality_and_logical_yield_bool() {
        assert_eq!(special("1 < 2"), SpecialType::Boolean);
        assert_eq!(special("1 == 2"), SpecialType::Boolean);
        assert_eq!(special("true != false"), SpecialType::Boolean);
        assert_eq!(special("true && false"), SpecialType::Boolean);
    }

    #[test]
    fn bitwise_and_shift_typing() {
        assert_eq!(special("1 & 2"), SpecialType::Int32);
        assert_eq!(special("true | false"), SpecialType::Boolean);
        assert_eq!(special("1 << 2"), SpecialType::Int32);
        assert_eq!(special("1L << 2"), SpecialType::Int64);
    }

    #[test]
    fn inapplicable_operators_are_cs0019() {
        assert_eq!(codes("true + 1"), [19]);
        assert_eq!(codes("1 && 2"), [19]);
        assert_eq!(codes("\"x\" - \"y\""), [19]);
        assert_eq!(codes("(true + 1) + 2"), [19]);
    }

    #[test]
    fn unary_operator_typing() {
        assert_eq!(special("-1"), SpecialType::Int32);
        assert_eq!(special("-1L"), SpecialType::Int64);
        assert_eq!(special("-1u"), SpecialType::Int64);
        assert_eq!(special("+1"), SpecialType::Int32);
        assert_eq!(special("!true"), SpecialType::Boolean);
        assert_eq!(special("~1"), SpecialType::Int32);
        assert_eq!(special("1++"), SpecialType::Int32);
        assert_eq!(special("++1L"), SpecialType::Int64);
    }

    #[test]
    fn inapplicable_unary_operators_are_cs0023() {
        assert_eq!(codes("-true"), [23]);
        assert_eq!(codes("!1"), [23]);
        assert_eq!(codes("~true"), [23]);
        assert_eq!(codes("true++"), [23]);
    }

    fn bound_in_scope(binder: &mut Binder, source: &str) -> TypeSymbol {
        binder.bind_expression(&parse_expression(source).expr).ty
    }

    #[test]
    fn simple_names_resolve_to_declared_locals() {
        let mut binder = Binder::new();
        binder.enter_scope();
        binder.declare_local("x", TypeSymbol::Special(SpecialType::Int32));
        binder.declare_local("name", TypeSymbol::Special(SpecialType::String));
        assert_eq!(
            bound_in_scope(&mut binder, "x"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert_eq!(
            bound_in_scope(&mut binder, "x + 1"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert_eq!(
            bound_in_scope(&mut binder, "name"),
            TypeSymbol::Special(SpecialType::String)
        );
        assert!(binder.diagnostics().is_empty());
    }

    #[test]
    fn unknown_names_are_cs0103() {
        let mut binder = Binder::new();
        binder.enter_scope();
        binder.bind_expression(&parse_expression("missing").expr);
        let codes: Vec<u16> = binder.diagnostics().iter().map(Diagnostic::code).collect();
        assert_eq!(codes, [103]);
    }

    #[test]
    fn cast_typetest_typeof_and_checked() {
        assert_eq!(special("(long)1"), SpecialType::Int64);
        assert_eq!(special("1 is int"), SpecialType::Boolean);
        assert_eq!(special("1 as object"), SpecialType::Object);
        assert_eq!(bound_type("typeof(int)").to_string(), "System.Type");
        assert_eq!(special("checked(1 + 2)"), SpecialType::Int32);
        assert_eq!(special("unchecked(1)"), SpecialType::Int32);
    }

    #[test]
    fn casts_require_an_explicit_conversion() {
        assert_eq!(codes("(byte)1"), []);
        assert_eq!(codes("(int)1u"), []);
        assert_eq!(codes("(long)1"), []);
        assert_eq!(codes("(string)1"), [30]);
        assert_eq!(codes("(bool)1"), [30]);
    }

    #[test]
    fn conditional_result_type_and_condition_check() {
        assert_eq!(special("true ? 1 : 2"), SpecialType::Int32);
        assert_eq!(special("true ? 1 : 2L"), SpecialType::Int64);
        assert_eq!(special("false ? 2L : 1"), SpecialType::Int64);
        assert_eq!(codes("1 ? 1 : 2"), [29]);
        assert_eq!(codes("true ? 1 : \"x\""), [173]);
    }

    #[test]
    fn assignment_typing_and_checks() {
        let mut binder = Binder::new();
        binder.enter_scope();
        binder.declare_local("x", TypeSymbol::Special(SpecialType::Int32));
        assert_eq!(
            bound_in_scope(&mut binder, "x = 1"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        bound_in_scope(&mut binder, "x += 2");
        assert!(binder.diagnostics().is_empty());
        let before = binder.diagnostics().len();
        bound_in_scope(&mut binder, "x = true");
        assert_eq!(binder.diagnostics()[before].code(), 29);
    }

    #[test]
    fn assigning_to_a_non_variable_is_cs0131() {
        assert_eq!(codes("1 = 2"), [131]);
    }

    #[test]
    fn member_access_resolves_fields_method_groups_and_missing_members() {
        use crate::symbols::{FieldSymbol, MethodSymbol, TypeInfo, TypeKind};
        let mut model = Model::new();
        let mut widget = TypeInfo::new("", "Widget", TypeKind::Class);
        widget.fields.push(FieldSymbol {
            name: "count".into(),
            ty: TypeSymbol::Special(SpecialType::Int32),
            is_static: false,
            is_readonly: false,
            accessibility: crate::symbols::Accessibility::Public,
            constant: None,
        });
        widget.methods.push(MethodSymbol {
            name: "Area".into(),
            return_type: TypeSymbol::Special(SpecialType::Double),
            parameters: Vec::new(),
            is_static: false,
            is_params: false,
            accessibility: crate::symbols::Accessibility::Public,
            conditional: Vec::new(),
        });
        model.insert(widget);

        let mut binder = Binder::with_model(model);
        binder.enter_scope();
        binder.declare_local("w", TypeSymbol::Named(["Widget".into()].into()));

        let count = binder.bind_expression(&parse_expression("w.count").expr);
        assert_eq!(count.ty, TypeSymbol::Special(SpecialType::Int32));
        let area = binder.bind_expression(&parse_expression("w.Area").expr);
        assert!(matches!(area.kind, BoundExprKind::MethodGroup { .. }));
        assert!(binder.diagnostics().is_empty());
        binder.bind_expression(&parse_expression("w.missing").expr);
        assert_eq!(binder.diagnostics().last().map(Diagnostic::code), Some(117));
    }

    #[test]
    fn internal_member_of_a_referenced_assembly_is_cs0122() {
        use crate::symbols::{Accessibility, FieldSymbol, TypeInfo, TypeKind};
        let internal_field = |name: &str| FieldSymbol {
            name: name.into(),
            ty: TypeSymbol::Special(SpecialType::Int32),
            is_static: false,
            is_readonly: false,
            accessibility: Accessibility::Internal,
            constant: None,
        };
        let mut model = Model::new();
        let mut external = TypeInfo::new("", "Lib", TypeKind::Class);
        external.is_external = true;
        external.fields.push(internal_field("Secret"));
        model.insert(external);
        let mut here = TypeInfo::new("", "Here", TypeKind::Class);
        here.fields.push(internal_field("Shared"));
        model.insert(here);

        let mut binder = Binder::with_model(model);
        binder.enter_scope();
        binder.declare_local("lib", TypeSymbol::Named(["Lib".into()].into()));
        binder.declare_local("here", TypeSymbol::Named(["Here".into()].into()));

        binder.bind_expression(&parse_expression("lib.Secret").expr);
        assert_eq!(binder.diagnostics().last().map(Diagnostic::code), Some(122));

        let before = binder.diagnostics().len();
        binder.bind_expression(&parse_expression("here.Shared").expr);
        assert_eq!(binder.diagnostics().len(), before);
    }

    #[test]
    fn array_creation_and_element_access() {
        assert_eq!(bound_type("new int[5]").to_string(), "int[]");
        assert_eq!(bound_type("new int[5, 6]").to_string(), "int[,]");
        assert_eq!(bound_type("new int[3][]").to_string(), "int[][]");

        let mut binder = Binder::new();
        binder.enter_scope();
        binder.declare_local("a", TypeSymbol::Special(SpecialType::Int32).into_array(1));
        assert_eq!(
            bound_in_scope(&mut binder, "a[0]"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert!(binder.diagnostics().is_empty());
        binder.declare_local("n", TypeSymbol::Special(SpecialType::Int32));
        bound_in_scope(&mut binder, "n[0]");
        assert!(binder.diagnostics().iter().any(|d| d.code() == 21));
    }

    #[test]
    fn object_creation_resolves_constructors() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit = parse_compilation_unit(
            "class Point { Point(int x, int y) { } Point(int x) { } } class Empty { }",
        )
        .unit;
        let model = collect_model(&unit);
        let bound = |source: &str| {
            Binder::with_model(model.clone()).bind_expression(&parse_expression(source).expr)
        };
        let codes = |source: &str| {
            let mut binder = Binder::with_model(model.clone());
            binder.bind_expression(&parse_expression(source).expr);
            binder
                .into_diagnostics()
                .iter()
                .map(Diagnostic::code)
                .collect::<Vec<_>>()
        };

        assert_eq!(bound("new Point(1, 2)").ty.to_string(), "Point");
        assert!(codes("new Point(1, 2)").is_empty());
        assert_eq!(bound("new Empty()").ty.to_string(), "Empty");
        assert!(codes("new Empty()").is_empty());
        assert_eq!(codes("new Point(1, 2, 3)"), [1729]);
        assert_eq!(codes("new Point(true, 2)"), [1503]);
        assert_eq!(codes("new Gadget()"), [246]);
    }

    #[test]
    fn this_and_bare_names_resolve_against_the_enclosing_type() {
        use crate::symbols::{FieldSymbol, MethodSymbol, TypeInfo, TypeKind};
        let mut widget = TypeInfo::new("", "Widget", TypeKind::Class);
        widget.fields.push(FieldSymbol {
            name: "count".into(),
            ty: TypeSymbol::Special(SpecialType::Int32),
            is_static: false,
            is_readonly: false,
            accessibility: crate::symbols::Accessibility::Public,
            constant: None,
        });
        widget.methods.push(MethodSymbol {
            name: "Area".into(),
            return_type: TypeSymbol::Special(SpecialType::Double),
            parameters: Vec::new(),
            is_static: false,
            is_params: false,
            accessibility: crate::symbols::Accessibility::Public,
            conditional: Vec::new(),
        });
        let mut model = Model::new();
        model.insert(widget);

        let mut binder = Binder::with_model(model);
        binder.enter_type(TypeSymbol::Named(["Widget".into()].into()));
        binder.enter_scope();

        assert_eq!(bound_in_scope(&mut binder, "this").to_string(), "Widget");
        assert_eq!(
            bound_in_scope(&mut binder, "count"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert_eq!(
            bound_in_scope(&mut binder, "this.count"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert_eq!(
            bound_in_scope(&mut binder, "Area()"),
            TypeSymbol::Special(SpecialType::Double)
        );
        assert_eq!(
            bound_in_scope(&mut binder, "this.Area()"),
            TypeSymbol::Special(SpecialType::Double)
        );
        assert!(binder.diagnostics().is_empty());
        bound_in_scope(&mut binder, "missing");
        assert!(binder.diagnostics().iter().any(|d| d.code() == 103));
    }

    #[test]
    fn member_lookup_walks_the_base_chain() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit = parse_compilation_unit(
            "class Animal { public int legs; public int Speed() { } } \
             class Dog : Animal { public string breed; }",
        )
        .unit;
        let model = collect_model(&unit);
        let mut binder = Binder::with_model(model);
        binder.enter_scope();
        binder.declare_local("d", TypeSymbol::Named(["Dog".into()].into()));

        assert_eq!(
            bound_in_scope(&mut binder, "d.breed"),
            TypeSymbol::Special(SpecialType::String)
        );
        assert_eq!(
            bound_in_scope(&mut binder, "d.legs"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert_eq!(
            bound_in_scope(&mut binder, "d.Speed()"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert!(binder.diagnostics().is_empty());
    }

    #[test]
    fn base_access_resolves_against_the_base_class() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit = parse_compilation_unit(
            "class Animal { public int Speed() { return 0; } } class Dog : Animal { }",
        )
        .unit;
        let model = collect_model(&unit);
        let mut binder = Binder::with_model(model);
        binder.enter_type(TypeSymbol::Named(["Dog".into()].into()));
        binder.enter_scope();
        assert_eq!(
            bound_in_scope(&mut binder, "base.Speed()"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert!(binder.diagnostics().is_empty());
    }

    #[test]
    fn static_access_through_a_type_name() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit = parse_compilation_unit(
            "class Calc { public static int Zero; public static int Pi() { return 3; } }",
        )
        .unit;
        let model = collect_model(&unit);
        let mut binder = Binder::with_model(model);
        binder.enter_scope();

        assert_eq!(
            bound_in_scope(&mut binder, "Calc.Zero"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert_eq!(
            bound_in_scope(&mut binder, "Calc.Pi()"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert!(binder.diagnostics().is_empty());
        bound_in_scope(&mut binder, "Nope");
        assert!(binder.diagnostics().iter().any(|d| d.code() == 103));
    }

    #[test]
    fn enum_members_and_enum_casts() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit = parse_compilation_unit("enum Color { Red, Green, Blue }").unit;
        let model = collect_model(&unit);
        let mut binder = Binder::with_model(model);
        binder.enter_scope();

        assert_eq!(
            bound_in_scope(&mut binder, "Color.Red").to_string(),
            "Color"
        );
        assert_eq!(
            bound_in_scope(&mut binder, "(int)Color.Red"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert_eq!(bound_in_scope(&mut binder, "(Color)1").to_string(), "Color");
        assert!(binder.diagnostics().is_empty());
    }

    #[test]
    fn qualified_namespace_names_resolve_to_types() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit = parse_compilation_unit(
            "namespace A.B { class Widget { } } namespace A { class Top { } }",
        )
        .unit;
        let model = collect_model(&unit);
        let mut binder = Binder::with_model(model);
        binder.enter_scope();

        assert_eq!(
            bound_in_scope(&mut binder, "A.B.Widget").to_string(),
            "A.B.Widget"
        );
        assert_eq!(bound_in_scope(&mut binder, "A.Top").to_string(), "A.Top");
        assert!(binder.diagnostics().is_empty());
        bound_in_scope(&mut binder, "A.Nope");
        assert!(binder.diagnostics().iter().any(|d| d.code() == 234));
    }

    #[test]
    fn using_directives_resolve_unqualified_type_names() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit = parse_compilation_unit(
            "namespace System { class Console { } } \
             namespace Drawing { class Pen { } } \
             namespace Text { class Pen { } }",
        )
        .unit;
        let model = collect_model(&unit);

        let mut bare = Binder::with_model(model.clone());
        bare.enter_scope();
        bare.bind_expression(&parse_expression("Console").expr);
        assert!(bare.diagnostics().iter().any(|d| d.code() == 103));

        let mut binder = Binder::with_model(model.clone());
        binder.import_namespace("System");
        binder.enter_scope();
        assert_eq!(
            bound_in_scope(&mut binder, "Console").to_string(),
            "System.Console"
        );
        assert!(binder.diagnostics().is_empty());

        let mut ambiguous = Binder::with_model(model);
        ambiguous.import_namespace("Drawing");
        ambiguous.import_namespace("Text");
        ambiguous.enter_scope();
        ambiguous.bind_expression(&parse_expression("Pen").expr);
        assert!(ambiguous.diagnostics().iter().any(|d| d.code() == 104));
    }

    #[test]
    fn property_access_and_member_assignment() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit = parse_compilation_unit(
            "class Box { public int Width { get { return 0; } set { } } public int height; }",
        )
        .unit;
        let model = collect_model(&unit);
        let mut binder = Binder::with_model(model);
        binder.enter_scope();
        binder.declare_local("b", TypeSymbol::Named(["Box".into()].into()));

        assert_eq!(
            bound_in_scope(&mut binder, "b.Width"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        bound_in_scope(&mut binder, "b.height = 5");
        bound_in_scope(&mut binder, "b.Width = 5");
        assert!(binder.diagnostics().is_empty());
    }

    #[test]
    fn reference_conversions_follow_the_base_chain() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit = parse_compilation_unit(
            "class Animal { } class Dog : Animal { } class Pen { public void Hold(Animal a) { } }",
        )
        .unit;
        let model = collect_model(&unit);
        let mut binder = Binder::with_model(model);
        binder.enter_scope();
        binder.declare_local("a", TypeSymbol::Named(["Animal".into()].into()));
        binder.declare_local("d", TypeSymbol::Named(["Dog".into()].into()));
        binder.declare_local("p", TypeSymbol::Named(["Pen".into()].into()));

        bound_in_scope(&mut binder, "a = d");
        bound_in_scope(&mut binder, "p.Hold(d)");
        assert!(binder.diagnostics().is_empty());
        assert!(binder.converts(
            &TypeSymbol::Named(["Dog".into()].into()),
            &TypeSymbol::Special(SpecialType::Object)
        ));
        bound_in_scope(&mut binder, "d = a");
        assert!(binder.diagnostics().iter().any(|d| d.code() == 266));
    }

    #[test]
    fn method_binding_checks_return_and_scopes_parameters() {
        use lamella_syntax::parser::parse_statement;
        let int = TypeSymbol::Special(SpecialType::Int32);
        let void = TypeSymbol::Special(SpecialType::Void);

        let codes = |return_type: TypeSymbol, source: &str| {
            let mut binder = Binder::new();
            let body = parse_statement(source).statement;
            binder.bind_method(None, "M", return_type, &[], &body);
            binder
                .into_diagnostics()
                .iter()
                .map(Diagnostic::code)
                .collect::<Vec<_>>()
        };

        assert_eq!(codes(int.clone(), "{ return 1; }"), []);
        assert_eq!(codes(int.clone(), "{ return; }"), [126]);
        assert_eq!(codes(int.clone(), "{ return \"x\"; }"), [29]);
        assert_eq!(codes(void.clone(), "{ return 1; }"), [127]);
        assert_eq!(codes(void, "{ return; }"), []);

        let mut binder = Binder::new();
        let body = parse_statement("{ return n; }").statement;
        binder.bind_method(None, "M", int.clone(), &[("n".into(), int)], &body);
        assert!(binder.diagnostics().is_empty());
    }

    #[test]
    fn definite_assignment_reports_cs0165() {
        use lamella_syntax::parser::parse_statement;
        let int = TypeSymbol::Special(SpecialType::Int32);
        let void = TypeSymbol::Special(SpecialType::Void);
        let codes = |source: &str| {
            let mut binder = Binder::new();
            let body = parse_statement(source).statement;
            binder.bind_method(None, "M", void.clone(), &[], &body);
            binder
                .into_diagnostics()
                .iter()
                .filter(|diagnostic| {
                    diagnostic.severity() == lamella_syntax::diagnostic::Severity::Error
                })
                .map(Diagnostic::code)
                .collect::<Vec<_>>()
        };

        assert_eq!(codes("{ int x; int y = x; }"), [165]);
        assert_eq!(codes("{ int x; x = 1; int y = x; }"), []);
        assert_eq!(
            codes("{ bool c = true; int x; if (c) x = 1; int y = x; }"),
            [165]
        );
        assert_eq!(
            codes("{ bool c = true; int x; if (c) x = 1; else x = 2; int y = x; }"),
            []
        );
        assert_eq!(
            codes("{ bool c = true; int x; if (c) return; else x = 1; int y = x; }"),
            []
        );
        assert_eq!(codes("{ int x; if (true) x = 1; int y = x; }"), []);

        let mut binder = Binder::new();
        let body = parse_statement("{ int y = p; }").statement;
        binder.bind_method(None, "M", void, &[("p".into(), int)], &body);
        assert!(!binder.diagnostics().iter().any(|diagnostic| {
            diagnostic.severity() == lamella_syntax::diagnostic::Severity::Error
        }));
    }

    #[test]
    fn not_all_paths_return_is_cs0161() {
        use lamella_syntax::parser::parse_statement;
        let int = TypeSymbol::Special(SpecialType::Int32);
        let void = TypeSymbol::Special(SpecialType::Void);

        let codes = |return_type: TypeSymbol, source: &str| {
            let mut binder = Binder::new();
            let body = parse_statement(source).statement;
            binder.bind_method(None, "M", return_type, &[], &body);
            binder
                .into_diagnostics()
                .iter()
                .filter(|diagnostic| {
                    diagnostic.severity() == lamella_syntax::diagnostic::Severity::Error
                })
                .map(Diagnostic::code)
                .collect::<Vec<_>>()
        };

        assert_eq!(codes(int.clone(), "{ int x = 1; }"), [161]);
        assert_eq!(
            codes(int.clone(), "{ if (true) return 1; else return 2; }"),
            []
        );
        assert_eq!(codes(int.clone(), "{ while (true) { } }"), []);
        assert_eq!(codes(int, "{ throw; }"), []);
        assert_eq!(codes(void, "{ int x = 1; }"), []);
    }

    #[test]
    fn invocation_does_overload_resolution() {
        use crate::symbols::{MethodSymbol, TypeInfo, TypeKind};

        fn method(
            name: &str,
            return_type: TypeSymbol,
            parameters: Vec<TypeSymbol>,
        ) -> MethodSymbol {
            MethodSymbol {
                name: name.into(),
                return_type,
                parameters,
                is_static: false,
                is_params: false,
                accessibility: crate::symbols::Accessibility::Public,
                conditional: Vec::new(),
            }
        }
        let int = TypeSymbol::Special(SpecialType::Int32);
        let long = TypeSymbol::Special(SpecialType::Int64);
        let double = TypeSymbol::Special(SpecialType::Double);
        let void = TypeSymbol::Special(SpecialType::Void);

        let mut calc = TypeInfo::new("", "Calc", TypeKind::Class);
        calc.methods
            .push(method("F", int.clone(), alloc::vec![int.clone()]));
        calc.methods
            .push(method("F", double.clone(), alloc::vec![double.clone()]));
        calc.methods
            .push(method("Take", void.clone(), alloc::vec![int.clone()]));
        calc.methods.push(method(
            "G",
            void.clone(),
            alloc::vec![int.clone(), long.clone()],
        ));
        calc.methods
            .push(method("G", void, alloc::vec![long, int.clone()]));
        let mut model = Model::new();
        model.insert(calc);

        let call_codes = |source: &str| {
            let mut binder = Binder::with_model(model.clone());
            binder.enter_scope();
            binder.declare_local("c", TypeSymbol::Named(["Calc".into()].into()));
            binder.bind_expression(&parse_expression(source).expr);
            binder
                .into_diagnostics()
                .iter()
                .map(Diagnostic::code)
                .collect::<Vec<_>>()
        };
        let call_type = |source: &str| {
            let mut binder = Binder::with_model(model.clone());
            binder.enter_scope();
            binder.declare_local("c", TypeSymbol::Named(["Calc".into()].into()));
            binder.bind_expression(&parse_expression(source).expr).ty
        };

        assert_eq!(call_type("c.F(1)"), int);
        assert_eq!(call_type("c.F(1.0)"), double);
        assert_eq!(call_type("c.F(1L)"), double);
        assert!(call_codes("c.F(1)").is_empty());
        assert_eq!(call_codes("c.Take(1, 2)"), [1501]);
        assert_eq!(call_codes("c.Take(\"x\")"), [1503]);
        assert_eq!(call_codes("c.G(1, 1)"), [121]);
    }

    #[test]
    fn private_member_access_from_outside_is_cs0122() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes(
                "class Counter { int Bump() { return 0; } } \
                 class Program { static int Run() { Counter c = new Counter(); return c.Bump(); } }"
            ),
            [122]
        );
        assert_eq!(
            codes(
                "class Counter { public int Bump() { return 0; } } \
                 class Program { static int Run() { Counter c = new Counter(); return c.Bump(); } }"
            ),
            []
        );
    }

    #[test]
    fn static_instance_mismatch_is_cs0120_and_cs0176() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes(
                "class A { public int I() { return 1; } } \
                 class C { static int Run() { return A.I(); } }"
            ),
            [120]
        );
        assert_eq!(
            codes(
                "class A { public static int S() { return 1; } } \
                 class C { static int Run() { A a = new A(); return a.S(); } }"
            ),
            [176]
        );
        assert_eq!(
            codes(
                "class A { public static int S() { return 1; } public int I() { return 1; } } \
                 class C { static int Run() { A a = new A(); return A.S() + a.I(); } }"
            ),
            []
        );
    }

    #[test]
    fn switch_binds_constant_cases_and_flags_a_non_constant_label() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes(
                "class C { static int Run() { int x = 1; int y = 2; \
                 switch (x) { case y: return 1; default: return 0; } } }"
            ),
            [150]
        );
        assert_eq!(
            codes(
                "class C { static int Run() { int x = 1; \
                 switch (x) { case 1: return 1; default: return 0; } } }"
            ),
            []
        );
    }

    #[test]
    fn switch_duplicate_label_is_cs0152_and_fall_through_is_cs0163() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes(
                "class C { static int Run(int x) { \
                 switch (x) { case 1: return 1; case 1: return 2; default: return 0; } } }"
            ),
            [152]
        );
        assert_eq!(
            codes(
                "class C { static int Run(int x) { int y = 0; \
                 switch (x) { case 1: y = 1; default: y = 2; break; } return y; } }"
            ),
            [163]
        );
        assert_eq!(
            codes(
                "class C { static int Run(int x) { int y = 0; \
                 switch (x) { case 1: y = 1; break; default: break; } return y; } }"
            ),
            []
        );
    }

    #[test]
    fn duplicate_local_is_cs0128_and_shadowing_is_cs0136() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes("class C { static int Run() { int x = 1; int x = 2; return x; } }"),
            [128]
        );
        assert_eq!(
            codes("class C { static int Run() { int x = 1; { int x = 2; return x; } } }"),
            [136]
        );
        assert_eq!(
            codes("class C { static int Run() { int x = 1; { int y = 2; return x + y; } } }"),
            []
        );
    }

    #[test]
    fn non_statement_expression_is_cs0201() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes("class C { static void Run() { int x = 1; x + 1; } }"),
            [201]
        );
        assert_eq!(
            codes(
                "class C { static int Get() { return 1; } \
                 static void Run() { int x = 0; x = x + 1; Get(); } }"
            ),
            []
        );
    }

    #[test]
    fn narrowing_is_cs0266_but_unrelated_types_are_cs0029() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes("class C { static int Run() { int x = 3.14; return x; } }"),
            [266]
        );
        assert_eq!(
            codes("class C { static int Run() { int x = \"s\"; return x; } }"),
            [29]
        );
    }

    #[test]
    fn unused_locals_warn_cs0219_and_cs0168() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes("class C { static int Run() { int spare = 5; return 0; } }"),
            [219]
        );
        assert_eq!(
            codes("class C { static int Run() { int spare; return 0; } }"),
            [168]
        );
        assert_eq!(
            codes("class C { static int Run() { int x = 5; return x; } }"),
            []
        );
        assert_eq!(
            codes("class C { static int Run() { Bogus b = null; return 0; } }"),
            [246]
        );
    }

    #[test]
    fn unreachable_code_is_cs0162() {
        use lamella_syntax::parser::parse_compilation_unit;
        let codes = |source: &str| {
            let unit = parse_compilation_unit(source).unit;
            let mut codes: Vec<u16> = crate::bind_compilation_unit(&unit)
                .iter()
                .map(Diagnostic::code)
                .collect();
            codes.sort_unstable();
            codes
        };

        assert_eq!(
            codes("class C { static int M() { return 1; int x = 5; return x; } }"),
            [162]
        );
        assert_eq!(
            codes("class C { static int M() { while (true) { } int x = 5; return x; } }"),
            [162]
        );
        assert_eq!(
            codes(
                "class C { static int M(int p) { \
                 if (p > 0) return 1; else return 2; int x = 5; return x; } }"
            ),
            [162]
        );
        assert_eq!(
            codes("class C { static int M() { while (true) { break; } return 42; } }"),
            []
        );
    }

    #[test]
    fn a_call_records_its_resolved_method() {
        use crate::symbols::{MethodSymbol, TypeInfo, TypeKind};

        let int = TypeSymbol::Special(SpecialType::Int32);
        let mut calc = TypeInfo::new("", "Calc", TypeKind::Class);
        calc.methods.push(MethodSymbol {
            name: "F".into(),
            return_type: int.clone(),
            parameters: alloc::vec![int.clone()],
            is_static: false,
            is_params: false,
            accessibility: crate::symbols::Accessibility::Public,
            conditional: Vec::new(),
        });
        let mut model = Model::new();
        model.insert(calc);

        let mut binder = Binder::with_model(model);
        binder.enter_scope();
        let calc_type = TypeSymbol::Named(["Calc".into()].into());
        binder.declare_local("c", calc_type.clone());
        let call = binder.bind_expression(&parse_expression("c.F(1)").expr);

        let BoundExprKind::Call {
            method: Some(method),
            ..
        } = call.kind
        else {
            panic!("the call should record its resolved method");
        };
        assert_eq!(&*method.name, "F");
        assert_eq!(method.parameters, alloc::vec![int.clone()]);
        assert_eq!(method.return_type, int);
        assert!(!method.is_static);
        assert_eq!(method.declaring_type, calc_type);
    }

    #[test]
    fn scopes_nest_and_unwind() {
        let mut binder = Binder::new();
        binder.enter_scope();
        binder.declare_local("outer", TypeSymbol::Special(SpecialType::Int32));
        binder.enter_scope();
        binder.declare_local("inner", TypeSymbol::Special(SpecialType::Boolean));
        assert!(!bound_in_scope(&mut binder, "outer").is_error());
        assert!(!bound_in_scope(&mut binder, "inner").is_error());
        binder.exit_scope();
        assert!(!bound_in_scope(&mut binder, "outer").is_error());
        let before = binder.diagnostics().len();
        assert!(bound_in_scope(&mut binder, "inner").is_error());
        assert_eq!(binder.diagnostics().len(), before + 1);
    }
}
