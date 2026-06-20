//! Positional semantic completion (the IntelliSense engine), clause-agnostic so both
//! the in-browser IDE (via the wasm `lamella_complete` ABI) and an LSP server can drive
//! the same logic. The first cut answers **member completion**: at `receiver.<caret>`,
//! the accessible members of the receiver's type -- its own and every inherited/BCL one.

use crate::bind::bind_type;
use crate::special::SpecialType;
use crate::symbols::Model;
use crate::types::TypeSymbol;
use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use lamella_syntax::ast::{
    CompilationUnit, Member, NamespaceMember, Stmt, StmtKind, TypeDecl, TypeRef,
};

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
    /// The text shown/inserted (the member name).
    pub label: Box<str>,
    /// The member category.
    pub kind: CompletionKind,
    /// A short type/signature hint (the member's type or return type).
    pub detail: Box<str>,
}

/// Completions for the caret at byte `offset` in `source`, given the parsed `unit` and
/// the bound `model`. The first cut returns member completions at `receiver.`; other
/// positions return an empty list (identifier-scope completion is a follow-up).
#[must_use]
pub fn complete(
    source: &str,
    unit: &CompilationUnit,
    model: &Model,
    offset: usize,
) -> Vec<Completion> {
    if let Some(receiver) = member_receiver(source, offset) {
        return receiver_type(unit, model, offset, receiver)
            .map(|ty| type_members(model, &ty))
            .unwrap_or_default();
    }
    scope_completions(source, unit, model, offset)
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
                out.push(Completion {
                    label: parameter.name.clone(),
                    kind: CompletionKind::Parameter,
                    detail: type_label(&bind_type(&parameter.ty)),
                });
            }
            if let Some(body) = body {
                walk_locals(body, &mut |ty, name| {
                    out.push(Completion {
                        label: name.into(),
                        kind: CompletionKind::Local,
                        detail: type_label(&bind_type(ty)),
                    });
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
                out.push(Completion {
                    label: name.into(),
                    kind: CompletionKind::Type,
                    detail: "type".into(),
                });
            }
        }
    }
    for keyword in KEYWORDS {
        out.push(Completion {
            label: (*keyword).into(),
            kind: CompletionKind::Keyword,
            detail: "keyword".into(),
        });
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

/// If the caret sits just after `<receiver>.<partial>`, the receiver identifier. Reads
/// backwards over the partial member name, a `.`, then the receiver name. `None` when the
/// caret is not in member-access position. Only a simple-identifier receiver for now.
fn member_receiver(source: &str, offset: usize) -> Option<&str> {
    let bytes = source.as_bytes();
    let mut index = offset.min(bytes.len());
    while index > 0 && is_ident_byte(bytes[index - 1]) {
        index -= 1;
    }
    if index == 0 || bytes[index - 1] != b'.' {
        return None;
    }
    let dot = index - 1;
    let mut start = dot;
    while start > 0 && is_ident_byte(bytes[start - 1]) {
        start -= 1;
    }
    if start == dot {
        return None;
    }
    source.get(start..dot)
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

/// The accessible members of `ty` and every base in its chain, de-duplicated by name
/// (a derived member hides a base one of the same name). Arrays expose `System.Array`.
fn type_members(model: &Model, ty: &TypeSymbol) -> Vec<Completion> {
    let mut out = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let lookup = match ty {
        TypeSymbol::Array { .. } => {
            TypeSymbol::Named([Box::from("System"), Box::from("Array")].into())
        }
        other => other.clone(),
    };
    let mut current = model.get_by_symbol(&lookup);
    while let Some(info) = current {
        for field in &info.fields {
            if seen.insert(field.name.to_string()) {
                out.push(Completion {
                    label: field.name.clone(),
                    kind: CompletionKind::Field,
                    detail: type_label(&field.ty),
                });
            }
        }
        for property in &info.properties {
            if seen.insert(property.name.to_string()) {
                out.push(Completion {
                    label: property.name.clone(),
                    kind: CompletionKind::Property,
                    detail: type_label(&property.ty),
                });
            }
        }
        for method in &info.methods {
            if seen.insert(method.name.to_string()) {
                out.push(Completion {
                    label: method.name.clone(),
                    kind: CompletionKind::Method,
                    detail: type_label(&method.return_type),
                });
            }
        }
        current = info.base.as_ref().and_then(|base| model.get_by_symbol(base));
    }
    out
}

/// A short display label for a type (its simple name).
fn type_label(ty: &TypeSymbol) -> Box<str> {
    ty.to_string().into()
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
}
