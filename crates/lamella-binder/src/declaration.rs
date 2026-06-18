//! Collecting the types declared in source (ECMA-334 1st ed, clauses 16-17).

use crate::resolve::TypeTable;
use alloc::string::String;
use lamella_syntax::ast::{CompilationUnit, NamespaceMember, QualifiedName};

/// Builds a [`TypeTable`] of every type declared in `unit`.
#[must_use]
pub fn collect_types(unit: &CompilationUnit) -> TypeTable {
    let mut table = TypeTable::new();
    for member in &unit.members {
        collect_member(member, "", &mut table);
    }
    table
}

fn collect_member(member: &NamespaceMember, namespace: &str, table: &mut TypeTable) {
    match member {
        NamespaceMember::Namespace(declaration) => {
            let inner = join_namespace(namespace, &declaration.name);
            for inner_member in &declaration.members {
                collect_member(inner_member, &inner, table);
            }
        }
        NamespaceMember::Type(declaration) => table.insert(namespace, &declaration.name),
        NamespaceMember::Enum(declaration) => table.insert(namespace, &declaration.name),
        NamespaceMember::Delegate(declaration) => table.insert(namespace, &declaration.name),
    }
}

/// Appends a (possibly dotted) namespace declaration name to the enclosing
/// namespace, e.g. `"A"` and `B.C` give `"A.B.C"`.
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
    fn collects_top_level_namespaced_and_nested_namespace_types() {
        let unit = parse_compilation_unit(
            "class Bar {} enum E { A } \
             namespace A.B { class Foo {} delegate void D(); \
                namespace C { struct S {} } }",
        )
        .unit;
        let table = collect_types(&unit);
        assert!(table.contains("", "Bar"));
        assert!(table.contains("", "E"));
        assert!(table.contains("A.B", "Foo"));
        assert!(table.contains("A.B", "D"));
        assert!(table.contains("A.B.C", "S"));
        assert!(!table.contains("", "Foo"));
        assert!(!table.contains("", "Missing"));
    }
}
