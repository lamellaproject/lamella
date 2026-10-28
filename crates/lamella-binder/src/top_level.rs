//! What a program's TOP-LEVEL STATEMENTS (C# 9) mean to the program as a whole.
//!
//! The parser has already made a compilation unit's statements the body of `Program.<Main>$` (see
//! [`lamella_syntax::ast::TOP_LEVEL_ENTRY_POINT`]), so the binder binds them as it binds any
//! method. What is left is what no single method can answer: which unit's statements are the
//! program, that they need an executable, that every other `Main` is set aside, and which names
//! they make unusable from the rest of the program.

use crate::bind::bind_type;
use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::special::SpecialType;
use crate::types::TypeSymbol;
use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::vec::Vec;
use lamella_syntax::ast::{
    Argument, CompilationUnit, DeconstructionTarget, Expr, ExprKind, Initializer, InterpolationPart,
    Member, MemberInitializerValue, Modifier, NamespaceMember, Parameter, Pattern, PredefinedType,
    Stmt, StmtKind, TypeDecl, TypeRef, TypeRefKind, UsingResource,
    TOP_LEVEL_ENTRY_POINT,
};
use lamella_syntax::lexer::OutputKind;
use lamella_syntax::span::Span;

/// The part of `Program` a unit's top-level statements declare, if the unit has any. The parser
/// places it first among the unit's members.
#[must_use]
pub fn top_level_part(unit: &CompilationUnit) -> Option<&TypeDecl> {
    match unit.members.first() {
        Some(NamespaceMember::Type(declaration)) if declaration.from_top_level_statements => {
            Some(declaration)
        }
        _ => None,
    }
}

/// Where a diagnostic about a unit's top-level statements as a whole is reported: where the first
/// of them begins, as csc reports `CS8802` and `CS8805`.
fn first_statement(part: &TypeDecl) -> Span {
    Span::empty_at(part.span.start)
}

/// `CS8802`: only one compilation unit may have top-level statements. The first unit that has
/// them keeps them; every later one is reported at its first statement and loses them, so its
/// statements are not bound at all -- as csc binds none of them (measured: a type error among a
/// second unit's statements draws `CS8802` alone).
///
/// `None` when at most one unit has top-level statements, which is every program that compiles.
/// Otherwise the units with the later parts removed, and each report with the index of the unit
/// it belongs to.
#[must_use]
pub fn without_later_top_level_statements(
    units: &[CompilationUnit],
) -> Option<(Vec<CompilationUnit>, Vec<(usize, Diagnostic)>)> {
    let mut with_statements = units
        .iter()
        .enumerate()
        .filter(|(_, unit)| top_level_part(unit).is_some())
        .map(|(index, _)| index);
    with_statements.next()?;
    let later: Vec<usize> = with_statements.collect();
    if later.is_empty() {
        return None;
    }
    let mut kept = units.to_vec();
    let mut reports = Vec::new();
    for index in later {
        if let Some(part) = top_level_part(&kept[index]) {
            reports.push((
                index,
                Diagnostic::new(
                    DiagnosticKind::TopLevelStatementsInSeveralUnits,
                    first_statement(part),
                ),
            ));
        }
        kept[index].members.remove(0);
    }
    Some((kept, reports))
}

/// The whole-program entry-point rules when the program has top-level statements, with the index
/// of the unit each report belongs to; `None` when it has none, and the ordinary rules apply.
///
/// The statements ARE the entry point (C# 9), so there is no `CS0017` to give and no `CS5001`:
///
/// - `CS8805` when a library is being compiled, at the first statement;
/// - `CS7022` (a warning) at every method that would otherwise be an entry point, at its name,
///   since it is set aside rather than chosen.
///
#[must_use]
pub fn top_level_entry_point_diagnostics(
    units: &[CompilationUnit],
    target: OutputKind,
) -> Option<Vec<(usize, Diagnostic)>> {
    let (index, part) = units
        .iter()
        .enumerate()
        .find_map(|(index, unit)| top_level_part(unit).map(|part| (index, part)))?;
    let mut reports = Vec::new();
    if target == OutputKind::Library {
        reports.push((
            index,
            Diagnostic::new(
                DiagnosticKind::TopLevelStatementsNeedExecutable,
                first_statement(part),
            ),
        ));
    }
    for (unit_index, unit) in units.iter().enumerate() {
        for member in &unit.members {
            if let NamespaceMember::Namespace(namespace) = member {
                ignored_in_namespace(&namespace.members, unit_index, &mut reports);
            }
        }
    }
    for (unit_index, unit) in units.iter().enumerate() {
        for member in &unit.members {
            if let NamespaceMember::Type(declaration) = member {
                ignored_in_type(declaration, &mut Vec::new(), unit_index, &mut reports);
            }
        }
    }
    Some(reports)
}

/// Every set-aside `Main` in a namespace body: its nested namespaces' first, then its types'.
fn ignored_in_namespace(
    members: &[NamespaceMember],
    unit: usize,
    reports: &mut Vec<(usize, Diagnostic)>,
) {
    for member in members {
        if let NamespaceMember::Namespace(namespace) = member {
            ignored_in_namespace(&namespace.members, unit, reports);
        }
    }
    for member in members {
        if let NamespaceMember::Type(declaration) = member {
            ignored_in_type(declaration, &mut Vec::new(), unit, reports);
        }
    }
}

/// Every set-aside `Main` in `declaration`, then in the types nested in it. `enclosing` is the
/// names of the types around it, which is how csc names the method.
fn ignored_in_type(
    declaration: &TypeDecl,
    enclosing: &mut Vec<Box<str>>,
    unit: usize,
    reports: &mut Vec<(usize, Diagnostic)>,
) {
    if declaration.from_top_level_statements || !declaration.type_parameters.is_empty() {
        return;
    }
    enclosing.push(declaration.name.clone());
    for member in &declaration.members {
        if let Member::Method {
            modifiers,
            return_type,
            name,
            name_span,
            type_parameters,
            parameters,
            ..
        } = member
        {
            if &**name == "Main"
                && modifiers.contains(&Modifier::Static)
                && type_parameters.is_empty()
                && returns_entry_point_type(return_type)
                && takes_entry_point_parameters(parameters)
            {
                let mut method = enclosing.join(".");
                method.push_str(if parameters.is_empty() { ".Main()" } else { ".Main(string[])" });
                reports.push((
                    unit,
                    Diagnostic::new(
                        DiagnosticKind::TopLevelStatementsIgnoreMain {
                            method: method.into(),
                        },
                        *name_span,
                    ),
                ));
            }
        }
    }
    for member in &declaration.members {
        if let Member::NestedType(nested) = member {
            if let NamespaceMember::Type(inner) = &**nested {
                ignored_in_type(inner, enclosing, unit, reports);
            }
        }
    }
    enclosing.pop();
}

/// Whether `ty` is a type an entry point may return (ECMA-334 7.1): `void`, `int`, `Task` or
/// `Task<int>`. The task types are recognized by how they are written -- `Task`, or
/// `System.Threading.Tasks.Task` -- since this runs over the whole program, outside the scope that
/// would resolve the name.
fn returns_entry_point_type(ty: &TypeRef) -> bool {
    let bound = bind_type(ty);
    if bound.is_void() || matches!(bound, TypeSymbol::Special(SpecialType::Int32)) {
        return true;
    }
    let is_task_name = |parts: &[&str]| matches!(parts, ["Task"] | ["System", "Threading", "Tasks", "Task"]);
    match &ty.kind {
        TypeRefKind::Name(parts) => {
            let parts: Vec<&str> = parts.iter().map(|part| &**part).collect();
            is_task_name(&parts)
        }
        TypeRefKind::Generic { parts } => {
            let names: Vec<&str> = parts.iter().map(|part| &*part.name).collect();
            let arguments_before_last = parts[..parts.len().saturating_sub(1)]
                .iter()
                .all(|part| part.arguments.is_empty());
            is_task_name(&names)
                && arguments_before_last
                && matches!(
                    parts.last().map(|part| part.arguments.as_slice()),
                    Some([argument]) if matches!(argument.kind, TypeRefKind::Predefined(PredefinedType::Int))
                )
        }
        _ => false,
    }
}

/// Whether `parameters` is a parameter list an entry point may take (ECMA-334 7.1): none, or one
/// value parameter of type `string[]`.
fn takes_entry_point_parameters(parameters: &[Parameter]) -> bool {
    match parameters {
        [] => true,
        [only] => {
            only.modifier.is_none()
                && matches!(
                    bind_type(&only.ty),
                    TypeSymbol::Array { element, rank: 1 }
                        if matches!(*element, TypeSymbol::Special(SpecialType::String))
                )
        }
        _ => false,
    }
}

/// The names a program's top-level statements declare at their OUTERMOST level -- the local
/// variables code outside them may not use (`CS8801`), however the name would otherwise resolve.
///
#[must_use]
pub fn top_level_local_names(part: &TypeDecl) -> BTreeSet<Box<str>> {
    let mut names = BTreeSet::new();
    for member in &part.members {
        if let Member::Method {
            name,
            body: Some(body),
            ..
        } = member
        {
            if &**name == TOP_LEVEL_ENTRY_POINT {
                if let StmtKind::Block(statements) = &body.kind {
                    statement_list_names(statements, &mut names);
                }
            }
        }
    }
    names
}

fn statement_list_names(statements: &[Stmt], names: &mut BTreeSet<Box<str>>) {
    for statement in statements {
        statement_names(statement, names);
    }
}

fn statement_names(statement: &Stmt, names: &mut BTreeSet<Box<str>>) {
    match &statement.kind {
        StmtKind::LocalDeclaration { declarators, .. } => {
            for declarator in declarators {
                names.insert(declarator.name.clone());
                if let Some(initializer) = &declarator.initializer {
                    expression_variables(initializer, names);
                }
            }
        }
        StmtKind::Expression(expression)
        | StmtKind::Return(Some(expression))
        | StmtKind::Throw(Some(expression)) => expression_variables(expression, names),
        StmtKind::If { condition, .. } => expression_variables(condition, names),
        StmtKind::Labeled { statement, .. } => statement_names(statement, names),
        StmtKind::LocalFunction(function) => {
            names.insert(function.name.clone());
        }
        StmtKind::Using {
            resource,
            body,
            declaration: true,
        } => {
            match resource {
                UsingResource::Declaration { declarators, .. } => {
                    for declarator in declarators {
                        names.insert(declarator.name.clone());
                        if let Some(initializer) = &declarator.initializer {
                            expression_variables(initializer, names);
                        }
                    }
                }
                UsingResource::Expression(expression) => expression_variables(expression, names),
            }
            if let StmtKind::Block(rest) = &body.kind {
                statement_list_names(rest, names);
            }
        }
        _ => {}
    }
}

/// The variables an expression declares in the scope it is evaluated in: an out variable, a
/// declaration pattern's, and a deconstruction's -- but none inside a lambda, whose body is a scope
/// of its own, or inside a switch expression's arm, which is the arm's.
fn expression_variables(expression: &Expr, names: &mut BTreeSet<Box<str>>) {
    match &expression.kind {
        ExprKind::DeclarationExpression { name, .. } => {
            names.insert(name.clone());
        }
        ExprKind::TypeTest {
            operand, target, ..
        } => {
            expression_variables(operand, names);
            pattern_variables(target, names);
        }
        ExprKind::Deconstruction { targets, value, .. } => {
            deconstruction_variables(targets, names);
            expression_variables(value, names);
        }
        ExprKind::SwitchExpression { governing, .. } => expression_variables(governing, names),
        ExprKind::Lambda { .. } => {}
        ExprKind::Parenthesized(inner)
        | ExprKind::Await(inner)
        | ExprKind::Throw(inner)
        | ExprKind::Dereference(inner)
        | ExprKind::AddressOf(inner)
        | ExprKind::Checked(inner)
        | ExprKind::Unchecked(inner)
        | ExprKind::MakeRef(inner)
        | ExprKind::RefType(inner) => expression_variables(inner, names),
        ExprKind::Unary { operand, .. }
        | ExprKind::PostfixUnary { operand, .. }
        | ExprKind::RefArgument { operand, .. }
        | ExprKind::Cast { operand, .. } => expression_variables(operand, names),
        ExprKind::RefValue { reference, .. } => expression_variables(reference, names),
        ExprKind::MemberAccess { receiver, .. } => expression_variables(receiver, names),
        ExprKind::ConstructedType { name, .. } => expression_variables(name, names),
        ExprKind::ConditionalAccess { receiver, access } => {
            expression_variables(receiver, names);
            expression_variables(access, names);
        }
        ExprKind::Invocation {
            receiver, arguments, ..
        }
        | ExprKind::ElementAccess {
            receiver, arguments, ..
        } => {
            expression_variables(receiver, names);
            argument_variables(arguments, names);
        }
        ExprKind::ArgListCall(arguments) => argument_variables(arguments, names),
        ExprKind::Tuple { elements } => {
            for element in elements {
                expression_variables(&element.value, names);
            }
        }
        ExprKind::Binary { left, right, .. }
        | ExprKind::NullCoalescing { left, right }
        | ExprKind::Assignment {
            target: left,
            value: right,
            ..
        } => {
            expression_variables(left, names);
            expression_variables(right, names);
        }
        ExprKind::Conditional {
            condition,
            when_true,
            when_false,
        } => {
            expression_variables(condition, names);
            expression_variables(when_true, names);
            expression_variables(when_false, names);
        }
        ExprKind::InterpolatedString(parts) => {
            for part in parts {
                if let InterpolationPart::Hole {
                    expression,
                    alignment,
                    ..
                } = part
                {
                    expression_variables(expression, names);
                    if let Some(alignment) = alignment {
                        expression_variables(alignment, names);
                    }
                }
            }
        }
        ExprKind::StackAlloc {
            count, initializer, ..
        } => {
            if let Some(count) = count {
                expression_variables(count, names);
            }
            for element in initializer.iter().flatten() {
                expression_variables(element, names);
            }
        }
        ExprKind::ObjectCreation {
            arguments,
            initializer,
            ..
        } => {
            argument_variables(arguments, names);
            if let Some(initializer) = initializer {
                initializer_variables(initializer, names);
            }
        }
        ExprKind::ArrayCreation {
            lengths,
            initializer,
            ..
        } => {
            for length in lengths {
                expression_variables(length, names);
            }
            if let Some(initializer) = initializer {
                expression_variables(initializer, names);
            }
        }
        ExprKind::ArrayInitializer(elements) => {
            for element in elements {
                expression_variables(element, names);
            }
        }
        ExprKind::Literal(_)
        | ExprKind::Name { .. }
        | ExprKind::PredefinedType(_)
        | ExprKind::This
        | ExprKind::Base
        | ExprKind::ConditionalReceiver
        | ExprKind::TypeOf(_)
        | ExprKind::SizeOf(_)
        | ExprKind::DefaultValue(_)
        | ExprKind::ArgListHandle
        | ExprKind::Error => {}
    }
}

fn argument_variables(arguments: &[Argument], names: &mut BTreeSet<Box<str>>) {
    for argument in arguments {
        expression_variables(&argument.value, names);
    }
}

fn initializer_variables(initializer: &Initializer, names: &mut BTreeSet<Box<str>>) {
    match initializer {
        Initializer::Object(members) => {
            for member in members {
                match &member.value {
                    MemberInitializerValue::Expression(value) => {
                        expression_variables(value, names);
                    }
                    MemberInitializerValue::Nested(nested) => initializer_variables(nested, names),
                }
            }
        }
        Initializer::Collection(elements) => {
            for element in elements {
                for argument in &element.arguments {
                    expression_variables(argument, names);
                }
            }
        }
    }
}

fn pattern_variables(pattern: &Pattern, names: &mut BTreeSet<Box<str>>) {
    match pattern {
        Pattern::Declaration { name, .. } => {
            names.insert(name.clone());
        }
        Pattern::Constant(value) => expression_variables(value, names),
        Pattern::Type(_) | Pattern::Discard(_) => {}
    }
}

fn deconstruction_variables(targets: &[DeconstructionTarget], names: &mut BTreeSet<Box<str>>) {
    for target in targets {
        match target {
            DeconstructionTarget::Declaration { name, .. } => {
                names.insert(name.clone());
            }
            DeconstructionTarget::Expression(expression) => expression_variables(expression, names),
            DeconstructionTarget::Nested { targets, .. } => {
                deconstruction_variables(targets, names);
            }
            DeconstructionTarget::Discard(_) => {}
        }
    }
}
