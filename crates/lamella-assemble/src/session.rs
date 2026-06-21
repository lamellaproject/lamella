//! The incremental-REPL compiler session (tiers 2/3).

use crate::EmitError;
use crate::compile::{Diagnostic, build_bootstrap_delta, build_submission_delta};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use lamella_binder::{
    Binder, Model, TypeSymbol, bind_compilation_unit_with_model, collect_into, load_assembly,
};
use lamella_metadata::Assembly;
use lamella_syntax::ast::{CompilationUnit, NamespaceMember, UsingDirective, UsingKind};
use lamella_syntax::parser::parse_submission;
use lamella_syntax::span::Span;

/// One accumulated session variable -- a persistent instance field of `__Repl`.
struct SessionField {
    /// The C# name the user wrote.
    source_name: Box<str>,
    /// The stable metadata field name on `__Repl` (equal to the source name until a
    /// type-changing redefinition renames it to `x$2` -- a following increment).
    stable_name: Box<str>,
    /// The field's type.
    ty: TypeSymbol,
}

/// A persistent incremental-REPL compile session: the accumulated `__Repl` shape (its
/// session variables), the reference world, and the submission counter.
///
/// [`Session::compile_submission`] binds each submission against the prior shape and
/// returns a [`SubmissionResult`] whose `delta` the runtime loads. Open a session with
/// [`Session::new`] and load [`Session::bootstrap`] once before the first submission.
pub struct Session {
    /// The reference world (the BCL), loaded once and cloned per submission to bind against.
    base_model: Model,
    /// The session variables, in declaration order.
    fields: Vec<SessionField>,
    /// The namespaces imported by `using` directives so far, in order (deduped), brought
    /// into scope for every submission so names resolve without qualification.
    imports: Vec<Box<str>>,
    /// The types declared by submissions so far, in order. Each is emitted as a TypeDef in
    /// its declaring delta (the runtime adds it to the module) and kept here so later
    /// submissions bind + reference it by name.
    types: Vec<NamespaceMember>,
    /// Submissions compiled so far; the next is `counter + 1`, so the emitted methods are
    /// `Submit$1`, `Submit$2`, ... on holder types `Submission$1`, `Submission$2`, ...
    counter: u64,
}

/// The outcome of compiling one submission: its diagnostics and, when they are clean and
/// emission succeeds, the delta assembly.
pub struct SubmissionResult {
    /// The parse diagnostics then the bind diagnostics, in that order.
    pub diagnostics: Vec<Diagnostic>,
    /// The emitted delta assembly image, or `None` when a diagnostic error -- or a
    /// construct that is not lowered yet -- blocked emission.
    pub delta: Option<Vec<u8>>,
    /// Why emission produced no image, when binding was clean but a construct is not
    /// lowered yet.
    pub emit_error: Option<EmitError>,
}

impl Session {
    /// Opens a session over `references` (the BCL / parity reference set), loaded once
    /// into the base model. The session starts with no variables; the bootstrap module
    /// (an empty `__Repl`) is [`Session::bootstrap`].
    #[must_use]
    pub fn new(references: &[Assembly]) -> Session {
        let mut base_model = Model::new();
        for reference in references {
            load_assembly(&mut base_model, reference);
        }
        base_model.link_bases();
        Session {
            base_model,
            fields: Vec::new(),
            imports: Vec::new(),
            types: Vec::new(),
            counter: 0,
        }
    }

    /// The one-time bootstrap module the runtime loads at session open: a library
    /// assembly defining an empty `<repl>.__Repl` the runtime instantiates once. Constant
    /// for a session (independent of any submission), so a caller emits it before the
    /// first [`Session::compile_submission`].
    ///
    /// # Errors
    /// Returns the [`EmitError`] if the bootstrap body cannot be written (not expected).
    pub fn bootstrap(&self) -> Result<Vec<u8>, EmitError> {
        build_bootstrap_delta("__repl", "__repl")
    }

    /// Compiles one REPL submission against the session's accumulated state, returning its
    /// diagnostics and -- when clean -- the delta assembly to load. On a clean compile the
    /// submission's new session variables are committed (visible to later submissions);
    /// on any error the session is left unchanged so a retry is not skewed.
    pub fn compile_submission(&mut self, source: &str) -> SubmissionResult {
        let parsed = parse_submission(source);
        let parse_diagnostics: Vec<Diagnostic> = parsed
            .diagnostics
            .iter()
            .map(Diagnostic::from_syntax)
            .collect();
        if parse_diagnostics.iter().any(Diagnostic::is_error) {
            return SubmissionResult {
                diagnostics: parse_diagnostics,
                delta: None,
                emit_error: None,
            };
        }

        let new_imports = self.new_imports(&parsed.usings);

        let model = self.build_model(&parsed.types);
        let repl_type = repl_type_symbol();

        let mut diagnostics = parse_diagnostics;
        if !parsed.types.is_empty() {
            let types_unit = CompilationUnit {
                usings: Vec::new(),
                members: parsed.types.clone(),
                span: Span::empty_at(0),
            };
            diagnostics.extend(
                bind_compilation_unit_with_model(&types_unit, model.clone())
                    .iter()
                    .map(Diagnostic::from_binder),
            );
        }

        let initial_fields = self.field_table();
        let occurrences = self.occurrences();
        let mut binder = Binder::with_model(model.clone());
        for namespace in self.imports.iter().chain(new_imports.iter()) {
            binder.import_namespace(namespace);
        }
        let binding = binder.bind_submission(
            repl_type.clone(),
            "s",
            &parsed.statements,
            parsed.trailing.as_ref(),
            initial_fields,
            occurrences,
        );
        diagnostics.extend(binder.into_diagnostics().iter().map(Diagnostic::from_binder));
        if diagnostics.iter().any(Diagnostic::is_error) {
            return SubmissionResult {
                diagnostics,
                delta: None,
                emit_error: None,
            };
        }

        let index = self.counter + 1;
        let module_name = format!("__repl.submission{index}");
        match build_submission_delta(
            &binding.body,
            &repl_type,
            index,
            &binding.return_type,
            &parsed.types,
            &model,
            &module_name,
            &module_name,
        ) {
            Ok(delta) => {
                for field in binding.new_fields {
                    self.fields.push(SessionField {
                        source_name: field.source,
                        stable_name: field.stable,
                        ty: field.ty,
                    });
                }
                self.imports.extend(new_imports);
                self.types.extend(parsed.types);
                self.counter = index;
                SubmissionResult {
                    diagnostics,
                    delta: Some(delta),
                    emit_error: None,
                }
            }
            Err(error) => SubmissionResult {
                diagnostics,
                delta: None,
                emit_error: Some(error),
            },
        }
    }

    /// The full reference world for binding a submission: the references plus every declared
    /// type (prior plus this submission's `new_types`), with base chains linked. The common
    /// case (no declared types) is just the references.
    fn build_model(&self, new_types: &[NamespaceMember]) -> Model {
        let mut model = self.base_model.clone();
        if !self.types.is_empty() || !new_types.is_empty() {
            let members: Vec<NamespaceMember> =
                self.types.iter().chain(new_types.iter()).cloned().collect();
            let unit = CompilationUnit {
                usings: Vec::new(),
                members,
                span: Span::empty_at(0),
            };
            collect_into(&mut model, &unit);
            model.link_bases();
        }
        model
    }

    /// The session-variable resolution table for the binder: each source name -> its
    /// CURRENT stable `__Repl` field name + type. The accumulated fields are walked in
    /// declaration order, so a redefined name ends mapped to its newest field (`x$2`).
    fn field_table(&self) -> BTreeMap<String, (Box<str>, TypeSymbol)> {
        let mut table = BTreeMap::new();
        for field in &self.fields {
            table.insert(
                field.source_name.to_string(),
                (field.stable_name.clone(), field.ty.clone()),
            );
        }
        table
    }

    /// How many times each source name has already been declared, so the binder picks a
    /// fresh `x$2`/`x$3` for the next redefinition.
    fn occurrences(&self) -> BTreeMap<String, u32> {
        let mut counts = BTreeMap::new();
        for field in &self.fields {
            *counts.entry(field.source_name.to_string()).or_insert(0) += 1;
        }
        counts
    }

    /// The namespaces a submission's `using` directives import that are not already in
    /// scope, as dotted strings, deduped against the session's imports and within the list.
    /// Alias directives (`using X = T;`) are not handled yet.
    fn new_imports(&self, usings: &[UsingDirective]) -> Vec<Box<str>> {
        let mut new_imports: Vec<Box<str>> = Vec::new();
        for directive in usings {
            let UsingKind::Namespace(name) = &directive.kind else {
                continue;
            };
            let mut dotted = String::new();
            for part in &name.parts {
                if !dotted.is_empty() {
                    dotted.push('.');
                }
                dotted.push_str(part);
            }
            let already = self
                .imports
                .iter()
                .chain(new_imports.iter())
                .any(|namespace| **namespace == *dotted);
            if !already {
                new_imports.push(dotted.into());
            }
        }
        new_imports
    }
}

/// The symbol of the persistent REPL type `<repl>.__Repl`.
fn repl_type_symbol() -> TypeSymbol {
    TypeSymbol::Named(["<repl>", "__Repl"].into_iter().map(Box::<str>::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_metadata::Assembly;
    use lamella_metadata::tables::table;

    /// The full `namespace.name` of every `TypeDef`, `TypeRef`, and the names of every
    /// `MemberRef` and `MethodDef` in an emitted delta -- and a structural check that the
    /// image is valid metadata.
    struct Delta<'a> {
        type_defs: Vec<String>,
        type_refs: Vec<String>,
        field_refs: Vec<(String, String)>,
        methods: Vec<String>,
        assembly: Assembly<'a>,
    }

    fn full(name: lamella_metadata::TypeName) -> String {
        if name.namespace.is_empty() {
            name.name.to_string()
        } else {
            format!("{}.{}", name.namespace, name.name)
        }
    }

    fn read_delta(image: &[u8]) -> Delta<'_> {
        let assembly = Assembly::read(image).expect("delta is valid metadata");
        let type_defs = assembly.type_defs().filter_map(|t| t.name().map(full)).collect();
        let type_refs = assembly.type_refs().filter_map(|t| t.name().map(full)).collect();
        let field_refs = assembly
            .member_refs()
            .filter_map(|member| {
                let name = member.name()?.to_string();
                let parent = member.parent();
                if parent.table() != table::TYPE_REF {
                    return None;
                }
                let parent_name = assembly
                    .type_ref(parent.row())
                    .and_then(|t| t.name())
                    .map(full)?;
                Some((name, parent_name))
            })
            .collect();
        let mut methods = Vec::new();
        let mut index = 1;
        while let Some(method) = assembly.method(index) {
            if let Some(name) = method.name() {
                methods.push(name.to_string());
            }
            index += 1;
        }
        Delta {
            type_defs,
            type_refs,
            field_refs,
            methods,
            assembly,
        }
    }

    /// The compiler half of incremental REPL increment 1: `int x = 5;` then
    /// `int y = x + 1;` produce two deltas referencing the persistent `__Repl` by name.
    #[test]
    fn two_statement_submissions_emit_deltas_referencing_repl_by_name() {
        let mut session = Session::new(&[]);

        let bootstrap = session.bootstrap().expect("bootstrap emits");
        let boot = read_delta(&bootstrap);
        assert!(boot.type_defs.iter().any(|t| t == "<repl>.__Repl"));
        let repl = boot
            .assembly
            .type_defs()
            .find(|t| t.name().map(full).as_deref() == Some("<repl>.__Repl"))
            .expect("__Repl TypeDef");
        assert!(repl.methods().filter_map(|m| m.name()).any(|n| n == ".ctor"));
        assert!(!boot.type_refs.iter().any(|t| t == "<repl>.__Repl"));

        let first = session.compile_submission("int x = 5;");
        assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);
        let delta1 = read_delta(first.delta.as_ref().expect("delta 1 emitted"));
        assert!(delta1.type_refs.iter().any(|t| t == "<repl>.__Repl"));
        assert!(!delta1.type_defs.iter().any(|t| t == "<repl>.__Repl"));
        assert!(delta1.type_defs.iter().any(|t| t == "<repl>.Submission$1"));
        assert!(delta1.methods.iter().any(|m| m == "Submit$1"));
        assert!(
            delta1
                .field_refs
                .iter()
                .any(|(name, parent)| name == "x" && parent == "<repl>.__Repl"),
            "field refs: {:?}",
            delta1.field_refs
        );

        let second = session.compile_submission("int y = x + 1;");
        assert!(second.diagnostics.is_empty(), "{:?}", second.diagnostics);
        let delta2 = read_delta(second.delta.as_ref().expect("delta 2 emitted"));
        assert!(delta2.type_defs.iter().any(|t| t == "<repl>.Submission$2"));
        assert!(delta2.methods.iter().any(|m| m == "Submit$2"));
        for field in ["x", "y"] {
            assert!(
                delta2
                    .field_refs
                    .iter()
                    .any(|(name, parent)| name == field && parent == "<repl>.__Repl"),
                "delta 2 missing FieldRef {field}: {:?}",
                delta2.field_refs
            );
        }
    }

    /// A name that resolves to no session variable or type is `CS0103`, and blocks the
    /// delta -- the session is left unchanged so the next submission is not skewed.
    #[test]
    fn unresolved_name_is_a_diagnostic_and_emits_no_delta() {
        let mut session = Session::new(&[]);
        let result = session.compile_submission("int y = q + 1;");
        assert!(result.delta.is_none());
        assert!(result.diagnostics.iter().any(|d| d.code == 103));
        let retry = session.compile_submission("int y = 2;");
        assert!(retry.diagnostics.is_empty(), "{:?}", retry.diagnostics);
        let delta = read_delta(retry.delta.as_ref().expect("retry delta"));
        assert!(
            delta
                .field_refs
                .iter()
                .any(|(name, parent)| name == "y" && parent == "<repl>.__Repl")
        );
        assert!(delta.methods.iter().any(|m| m == "Submit$1"));
    }

    /// A submission that declares a type emits its full TypeDef (alongside the holder +
    /// Submit$N) in the delta; a later submission references the type BY NAME (a TypeRef,
    /// like __Repl), never re-defining it.
    #[test]
    fn declared_type_emits_a_typedef_then_later_submissions_reference_it() {
        let mut session = Session::new(&[]);

        let decl = session.compile_submission("class Foo { public int X; }");
        assert!(decl.diagnostics.is_empty(), "{:?}", decl.diagnostics);
        let d1 = read_delta(decl.delta.as_ref().expect("delta 1"));
        assert!(
            d1.type_defs.iter().any(|t| t == "Foo"),
            "expected a Foo TypeDef: {:?}",
            d1.type_defs
        );
        assert!(d1.type_defs.iter().any(|t| t == "<repl>.Submission$1"));
        assert!(d1.methods.iter().any(|m| m == "Submit$1"));

        let use_it = session.compile_submission("Foo f = new Foo();");
        assert!(use_it.diagnostics.is_empty(), "{:?}", use_it.diagnostics);
        let d2 = read_delta(use_it.delta.as_ref().expect("delta 2"));
        assert!(
            d2.type_refs.iter().any(|t| t == "Foo"),
            "expected a Foo TypeRef: {:?}",
            d2.type_refs
        );
        assert!(!d2.type_defs.iter().any(|t| t == "Foo"));
        assert!(
            d2.field_refs
                .iter()
                .any(|(name, parent)| name == "f" && parent == "<repl>.__Repl")
        );
    }

    /// The return type (Void / Object) of `Submit$<index>` in an emitted delta.
    fn submit_return(image: &[u8], index: u32) -> lamella_metadata::SigType {
        let assembly = Assembly::read(image).expect("delta is valid metadata");
        let name = format!("Submit${index}");
        (1..)
            .map_while(|i| assembly.method(i))
            .find(|method| method.name() == Some(&name))
            .and_then(|method| method.signature())
            .expect("Submit$N has a signature")
            .return_type
    }

    /// A trailing bare expression is the submission's display value: `Submit$N` returns it
    /// boxed to object, while a `;`-terminated statement submission returns void.
    #[test]
    fn trailing_expression_returns_boxed_object_statement_returns_void() {
        use lamella_metadata::SigType;
        let mut session = Session::new(&[]);

        let first = session.compile_submission("int x = 5;");
        assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);
        assert_eq!(
            submit_return(first.delta.as_ref().expect("delta 1"), 1),
            SigType::Void
        );

        let second = session.compile_submission("x * 2");
        assert!(second.diagnostics.is_empty(), "{:?}", second.diagnostics);
        let image = second.delta.as_ref().expect("delta 2");
        assert_eq!(submit_return(image, 2), SigType::Object);
        let assembly = Assembly::read(image).expect("valid metadata");
        assert!(
            assembly
                .type_refs()
                .filter_map(|t| t.name())
                .any(|n| n.namespace == "System" && n.name == "Int32")
        );
        assert!(
            assembly
                .member_refs()
                .filter_map(|m| m.name())
                .any(|n| n == "x")
        );
    }

    /// A type-changing redefinition adds a fresh field `x$2` (the old `x` stays, harmless)
    /// and rebinds the source name, so a later read of `x` resolves to `x$2`.
    #[test]
    fn redefinition_renames_the_field_and_rebinds_the_name() {
        use lamella_metadata::SigType;
        let mut session = Session::new(&[]);

        let first = session.compile_submission("int x = 5;");
        assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);
        let d1 = read_delta(first.delta.as_ref().expect("delta 1"));
        assert!(
            d1.field_refs
                .iter()
                .any(|(name, parent)| name == "x" && parent == "<repl>.__Repl")
        );

        let second = session.compile_submission("string x = \"hi\";");
        assert!(second.diagnostics.is_empty(), "{:?}", second.diagnostics);
        let d2 = read_delta(second.delta.as_ref().expect("delta 2"));
        assert!(
            d2.field_refs
                .iter()
                .any(|(name, parent)| name == "x$2" && parent == "<repl>.__Repl"),
            "expected a FieldRef x$2: {:?}",
            d2.field_refs
        );
        assert!(
            !d2.field_refs.iter().any(|(name, _)| name == "x"),
            "the redefinition must not reference the old x: {:?}",
            d2.field_refs
        );

        let third = session.compile_submission("x");
        assert!(third.diagnostics.is_empty(), "{:?}", third.diagnostics);
        let image = third.delta.as_ref().expect("delta 3");
        let d3 = read_delta(image);
        assert!(
            d3.field_refs
                .iter()
                .any(|(name, parent)| name == "x$2" && parent == "<repl>.__Repl"),
            "the read of x must resolve to x$2: {:?}",
            d3.field_refs
        );
        assert_eq!(submit_return(image, 3), SigType::Object);
    }

    /// A `using` directive accumulates in the session and brings its namespace into scope,
    /// so a later submission resolves an unqualified name from it (CS0246 without it).
    #[test]
    fn using_directive_brings_a_namespace_into_scope() {
        use lamella_syntax::parser::parse_compilation_unit;

        let parsed = parse_compilation_unit("namespace Foo { public class Bar { } }");
        let refs = crate::compile_unit(&parsed.unit, "refs", "refs")
            .image
            .expect("reference image");
        let reference = Assembly::read(&refs).expect("reference assembly");

        let mut bare = Session::new(core::slice::from_ref(&reference));
        let unqualified = bare.compile_submission("Bar b = new Bar();");
        assert!(unqualified.delta.is_none());
        assert!(
            unqualified.diagnostics.iter().any(|d| d.code == 246),
            "{:?}",
            unqualified.diagnostics
        );

        let mut session = Session::new(core::slice::from_ref(&reference));
        let directive = session.compile_submission("using Foo;");
        assert!(directive.diagnostics.is_empty(), "{:?}", directive.diagnostics);
        assert!(directive.delta.is_some());
        let resolved = session.compile_submission("Bar b = new Bar();");
        assert!(resolved.diagnostics.is_empty(), "{:?}", resolved.diagnostics);
        let delta = read_delta(resolved.delta.as_ref().expect("delta"));
        assert!(
            delta.type_refs.iter().any(|t| t == "Foo.Bar"),
            "the delta should reference Foo.Bar: {:?}",
            delta.type_refs
        );
    }

    /// A method declared inside a session type emits in its TypeDef, and a later submission
    /// can call it (a MemberRef on the now-external type) and display its result.
    #[test]
    fn declared_type_methods_emit_and_are_callable() {
        use lamella_metadata::SigType;
        let mut session = Session::new(&[]);

        let decl = session.compile_submission(
            "class Counter { public int N; public int Next() { N = N + 1; return N; } }",
        );
        assert!(decl.diagnostics.is_empty(), "{:?}", decl.diagnostics);
        let d1 = read_delta(decl.delta.as_ref().expect("delta 1"));
        assert!(d1.type_defs.iter().any(|t| t == "Counter"));
        assert!(
            d1.methods.iter().any(|m| m == "Next"),
            "expected a Next method: {:?}",
            d1.methods
        );

        let create = session.compile_submission("Counter c = new Counter();");
        assert!(create.diagnostics.is_empty(), "{:?}", create.diagnostics);

        let call = session.compile_submission("c.Next()");
        assert!(call.diagnostics.is_empty(), "{:?}", call.diagnostics);
        let image = call.delta.as_ref().expect("delta 3");
        let assembly = Assembly::read(image).expect("valid metadata");
        assert!(
            assembly
                .member_refs()
                .filter_map(|m| m.name())
                .any(|n| n == "Next")
        );
        assert_eq!(submit_return(image, 3), SigType::Object);
    }

    /// A STATIC method with a parameter on a session type is callable as `Type.Method(arg)`
    /// from a later submission (a static MemberRef on the external type).
    #[test]
    fn declared_type_static_method_with_a_parameter_is_callable() {
        use lamella_metadata::SigType;
        let mut session = Session::new(&[]);

        let decl =
            session.compile_submission("class M { public static int Twice(int n) { return n * 2; } }");
        assert!(decl.diagnostics.is_empty(), "{:?}", decl.diagnostics);
        assert!(
            read_delta(decl.delta.as_ref().expect("delta 1"))
                .methods
                .iter()
                .any(|m| m == "Twice")
        );

        let call = session.compile_submission("M.Twice(21)");
        assert!(call.diagnostics.is_empty(), "{:?}", call.diagnostics);
        let image = call.delta.as_ref().expect("delta 2");
        let assembly = Assembly::read(image).expect("valid metadata");
        assert!(
            assembly
                .member_refs()
                .filter_map(|m| m.name())
                .any(|n| n == "Twice")
        );
        assert_eq!(submit_return(image, 2), SigType::Object);
    }
}
