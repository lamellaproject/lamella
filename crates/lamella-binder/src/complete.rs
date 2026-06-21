//! Positional semantic completion (the IntelliSense engine), clause-agnostic so both
//! the in-browser IDE (via the wasm `lamella_complete` ABI) and an LSP server can drive
//! the same logic. The first cut answers **member completion**: at `receiver.<caret>`,
//! the accessible members of the receiver's type -- its own and every inherited/BCL one.

use crate::bind::bind_type;
use crate::special::SpecialType;
use crate::symbols::{MethodSymbol, Model};
use crate::types::TypeSymbol;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use lamella_syntax::ast::{
    CompilationUnit, Member, NamespaceMember, Stmt, StmtKind, TypeDecl, TypeRef,
};
use lamella_syntax::version::{Feature, LanguageVersion};

/// What a completion item names, so the IDE can pick an icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    /// A field.
    Field,
    /// A property.
    Property,
    /// A method (a method group; overloads collapse to one entry).
    Method,
    /// A type.
    Type,
    /// A namespace (offered after a namespace qualifier, e.g. `System.` -> `Collections`).
    Namespace,
    /// A local variable.
    Local,
    /// A method parameter.
    Parameter,
    /// A language keyword.
    Keyword,
}

/// One completion suggestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The text shown in the list (the member name).
    pub label: Box<str>,
    /// The member category.
    pub kind: CompletionKind,
    /// A short type/signature hint (the member's type, or a method's signature with an
    /// overload count).
    pub detail: Box<str>,
    /// The text to insert when the item is chosen. The bare name for most items; a
    /// method inserts `name(` so the editor can offer signature help for the arguments.
    pub insert_text: Box<str>,
}

impl Completion {
    /// An item whose inserted text is just its label (a field, type, local, keyword,
    /// namespace -- everything but a method).
    fn simple(
        label: impl Into<Box<str>>,
        kind: CompletionKind,
        detail: impl Into<Box<str>>,
    ) -> Completion {
        let label = label.into();
        Completion {
            insert_text: label.clone(),
            label,
            kind,
            detail: detail.into(),
        }
    }
}

/// Completions for the caret at byte `offset` in `source`, given the parsed `unit` and
/// the bound `model`. After a `.`, the receiver's members (a value/type) or the members
/// of a namespace (`System.` -> its types and child namespaces); otherwise the names in
/// scope. Generics (a C# 2.0 feature) are never offered for the 1.0 target.
#[must_use]
pub fn complete(
    source: &str,
    unit: &CompilationUnit,
    model: &Model,
    offset: usize,
) -> Vec<Completion> {
    let mut items = if let Some(receiver) = receiver_expression(source, offset) {
        qualified_completions(unit, model, offset, receiver)
    } else {
        scope_completions(source, unit, model, offset)
    };
    if !LanguageVersion::DEFAULT.supports(Feature::Generics) {
        items.retain(|item| !item.label.contains('`'));
    }
    items
}

/// Completions after `<receiver>.`: the members of the receiver expression when it
/// resolves to a typed value/type (a name `a`, a member chain `a.b.c`, or a call
/// `Foo().Bar`), otherwise the members (types and child namespaces) of the namespace the
/// text names.
fn qualified_completions(
    unit: &CompilationUnit,
    model: &Model,
    offset: usize,
    receiver: &str,
) -> Vec<Completion> {
    if let Some(ty) = expression_type(unit, model, offset, receiver) {
        return type_members(model, &ty);
    }
    namespace_members(model, receiver)
}

/// The type a dotted receiver expression resolves to. It splits on top-level dots (those
/// outside any parentheses) into segments; each segment is a name or a call `name(...)`.
/// The first segment is a local/parameter/field/type, or -- if a call -- a method of the
/// enclosing type; each later segment is a member (field/property/method) of the type so
/// far, a method or a call contributing its return type. `None` if any segment does not
/// resolve. Covers `a`, `a.b.c`, `Foo()`, and `Foo().Bar` style receivers (20.x).
fn expression_type(
    unit: &CompilationUnit,
    model: &Model,
    offset: usize,
    expression: &str,
) -> Option<TypeSymbol> {
    let mut segments = top_level_segments(expression).into_iter();
    let (first, first_is_call) = segment_name(segments.next()?);
    let mut ty = if first_is_call {
        let (namespace, type_decl, _) = enclosing_method(unit, offset)?;
        member_type(model, &type_symbol(&namespace, &type_decl.name), first)?
    } else {
        receiver_type(unit, model, offset, first)?
    };
    for segment in segments {
        ty = member_type(model, &ty, segment_name(segment).0)?;
    }
    Some(ty)
}

/// Splits a receiver expression on dots that are not inside parentheses, so a call's
/// argument list does not break the chain (`a.Foo(x.y).b` -> `a`, `Foo(x.y)`, `b`).
fn top_level_segments(expression: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut depth: i32 = 0;
    let mut start = 0;
    for (index, byte) in expression.bytes().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'.' if depth == 0 => {
                segments.push(&expression[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    segments.push(&expression[start..]);
    segments
}

/// A segment's member name and whether it is a call: `Foo(args)` -> (`Foo`, true), a
/// plain `Foo` -> (`Foo`, false).
fn segment_name(segment: &str) -> (&str, bool) {
    match segment.find('(') {
        Some(paren) => (segment[..paren].trim(), true),
        None => (segment.trim(), false),
    }
}

/// The type of the member named `name` on `ty` or any base in its chain: a field's or
/// property's type, or a method's return type (a method group resolves to its return
/// type for the purpose of continuing a `.` chain). `None` if no such member exists.
fn member_type(model: &Model, ty: &TypeSymbol, name: &str) -> Option<TypeSymbol> {
    let mut current = model.get_by_symbol(&member_lookup_type(ty));
    while let Some(info) = current {
        if let Some(field) = info.find_field(name) {
            return Some(field.ty.clone());
        }
        if let Some(property) = info.find_property(name) {
            return Some(property.ty.clone());
        }
        if let Some(method) = info.methods_named(name).next() {
            return Some(method.return_type.clone());
        }
        current = info.base.as_ref().and_then(|base| model.get_by_symbol(base));
    }
    None
}

/// The model type to look members up on: an array exposes `System.Array`; every other
/// type is looked up as itself (a predefined type resolves to its `System.<Name>`).
fn member_lookup_type(ty: &TypeSymbol) -> TypeSymbol {
    match ty {
        TypeSymbol::Array { .. } => {
            TypeSymbol::Named([Box::from("System"), Box::from("Array")].into())
        }
        other => other.clone(),
    }
}

/// Completions for an identifier position (not after `.`): the locals and parameters in
/// the enclosing method, that type's accessible members, prefix-matched visible types,
/// and the keywords -- filtered to what the partial identifier at the caret prefixes.
fn scope_completions(
    source: &str,
    unit: &CompilationUnit,
    model: &Model,
    offset: usize,
) -> Vec<Completion> {
    let prefix = ident_prefix(source, offset);
    let mut out = Vec::new();
    if let Some((namespace, type_decl, method)) = enclosing_method(unit, offset) {
        if let Member::Method {
            parameters, body, ..
        } = method
        {
            for parameter in parameters {
                out.push(Completion::simple(
                    parameter.name.clone(),
                    CompletionKind::Parameter,
                    type_label(&bind_type(&parameter.ty)),
                ));
            }
            if let Some(body) = body {
                walk_locals(body, &mut |ty, name| {
                    out.push(Completion::simple(
                        name,
                        CompletionKind::Local,
                        type_label(&bind_type(ty)),
                    ));
                });
            }
        }
        let enclosing = type_symbol(&namespace, &type_decl.name);
        out.extend(type_members(model, &enclosing));
    }
    if !prefix.is_empty() {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for name in model.type_names() {
            if name.starts_with(prefix) && seen.insert(name.to_string()) {
                out.push(Completion::simple(name, CompletionKind::Type, "type"));
            }
        }
    }
    for keyword in KEYWORDS {
        out.push(Completion::simple(
            *keyword,
            CompletionKind::Keyword,
            "keyword",
        ));
    }
    if !prefix.is_empty() {
        out.retain(|item| item.label.starts_with(prefix));
    }
    out
}

/// The partial identifier ending at the caret (`Con|` -> `"Con"`), empty if the caret is
/// not on identifier text.
fn ident_prefix(source: &str, offset: usize) -> &str {
    let bytes = source.as_bytes();
    let end = offset.min(bytes.len());
    let mut start = end;
    while start > 0 && is_ident_byte(bytes[start - 1]) {
        start -= 1;
    }
    source.get(start..end).unwrap_or("")
}

/// The C# keywords offered in identifier position (1.0 set).
const KEYWORDS: &[&str] = &[
    "abstract", "as", "base", "bool", "break", "byte", "case", "catch", "char", "checked",
    "class", "const", "continue", "default", "delegate", "do", "double", "else", "enum",
    "event", "explicit", "extern", "false", "finally", "fixed", "float", "for", "foreach",
    "goto", "if", "implicit", "in", "int", "interface", "internal", "is", "lock", "long",
    "namespace", "new", "null", "object", "operator", "out", "override", "params", "private",
    "protected", "public", "readonly", "ref", "return", "sbyte", "sealed", "short", "sizeof",
    "static", "string", "struct", "switch", "this", "throw", "true", "try", "typeof", "uint",
    "ulong", "unchecked", "unsafe", "ushort", "using", "virtual", "void", "volatile", "while",
];

/// If the caret sits just after `<receiver>.<partial>`, the receiver expression text.
/// Reads backwards over the partial member name, the `.`, then the receiver: a run of
/// identifiers, dots, and balanced `(...)` call lists (`a`, `System.Collections`,
/// `Foo()`, `a.Foo(x).b`). `None` when the caret is not in member-access position or the
/// parentheses are unbalanced.
fn receiver_expression(source: &str, offset: usize) -> Option<&str> {
    let bytes = source.as_bytes();
    let mut index = offset.min(bytes.len());
    while index > 0 && is_ident_byte(bytes[index - 1]) {
        index -= 1;
    }
    if index == 0 || bytes[index - 1] != b'.' {
        return None;
    }
    let end = index - 1;
    let mut start = end;
    while start > 0 {
        let byte = bytes[start - 1];
        if is_ident_byte(byte) || byte == b'.' {
            start -= 1;
        } else if byte == b')' {
            let mut depth = 0;
            loop {
                if start == 0 {
                    return None;
                }
                start -= 1;
                match bytes[start] {
                    b')' => depth += 1,
                    b'(' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        } else {
            break;
        }
    }
    let receiver = source.get(start..end)?.trim();
    if receiver.is_empty() || receiver.starts_with('.') {
        return None;
    }
    Some(receiver)
}

/// The members of the namespace `namespace`: the simple names of the types declared
/// directly in it, plus its immediate child namespace segments (`System` -> `Console`,
/// `String`, ..., and `Collections`, `IO`, ...). Deduplicated.
fn namespace_members(model: &Model, namespace: &str) -> Vec<Completion> {
    let mut out = Vec::new();
    let mut seen_types: BTreeSet<&str> = BTreeSet::new();
    let mut seen_namespaces: BTreeSet<&str> = BTreeSet::new();
    for (type_namespace, name) in model.type_keys() {
        if type_namespace == namespace {
            if seen_types.insert(name) {
                out.push(Completion::simple(name, CompletionKind::Type, "type"));
            }
        } else if let Some(child) = child_namespace_segment(namespace, type_namespace) {
            if seen_namespaces.insert(child) {
                out.push(Completion::simple(
                    child,
                    CompletionKind::Namespace,
                    "namespace",
                ));
            }
        }
    }
    out
}

/// The immediate child segment of `namespace` along `candidate`, or `None` if
/// `candidate` is not a strict descendant. `("System", "System.Collections.Generic")`
/// -> `"Collections"`; an empty `namespace` takes the first segment of `candidate`.
fn child_namespace_segment<'a>(namespace: &str, candidate: &'a str) -> Option<&'a str> {
    let rest = if namespace.is_empty() {
        candidate
    } else {
        candidate.strip_prefix(namespace)?.strip_prefix('.')?
    };
    if rest.is_empty() {
        return None;
    }
    Some(rest.split('.').next().unwrap_or(rest))
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Resolves the receiver identifier to its type: a parameter or local of the enclosing
/// method, else a field of the enclosing type, else `receiver` used as a type name.
fn receiver_type(
    unit: &CompilationUnit,
    model: &Model,
    offset: usize,
    receiver: &str,
) -> Option<TypeSymbol> {
    if let Some((_, type_decl, method)) = enclosing_method(unit, offset) {
        if let Member::Method { parameters, .. } = method {
            for parameter in parameters {
                if &*parameter.name == receiver {
                    return Some(bind_type(&parameter.ty));
                }
            }
        }
        if let Member::Method { body: Some(body), .. } = method {
            if let Some(ty) = local_type(body, receiver) {
                return Some(bind_type(ty));
            }
        }
        for member in &type_decl.members {
            if let Member::Field { ty, declarators, .. } = member {
                if declarators.iter().any(|d| &*d.name == receiver) {
                    return Some(bind_type(ty));
                }
            }
        }
    }
    resolve_type_name(model, receiver)
}

/// The declared type of a local named `name` anywhere in `body` (a first cut ignores
/// scope nesting and shadowing -- the most recent textual declaration wins).
fn local_type<'a>(body: &'a Stmt, name: &str) -> Option<&'a TypeRef> {
    let mut found = None;
    walk_locals(body, &mut |ty, declared| {
        if declared == name {
            found = Some(ty);
        }
    });
    found
}

/// Visits every `T name` local declaration in `stmt` (and nested blocks/loops).
fn walk_locals<'a>(stmt: &'a Stmt, visit: &mut dyn FnMut(&'a TypeRef, &str)) {
    match &stmt.kind {
        StmtKind::LocalDeclaration { ty, declarators } => {
            for declarator in declarators {
                visit(ty, &declarator.name);
            }
        }
        StmtKind::Block(statements) => {
            for inner in statements {
                walk_locals(inner, visit);
            }
        }
        StmtKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            walk_locals(then_branch, visit);
            if let Some(branch) = else_branch {
                walk_locals(branch, visit);
            }
        }
        StmtKind::While { body, .. }
        | StmtKind::DoWhile { body, .. }
        | StmtKind::ForEach { body, .. }
        | StmtKind::Lock { body, .. }
        | StmtKind::Using { body, .. }
        | StmtKind::Checked(body)
        | StmtKind::Unchecked(body)
        | StmtKind::Labeled {
            statement: body, ..
        } => walk_locals(body, visit),
        StmtKind::For {
            initializer, body, ..
        } => {
            if let Some(lamella_syntax::ast::ForInitializer::Declaration { ty, declarators }) =
                initializer
            {
                for declarator in declarators {
                    visit(ty, &declarator.name);
                }
            }
            walk_locals(body, visit);
        }
        StmtKind::Try {
            body,
            catches,
            finally_block,
        } => {
            walk_locals(body, visit);
            for catch in catches {
                walk_locals(&catch.body, visit);
            }
            if let Some(block) = finally_block {
                walk_locals(block, visit);
            }
        }
        _ => {}
    }
}

/// The (namespace, type, method member) whose method body span contains `offset`.
fn enclosing_method(
    unit: &CompilationUnit,
    offset: usize,
) -> Option<(String, &TypeDecl, &Member)> {
    let offset = offset as u32;
    let mut found = None;
    for member in &unit.members {
        find_method_in_member(member, "", offset, &mut found);
    }
    found
}

fn find_method_in_member<'a>(
    member: &'a NamespaceMember,
    namespace: &str,
    offset: u32,
    found: &mut Option<(String, &'a TypeDecl, &'a Member)>,
) {
    match member {
        NamespaceMember::Type(type_decl) => {
            for type_member in &type_decl.members {
                if let Member::Method {
                    body: Some(body), ..
                } = type_member
                {
                    if body.span.start <= offset && offset <= body.span.end {
                        *found = Some((namespace.into(), type_decl, type_member));
                    }
                }
            }
        }
        NamespaceMember::Namespace(declaration) => {
            let mut inner = String::from(namespace);
            for part in &declaration.name.parts {
                if !inner.is_empty() {
                    inner.push('.');
                }
                inner.push_str(part);
            }
            for member in &declaration.members {
                find_method_in_member(member, &inner, offset, found);
            }
        }
        _ => {}
    }
}

/// A named-type symbol from a namespace (possibly empty/dotted) and a simple name.
fn type_symbol(namespace: &str, name: &str) -> TypeSymbol {
    let mut parts: Vec<Box<str>> = Vec::new();
    if !namespace.is_empty() {
        for part in namespace.split('.') {
            parts.push(part.into());
        }
    }
    parts.push(name.into());
    TypeSymbol::Named(parts.into_boxed_slice())
}

/// Resolves a written type name to a model symbol: a predefined keyword (`string`), an
/// exact model entry, else a uniquely-named model type (stands in for `using` lookup).
fn resolve_type_name(model: &Model, name: &str) -> Option<TypeSymbol> {
    if let Some(special) = special_keyword(name) {
        return Some(TypeSymbol::Special(special));
    }
    if model.get("", name).is_some() {
        return Some(TypeSymbol::Named([Box::from(name)].into()));
    }
    model.type_with_simple_name(name)
}

/// The predefined `SpecialType` a primitive keyword names (`string`, `int`, ...), so
/// `string.` etc. resolve to System.String's members.
fn special_keyword(name: &str) -> Option<SpecialType> {
    Some(match name {
        "string" => SpecialType::String,
        "object" => SpecialType::Object,
        "bool" => SpecialType::Boolean,
        "char" => SpecialType::Char,
        "byte" => SpecialType::Byte,
        "sbyte" => SpecialType::SByte,
        "short" => SpecialType::Int16,
        "ushort" => SpecialType::UInt16,
        "int" => SpecialType::Int32,
        "uint" => SpecialType::UInt32,
        "long" => SpecialType::Int64,
        "ulong" => SpecialType::UInt64,
        "float" => SpecialType::Single,
        "double" => SpecialType::Double,
        _ => return None,
    })
}

/// A method group accumulated across a type's base chain: the most-derived overload's
/// signature for display, the set of distinct overload signatures (so an override across
/// levels counts once), and the overload count.
struct MethodGroup {
    signature: String,
    seen_signatures: BTreeSet<String>,
    count: usize,
}

/// The accessible members of `ty` and every base in its chain. Fields and properties
/// dedup by name (a derived member hides a base one); methods of the same name collapse
/// to one entry that reports its overload count, and a value member shadows a method
/// group of the same name. Arrays expose `System.Array`. Each method inserts `name(`.
fn type_members(model: &Model, ty: &TypeSymbol) -> Vec<Completion> {
    let mut out = Vec::new();
    let mut values: BTreeSet<String> = BTreeSet::new();
    let mut method_order: Vec<Box<str>> = Vec::new();
    let mut method_groups: BTreeMap<String, MethodGroup> = BTreeMap::new();

    let mut current = model.get_by_symbol(&member_lookup_type(ty));
    while let Some(info) = current {
        for field in &info.fields {
            if values.insert(field.name.to_string()) && !method_groups.contains_key(&*field.name) {
                out.push(Completion::simple(
                    field.name.clone(),
                    CompletionKind::Field,
                    type_label(&field.ty),
                ));
            }
        }
        for property in &info.properties {
            if values.insert(property.name.to_string())
                && !method_groups.contains_key(&*property.name)
            {
                out.push(Completion::simple(
                    property.name.clone(),
                    CompletionKind::Property,
                    type_label(&property.ty),
                ));
            }
        }
        for method in &info.methods {
            if values.contains(&*method.name) {
                continue;
            }
            let signature = method_signature_label(method);
            let group = method_groups
                .entry(method.name.to_string())
                .or_insert_with(|| {
                    method_order.push(method.name.clone());
                    MethodGroup {
                        signature: signature.clone(),
                        seen_signatures: BTreeSet::new(),
                        count: 0,
                    }
                });
            if group.seen_signatures.insert(signature) {
                group.count += 1;
            }
        }
        current = info.base.as_ref().and_then(|base| model.get_by_symbol(base));
    }

    for name in &method_order {
        let group = &method_groups[&**name];
        let detail = if group.count > 1 {
            format!("{} (+{} overloads)", group.signature, group.count - 1)
        } else {
            group.signature.clone()
        };
        out.push(Completion {
            label: name.clone(),
            kind: CompletionKind::Method,
            detail: detail.into(),
            insert_text: format!("{name}(").into(),
        });
    }
    out
}

/// A method's display signature, `ReturnType Name(ParamType, ...)` -- parameter types
/// only (the model does not keep parameter names for overload display).
fn method_signature_label(method: &MethodSymbol) -> String {
    let mut label = String::new();
    label.push_str(&type_label(&method.return_type));
    label.push(' ');
    label.push_str(&method.name);
    label.push('(');
    for (index, parameter) in method.parameters.iter().enumerate() {
        if index > 0 {
            label.push_str(", ");
        }
        label.push_str(&type_label(parameter));
    }
    label.push(')');
    label
}

/// A short display label for a type (its simple name).
fn type_label(ty: &TypeSymbol) -> Box<str> {
    ty.to_string().into()
}

/// Hover information for the symbol at a caret offset: its category and a one-line
/// signature (a value's `Type name`, a method's `ReturnType Name(types)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hover {
    /// The symbol category.
    pub kind: CompletionKind,
    /// A one-line description: a typed name, a method signature, or a type/keyword note.
    pub signature: Box<str>,
}

/// Describes the symbol at byte `offset`: a member access `recv.Name`, a local or
/// parameter, a member of the enclosing type, a type name, or a keyword. `None` when the
/// caret is not on a resolvable identifier.
#[must_use]
pub fn hover(source: &str, unit: &CompilationUnit, model: &Model, offset: usize) -> Option<Hover> {
    let (start, end) = identifier_span(source, offset)?;
    let name = source.get(start..end)?;
    let bytes = source.as_bytes();
    if start > 0 && bytes[start - 1] == b'.' {
        let receiver = receiver_expression(source, start)?;
        let ty = expression_type(unit, model, offset, receiver)?;
        return describe_member(model, &ty, name);
    }
    if let Some((namespace, type_decl, method)) = enclosing_method(unit, offset) {
        if let Member::Method {
            parameters, body, ..
        } = method
        {
            for parameter in parameters {
                if &*parameter.name == name {
                    return Some(Hover {
                        kind: CompletionKind::Parameter,
                        signature: typed_name(&bind_type(&parameter.ty), name),
                    });
                }
            }
            if let Some(body) = body {
                if let Some(ty) = local_type(body, name) {
                    return Some(Hover {
                        kind: CompletionKind::Local,
                        signature: typed_name(&bind_type(ty), name),
                    });
                }
            }
        }
        if let Some(found) =
            describe_member(model, &type_symbol(&namespace, &type_decl.name), name)
        {
            return Some(found);
        }
    }
    if special_keyword(name).is_some() || model.type_with_simple_name(name).is_some() {
        return Some(Hover {
            kind: CompletionKind::Type,
            signature: format!("type {name}").into(),
        });
    }
    if KEYWORDS.contains(&name) {
        return Some(Hover {
            kind: CompletionKind::Keyword,
            signature: format!("keyword {name}").into(),
        });
    }
    None
}

/// A `Type name` description (a field, property, local, or parameter on hover).
fn typed_name(ty: &TypeSymbol, name: &str) -> Box<str> {
    format!("{} {name}", type_label(ty)).into()
}

/// Describes the member `name` on `ty` or a base: a field/property's typed name, or a
/// method's signature with its overload count.
fn describe_member(model: &Model, ty: &TypeSymbol, name: &str) -> Option<Hover> {
    let mut current = model.get_by_symbol(&member_lookup_type(ty));
    while let Some(info) = current {
        if let Some(field) = info.find_field(name) {
            return Some(Hover {
                kind: CompletionKind::Field,
                signature: typed_name(&field.ty, name),
            });
        }
        if let Some(property) = info.find_property(name) {
            return Some(Hover {
                kind: CompletionKind::Property,
                signature: typed_name(&property.ty, name),
            });
        }
        if let Some(method) = info.methods_named(name).next() {
            let overloads = info.methods_named(name).count();
            let signature = method_signature_label(method);
            let signature = if overloads > 1 {
                format!("{signature} (+{} overloads)", overloads - 1)
            } else {
                signature
            };
            return Some(Hover {
                kind: CompletionKind::Method,
                signature: signature.into(),
            });
        }
        current = info.base.as_ref().and_then(|base| model.get_by_symbol(base));
    }
    None
}

/// The identifier covering byte `offset` (its byte range), reading back and forward over
/// identifier characters. `None` when the caret is not on an identifier.
fn identifier_span(source: &str, offset: usize) -> Option<(usize, usize)> {
    let bytes = source.as_bytes();
    let mut start = offset.min(bytes.len());
    let mut end = start;
    while start > 0 && is_ident_byte(bytes[start - 1]) {
        start -= 1;
    }
    while end < bytes.len() && is_ident_byte(bytes[end]) {
        end += 1;
    }
    (start != end).then_some((start, end))
}

/// Signature help for a call whose argument list the caret is inside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureHelp {
    /// The overload signatures of the called method, most-derived first.
    pub signatures: Vec<Box<str>>,
    /// The zero-based index of the parameter the caret is on (the top-level comma count).
    pub active_parameter: usize,
}

/// Signature help for the caret at `offset`: when it sits inside `Method(...)` -- either
/// `Method(` (a method of the enclosing type) or `recv.Method(` -- the overloads of that
/// method and which parameter the caret is on. `None` when not inside a call argument
/// list or the method does not resolve.
#[must_use]
pub fn signature_help(
    source: &str,
    unit: &CompilationUnit,
    model: &Model,
    offset: usize,
) -> Option<SignatureHelp> {
    let (open_paren, active_parameter) = enclosing_call(source, offset)?;
    let bytes = source.as_bytes();
    let mut name_start = open_paren;
    while name_start > 0 && is_ident_byte(bytes[name_start - 1]) {
        name_start -= 1;
    }
    let name = source.get(name_start..open_paren)?;
    if name.is_empty() {
        return None;
    }
    let signatures = if name_start > 0 && bytes[name_start - 1] == b'.' {
        let receiver = receiver_expression(source, name_start)?;
        let ty = expression_type(unit, model, offset, receiver)?;
        method_overloads(model, &ty, name)
    } else {
        let (namespace, type_decl, _) = enclosing_method(unit, offset)?;
        method_overloads(model, &type_symbol(&namespace, &type_decl.name), name)
    };
    if signatures.is_empty() {
        return None;
    }
    Some(SignatureHelp {
        signatures,
        active_parameter,
    })
}

/// The open parenthesis of the innermost call the caret is inside and the active
/// parameter index (top-level commas before the caret). `None` when the caret is not
/// within an unclosed `(` on the current statement (a `;`/`{`/`}` at depth 0 ends it).
fn enclosing_call(source: &str, offset: usize) -> Option<(usize, usize)> {
    let bytes = source.as_bytes();
    let mut index = offset.min(bytes.len());
    let mut depth = 0i32;
    let mut commas = 0usize;
    while index > 0 {
        index -= 1;
        match bytes[index] {
            b')' => depth += 1,
            b'(' => {
                if depth == 0 {
                    return Some((index, commas));
                }
                depth -= 1;
            }
            b',' if depth == 0 => commas += 1,
            b';' | b'{' | b'}' if depth == 0 => return None,
            _ => {}
        }
    }
    None
}

/// The distinct overload signatures of methods named `name` on `ty` or any base, most-
/// derived first (an override across levels appears once).
fn method_overloads(model: &Model, ty: &TypeSymbol, name: &str) -> Vec<Box<str>> {
    let mut signatures = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut current = model.get_by_symbol(&member_lookup_type(ty));
    while let Some(info) = current {
        for method in info.methods_named(name) {
            let signature = method_signature_label(method);
            if seen.insert(signature.clone()) {
                signatures.push(signature.into());
            }
        }
        current = info.base.as_ref().and_then(|base| model.get_by_symbol(base));
    }
    signatures
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::declaration::collect_into;
    use lamella_syntax::parser::parse_compilation_unit;

    fn labels_at(source: &str, marker: &str) -> Vec<String> {
        let offset = source.find(marker).expect("marker") + marker.len();
        let unit = parse_compilation_unit(source).unit;
        let mut model = Model::new();
        collect_into(&mut model, &unit);
        model.link_bases();
        complete(source, &unit, &model, offset)
            .iter()
            .map(|item| item.label.to_string())
            .collect()
    }

    #[test]
    fn completes_members_of_a_local() {
        let source = "class Widget { public int Count; public int Area() { return 0; } } \
                      class P { static int M() { Widget w; return w.Z; } }";
        let labels = labels_at(source, "w.");
        assert!(labels.contains(&"Count".to_string()), "got {labels:?}");
        assert!(labels.contains(&"Area".to_string()), "got {labels:?}");
    }

    #[test]
    fn completes_a_qualified_member_chain() {
        let source = "class Point { public int X; public int Y; } \
                      class Line { public Point Origin; } \
                      class P { static int M() { Line line; return line.Origin.; } }";
        let labels = labels_at(source, "line.Origin.");
        assert!(labels.contains(&"X".to_string()), "got {labels:?}");
        assert!(labels.contains(&"Y".to_string()), "got {labels:?}");
    }

    #[test]
    fn completes_a_call_result_receiver() {
        let source = "class Point { public int X; public int Y; } \
                      class P { Point Make() { return new Point(); } \
                      int M() { return Make().; } }";
        let labels = labels_at(source, "Make().");
        assert!(labels.contains(&"X".to_string()), "got {labels:?}");
        assert!(labels.contains(&"Y".to_string()), "got {labels:?}");
    }

    #[test]
    fn methods_carry_signatures_overload_counts_and_insert_text() {
        let source = "class Widget { \
                      public int Area() { return 0; } \
                      public int Add(int a) { return a; } \
                      public int Add(int a, int b) { return a + b; } } \
                      class P { static int M() { Widget w; return w.; } }";
        let unit = parse_compilation_unit(source).unit;
        let mut model = Model::new();
        collect_into(&mut model, &unit);
        model.link_bases();
        let offset = source.find("w.").unwrap() + 2;
        let items = complete(source, &unit, &model, offset);

        let add = items.iter().find(|item| &*item.label == "Add").expect("Add");
        assert_eq!(add.kind, CompletionKind::Method);
        assert_eq!(&*add.insert_text, "Add(");
        assert!(add.detail.contains("Add("), "got {:?}", add.detail);
        assert!(add.detail.contains("overloads"), "got {:?}", add.detail);

        let area = items.iter().find(|item| &*item.label == "Area").expect("Area");
        assert_eq!(&*area.insert_text, "Area(");
        assert!(area.detail.contains("Area()"), "got {:?}", area.detail);
        assert!(!area.detail.contains("overloads"), "got {:?}", area.detail);
    }

    fn built_model(source: &str) -> (CompilationUnit, Model) {
        let unit = parse_compilation_unit(source).unit;
        let mut model = Model::new();
        collect_into(&mut model, &unit);
        model.link_bases();
        (unit, model)
    }

    #[test]
    fn hover_describes_a_member_access_and_a_local() {
        let source = "class Widget { public int Count; public int Area(int s) { return s; } } \
                      class P { static int M() { Widget w; return w.Count; } }";
        let (unit, model) = built_model(source);

        let count = source.find("w.Count").unwrap() + "w.C".len();
        let on_count = hover(source, &unit, &model, count).expect("hover Count");
        assert_eq!(on_count.kind, CompletionKind::Field);
        assert!(on_count.signature.contains("Count"), "got {:?}", on_count.signature);

        let local = source.find("return w.Count").unwrap() + "return ".len();
        let on_w = hover(source, &unit, &model, local).expect("hover w");
        assert_eq!(on_w.kind, CompletionKind::Local);
        assert!(on_w.signature.contains("Widget"), "got {:?}", on_w.signature);
    }

    #[test]
    fn signature_help_lists_overloads_and_the_active_parameter() {
        let source = "class Widget { \
                      public int Add(int a) { return a; } \
                      public int Add(int a, int b) { return a + b; } \
                      int M(Widget w) { return w.Add(1, ); } }";
        let (unit, model) = built_model(source);
        let offset = source.find("w.Add(1, ").unwrap() + "w.Add(1, ".len();
        let help = signature_help(source, &unit, &model, offset).expect("signature help");
        assert_eq!(help.active_parameter, 1);
        assert_eq!(help.signatures.len(), 2, "got {:?}", help.signatures);
        assert!(
            help.signatures.iter().all(|sig| sig.contains("Add(")),
            "got {:?}",
            help.signatures
        );
    }

    #[test]
    fn completes_members_of_a_parameter_and_field() {
        let source = "class Widget { public int Count; } \
                      class P { Widget f; int M(Widget p) { int a = p.Z; return f.Z; } }";
        assert!(labels_at(source, "p.").contains(&"Count".to_string()));
        assert!(labels_at(source, "f.").contains(&"Count".to_string()));
    }

    fn labels_at_offset(source: &str, offset: usize) -> Vec<String> {
        let unit = parse_compilation_unit(source).unit;
        let mut model = Model::new();
        collect_into(&mut model, &unit);
        model.link_bases();
        complete(source, &unit, &model, offset)
            .iter()
            .map(|item| item.label.to_string())
            .collect()
    }

    #[test]
    fn scope_completes_a_parameter() {
        let source = "class P { static int M(int width) { return wid; } }";
        let offset = source.rfind("wid").unwrap() + 3;
        let labels = labels_at_offset(source, offset);
        assert!(labels.contains(&"width".to_string()), "got {labels:?}");
    }

    #[test]
    fn scope_completes_a_local_and_an_enclosing_method() {
        let source = "class P { int Helper() { return 0; } int M() { int counter = 0; co } }";
        let offset = source.rfind("co").unwrap() + 2;
        let labels = labels_at_offset(source, offset);
        assert!(labels.contains(&"counter".to_string()), "got {labels:?}");
    }

    #[test]
    fn scope_completes_a_keyword() {
        let source = "class P { static int M() { retu } }";
        let offset = source.rfind("retu").unwrap() + 4;
        assert!(labels_at_offset(source, offset).contains(&"return".to_string()));
    }

    #[test]
    fn scope_prefix_filters() {
        let source = "class P { static int M() { wh } }";
        let offset = source.rfind("wh").unwrap() + 2;
        let labels = labels_at_offset(source, offset);
        assert!(labels.contains(&"while".to_string()), "got {labels:?}");
        assert!(!labels.contains(&"return".to_string()));
    }

    /// A model holding just the given `(namespace, simple name)` types, for the
    /// namespace/version-gating completion tests.
    fn model_with_types(types: &[(&str, &str)]) -> Model {
        use crate::symbols::{TypeInfo, TypeKind};
        let mut model = Model::new();
        for (namespace, name) in types {
            model.insert(TypeInfo::new(namespace, name, TypeKind::Class));
        }
        model.link_bases();
        model
    }

    fn namespace_labels(model: &Model, source: &str) -> Vec<String> {
        let unit = parse_compilation_unit(source).unit;
        complete(source, &unit, model, source.len())
            .iter()
            .map(|item| item.label.to_string())
            .collect()
    }

    #[test]
    fn completes_types_and_child_namespaces_of_a_namespace() {
        let model = model_with_types(&[
            ("System", "Console"),
            ("System", "String"),
            ("System.IO", "Stream"),
            ("System.Collections", "ArrayList"),
        ]);
        let labels = namespace_labels(&model, "System.");
        assert!(labels.contains(&"Console".to_string()), "got {labels:?}");
        assert!(labels.contains(&"String".to_string()), "got {labels:?}");
        assert!(labels.contains(&"IO".to_string()), "got {labels:?}");
        assert!(labels.contains(&"Collections".to_string()), "got {labels:?}");
        assert!(!labels.contains(&"Stream".to_string()), "got {labels:?}");
    }

    #[test]
    fn generic_types_are_not_offered_for_the_1_0_target() {
        let model = model_with_types(&[
            ("System.Collections.Generic", "List`1"),
            ("System.Collections.Generic", "Dictionary`2"),
            ("System.Collections.Generic", "Marker"),
        ]);
        let labels = namespace_labels(&model, "System.Collections.Generic.");
        assert!(
            !labels.iter().any(|label| label.contains('`')),
            "a generic leaked: {labels:?}"
        );
        assert!(labels.contains(&"Marker".to_string()), "got {labels:?}");
    }
}
