use super::*;
use lamella_syntax::parser::parse_compilation_unit;

fn codes(source: &str) -> Vec<u16> {
    let mut model = Model::new();
    model.insert(TypeInfo::new("System", "Object", TypeKind::Class));
    model.insert(TypeInfo::new("System", "Attribute", TypeKind::Class));
    let parsed = parse_compilation_unit(source);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let mut codes: Vec<_> = crate::bind_compilation_unit_with_model(&parsed.unit, model)
        .iter()
        .map(Diagnostic::code)
        .collect();
    codes.sort_unstable();
    codes
}

#[test]
fn explicit_property_provider_requires_the_requested_interface() {
    for other_type in ["int", "string"] {
        let source = alloc::format!(
            "interface IA {{ int Value {{ get; }} }} \
             interface IB {{ {other_type} Value {{ get; }} }} \
             class Cell : IA, IB {{ int IA.Value {{ get {{ return 1; }} }} }}"
        );
        assert_eq!(codes(&source), [535], "{source}");
    }
}

#[test]
fn explicit_property_provider_preserves_valid_implementations() {
    for source in [
        "interface IA { int Value { get; } } interface IB { int Value { get; } } \
         class Cell : IA, IB { int IA.Value { get { return 1; } } \
                              int IB.Value { get { return 2; } } }",
        "interface IA { int Value { get; } } interface IB { int Value { get; } } \
         class Base { public int Value { get { return 2; } } } \
         class Cell : Base, IA, IB { int IA.Value { get { return 1; } } }",
        "using N; namespace N { interface IA { int Value { get; } } } \
         class Cell : N.IA { int IA.Value { get { return 1; } } }",
        "interface I<T> { T Value { get; } } \
         class Cell : I<int>, I<string> { int I<int>.Value { get { return 1; } } \
             string I<string>.Value { get { return null; } } }",
    ] {
        assert_eq!(codes(source), [], "{source}");
    }
}

#[test]
fn explicit_property_reimplementation_walks_base_classes() {
    for source in [
        "interface IA { int Value { get; } } interface IB { int Value { get; } } \
         class Base : IB { int IB.Value { get { return 2; } } } \
         class Cell : Base, IA, IB { int IA.Value { get { return 1; } } }",
        "interface IB { int Value { get; } } \
         class Base : IB { int IB.Value { get { return 2; } } } \
         class Cell : Base, IB { }",
        "interface IB { int Value { get; set; } } \
         class Base : IB { int IB.Value { get { return 2; } set { } } } \
         class Middle : Base { } class Cell : Middle, IB { }",
        "namespace N { public interface IB { int Value { get; } } \
         public class Base : IB { int IB.Value { get { return 2; } } } } \
         class Cell : N.Base, N.IB { }",
    ] {
        assert_eq!(codes(source), [], "{source}");
    }
}

#[test]
fn explicit_property_reimplementation_skips_nonmatching_declarations() {
    for declaration in [
        "private int Value { get { return 3; } set { } }",
        "public static int Value { get { return 3; } set { } }",
        "public string Value { get { return null; } set { } }",
    ] {
        let source = alloc::format!(
            "interface IB {{ int Value {{ get; set; }} }} \
             class Base : IB {{ int IB.Value {{ get {{ return 2; }} set {{ }} }} }} \
             class Middle : Base {{ {declaration} }} class Cell : Middle, IB {{ }}"
        );
        assert_eq!(codes(&source), [], "{source}");
    }
}

#[test]
fn explicit_property_reimplementation_requires_the_exact_interface() {
    for source in [
        "interface IA { int Value { get; } } interface IB { int Value { get; } } \
         class Base : IA { int IA.Value { get { return 2; } } } class Cell : Base, IB { }",
        "interface I<T> { int Value { get; } } \
         class Base : I<int> { int I<int>.Value { get { return 2; } } } \
         class Cell : Base, I<string> { }",
    ] {
        assert_eq!(codes(source), [535], "{source}");
    }
}

#[test]
fn constructed_explicit_members_do_not_capture_a_derived_type_parameter() {
    for (declaration, implementation) in [
        ("int Value { get; }", "int I<T>.Value { get { return 2; } }"),
        ("int Read();", "int I<T>.Read() { return 2; }"),
    ] {
        for parameter in ["T", "U"] {
            for base in ["Base<string>", "Middle<string>"] {
                let source = alloc::format!(
                    "interface I<T> {{ {declaration} }} \
                     class Base<T> : I<T> {{ {implementation} }} \
                     class Middle<V> : Base<V> {{ }} \
                     class Cell<{parameter}> : {base}, I<{parameter}> {{ }}"
                );
                assert_eq!(codes(&source), [535], "{source}");
            }
        }
    }
}

#[test]
fn constructed_explicit_members_implement_the_substituted_interface() {
    for (declaration, implementation) in [
        ("int Value { get; }", "int I<T>.Value { get { return 2; } }"),
        ("int Read();", "int I<T>.Read() { return 2; }"),
    ] {
        for (class, base, argument) in [
            ("Cell", "Base<int>", "int"),
            ("Cell", "Middle<int>", "int"),
            ("Cell<T>", "Base<T>", "T"),
            ("Cell<U>", "Middle<U>", "U"),
        ] {
            let source = alloc::format!(
                "interface I<T> {{ {declaration} }} \
                 class Base<T> : I<T> {{ {implementation} }} \
                 class Middle<V> : Base<V> {{ }} \
                 class {class} : {base}, I<{argument}> {{ }}"
            );
            assert_eq!(codes(&source), [], "{source}");
        }
    }
}

fn global_argument_cases(declaration: &str, implementation: &str) {
    for (base_declarations, base) in [
        (
            alloc::format!(
                "class Base<T> : I<T> {{ {implementation} }} \
             class Middle : Base<Foo> {{ }}"
            ),
            "Middle",
        ),
        (
            alloc::format!(
                "class Base : I<Foo> {{ {} }}",
                implementation.replace("I<T>", "I<Foo>")
            ),
            "Base",
        ),
    ] {
        let source = alloc::format!(
            "class Foo {{ }} interface I<T> {{ {declaration} }} \
             {base_declarations} class Cell<Foo> : {base}, I<Foo> {{ }}"
        );
        assert_eq!(codes(&source), [535], "{source}");
    }
}

#[test]
fn explicit_property_preserves_global_argument_identity() {
    global_argument_cases("int Value { get; }", "int I<T>.Value { get { return 2; } }");
}

#[test]
fn explicit_method_preserves_global_argument_identity() {
    global_argument_cases("int Read();", "int I<T>.Read() { return 2; }");
}

fn namespace_qualifier_cases(declaration: &str, implementation: &str) {
    for (requested, expected) in [("IV", alloc::vec![535]), ("global::IV", alloc::vec![])] {
        let source = alloc::format!(
            "interface IV {{ {declaration} }} class Base : IV {{ {implementation} }} \
             namespace N {{ interface IV {{ {declaration} }} class Cell : Base, {requested} {{ }} }}"
        );
        assert_eq!(codes(&source), expected, "{source}");
    }
}

#[test]
fn explicit_property_preserves_base_namespace_identity() {
    namespace_qualifier_cases("int Value { get; }", "int IV.Value { get { return 2; } }");
}

#[test]
fn explicit_method_preserves_base_namespace_identity() {
    namespace_qualifier_cases("int Read();", "int IV.Read() { return 2; }");
}

#[test]
fn explicit_members_preserve_open_and_global_argument_controls() {
    for (declaration, implementation) in [
        ("int Value { get; }", "int I<T>.Value { get { return 2; } }"),
        ("int Read();", "int I<T>.Read() { return 2; }"),
    ] {
        for derived in [
            "class Cell<Foo> : Base<Foo>, I<Foo> { }",
            "class Cell<U> : Base<U>, I<U> { }",
            "class Cell : Base<Foo>, I<Foo> { }",
            "class Middle : Base<Foo> { } class Cell : Middle, I<Foo> { }",
        ] {
            let source = alloc::format!(
                "class Foo {{ }} interface I<T> {{ {declaration} }} \
                 class Base<T> : I<T> {{ {implementation} }} {derived}"
            );
            assert_eq!(codes(&source), [], "{source}");
        }
    }
}

/// An intermediate generic base whose own parameter is spelled like the global type its base
/// list names. Model symbols spell a type parameter by its simple name, so constructing `Mid<int>`
/// would rewrite the global `Foo` too and credit `I<int>` with `I<Foo>`'s implementation: the
/// declaration is refused (LAM0001) rather than mapped through the wrong type.
#[test]
fn an_intermediate_parameter_spelled_like_a_global_base_argument_is_refused() {
    for (declaration, implementation) in [
        ("int Value { get; }", "int I<T>.Value { get { return 2; } }"),
        ("int Read();", "int I<T>.Read() { return 2; }"),
    ] {
        for (directive, argument) in [("", "global::Foo"), ("using GlobalFoo = Foo; ", "GlobalFoo")] {
            let source = alloc::format!(
                "{directive}class Foo {{ }} interface I<T> {{ {declaration} }} \
                 class Base<T> : I<T> {{ {implementation} }} \
                 class Mid<Foo> : Base<{argument}> {{ }} class Cell : Mid<int>, I<int> {{ }}"
            );
            assert_eq!(codes(&source), [1], "{source}");
        }
    }
}

#[test]
fn a_global_type_spelled_like_a_type_parameter_in_scope_is_refused_in_signatures() {
    for source in [
        "class Foo { } class Base<T> { public T Get(T value) { return value; } } \
         class Mid<Foo> : Base<global::Foo> { }",
        "class Foo { } class Mid<Foo> { global::Foo Make() { return null; } }",
        "class Foo { } class C { static void M<Foo>(global::Foo value) { } }",
        "using GlobalFoo = Foo; class Foo { } class Mid<Foo> { GlobalFoo Make() { return null; } }",
    ] {
        assert_eq!(codes(source), [1], "{source}");
    }
}

#[test]
fn a_global_type_beside_differently_spelled_type_parameters_still_binds() {
    for (declaration, implementation) in [
        ("int Value { get; }", "int I<T>.Value { get { return 2; } }"),
        ("int Read();", "int I<T>.Read() { return 2; }"),
    ] {
        for derived in [
            "class Mid<U> : Base<global::Foo> { } class Cell : Mid<int>, I<Foo> { }",
            "using GlobalFoo = Foo; class Mid<U> : Base<GlobalFoo> { } class Cell : Mid<int>, I<Foo> { }",
            "class Mid<Foo> : Base<Foo> { } class Cell : Mid<int>, I<int> { }",
            "class Cell : Base<global::Foo>, I<Foo> { }",
            "namespace N { class Foo { } } class Mid<Foo> : Base<global::N.Foo> { } \
             class Cell : Mid<int>, I<N.Foo> { }",
        ] {
            let (directive, rest) = derived.split_at(if derived.starts_with("using") {
                derived.find(';').map_or(0, |end| end + 2)
            } else {
                0
            });
            let source = alloc::format!(
                "{directive}class Foo {{ }} interface I<T> {{ {declaration} }} \
                 class Base<T> : I<T> {{ {implementation} }} {rest}"
            );
            assert_eq!(codes(&source), [], "{source}");
        }
    }
}

#[test]
fn explicit_interface_identity_follows_nested_arguments_and_distinct_parameters() {
    for (declaration, implementation) in [
        ("int Value { get; }", "int I<T>.Value { get { return 2; } }"),
        ("int Read();", "int I<T>.Read() { return 2; }"),
    ] {
        for (base, requested, expected) in [
            ("Closed", "Box<Foo[]>", alloc::vec![535]),
            ("Middle<Foo>", "Box<Foo[]>", alloc::vec![]),
            ("Middle<Bar>", "Box<Foo[]>", alloc::vec![535]),
            ("Middle<Bar>", "Box<Bar[]>", alloc::vec![]),
        ] {
            let source = alloc::format!(
                "class Foo {{ }} class Box<X> {{ }} interface I<T> {{ {declaration} }} \
                 class Base<T> : I<T> {{ {implementation} }} \
                 class Middle<V> : Base<Box<V[]>> {{ }} class Closed : Middle<Foo> {{ }} \
                 class Cell<Foo, Bar> : {base}, I<{requested}> {{ }}"
            );
            assert_eq!(codes(&source), expected, "{source}");
        }
    }
}

#[test]
fn interface_identity_mapping_preserves_generic_member_signatures() {
    for source in [
        "interface I<T> { T Read(T value); T Value { get; } } \
         class Base<T> : I<T> { T I<T>.Read(T value) { return value; } \
             T I<T>.Value { get { return default(T); } } } \
         class Cell<T> : Base<T>, I<T> { }",
        "interface I<T> { void Read<U>(U value); } \
         class Base<U> : I<U> { void I<U>.Read<U>(U value) { } } \
         class Cell<U> : Base<U>, I<U> { }",
        "interface I<T> { void Read<U>(U value); } \
         class Base<U> : I<U> { public void Read<U>(U value) { } } \
         class Cell<U> : Base<U>, I<U> { }",
    ] {
        assert_eq!(codes(source), [], "{source}");
    }
}

#[test]
fn constructed_explicit_members_preserve_qualified_interface_identity() {
    for (declaration, implementation) in [
        (
            "int Value { get; }",
            "int N.I<T>.Value { get { return 2; } }",
        ),
        ("int Read();", "int N.I<T>.Read() { return 2; }"),
    ] {
        for (base_argument, requested, expected) in [
            ("int", "N.I<int>", alloc::vec![]),
            ("string", "N.I<int>", alloc::vec![535]),
            ("int", "I<int>", alloc::vec![535]),
        ] {
            let source = alloc::format!(
                "namespace N {{ interface I<T> {{ {declaration} }} \
                     class Base<T> : I<T> {{ {implementation} }} }} \
                 interface I<T> {{ {declaration} }} \
                 class Cell : N.Base<{base_argument}>, {requested} {{ }}"
            );
            assert_eq!(codes(&source), expected, "{source}");
        }
    }
}

#[test]
fn explicit_property_accessor_mapping_uses_only_exact_accessors() {
    for (property, method, is_accessor, expected) in [
        (
            "int IProp.Value { get { return 1; } set { } }",
            "get_Value",
            true,
            true,
        ),
        (
            "int IProp.Value { get { return 1; } set { } }",
            "set_Value",
            true,
            true,
        ),
        (
            "public int Value { get { return 1; } set { } }",
            "get_Value",
            true,
            true,
        ),
        (
            "public int Value { get { return 1; } set { } }",
            "set_Value",
            true,
            true,
        ),
        (
            "int IOther.Value { get { return 1; } }",
            "get_Value",
            true,
            false,
        ),
        (
            "string IProp.Value { get { return null; } }",
            "get_Value",
            true,
            false,
        ),
        (
            "public string Value { get { return null; } }",
            "get_Value",
            true,
            false,
        ),
        (
            "public static int Value { get { return 1; } }",
            "get_Value",
            true,
            false,
        ),
        (
            "private int Value { get { return 1; } }",
            "get_Value",
            true,
            false,
        ),
        (
            "public int Value { private get { return 1; } set { } }",
            "get_Value",
            true,
            false,
        ),
        (
            "public int Value { get { return 1; } private set { } }",
            "set_Value",
            true,
            false,
        ),
        ("int IProp.Value { set { } }", "get_Value", true, false),
        (
            "int IProp.Value { get { return 1; } }",
            "get_Value",
            false,
            false,
        ),
        (
            "public int Value { get { return 1; } }",
            "get_Value",
            false,
            false,
        ),
    ] {
        let source = alloc::format!(
            "interface IProp {{ int Value {{ get; set; }} }} \
             interface IOther {{ int Value {{ get; }} }} \
             interface Slots {{ int get_Value(); void set_Value(int value); }} \
             class Impl : IProp {{ {property} }} class Leaf : Impl, IProp {{ }}"
        );
        let model = crate::declaration::collect_model(&parse_compilation_unit(&source).unit);
        let mut member = model
            .get("", "Slots")
            .unwrap()
            .methods_named(method)
            .next()
            .unwrap()
            .clone();
        member.is_accessor = is_accessor;
        for context in ["Impl", "Leaf"] {
            let mut binder = Binder::with_model(model.clone());
            let class = named_symbol_from_dotted(context);
            binder.enter_type(class.clone());
            let status =
                binder.interface_member_status(&class, &named_symbol_from_dotted("IProp"), &member);
            assert_eq!(
                status == InterfaceMemberStatus::Implemented,
                expected,
                "{context}: {property}, {method}, accessor={is_accessor}: {status:?}"
            );
        }
    }
}

#[test]
fn explicit_property_accessor_does_not_implement_another_interfaces_method() {
    assert_eq!(
        codes(
            "interface IA { int Value { get; } } interface IB { int get_Value(); } \
         class Cell : IA, IB { int IA.Value { get { return 1; } } }"
        ),
        [535]
    );
}

fn attribute_source(ordinary_first: bool, inherited: bool) -> String {
    let explicit = "int ITag.Value { get { return 1; } set { } }";
    let ordinary = "public string Value { get { return \"\"; } set { } }";
    let (base_members, members) = if inherited {
        (ordinary, explicit.to_string())
    } else if ordinary_first {
        ("", alloc::format!("{ordinary} {explicit}"))
    } else {
        ("", alloc::format!("{explicit} {ordinary}"))
    };
    alloc::format!(
        "interface ITag {{ int Value {{ get; set; }} }} \
         class BaseAttribute : System.Attribute {{ {base_members} }} \
         class MarkAttribute : BaseAttribute, ITag {{ public MarkAttribute() {{ }} {members} }} \
         [Mark(Value = \"ok\")] class C {{ }}"
    )
}

#[test]
fn explicit_property_attribute_lookup_uses_ordinary_members() {
    for (first, inherited) in [(false, false), (true, false), (false, true)] {
        let source = attribute_source(first, inherited);
        assert_eq!(codes(&source), [], "{source}");
    }
    assert_eq!(
        codes(
            "interface ITag { int Value { get; set; } } \
         class MarkAttribute : System.Attribute, ITag { public MarkAttribute() { } \
             int ITag.Value { get { return 1; } set { } } } \
         [Mark(Value = 1)] class C { }"
        ),
        [246]
    );
}

#[test]
fn explicit_property_attribute_binding_preserves_named_value() {
    use lamella_syntax::ast::NamespaceMember;
    for (first, inherited) in [(false, false), (true, false), (false, true)] {
        let source = attribute_source(first, inherited);
        let parsed = parse_compilation_unit(&source);
        assert!(parsed.diagnostics.is_empty());
        let NamespaceMember::Type(decorated) = parsed.unit.members.last().unwrap() else {
            panic!("decorated type");
        };
        let mut model = Model::new();
        model.insert(TypeInfo::new("System", "Object", TypeKind::Class));
        model.insert(TypeInfo::new("System", "Attribute", TypeKind::Class));
        crate::declaration::collect_into(&mut model, &parsed.unit);
        model.link_bases();
        let mut binder = Binder::with_model(model);
        let attribute = binder
            .bind_attribute(
                Some(&named_symbol_from_dotted("C")),
                &decorated.attributes[0].attributes[0],
            )
            .expect("named attribute must remain bound");
        assert_eq!(attribute.named.len(), 1);
        let value = &attribute.named[0];
        assert_eq!(&*value.name, "Value");
        assert!(!value.is_field);
        assert_eq!(value.ty, TypeSymbol::Special(SpecialType::String));
        assert_eq!(
            value.value,
            AttributeValue::Constant(Literal::String(Box::from([111, 107])))
        );
        assert!(
            binder.diagnostics().is_empty(),
            "{:?}",
            binder.diagnostics()
        );
    }
}
